//! 一条被控会话：握手鉴权 → 采集/差分/编码/发送 + 接收线程（输入 / ack / ping）。

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lrc_adapt::{Controller, ControllerConfig, LinkStats, Mode as AdaptMode, Target};
use lrc_capture::cursor::CursorWatcher;
use lrc_capture::{CaptureError, Capturer, Frame, FrameView, MoveRect, ScreenInfo};
use lrc_codec::{self, Options};
use lrc_proto::{
    auth_reason, frame_flags, AuthResult, BlockEntry, ClientHello, CursorPos, CursorShape,
    DisplayInfo, Grid, Message, Pong, Rect, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::link::{LinkReader, LinkWriter};
use lrc_udp::UdpStats;
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::auth;
use crate::cli::Args;
use crate::input;
use crate::stats::{self, Stats};
use crate::video::{Mode, ModeEvent, ModeSwitch, VideoCounters, VideoPipeline};

/// 块边长（DESIGN §0：64×64 块）。
const BLOCK_SIZE: u16 = 64;
/// 握手阶段单条消息的读/写超时，防止"连上不说话"的连接占住单流。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 会话期间单次写的超时：对端完全不读时不能永久阻塞。
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// 未确认字节上限（DESIGN §1 的滑动窗口）：超过就丢帧、不排队。
const MAX_INFLIGHT_BYTES: u64 = 8 * 1024 * 1024;
/// 没有新帧时的轮询间隔：Ok(None) 后睡一下再 grab（零发送 + 低 CPU 的来源）。
const IDLE_POLL: Duration = Duration::from_millis(2);
/// 读取线程的轮询粒度：TCP/UDP 统一用它做 recv_timeout，
/// 既能让 UDP 内部状态机（重传/保活）按时跑，也能让停止标志及时生效。
const READ_POLL: Duration = Duration::from_millis(200);
/// 帧率节流时单次睡眠的上限，保证停止标志/光标能被及时处理。
const MAX_THROTTLE_SLEEP: Duration = Duration::from_millis(20);
/// 采集器连续重建失败的次数上限，超过就结束这条会话。
const CAPTURE_REOPEN_TRIES: u32 = 20;
/// 并联判据②：块模式实际码率超过这个值（kbps）……
const BLOCK_OVER_KBPS: f64 = 6_000.0;
/// ……连续这么多秒，就切视频模式（窗口化播视频时 changed_ratio 只有 20-30%，不能只靠它）。
const BLOCK_OVER_SECS: u32 = 3;
/// CursorShape 消息的边长上限：lrc-proto 的 MAX_CURSOR_BYTES = 256*256*4，
/// 超过客户端会解不动（更大会话就断），所以宁可不发形状只发位置。
const MAX_CURSOR_SHAPE_DIM: u32 = 256;

/// 会话期的共享状态：写端 + 客户端反馈 + 停止标志。
///
/// pub(crate) 是因为视频编码线程（crate::video）也要通过它写 socket。
pub(crate) struct Shared {
    /// 收发线程共享同一个写端；一帧必须整条原子写入。
    writer: Mutex<LinkWriter>,
    /// 累计发出的线上字节（含 4 字节长度前缀；UDP 分片开销不计）
    sent_bytes: AtomicU64,
    /// 主线程要求结束（接收线程断开 / 发送失败）。
    stop: AtomicBool,
    /// 接收线程视角：客户端已断开。
    peer_gone: AtomicBool,
    /// 客户端最近一次 ack 的 seq。
    ack_seq: AtomicU32,
    /// 是否收到过至少一条 VideoAck。没收到过就不做基于 ack 的流控，
    /// 只靠 TCP 背压 + 写超时兜底（避免客户端没实现 ack 时被误判为拥塞而完全停发）。
    ack_seen: AtomicBool,
    /// 客户端最近一次上报的累计接收字节（用于估算在途字节）。
    ack_bytes: AtomicU64,
    /// 会话起点（算 ack RTT 用）
    session_start: Instant,
    /// 最近一次发出帧的时刻（会话内毫秒）
    last_send_ms: AtomicU64,
    /// 最近一次发出帧的 seq
    last_sent_seq: AtomicU32,
    /// ack 往返（毫秒，平滑值；没有 ack 时为 0）
    rtt_ms: AtomicU32,
    /// 最近一次收到 ack 的时刻（会话内毫秒）
    last_ack_ms: AtomicU64,
    /// UDP 路径的 RTT（毫秒 ×10，避免浮点原子；TCP 路径恒 0）
    udp_rtt_x10: AtomicU32,
    /// UDP 路径的丢包（千分比）
    udp_loss_permille: AtomicU32,
    /// 客户端请求整帧。
    request_full: AtomicBool,
    /// 当前采集输出在虚拟桌面里的原点（输入坐标换算用）。
    origin_x: AtomicI32,
    origin_y: AtomicI32,
    /// 客户端请求切换显示输出（-1 = 无请求）
    select_display: AtomicI32,
}

impl Shared {
    fn new(writer: LinkWriter, origin: (i32, i32)) -> Self {
        Self {
            writer: Mutex::new(writer),
            sent_bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            peer_gone: AtomicBool::new(false),
            ack_seq: AtomicU32::new(0),
            ack_seen: AtomicBool::new(false),
            ack_bytes: AtomicU64::new(0),
            session_start: Instant::now(),
            last_send_ms: AtomicU64::new(0),
            last_sent_seq: AtomicU32::new(0),
            rtt_ms: AtomicU32::new(0),
            last_ack_ms: AtomicU64::new(0),
            udp_rtt_x10: AtomicU32::new(0),
            udp_loss_permille: AtomicU32::new(0),
            request_full: AtomicBool::new(false),
            origin_x: AtomicI32::new(origin.0),
            origin_y: AtomicI32::new(origin.1),
            select_display: AtomicI32::new(-1),
        }
    }

    /// 锁中毒时照常取回：socket 还能用，没有理由因此断流。
    fn writer_guard(&self) -> std::sync::MutexGuard<'_, LinkWriter> {
        match self.writer.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }

    /// 写一条已编码消息（同时累计线上字节，供流控用）。
    pub(crate) fn send(&self, payload: &[u8]) -> io::Result<()> {
        self.writer_guard().send(payload)?;
        // TCP 的 4 字节长度前缀也算；UDP 的分片头不精确统计（流控只需要量级）
        self.sent_bytes
            .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        Ok(())
    }

    fn shutdown(&self) {
        self.writer_guard().shutdown();
    }

    fn stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 已发出但客户端还没确认的字节数（估算，用于流控）。
    pub(crate) fn inflight_bytes(&self) -> u64 {
        self.sent_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(self.ack_bytes.load(Ordering::Relaxed))
    }

    fn ack_seq(&self) -> u32 {
        self.ack_seq.load(Ordering::Relaxed)
    }

    /// 会话内毫秒时钟（ack RTT 用；不跨会话）
    fn elapsed_ms(&self) -> u64 {
        self.session_start.elapsed().as_millis() as u64
    }

    /// 记下"刚发出一帧"（块帧或视频 AU 都算），供 ack RTT 反算。
    pub(crate) fn note_send(&self, seq: u32) {
        self.last_sent_seq.store(seq, Ordering::Relaxed);
        self.last_send_ms.store(self.elapsed_ms(), Ordering::Relaxed);
    }

    /// 收到 ack：如果确认的就是最近发出的那一帧，就得到一次 RTT 采样（做简单平滑）。
    ///
    /// 注：Ping/Pong 里 host 侧算不出 RTT（Pong 只是回显客户端的 ts），所以这里用
    /// 帧级 ack 反算——含义是"发出这一帧 → 收到它的 ack"，含客户端解码+回执时间。
    fn note_ack(&self, seq: u32) {
        self.ack_seq.store(seq, Ordering::Relaxed);
        self.last_ack_ms.store(self.elapsed_ms(), Ordering::Relaxed);
        // 客户端 ack 的往往不是"最近一帧"（会滞后一两帧），所以用"收到 ack 时距上次发送的时间"
        // 作为往返代理；这比只匹配 last_sent_seq 稳健得多（后者会长期采不到样）。
        let sent = self.last_send_ms.load(Ordering::Relaxed);
        if sent == 0 {
            return;
        }
        let now = self.elapsed_ms();
        let sample = now.saturating_sub(sent);
        // 空闲期后的第一个 ack 距上次发送很远，那不是 RTT，丢掉这种样本
        if sample > 500 {
            return;
        }
        let sample = sample as u32;
        let prev = self.rtt_ms.load(Ordering::Relaxed);
        let smoothed = if prev == 0 {
            sample
        } else {
            (prev.saturating_mul(3) + sample) / 4
        };
        self.rtt_ms.store(smoothed, Ordering::Relaxed);
    }

    /// 平滑后的 ack RTT（毫秒）；一次都没采到时是 0。
    fn rtt_ms(&self) -> u32 {
        self.rtt_ms.load(Ordering::Relaxed)
    }

    /// 接收线程把 UDP 链路的 RTT/丢包发布出来（TCP 路径不会调用）。
    fn set_udp_stats(&self, st: UdpStats) {
        let rtt = if st.rtt_ms.is_finite() && st.rtt_ms > 0.0 {
            (st.rtt_ms * 10.0).min(60_000.0) as u32
        } else {
            0
        };
        self.udp_rtt_x10.store(rtt, Ordering::Relaxed);
        self.udp_loss_permille
            .store(u32::from(st.loss_permille), Ordering::Relaxed);
    }

    /// UDP 链路 RTT（毫秒）；没有 UDP 或还没采样到就是 0。
    fn udp_rtt_ms(&self) -> u32 {
        self.udp_rtt_x10.load(Ordering::Relaxed) / 10
    }

    fn udp_loss_permille(&self) -> u16 {
        u16::try_from(self.udp_loss_permille.load(Ordering::Relaxed)).unwrap_or(u16::MAX)
    }

    /// 基于 ack 的流控是否可信：收到过 ack 且最近 2 秒内还有 ack。
    ///
    /// 客户端如果只 ack 块帧、不 ack 视频 AU，未确认字节会一直涨；这种情况下
    /// 不能按它丢帧，改由 TCP 背压 + 写超时兜底。
    pub(crate) fn ack_fresh(&self) -> bool {
        if !self.ack_seen.load(Ordering::Relaxed) {
            return false;
        }
        let last = self.last_ack_ms.load(Ordering::Relaxed);
        last != 0 && self.elapsed_ms().saturating_sub(last) <= 2_000
    }

    fn set_origin(&self, origin: (i32, i32)) {
        self.origin_x.store(origin.0, Ordering::Relaxed);
        self.origin_y.store(origin.1, Ordering::Relaxed);
    }

    fn origin(&self) -> (i32, i32) {
        (
            self.origin_x.load(Ordering::Relaxed),
            self.origin_y.load(Ordering::Relaxed),
        )
    }

    /// 连续发多条消息（一次持锁，中间不会插进音频/视频线程的消息）。
    /// 大帧拆条用：保证同一条逻辑帧的分片在线上是连续的。
    pub(crate) fn send_batch(&self, payloads: &[Vec<u8>]) -> io::Result<()> {
        let mut w = self.writer_guard();
        let mut total = 0u64;
        for p in payloads {
            w.send(p)?;
            total = total.saturating_add(p.len() as u64 + 4);
        }
        self.sent_bytes.fetch_add(total, Ordering::Relaxed);
        Ok(())
    }

    /// 接收线程登记"客户端要切显示输出"。
    fn request_display(&self, index: u8) {
        self.select_display.store(i32::from(index), Ordering::Relaxed);
    }

    /// 采集线程取走切换请求（-1 = 无）。
    fn take_display_request(&self) -> Option<u8> {
        let v = self.select_display.swap(-1, Ordering::Relaxed);
        if v < 0 {
            None
        } else {
            u8::try_from(v).ok()
        }
    }
}

/// 握手期的拒绝原因。
#[derive(Debug)]
enum Reject {
    Io(io::Error),
    /// 握手超时（连上但不说话；LinkReader 的 recv_timeout 分不出"超时"和"对端关闭"）
    Timeout,
    Version(u16),
    Proto(String),
    BadMessage(String),
    BadPassword,
    Auth(auth::AuthError),
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reject::Io(e) => write!(f, "网络读写失败 {e}"),
            Reject::Timeout => write!(f, "握手超时/对端未发消息"),
            Reject::Version(v) => write!(f, "协议版本不匹配 {v} != {PROTO_VERSION}"),
            Reject::Proto(m) => write!(f, "消息解析失败 {m}"),
            Reject::BadMessage(m) => write!(f, "握手顺序错误 {m}"),
            Reject::BadPassword => write!(f, "密码错误"),
            Reject::Auth(e) => write!(f, "鉴权计算失败 {e}"),
        }
    }
}

impl std::error::Error for Reject {}

/// 处理一条已就绪的链路（TCP 或可靠 UDP），直到会话结束。
/// 返回 Ok(()) 表示可以继续接入下一条。
pub fn run(link: crate::path::AcquiredLink, args: &Args, nonce_seq: u64) -> Result<(), String> {
    let crate::path::AcquiredLink {
        writer,
        reader,
        path,
        udp,
    } = link;
    let (mut writer, mut reader) = (writer, reader);
    log::info!("会话建立：path={path}");
    if let Some(u) = udp.as_ref() {
        log::info!(
            "UDP 链路：local={:?} peer={:?} mode={:?}",
            u.local_addr().ok(),
            u.peer_addr(),
            u.mode()
        );
    }
    if let Err(e) = writer.set_write_timeout(Some(HANDSHAKE_TIMEOUT)) {
        log::warn!("设置握手写超时失败：{e}");
    }

    // 1) ClientHello：先看版本，便宜的检查放在建采集器之前
    let hello = match recv_client_hello(&mut reader) {
        Ok(h) => h,
        Err(Reject::Version(v)) => {
            log::warn!("拒绝：客户端协议 {v}，本机只支持 {PROTO_VERSION}");
            let msg = Message::AuthResult(AuthResult {
                ok: false,
                reason: auth_reason::VERSION_MISMATCH,
            })
            .encode();
            let _ = writer.send(&msg);
            writer.shutdown();
            return Ok(());
        }
        Err(why) => {
            log::warn!("握手失败：{why}");
            writer.shutdown();
            return Ok(());
        }
    };

    // 2) 采集器：ServerHello 里的屏幕尺寸必须来自真实输出
    let (mut capturer, mut info) = match open_capturer(args.display) {
        Ok(v) => v,
        Err(e) => {
            log::error!("初始化采集失败：{e}");
            let msg = Message::AuthResult(AuthResult {
                ok: false,
                reason: auth_reason::RATE_LIMITED,
            })
            .encode();
            let _ = writer.send(&msg);
            writer.shutdown();
            return Ok(());
        }
    };

    let nonce_s = auth::make_nonce(nonce_seq);
    let srv_hello = Message::ServerHello(ServerHello {
        version: PROTO_VERSION,
        width: info.width,
        height: info.height,
        rotation: info.rotation,
        nonce_s,
    });
    if let Err(e) = writer.send(&srv_hello.encode()) {
        log::warn!("发送 ServerHello 失败：{e}");
        writer.shutdown();
        return Ok(());
    }
    log::info!(
        "采集输出 #{}: {}x{} origin=({}, {}) rotation={}",
        args.display,
        info.width,
        info.height,
        info.origin_x,
        info.origin_y,
        info.rotation
    );

    // 3) 鉴权（HMAC-SHA256，msg = nonce_c || nonce_s）
    if let Err(why) = await_auth(&mut reader, &mut writer, &args.password, &hello.nonce_c, &nonce_s)
    {
        log::warn!("鉴权失败：{why}");
        writer.shutdown();
        return Ok(());
    }
    log::info!(
        "客户端已鉴权，开始推流（视频通道 --video={}，{}kbps，{}fps 上限）",
        args.video.label(),
        args.video_bitrate / 1000,
        args.video_max_fps
    );

    // 显示输出列表：客户端据此做"切换显示器"菜单（之后会回 SelectDisplay）
    match lrc_capture::list_outputs() {
        Ok(list) => {
            let infos: Vec<DisplayInfo> = list
                .iter()
                .enumerate()
                .map(|(i, s)| DisplayInfo {
                    index: u8::try_from(i).unwrap_or(u8::MAX),
                    width: s.width,
                    height: s.height,
                    origin_x: s.origin_x,
                    origin_y: s.origin_y,
                    // 主显示器在虚拟桌面里恒为 (0,0)
                    primary: s.origin_x == 0 && s.origin_y == 0,
                })
                .collect();
            let payload = Message::DisplayList(infos).encode();
            match writer.send(&payload) {
                Ok(()) => log::info!("已下发 DisplayList（{} 个输出）", list.len()),
                Err(e) => log::warn!("发送 DisplayList 失败：{e}"),
            }
        }
        Err(e) => log::warn!("枚举显示输出失败，跳过 DisplayList：{e}"),
    }

    // 会话期间：写端保留超时（对端不读时不能永久阻塞）；
    // 读端用 200ms 轮询式 recv_timeout（TCP/UDP 同一套语义，见 reader_loop）
    if let Err(e) = writer.set_write_timeout(Some(SEND_TIMEOUT)) {
        log::warn!("设置写超时失败：{e}");
    }

    let shared = Arc::new(Shared::new(writer, (info.origin_x, info.origin_y)));
    // 视频通道：会话建立时创建编码器 + 预热探测（失败就一直用块模式，不重试）
    let pipeline = crate::video::start(args, info.width, info.height, Arc::clone(&shared));
    // 音频通道（M3）：独立线程；设备/编码器不可用就告警一次并永久关闭
    let mut audio_pipe = if args.audio {
        crate::audio::start(Arc::clone(&shared))
    } else {
        log::info!("音频通道被 --audio off 关闭");
        None
    };

    let spawned = {
        let sh = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("lrc-host-recv".to_string())
            .spawn(move || reader_loop(reader, sh))
    };
    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            log::error!("启动接收线程失败：{e}");
            shared.shutdown();
            return Err(format!("启动接收线程失败：{e}"));
        }
    };

    let session_clock = Instant::now();
    let result = capture_loop(
        args,
        &mut capturer,
        &mut info,
        Arc::clone(&shared),
        pipeline,
        path,
    );

    // 音频通道收尾 + 统计
    if let Some(a) = audio_pipe.as_mut() {
        let (packets, bytes, fmt) = a.stats();
        let secs = session_clock.elapsed().as_secs_f64().max(0.001);
        let kbps = (bytes as f64 * 8.0 / 1000.0) / secs;
        log::info!(
            "音频通道：{} 包 / {} 字节（实测 {:.1} kbps，{}Hz/{}ch）",
            packets,
            bytes,
            kbps,
            fmt.sample_rate,
            fmt.channels
        );
        a.stop();
    }

    // 收尾：先让接收线程的阻塞读返回，再等它退出
    shared.stop.store(true, Ordering::Relaxed);
    shared.shutdown();
    if handle.join().is_err() {
        log::error!("接收线程 panic");
    }
    if shared.peer_gone.load(Ordering::Relaxed) {
        log::info!("会话结束：客户端已断开");
    } else {
        log::info!("会话结束：服务端主动关闭");
    }
    result
}

/// 握手阶段单条消息的读超时（TCP/UDP 通用：LinkReader::recv_timeout 超时返回 Ok(None)）。
fn recv_msg_timeout(reader: &mut LinkReader, buf: &mut Vec<u8>, timeout: Duration) -> Result<usize, Reject> {
    match reader.recv_timeout(buf, timeout).map_err(Reject::Io)? {
        Some(n) => Ok(n),
        // 超时 = 对端连上但不说话
        None => Err(Reject::Timeout),
    }
}

fn open_capturer(display: usize) -> Result<(Capturer, ScreenInfo), CaptureError> {
    // timeout_ms = 0：非阻塞轮询。没有变化时 grab() 返回 Ok(None)，一个字节都不发。
    let capturer = Capturer::new(display, 0)?;
    let info = capturer.info();
    Ok((capturer, info))
}

/// 等 ClientHello 并检查协议版本。
fn recv_client_hello(reader: &mut LinkReader) -> Result<ClientHello, Reject> {
    let mut buf = Vec::with_capacity(256);
    recv_msg_timeout(reader, &mut buf, HANDSHAKE_TIMEOUT)?;
    match Message::decode(&buf) {
        Ok(Message::ClientHello(hello)) => {
            if hello.version != PROTO_VERSION {
                return Err(Reject::Version(hello.version));
            }
            log::debug!(
                "ClientHello version={} client_id={:02x?}",
                hello.version,
                hello.client_id
            );
            Ok(hello)
        }
        Ok(other) => Err(Reject::BadMessage(format!(
            "期待 ClientHello，收到 type=0x{:02x}",
            other.msg_type()
        ))),
        Err(e) => Err(Reject::Proto(e.to_string())),
    }
}

/// 等 AuthRequest、校验 HMAC、回 AuthResult。
fn await_auth(
    reader: &mut LinkReader,
    writer: &mut LinkWriter,
    password: &str,
    nonce_c: &[u8; auth::NONCE_LEN],
    nonce_s: &[u8; auth::NONCE_LEN],
) -> Result<(), Reject> {
    let mut buf = Vec::with_capacity(128);
    recv_msg_timeout(reader, &mut buf, HANDSHAKE_TIMEOUT)?;
    let mac = match Message::decode(&buf) {
        Ok(Message::AuthRequest(req)) => req.mac,
        Ok(other) => {
            return Err(Reject::BadMessage(format!(
                "期待 AuthRequest，收到 type=0x{:02x}",
                other.msg_type()
            )))
        }
        Err(e) => return Err(Reject::Proto(e.to_string())),
    };
    let matched = auth::verify(password, nonce_c, nonce_s, &mac).map_err(Reject::Auth)?;
    if !matched {
        let msg = Message::AuthResult(AuthResult {
            ok: false,
            reason: auth_reason::BAD_PASSWORD,
        })
        .encode();
        let _ = writer.send(&msg);
        return Err(Reject::BadPassword);
    }
    let ok = Message::AuthResult(AuthResult {
        ok: true,
        reason: auth_reason::OK,
    })
    .encode();
    writer.send(&ok).map_err(Reject::Io)?;
    Ok(())
}

/// 接收线程：阻塞读，直到对端关闭或主线程 shutdown。
fn reader_loop(mut reader: LinkReader, shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    loop {
        if shared.stop() {
            break;
        }
        // 统一用 recv_timeout：TCP 下等价于原来的阻塞读（超时返回 None 再轮询），
        // UDP 下还能顺便让 UdpConn 内部状态机（重传/保活）跑起来。
        match reader.recv_timeout(&mut buf, READ_POLL) {
            Ok(Some(n)) => {
                if let Some(st) = reader.udp_stats() {
                    shared.set_udp_stats(st);
                }
                handle_incoming(&buf[..n], &shared)
            }
            Ok(None) => {
                if let Some(st) = reader.udp_stats() {
                    shared.set_udp_stats(st);
                }
                continue;
            }
            Err(e) => {
                // 对端断开的各种表现（含 wire-codec 强调的 UnexpectedEof）都算正常收尾
                let kind = e.kind();
                let peer_closed = matches!(
                    kind,
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                );
                if peer_closed || shared.stop() {
                    log::info!("客户端连接结束（{e}）");
                } else {
                    log::warn!("读取失败：{e}");
                }
                break;
            }
        }
    }
    shared.peer_gone.store(true, Ordering::Relaxed);
    shared.stop.store(true, Ordering::Relaxed);
    // 唤醒可能阻塞在 send 里的主线程
    shared.shutdown();
}

fn handle_incoming(payload: &[u8], shared: &Shared) {
    let msg = match Message::decode(payload) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("丢弃无法解析的消息：{e}");
            return;
        }
    };
    match msg {
        Message::Input(ev) => {
            let origin = shared.origin();
            if let Err(e) = input::apply(ev, origin) {
                log::warn!("输入注入失败 {ev:?}：{e}");
            }
        }
        Message::VideoAck(ack) => {
            shared.note_ack(ack.seq);
            shared.ack_seen.store(true, Ordering::Relaxed);
            shared.ack_bytes.store(ack.recv_bytes, Ordering::Relaxed);
            log::trace!("ack seq={} recv_bytes={}", ack.seq, ack.recv_bytes);
        }
        Message::RequestFull => {
            shared.request_full.store(true, Ordering::Relaxed);
            log::info!("客户端请求整帧");
        }
        Message::SelectDisplay(sel) => {
            log::info!("客户端请求切换显示输出 → #{}", sel.index);
            shared.request_display(sel.index);
        }
        Message::Ping(ping) => {
            let pong = Message::Pong(Pong { ts_ms: ping.ts_ms }).encode();
            if let Err(e) = shared.send(&pong) {
                log::warn!("回 Pong 失败：{e}");
            }
        }
        other => log::debug!("忽略消息 type=0x{:02x}", other.msg_type()),
    }
}

/// 块级状态：跟客户端已知画面保持一致。
struct Track {
    grid: Grid,
    width: u32,
    height: u32,
    /// 客户端已知状态的块哈希（真正发出去之后才更新，所以丢帧不会丢内容）
    hashes: Vec<u64>,
    /// 是否已经至少发过一帧
    have_prev: bool,
    /// 下一帧是否忽略 dirty 提示全盘重扫（丢帧 / 重建之后自愈用）
    scan_all: bool,
}

impl Track {
    fn new(width: u32, height: u32) -> Option<Self> {
        let grid = Grid::new(BLOCK_SIZE, width, height)?;
        Some(Self {
            grid,
            width,
            height,
            hashes: vec![0u64; grid.count() as usize],
            have_prev: false,
            scan_all: true,
        })
    }
}

/// 一块编码结果（复用 Vec 缓冲，避免每帧反复分配）。
#[derive(Default)]
struct BlockSlot {
    idx: u32,
    codec: u8,
    hash: u64,
    data: Vec<u8>,
}

/// 自适应控制器的"落地参数"：把 Target 变成主循环真正用的值（纯逻辑，可单测）。
#[derive(Debug, Clone, Copy)]
struct AdaptState {
    /// 是否启用（--budget-bps > 0）
    enabled: bool,
    fps_limit: u32,
    block_quality: u8,
    scale: f32,
    video_bitrate: u32,
    /// 控制器是否允许视频模式（内容驱动的 ModeSwitch 还要再判一次，取更保守的）
    allow_video: bool,
    level: u8,
    stage: &'static str,
}

impl AdaptState {
    fn new(args: &Args) -> Self {
        Self {
            enabled: args.budget_bps > 0,
            fps_limit: args.fps.max(1),
            block_quality: args.quality,
            scale: 1.0,
            video_bitrate: args.video_bitrate,
            allow_video: true,
            level: 0,
            stage: "原生",
        }
    }

    /// 应用控制器目标；返回参数是否有变化。
    ///
    /// 注意 --fps 仍然是硬上限（用户显式给的），控制器只能在它之下往下调。
    fn apply(&mut self, target: &Target, args: &Args, video_present: bool) -> bool {
        let mut changed = false;
        let fps = u32::from(target.fps.max(1)).min(args.fps.max(1));
        if fps != self.fps_limit {
            self.fps_limit = fps;
            changed = true;
        }
        if target.block_quality != self.block_quality {
            self.block_quality = target.block_quality;
            changed = true;
        }
        let scale = if target.resolution_scale.is_finite() {
            target.resolution_scale.clamp(0.25, 1.0)
        } else {
            1.0
        };
        if (scale - self.scale).abs() > 0.01 {
            self.scale = scale;
            changed = true;
        }
        let bitrate = target.video_bitrate_bps.max(1);
        if bitrate != self.video_bitrate {
            self.video_bitrate = bitrate;
            changed = true;
        }
        self.allow_video = video_present && matches!(target.mode, AdaptMode::Video);
        changed
    }

    fn scale_active(&self) -> bool {
        (self.scale - 1.0).abs() > 0.01
    }
}

/// 一秒的采样（喂控制器 + 打 stats）。
#[derive(Debug, Clone, Copy)]
struct SecondSample {
    interval_secs: f64,
    /// 这一秒的线上总字节
    bytes: u64,
    /// 其中视频 AU 的字节（用来算"纯块模式码率"）
    video_bytes: u64,
    dropped: u32,
    fps: f32,
}

/// 并联判据②的判定：块模式实际码率是否明显超过"整帧视频"的代价。
///
/// 阈值取 max(6 Mbps, --video-bitrate × 1.5)：这样即使把视频码率配得很低，
/// 只要块模式码率远高于它，切视频依然划算。
fn block_bitrate_trigger(block_kbps: f64, video_bitrate_bps: u32) -> bool {
    let threshold = BLOCK_OVER_KBPS.max(f64::from(video_bitrate_bps) / 1000.0 * 1.5);
    block_kbps > threshold
}

/// 缩放后的画面尺寸：取 block_size 的整数倍，保证块网格没有半块。
fn scaled_dims(width: u32, height: u32, scale: f32) -> (u32, u32) {
    let bs = u32::from(BLOCK_SIZE);
    let s = if scale.is_finite() {
        scale.clamp(0.25, 1.0)
    } else {
        1.0
    };
    let round_blocks = |v: u32| -> u32 {
        let scaled = (v as f32 * s).round().max(bs as f32) as u32;
        let blocks = ((scaled + bs / 2) / bs).max(1);
        (blocks * bs).min(v.max(bs))
    };
    (round_blocks(width), round_blocks(height))
}

/// 面积平均（box filter）降采样 BGRA。dst 会被 resize 成 4*sw*sh。
fn downscale_bgra(
    src: &[u8],
    src_stride: usize,
    src_w: u32,
    src_h: u32,
    dst: &mut Vec<u8>,
    dst_w: u32,
    dst_h: u32,
) -> bool {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return false;
    }
    let dst_stride = dst_w as usize * 4;
    dst.clear();
    dst.resize(dst_stride.saturating_mul(dst_h as usize), 0);
    let (sw, sh) = (src_w as usize, src_h as usize);
    for dy in 0..dst_h as usize {
        let sy0 = dy * sh / dst_h as usize;
        let sy1 = (((dy + 1) * sh / dst_h as usize).max(sy0 + 1)).min(sh);
        let row_out = dy * dst_stride;
        for dx in 0..dst_w as usize {
            let sx0 = dx * sw / dst_w as usize;
            let sx1 = (((dx + 1) * sw / dst_w as usize).max(sx0 + 1)).min(sw);
            let mut sum = [0u32; 4];
            let mut n = 0u32;
            for sy in sy0..sy1 {
                let row = sy * src_stride;
                for sx in sx0..sx1 {
                    let p = row + sx * 4;
                    let Some(px) = src.get(p..p + 4) else {
                        continue;
                    };
                    sum[0] += u32::from(px[0]);
                    sum[1] += u32::from(px[1]);
                    sum[2] += u32::from(px[2]);
                    sum[3] += u32::from(px[3]);
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let o = row_out + dx * 4;
            dst[o] = (sum[0] / n) as u8;
            dst[o + 1] = (sum[1] / n) as u8;
            dst[o + 2] = (sum[2] / n) as u8;
            dst[o + 3] = (sum[3] / n) as u8;
        }
    }
    true
}

/// 当前采集输出的映射口径：输入注入和光标反算都用它（切换显示器后就换它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayMapping {
    index: usize,
    /// 该输出在虚拟桌面里的原点
    origin: (i32, i32),
    /// 该输出的原生尺寸
    size: (u32, u32),
}

impl DisplayMapping {
    fn from_screen(index: usize, info: &ScreenInfo) -> Self {
        Self {
            index,
            origin: (info.origin_x, info.origin_y),
            size: (info.width, info.height),
        }
    }

    /// 客户端本地坐标 → 虚拟桌面绝对坐标（0..=65535），给 SendInput 用。
    ///
    /// 生产路径上输入注入在接收线程里按 Shared::origin 算（那边只存 origin），
    /// 这里保留给单测验证"切显示输出后同一本地坐标落到不同屏"。
    #[cfg(test)]
    fn to_absolute(self, local: (i32, i32), vd: input::VirtualDesktop) -> (i32, i32) {
        input::local_to_absolute(local.0, local.1, self.origin, vd)
    }

    /// 桌面光标坐标 → 画面本地坐标 + 是否在画面内。
    fn to_local(self, desk: (i32, i32), scale: f32) -> ((i32, i32), bool) {
        map_cursor_pos(desk, self.origin, self.size, scale)
    }
}

/// 一帧像素的统一视图：既可以是零拷贝借用的 FrameView，也可以是拥有的 Frame。
/// 块路径只认这个类型，所以"零拷贝/拷贝"两种来源走同一套差分+编码代码。
#[derive(Clone, Copy, Debug)]
struct Pixels<'a> {
    width: u32,
    height: u32,
    stride: usize,
    bgra: &'a [u8],
    dirty: &'a [Rect],
    moves: &'a [MoveRect],
    full: bool,
    ts_ms: u64,
}

impl<'a> Pixels<'a> {
    fn from_frame(f: &'a Frame) -> Self {
        Self {
            width: f.width,
            height: f.height,
            stride: f.stride,
            bgra: &f.bgra,
            dirty: &f.dirty,
            moves: &f.moves,
            full: f.full,
            ts_ms: f.ts_ms,
        }
    }

    fn from_view(v: &'a FrameView<'a>) -> Self {
        Self {
            width: v.width,
            height: v.height,
            stride: v.stride,
            bgra: v.bgra,
            dirty: v.dirty,
            moves: v.moves,
            full: v.full,
            ts_ms: v.ts_ms,
        }
    }
}

/// 这一轮抓到的东西：视频模式要跨线程送编码器，所以必须拥有数据（grab() 会复制 8MB）；
/// 块模式只在借用期内做差分+编码，用 grab_view() 零拷贝。
enum Grabbed<'a> {
    Owned(Frame),
    Borrowed(FrameView<'a>),
    Idle,
}

/// 整帧缓存刷新间隔：RequestFull 在静止桌面需要一帧拥有数据的副本，但**不能每帧都拷**。
/// 每 500ms 拷一份 8MB 约等于 16MB/s，占用可以忽略；静止桌面下这份副本就是当前画面。
const CACHE_REFRESH: Duration = Duration::from_millis(500);

/// 把一帧零拷贝视图拷进"整帧缓存"（供静止桌面的 RequestFull 兜底）。
fn refresh_cache_from_view(cached: &mut Option<Arc<Frame>>, view: &FrameView<'_>) {
    let need = view.stride.saturating_mul(view.height as usize);
    if need == 0 || view.bgra.len() < need {
        return;
    }
    if let Some(arc) = cached.as_mut() {
        if let Some(f) = Arc::get_mut(arc) {
            if f.bgra.len() == need {
                f.bgra.copy_from_slice(&view.bgra[..need]);
                f.width = view.width;
                f.height = view.height;
                f.stride = view.stride;
                f.full = view.full;
                f.ts_ms = view.ts_ms;
                f.dirty.clear();
                f.moves.clear();
                return;
            }
        }
    }
    *cached = Some(Arc::new(Frame {
        width: view.width,
        height: view.height,
        stride: view.stride,
        bgra: view.bgra[..need].to_vec(),
        dirty: Vec::new(),
        moves: Vec::new(),
        full: view.full,
        ts_ms: view.ts_ms,
    }));
}

/// 把缩放结果放进一个可复用的 Frame（避免每帧重新分配几 MB）。
fn prepare_scaled_frame(scaled: &mut Option<Frame>, src: &Pixels<'_>, scale: f32) -> Option<(u32, u32)> {
    let (sw, sh) = scaled_dims(src.width, src.height, scale);
    let slot = scaled.get_or_insert_with(|| Frame {
        width: 0,
        height: 0,
        stride: 0,
        bgra: Vec::new(),
        dirty: Vec::new(),
        moves: Vec::new(),
        full: true,
        ts_ms: 0,
    });
    if !downscale_bgra(
        src.bgra,
        src.stride,
        src.width,
        src.height,
        &mut slot.bgra,
        sw,
        sh,
    ) {
        return None;
    }
    slot.width = sw;
    slot.height = sh;
    slot.stride = sw as usize * 4;
    slot.full = true;
    slot.ts_ms = src.ts_ms;
    // 缩放帧是整帧重算的：dirty/move 不再有意义（走全网格扫描）
    slot.dirty.clear();
    slot.moves.clear();
    Some((sw, sh))
}

/// 单条 VideoFrame 的 payload 预算：UDP 一个分片约 1.2 KB，32 KB ≈ 26 个分片。
///
/// 超过就拆成多条消息发：lrc-udp 是**有序交付**，一条大消息里只要丢一个分片，
/// 整条就得等重传补齐（消息级队头阻塞）。首帧几 MB 时会被拖到 10 秒以上。
const VIDEO_FRAME_CHUNK_BYTES: usize = 32 * 1024;
/// VideoFrame 固定头：type(1)+seq(4)+flags(1)+base_seq(4)+block_size(2)+blocks_x(2)+blocks_y(2)+count(2)
const VIDEO_FRAME_HEAD_BYTES: usize = 18;
/// 每个块条目的固定开销：idx(4)+codec(1)+len(4)
const BLOCK_ENTRY_BYTES: usize = 9;

/// 把已编码块按"单条消息 payload ≤ budget"切成若干组（返回 blocks 的下标区间）。
///
/// - 区间按顺序拼起来正好覆盖 0..n，**不重不漏**（客户端按块覆盖写，拼回去就是整帧）；
/// - 单块自己就超预算时独占一条（块不可再拆；64×64 BGRA 最大也才 16 KB）。
fn split_block_ranges(block_lens: &[usize], budget: usize) -> Vec<std::ops::Range<usize>> {
    let budget = budget.max(VIDEO_FRAME_HEAD_BYTES + BLOCK_ENTRY_BYTES + 1);
    let mut out = Vec::new();
    if block_lens.is_empty() {
        return out;
    }
    let mut start = 0usize;
    let mut used = VIDEO_FRAME_HEAD_BYTES;
    for (i, len) in block_lens.iter().enumerate() {
        let need = BLOCK_ENTRY_BYTES.saturating_add(*len);
        if i > start && used.saturating_add(need) > budget {
            out.push(start..i);
            start = i;
            used = VIDEO_FRAME_HEAD_BYTES;
        }
        used = used.saturating_add(need);
    }
    out.push(start..block_lens.len());
    out
}

/// 第 index 条分片消息的 flags：**FULL|KEY 只在第一条带**，否则客户端会反复整屏重建。
fn chunk_flags(index: usize, send_full: bool) -> u8 {
    if index == 0 && send_full {
        frame_flags::FULL | frame_flags::KEY
    } else {
        0
    }
}

/// 采集/差分/编码/发送主循环。
fn capture_loop(
    args: &Args,
    capturer: &mut Capturer,
    info: &mut ScreenInfo,
    shared: Arc<Shared>,
    pipeline: Option<VideoPipeline>,
    path: &'static str,
) -> Result<(), String> {
    let mut track = Track::new(info.width, info.height)
        .ok_or_else(|| format!("采集尺寸非法 {}x{}", info.width, info.height))?;
    let mut opts = Options {
        jpeg_quality: args.quality,
        ..Options::default()
    };
    // 块模式节拍（--budget-bps 时会被控制器往下调，--fps 仍是硬上限）
    let mut frame_interval = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    // 视频模式不跟随 --fps，单独按 --video-max-fps 采集
    let video_interval = Duration::from_secs_f64(1.0 / f64::from(args.video_max_fps.max(1)));
    let session_start = Instant::now();

    let mut seq: u32 = 0;
    let mut force_full = true;
    let mut next_grab_at = Instant::now();
    let mut cursor = CursorTracker::new(DisplayMapping::from_screen(args.display, info));
    // 最近一帧像素：RequestFull 在静止桌面时靠它补整帧。
    // 用 Arc 是为了视频模式能零拷贝地把同一帧交给编码线程（cached 自己还要留着补帧）。
    let mut cached: Option<Arc<Frame>> = None;
    // 块/视频模式判定
    let mut mode_switch = ModeSwitch::new(pipeline.is_some());
    let mut video_gen: u32 = 0;
    // 一次性标志：本轮若没有新帧，就用缓存帧发一帧整帧（RequestFull / 模式退出）
    let mut force_cache_resend = false;
    let mut last_video = VideoCounters::default();
    // 自适应控制器（--budget-bps 0 = 关闭，此时所有参数与不加该参数时完全一致）
    let mut adapt = AdaptState::new(args);
    let mut controller = if args.budget_bps > 0 {
        log::info!("自适应控制器启用：链路预算 {}kbps", args.budget_bps / 1000);
        Some(Controller::new(ControllerConfig {
            link_budget_bps: args.budget_bps,
            video_enabled: pipeline.is_some(),
            ..ControllerConfig::default()
        }))
    } else {
        None
    };
    // 这一秒的变化块比例累加（算平均值喂控制器）
    let mut ratio_sum = 0.0f64;
    let mut ratio_n = 0u64;
    // 并联判据②：块模式实际码率连续超阈值的秒数
    let mut block_over_kbps_streak: u32 = 0;
    // 码率判据强制切视频时置位：此时忽略控制器的 mode 否决（控制器只按 changed_ratio
    // 判模式，窗口化播视频正是它看不见的场景）；内容安静后由 ModeSwitch 退出并清位。
    let mut video_forced_by_bitrate = false;
    // 已经请求过的视频码率（避免每秒重复请求导致编码进程反复重启）
    let mut requested_bitrate = args.video_bitrate;
    // 降分辨率用的复用缓冲
    let mut scaled: Option<Frame> = None;
    let mut slots: Vec<BlockSlot> = Vec::new();
    let mut cand: Vec<u32> = Vec::new();
    let mut rects: Vec<Rect> = Vec::new();
    let mut changed: Vec<(u32, u64)> = Vec::new();
    // 一条逻辑帧拆出来的多条消息（复用，避免每帧重新分配）
    let mut frame_chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = Stats::default();
    let mut sec = Stats::default();
    let mut last_report = Instant::now();
    let mut reopen_failures: u32 = 0;
    // 当前采集的显示输出序号（会被 SelectDisplay 改）
    let mut cur_display = args.display;
    // 采集失败要重建采集器，但崩溃点那里还借用着 capturer（grab_view 的帧视图），
    // 所以记下来到下一轮开头（借用已结束）再重建。
    let mut pending_reopen: Option<CaptureError> = None;
    // 整帧缓存上次刷新的时刻（RequestFull 静止兜底用，别每帧都拷）
    let mut last_cache_copy = Instant::now() - CACHE_REFRESH;


    log::info!(
        "推流 {}x{} = {}x{} 块（{}px），上限 {}fps，JPEG q{}",
        track.width,
        track.height,
        track.grid.blocks_x,
        track.grid.blocks_y,
        BLOCK_SIZE,
        args.fps,
        args.quality
    );

    loop {
        if shared.stop() {
            break;
        }
        // 上一轮采集失败：在这里重建采集器（此时没有任何帧借用存活）
        if let Some(err) = pending_reopen.take() {
            reopen_failures += 1;
            if reopen_failures > CAPTURE_REOPEN_TRIES {
                return Err(format!("采集器连续 {reopen_failures} 次重建失败，放弃会话"));
            }
            log::warn!("采集出错（{err}），重建采集器（第 {} 次）", reopen_failures);
            let backoff = u64::from(reopen_failures).min(10) * 100;
            std::thread::sleep(Duration::from_millis(backoff));
            match open_capturer(cur_display) {
                Ok((fresh, fresh_info)) => {
                    *capturer = fresh;
                    *info = fresh_info;
                    track = Track::new(fresh_info.width, fresh_info.height).ok_or_else(|| {
                        format!("重建后尺寸非法 {}x{}", fresh_info.width, fresh_info.height)
                    })?;
                    cached = None;
                    scaled = None;
                    let mapping = DisplayMapping::from_screen(cur_display, &fresh_info);
                    cursor.set_mapping(mapping, 1.0);
                    shared.set_origin(mapping.origin);
                    reopen_failures = 0;
                    log::info!(
                        "采集器已重建 {}x{} origin=({}, {})",
                        fresh_info.width,
                        fresh_info.height,
                        fresh_info.origin_x,
                        fresh_info.origin_y
                    );
                }
                Err(e2) => log::warn!("重建采集器失败：{e2}"),
            }
            next_grab_at = Instant::now() + frame_interval;
            continue;
        }
        // 客户端请求切换显示输出：重建 Capturer + 更新 origin + 下一帧整帧重同步
        if let Some(idx) = shared.take_display_request() {
            log::info!("处理显示输出切换请求：#{} → #{}", cur_display, idx);
            let exists = lrc_capture::list_outputs()
                .map(|v| usize::from(idx) < v.len())
                .unwrap_or(true);
            if !exists {
                log::warn!("显示输出 #{} 不存在，忽略切换请求", idx);
            } else {
                match open_capturer(usize::from(idx)) {
                    Ok((fresh, fresh_info)) => {
                        let mapping = DisplayMapping::from_screen(usize::from(idx), &fresh_info);
                        *capturer = fresh;
                        *info = fresh_info;
                        cur_display = mapping.index;
                        track = Track::new(fresh_info.width, fresh_info.height).ok_or_else(|| {
                            format!("切换后尺寸非法 {}x{}", fresh_info.width, fresh_info.height)
                        })?;
                        cached = None;
                        scaled = None;
                        last_cache_copy = Instant::now();
                        cursor.set_mapping(mapping, adapt.scale);
                        shared.set_origin(mapping.origin);
                        // 视频编码器是按旧尺寸建的：尺寸不一致就本会话禁用视频，否则回块模式重同步
                        if let Some(p) = pipeline.as_ref() {
                            if p.frame_size() != mapping.size {
                                log::warn!(
                                    "新显示 {}x{} 与视频编码器 {}x{} 尺寸不一致 → 本会话禁用视频通道",
                                    mapping.size.0,
                                    mapping.size.1,
                                    p.frame_size().0,
                                    p.frame_size().1
                                );
                                mode_switch.disable_video();
                            } else {
                                mode_switch.force_blocks();
                            }
                        }
                        force_full = true;
                        log::info!(
                            "已切换到显示输出 #{}：{}x{} origin=({}, {})，下一帧发整帧重同步",
                            mapping.index,
                            mapping.size.0,
                            mapping.size.1,
                            mapping.origin.0,
                            mapping.origin.1
                        );
                    }
                    Err(e) => log::warn!("切换显示输出 #{} 失败：{e}", idx),
                }
            }
            next_grab_at = Instant::now();
            continue;
        }

        let video_now = pipeline
            .as_ref()
            .map(|p| p.counters())
            .unwrap_or_default();
        let path_line = stats::PathLine {
            path,
            rtt_ms: shared.udp_rtt_ms(),
            loss_permille: shared.udp_loss_permille(),
        };
        if let Some(sample) = report_if_due(
            args,
            &mut sec,
            &mut total,
            &mut last_report,
            session_start,
            mode_switch.mode().label(),
            &adapt,
            video_now,
            &mut last_video,
            &path_line,
        ) {
            // 每秒喂一次自适应控制器
            if let Some(ctrl) = controller.as_mut() {
                let ratio = if ratio_n > 0 {
                    (ratio_sum / ratio_n as f64) as f32
                } else {
                    0.0
                };
                let ack_fresh = shared.ack_fresh();
                // UDP 路径有链路层 RTT（含重传），比 ack 反算更准；TCP 用 ack 估算
                let udp_rtt = shared.udp_rtt_ms();
                let rtt_ms = if udp_rtt > 0 {
                    udp_rtt
                } else if ack_fresh {
                    shared.rtt_ms()
                } else {
                    0
                };
                let link = LinkStats {
                    bytes_sent: sample.bytes,
                    // ack 停了就别把积压/延迟当拥塞信号（否则会一直降档到停发）
                    inflight_bytes: if ack_fresh { shared.inflight_bytes() } else { 0 },
                    rtt_ms,
                    dropped_frames: sample.dropped,
                    changed_ratio: ratio.clamp(0.0, 1.0),
                    fps: sample.fps,
                };
                let target = ctrl.on_tick(
                    &link,
                    (sample.interval_secs * 1000.0).round() as u32,
                );
                let prev_level = adapt.level;
                let prev_scale = adapt.scale;
                let changed = adapt.apply(&target, args, pipeline.is_some());
                adapt.level = ctrl.level();
                adapt.stage = ctrl.stage();
                if changed || prev_level != adapt.level {
                    log::info!(
                        "自适应：level={}（{}）fps={} q={} scale={:.2} 视频码率={}kbps｜实测 {:.0}kbps rtt={}ms inflight={}KB ratio={:.2} fps={:.1}",
                        adapt.level,
                        adapt.stage,
                        adapt.fps_limit,
                        adapt.block_quality,
                        adapt.scale,
                        adapt.video_bitrate / 1000,
                        link.bps() / 1000.0,
                        link.rtt_ms,
                        link.inflight_bytes / 1024,
                        link.changed_ratio,
                        link.fps
                    );
                }
                // 帧率 / 块质量 / 视频码率
                frame_interval =
                    Duration::from_secs_f64(1.0 / f64::from(adapt.fps_limit.max(1)));
                opts.jpeg_quality = adapt.block_quality;
                if adapt.video_bitrate != requested_bitrate {
                    if let Some(p) = pipeline.as_ref() {
                        p.request_bitrate(adapt.video_bitrate);
                    }
                    requested_bitrate = adapt.video_bitrate;
                }
                // 分辨率缩放变化：立刻重建网格并要求下一帧整帧重发
                if (adapt.scale - prev_scale).abs() > 0.01 {
                    let (sw, sh) = scaled_dims(info.width, info.height, adapt.scale);
                    if adapt.scale_active() {
                        log::info!(
                            "自适应：降分辨率 → {}x{}（scale={:.2}）",
                            sw,
                            sh,
                            adapt.scale
                        );
                    } else {
                        log::info!("自适应：恢复原生分辨率 {}x{}", sw, sh);
                    }
                    track = Track::new(sw, sh)
                        .ok_or_else(|| format!("缩放尺寸非法 {sw}x{sh}"))?;
                    cursor.set_mapping(
                        DisplayMapping::from_screen(cur_display, info),
                        adapt.scale,
                    );
                    force_full = true;
                    scaled = None;
                }
            }

            // ---- 并联判据②：块模式实际码率连续超阈值 → 也切视频 ----
            // 与 changed_ratio 判据并联（任一满足就切）。典型场景：窗口化播放视频，
            // 只有 20%-30% 屏幕在变，但逐块 JPEG 会打到 20-40 Mbps。
            if mode_switch.video_available() && mode_switch.mode() == Mode::Blocks {
                let block_bytes = sample.bytes.saturating_sub(sample.video_bytes);
                let block_kbps =
                    (block_bytes as f64 * 8.0 / 1000.0) / sample.interval_secs.max(0.001);
                if block_bitrate_trigger(block_kbps, args.video_bitrate) {
                    block_over_kbps_streak = block_over_kbps_streak.saturating_add(1);
                } else {
                    block_over_kbps_streak = 0;
                }
                if block_over_kbps_streak >= BLOCK_OVER_SECS {
                    log::info!(
                        "块模式码率连续 {} 秒超阈值（实测 {:.0}kbps，阈值 max({:.0}, video×1.5)kbps），切视频模式并请求 IDR",
                        block_over_kbps_streak,
                        block_kbps,
                        BLOCK_OVER_KBPS
                    );
                    mode_switch.force_video();
                    video_forced_by_bitrate = true;
                    block_over_kbps_streak = 0;
                    if let Some(p) = pipeline.as_ref() {
                        video_gen = p.new_generation();
                        p.request_keyframe();
                    }
                    force_full = false;
                    next_grab_at = Instant::now();
                }
            }

            ratio_sum = 0.0;
            ratio_n = 0;
        }

        // 帧率节流：采集前等待。丢帧不能只丢"发送"，因为 dirty rect 一旦被
        // AcquireNextFrame 消费掉就补不回来了，所以宁可少采。
        let now = Instant::now();
        if now < next_grab_at {
            let wait = next_grab_at
                .saturating_duration_since(now)
                .min(MAX_THROTTLE_SLEEP);
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            continue;
        }

        let t_cap = Instant::now();
        // 视频模式要把整帧交给编码线程（跨线程）→ 必须拥有数据，走 grab()（复制 8MB）；
        // 块模式只在本轮借用期内做差分+编码 → 走 grab_view()（零拷贝，省 ~2.7ms/帧）。
        let want_owned = mode_switch.mode() == Mode::Video;
        let grabbed: Result<Grabbed<'_>, CaptureError> = if want_owned {
            capturer.grab().map(|o| match o {
                Some(f) => Grabbed::Owned(f),
                None => Grabbed::Idle,
            })
        } else {
            capturer.grab_view().map(|o| match o {
                Some(v) => Grabbed::Borrowed(v),
                None => Grabbed::Idle,
            })
        };
        let capture_ms = t_cap.elapsed().as_secs_f64() * 1000.0;

        let mut has_new_frame = false;
        let mut owned_frame: Option<Frame> = None;
        let mut view_frame: Option<FrameView<'_>> = None;
        // 抓帧完成时刻（视频 AU 的 ts_ms 用它）：GetTickCount64 低 32 位。
        // 和客户端 lat_ms 的 now 同源，**只在同机测试时有效**，跨机只能作参考。
        let capture_tick = unsafe { GetTickCount64() } as u32;
        match grabbed {
            Ok(Grabbed::Owned(f)) => {
                has_new_frame = true;
                owned_frame = Some(f);
            }
            Ok(Grabbed::Borrowed(v)) => {
                has_new_frame = true;
                view_frame = Some(v);
            }
            Ok(Grabbed::Idle) => {
                // 没有任何像素变化：一个字节的画面数据都不发（零变化零发送的源头）
                sec.idle += 1;
            }
            Err(e @ (CaptureError::Lost | CaptureError::Runtime(_))) => {
                // 此刻帧视图还借用着 capturer，不能直接重建：记下来下一轮开头建
                log::warn!("采集出错：{e}");
                pending_reopen = Some(e);
                continue;
            }
            Err(CaptureError::Unsupported(m)) => return Err(format!("采集不支持：{m}")),
            Err(CaptureError::Init(m)) => return Err(format!("采集初始化失败：{m}")),
        }

        // 视频模式：owned 帧直接进缓存（Arc 复用，编码线程零拷贝拿同一份）
        if let Some(f) = owned_frame.take() {
            cached = Some(Arc::new(f));
        }
        // 块模式：维护"整帧缓存"（RequestFull 静止时兜底），每 500ms 拷一份，不每帧拷
        if let Some(v) = view_frame.as_ref() {
            if cached.is_none() || last_cache_copy.elapsed() >= CACHE_REFRESH {
                refresh_cache_from_view(&mut cached, v);
                last_cache_copy = Instant::now();
            }
        }

        let request_full_now = shared.request_full.swap(false, Ordering::Relaxed);

        // ---------- 视频模式：整帧丢给编码线程，块路径不发 ----------
        if mode_switch.mode() == Mode::Video {
            let mut back_to_blocks = false;
            match pipeline.as_ref() {
                None => {
                    // 不该发生：进视频模式的前提是有流水线
                    log::warn!("视频流水线不存在，回块模式");
                    mode_switch.disable_video();
                    back_to_blocks = true;
                }
                Some(p) => {
                    if p.send_failed() {
                        log::warn!("视频码流发送失败，结束会话");
                        break;
                    }
                    if p.is_broken() {
                        log::warn!("视频编码器已失效，永久回块模式");
                        mode_switch.disable_video();
                        back_to_blocks = true;
                    } else if adapt.enabled && !adapt.allow_video && !video_forced_by_bitrate {
                        // 控制器要求块模式 → 退出视频模式。
                        // 注意：resolution_scale 只作用于块模式（编码器按原生尺寸创建），
                        // 视频模式下用原生整帧编码、靠 video_bitrate 控流量。
                        log::info!("自适应要求块模式（allow_video=false），退出视频模式");
                        p.new_generation();
                        back_to_blocks = true;
                    } else {
                        if request_full_now {
                            // 视频模式下整帧请求 = 让编码器出 IDR（不要回块模式）
                            p.request_keyframe();
                            log::info!("视频模式收到整帧请求 → 请求 IDR");
                        }
                        let total_blocks = track.grid.count() as usize;
                        let changed_n = match cached.as_deref() {
                            Some(f) => video_changed_ratio(&mut track, f, &mut rects, &mut cand),
                            None => 0,
                        };
                        if total_blocks > 0 && has_new_frame {
                            ratio_sum += changed_n as f64 / total_blocks as f64;
                            ratio_n += 1;
                        }
                        if has_new_frame {
                            match mode_switch.observe(changed_n, total_blocks) {
                                ModeEvent::ExitToBlocks => {
                                    log::info!(
                                        "变化比例回落到 {:.1}%，切回块模式（发整帧重同步）",
                                        100.0 * changed_n as f32 / total_blocks.max(1) as f32
                                    );
                                    // 丢掉在途/待编码的旧世代 AU
                                    p.new_generation();
                                    back_to_blocks = true;
                                }
                                ModeEvent::EnterVideo => {}
                                ModeEvent::None => {
                                    if shared.ack_fresh()
                                        && shared.inflight_bytes() > MAX_INFLIGHT_BYTES
                                    {
                                        sec.dropped += 1;
                                        log::debug!("视频流控丢帧");
                                    } else if let Some(frame) = cached.clone() {
                                        p.submit(frame, seq, video_gen, capture_tick);
                                        seq = seq.wrapping_add(1);
                                    }
                                }
                            }
                        } else {
                            // 静止的一拍：按视频帧率算一帧 0 变化，攒够 30 拍回块模式
                            mode_switch.observe(0, total_blocks);
                        }
                    }
                }
            }

            if back_to_blocks {
                // 状态机必须跟着实际发送路径走，否则会一直"名义上视频、实际发块"
                mode_switch.force_blocks();
                video_forced_by_bitrate = false;
                force_full = true;
                track.scan_all = true;
                force_cache_resend = cached.is_some();
                next_grab_at = Instant::now();
                // 落到下面的块模式路径，同一轮就把整帧发出去
            } else {
                if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                    log_send_failure(&shared, "发送光标", &e);
                    break;
                }
                next_grab_at = t_cap + video_interval;
                continue;
            }
        }

        // ---------- 块模式 ----------
        if request_full_now {
            // 整帧请求：有新帧就用新帧；静止桌面（grab 返回 None）用缓存帧补发整帧，
            // 否则客户端在完全静止时会一直等不到 full 帧。
            force_full = true;
            force_cache_resend = true;
        }
        if has_new_frame {
            force_cache_resend = false;
        }
        let resend_from_cache = force_cache_resend && !has_new_frame;
        if resend_from_cache {
            force_cache_resend = false;
        }

        if !has_new_frame && !resend_from_cache {
            // 静止且没有整帧请求：只转发光标，画面 0 字节
            if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                log_send_failure(&shared, "发送光标", &e);
                break;
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        // 像素来源：有新帧 → 零拷贝视图（块模式）或拥有帧（视频模式回退时进 cached）；
        // 没有新帧的补帧 → 整帧缓存。
        let mut px = match view_frame.as_ref() {
            Some(v) => Some(Pixels::from_view(v)),
            None => cached.as_deref().map(Pixels::from_frame),
        };

        let Some(mut px) = px.take() else {
            // 还没有抓到过任何像素：把整帧请求放回去，抓到帧之后再补
            if request_full_now {
                shared.request_full.store(true, Ordering::Relaxed);
                force_cache_resend = true;
            }
            if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                log_send_failure(&shared, "发送光标", &e);
                break;
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        };

        // 自适应降分辨率：编码前把整帧缩到 scale 尺寸。
        // 缩放帧一律当整帧处理（网格按缩放后尺寸重建、光标坐标按 scale 换算、客户端画面也变小）。
        if adapt.scale_active() {
            match prepare_scaled_frame(&mut scaled, &px, adapt.scale) {
                Some(_) => {
                    if let Some(f) = scaled.as_ref() {
                        px = Pixels::from_frame(f);
                    }
                }
                None => {
                    log::warn!("降分辨率失败（帧缓冲异常），本帧跳过");
                    if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                        log_send_failure(&shared, "发送光标", &e);
                        break;
                    }
                    next_grab_at = t_cap + frame_interval;
                    continue;
                }
            }
        }

        // 流控：客户端没确认的字节太多 → 丢这一帧、不排队。
        // 这一帧的 dirty rect 已经消费掉了，所以下一帧必须全盘重扫（否则变化块永久丢失）。
        if shared.ack_fresh() && shared.inflight_bytes() > MAX_INFLIGHT_BYTES {
            sec.dropped += 1;
            track.scan_all = true;
            if request_full_now {
                // 整帧请求没满足，放回去下轮再试
                shared.request_full.store(true, Ordering::Relaxed);
            }
            next_grab_at = Instant::now() + frame_interval;
            log::debug!("流控丢帧：未确认字节 {} ", shared.inflight_bytes());
            continue;
        }

        // 分辨率 / 显示切换（只会发生在新帧上：缓存帧的尺寸就是 track 的尺寸）
        if px.width != track.width || px.height != track.height {
            log::info!(
                "采集尺寸变化 {}x{} → {}x{}，重建块网格",
                track.width,
                track.height,
                px.width,
                px.height
            );
            track = Track::new(px.width, px.height)
                .ok_or_else(|| format!("帧尺寸非法 {}x{}", px.width, px.height))?;
            cursor.set_mapping(
                DisplayMapping::from_screen(cur_display, info),
                adapt.scale,
            );
        }

        // 首帧 / 客户端请求整帧 / 刚重建过 → 整帧发，flags 带 FULL|KEY
        let send_full = force_full || !track.have_prev;
        let scan_all = send_full || track.scan_all || px.full;

        if scan_all {
            cand.clear();
            cand.extend(0..track.grid.count());
        } else {
            rects.clear();
            rects.extend_from_slice(px.dirty);
            for mv in px.moves {
                rects.push(mv.dst);
                rects.push(mv.src);
            }
            candidates_for_rects(&track.grid, &rects, &mut cand);
        }

        // 差分：只对候选块算哈希，与"客户端已知状态"比
        changed.clear();
        for &idx in &cand {
            let Some(rect) = track.grid.block_rect(idx, track.width, track.height) else {
                continue;
            };
            let Some(offset) = pixel_offset(&rect, px.stride) else {
                continue;
            };
            if offset >= px.bgra.len() {
                continue;
            }
            let hash = norm_hash(lrc_capture::hash_block(
                &px.bgra[offset..],
                px.stride,
                rect.w,
                rect.h,
            ));
            let known = track.hashes.get(idx as usize).copied();
            if send_full || !track.have_prev || known != Some(hash) {
                changed.push((idx, hash));
            }
        }

        // 这一秒的变化块比例（喂控制器）
        let total_blocks = track.grid.count() as usize;
        if total_blocks > 0 {
            ratio_sum += changed.len() as f64 / total_blocks as f64;
            ratio_n += 1;
        }

        // 块模式下也观察变化比例：连续 3 帧 > 45% 就切视频通道。
        // 控制器/缩放有否决权（取更保守的）：不允许时**根本不喂 observe**，
        // 否则状态机会先切到 Video 再被打回来，导致每帧来回抖。
        let can_enter_video = !adapt.enabled || adapt.allow_video || video_forced_by_bitrate;
        let mode_event = if can_enter_video {
            mode_switch.observe(changed.len(), total_blocks)
        } else {
            ModeEvent::None
        };
        match mode_event {
            ModeEvent::EnterVideo => {
                if let Some(p) = pipeline.as_ref() {
                    log::info!(
                        "变化比例 {} / {} 块，切到视频模式（请求 IDR，码率 {}kbps）",
                        changed.len(),
                        track.grid.count(),
                        args.video_bitrate / 1000
                    );
                    video_gen = p.new_generation();
                    p.request_keyframe();
                    force_full = false;
                    next_grab_at = Instant::now();
                    if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                        log_send_failure(&shared, "发送光标", &e);
                        break;
                    }
                    continue;
                }
            }
            ModeEvent::ExitToBlocks | ModeEvent::None => {}
        }

        if changed.is_empty() {
            // 采集到了帧，但候选块内容没变：依然一个字节的画面数据都不发
            sec.idle += 1;
            if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
                log_send_failure(&shared, "发送光标", &e);
                break;
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        // 逐块编码：文字无损优先，图像走 JPEG，取小者（策略在 lrc-codec 里）
        let mut used = 0usize;
        for &(idx, hash) in &changed {
            let Some(rect) = track.grid.block_rect(idx, track.width, track.height) else {
                continue;
            };
            let Some(offset) = pixel_offset(&rect, px.stride) else {
                continue;
            };
            if offset >= px.bgra.len() {
                continue;
            }
            while slots.len() <= used {
                slots.push(BlockSlot::default());
            }
            let slot = &mut slots[used];
            match lrc_codec::encode_block(
                &px.bgra[offset..],
                px.stride,
                rect.w,
                rect.h,
                &opts,
                &mut slot.data,
            ) {
                Ok(codec) => {
                    slot.idx = idx;
                    slot.codec = codec;
                    slot.hash = hash;
                    used += 1;
                }
                Err(e) => log::warn!("块 {idx} 编码失败：{e}"),
            }
        }
        if used == 0 {
            log::warn!("本帧变化块全部编码失败，跳过");
            next_grab_at = t_cap + frame_interval;
            continue;
        }

        // 大帧分片发送：变化块编码总字节超过预算就拆成多条 VideoFrame，**连续发**（中间不插别的消息）。
        // 目的：把 UDP 有序交付下的"消息级队头阻塞"单位从"整帧"缩小到 32 KB。
        let lens: Vec<usize> = slots[..used].iter().map(|s| s.data.len()).collect();
        let ranges = split_block_ranges(&lens, VIDEO_FRAME_CHUNK_BYTES);
        frame_chunks.clear();
        let mut frame_ok = true;
        for (ci, range) in ranges.iter().enumerate() {
            if range.is_empty() {
                continue;
            }
            let entries: Vec<BlockEntry<'_>> = slots[range.clone()]
                .iter()
                .map(|s| BlockEntry {
                    idx: s.idx,
                    codec: s.codec,
                    data: s.data.as_slice(),
                })
                .collect();
            let msg = Message::VideoFrame(VideoFrame {
                seq,
                flags: chunk_flags(ci, send_full),
                base_seq: shared.ack_seq(),
                block_size: BLOCK_SIZE,
                blocks_x: track.grid.blocks_x,
                blocks_y: track.grid.blocks_y,
                entries,
            });
            let payload = msg.encode();
            // 分片后单条不可能撞传输层上限；留个保护防止将来预算被改大
            if payload.len() > lrc_proto::MAX_PAYLOAD {
                log::warn!(
                    "分片仍过大 {} > {}，本帧丢弃",
                    payload.len(),
                    lrc_proto::MAX_PAYLOAD
                );
                frame_ok = false;
                break;
            }
            frame_chunks.push(payload);
        }
        let chunks_sent = frame_chunks.len();
        let payload_total: usize = frame_chunks.iter().map(|p| p.len()).sum();
        if chunks_sent == 0 {
            frame_ok = false;
        }
        // 一次持锁把这一帧的所有分片连续发出去（中间不会插别的消息）
        if frame_ok {
            if let Err(e) = shared.send_batch(&frame_chunks) {
                log_send_failure(&shared, "发送帧", &e);
                break;
            }
        }
        if !frame_ok {
            // 一条都没发成/发一半：不推进客户端已知状态，下一帧整盘重扫自愈
            track.scan_all = true;
            next_grab_at = t_cap + frame_interval;
            continue;
        }
        seq = seq.wrapping_add(1);
        sec.frames += 1;
        sec.blocks += used as u64;
        sec.bytes += payload_total as u64 + 4 * chunks_sent as u64;
        sec.capture_ms_sum += capture_ms;
        // 只有真正发出去，才把"客户端已知状态"推进到当前
        for slot in &slots[..used] {
            if let Some(h) = track.hashes.get_mut(slot.idx as usize) {
                *h = slot.hash;
            }
        }
        track.have_prev = true;
        track.scan_all = false;
        force_full = false;
        if chunks_sent > 1 {
            log::debug!(
                "大帧拆成 {chunks_sent} 条发（{used} 块 / {payload_total} 字节 / 预算 {VIDEO_FRAME_CHUNK_BYTES}）"
            );
        }
        if request_full_now {
            sec.full_resends += 1;
            log::debug!(
                "已响应整帧请求（{}，{} 块，{} 条）",
                if resend_from_cache {
                    "静止桌面用缓存帧"
                } else {
                    "新帧"
                },
                used,
                chunks_sent
            );
        }

        if let Err(e) = cursor.poll_and_send(&shared, &mut sec) {
            log_send_failure(&shared, "发送光标", &e);
            break;
        }
        next_grab_at = t_cap + frame_interval;
    }

    total.add(&sec);
    let session_secs = session_start.elapsed().as_secs_f64();
    let capture_avg = if total.frames == 0 {
        0.0
    } else {
        total.capture_ms_sum / total.frames as f64
    };
    let cs = cursor.stats();
    log::info!(
        "光标轮询 polls={} changes={} shape_captures={} last_shape_us={}",
        cs.polls,
        cs.changes,
        cs.shape_captures,
        cs.last_shape_capture_us
    );
    let final_video = pipeline
        .as_ref()
        .map(|p| p.counters())
        .unwrap_or_default();
    let video_summary = stats::VideoLine {
        mode: mode_switch.mode().label(),
        fps: final_video.aus as f64 / session_secs.max(0.001),
        kbps: (final_video.bytes as f64 * 8.0 / 1000.0) / session_secs.max(0.001),
        level: adapt.level,
        stage: adapt.stage,
        scale: adapt.scale,
    };
    if let Some(p) = pipeline.as_ref() {
        log::info!(
            "视频通道 {}：发出 {} 个 AU / {} 字节，待编码队列丢弃 {} 帧",
            p.codec_name(),
            final_video.aus,
            final_video.bytes,
            p.dropped_frames()
        );
    }
    let final_path = stats::PathLine {
        path,
        rtt_ms: shared.udp_rtt_ms(),
        loss_permille: shared.udp_loss_permille(),
    };
    log::info!(
        "会话结束，总计 {} capture_ms_avg={capture_avg:.2}",
        stats::format_summary(&total, session_secs, &video_summary, &final_path)
    );
    Ok(())
}

/// 发送失败的日志分级：对端已经断开属于正常收尾，不该按错误刷屏。
fn log_send_failure(shared: &Shared, what: &str, e: &io::Error) {
    if shared.stop() {
        log::info!("{what}时连接已断开，结束推流（{e}）");
    } else {
        log::warn!("{what}失败：{e}");
    }
}

/// 每秒一行统计（--stats）。格式见 stats.rs，被 Lead 的验收脚本按 kbps= 抓数。
#[allow(clippy::too_many_arguments)]
fn report_if_due(
    args: &Args,
    sec: &mut Stats,
    total: &mut Stats,
    last_report: &mut Instant,
    session_start: Instant,
    mode: &'static str,
    adapt: &AdaptState,
    video_now: VideoCounters,
    last_video: &mut VideoCounters,
    path: &stats::PathLine,
) -> Option<SecondSample> {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(*last_report);
    if elapsed < Duration::from_secs(1) {
        return None;
    }
    let interval = elapsed.as_secs_f64();

    // 视频 AU 也计入总帧数/字节数（这样 kbps 是真实线上码率），再单独给出 video_kbps/video_fps
    let d_aus = video_now.aus.saturating_sub(last_video.aus);
    let d_bytes = video_now.bytes.saturating_sub(last_video.bytes);
    *last_video = video_now;
    sec.frames += d_aus;
    sec.bytes += d_bytes;
    let video_line = stats::VideoLine {
        mode,
        fps: d_aus as f64 / interval,
        kbps: (d_bytes as f64 * 8.0 / 1000.0) / interval,
        level: adapt.level,
        stage: adapt.stage,
        scale: adapt.scale,
    };

    let sample = SecondSample {
        interval_secs: interval,
        bytes: sec.bytes,
        video_bytes: d_bytes,
        dropped: u32::try_from(sec.dropped).unwrap_or(u32::MAX),
        fps: sec.frames as f32 / interval as f32,
    };

    total.add(sec);
    if args.stats {
        let session_secs = now.saturating_duration_since(session_start).as_secs_f64();
        println!(
            "{}",
            stats::format_line(total, sec, session_secs, interval, &video_line, path)
        );
        let _ = io::stdout().flush();
    }
    sec.reset();
    *last_report = now;
    Some(sample)
}

/// 把 dirty/move 矩形规约成"需要重新哈希的块下标"（升序、去重）。
fn candidates_for_rects(grid: &Grid, rects: &[Rect], out: &mut Vec<u32>) {
    out.clear();
    let bs = u32::from(grid.block_size).max(1);
    let bx = u32::from(grid.blocks_x);
    let by = u32::from(grid.blocks_y);
    for r in rects {
        if r.is_empty() {
            continue;
        }
        let x0 = (r.x.max(0) as u32) / bs;
        let y0 = (r.y.max(0) as u32) / bs;
        let x1 = ((r.right().max(0) as u32).saturating_add(bs - 1) / bs).min(bx);
        let y1 = ((r.bottom().max(0) as u32).saturating_add(bs - 1) / bs).min(by);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for gy in y0..y1 {
            for gx in x0..x1 {
                out.push(gy * bx + gx);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
}

/// 视频模式只关心"变化块比例"：逐候选块算哈希并**立即**写回（不编码、不发块），
/// 返回这一帧变化的块数。
fn video_changed_ratio(
    track: &mut Track,
    frame: &Frame,
    rects: &mut Vec<Rect>,
    cand: &mut Vec<u32>,
) -> usize {
    if frame.full {
        cand.clear();
        cand.extend(0..track.grid.count());
    } else {
        rects.clear();
        rects.extend_from_slice(&frame.dirty);
        for mv in &frame.moves {
            rects.push(mv.dst);
            rects.push(mv.src);
        }
        candidates_for_rects(&track.grid, &rects, cand);
    }
    let mut changed = 0usize;
    for &idx in cand.iter() {
        let Some(rect) = track.grid.block_rect(idx, track.width, track.height) else {
            continue;
        };
        let Some(offset) = pixel_offset(&rect, frame.stride) else {
            continue;
        };
        if offset >= frame.bgra.len() {
            continue;
        }
        let hash = norm_hash(lrc_capture::hash_block(
            &frame.bgra[offset..],
            frame.stride,
            rect.w,
            rect.h,
        ));
        if track.hashes.get(idx as usize).copied() != Some(hash) {
            changed += 1;
        }
        if let Some(slot) = track.hashes.get_mut(idx as usize) {
            *slot = hash;
        }
    }
    changed
}

/// 块左上角在帧缓冲里的字节偏移。
fn pixel_offset(rect: &Rect, stride: usize) -> Option<usize> {
    let x = usize::try_from(rect.x).ok()?;
    let y = usize::try_from(rect.y).ok()?;
    y.checked_mul(stride)?.checked_add(x.checked_mul(4)?)
}

/// 0 换成 1：哈希数组初值就是 0，避免"恰好哈希成 0 的块"被当成从未变化。
fn norm_hash(h: u64) -> u64 {
    if h == 0 {
        1
    } else {
        h
    }
}

/// 光标转发：位置/可见性变化发一条 CursorPos，形状真的变了再补一条 CursorShape。
///
/// 坐标系：CursorWatcher 给的是**虚拟桌面物理像素**，减掉采集输出原点才是客户端
/// 需要的"画面本地坐标"；出了采集区域就当不可见（多屏时另一块屏上的光标不该画过来）。
struct CursorTracker {
    watcher: CursorWatcher,
    last_pos: Option<(i16, i16, bool)>,
    last_shape: Option<Arc<lrc_capture::CursorShape>>,
    /// 当前显示输出的映射口径（原点 + 原生尺寸）
    mapping: DisplayMapping,
    /// 自适应降分辨率时的坐标缩放（1.0 = 原生）；客户端画面 = size × scale
    scale: f32,
}

impl CursorTracker {
    fn new(mapping: DisplayMapping) -> Self {
        Self {
            watcher: CursorWatcher::new(),
            last_pos: None,
            last_shape: None,
            mapping,
            scale: 1.0,
        }
    }

    fn stats(&self) -> lrc_capture::CursorWatcherStats {
        self.watcher.stats()
    }

    /// 采集器重建 / 分辨率变化 / 自适应缩放变化时更新映射口径。
    fn set_mapping(&mut self, mapping: DisplayMapping, scale: f32) {
        self.mapping = mapping;
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        // 口径变了：下一帧无条件把所有状态重发一遍
        self.last_pos = None;
        self.last_shape = None;
    }

    /// 主循环每轮调一次；只有真的有变化才发消息（静止时 0 字节）。
    fn poll_and_send(&mut self, shared: &Shared, sec: &mut Stats) -> io::Result<()> {
        let Some(state) = self.watcher.poll() else {
            return Ok(());
        };

        let ((sx, sy), inside) = self.mapping.to_local((state.x, state.y), self.scale);
        let pos = (clamp_i16(sx), clamp_i16(sy), state.visible && inside);
        if self.last_pos != Some(pos) {
            let payload = Message::CursorPos(CursorPos {
                x: pos.0,
                y: pos.1,
                visible: pos.2,
            })
            .encode();
            shared.send(&payload)?;
            self.last_pos = Some(pos);
            sec.cursor_msgs += 1;
            sec.bytes += payload.len() as u64 + 4;
        }

        // 形状：Arc 指针变了才是新形状（capture 侧内容没变会复用同一个 Arc）
        if let Some(shape) = state.shape {
            let changed = match &self.last_shape {
                Some(prev) => !Arc::ptr_eq(prev, &shape),
                None => true,
            };
            if changed {
                self.last_shape = Some(Arc::clone(&shape));
                if let Some(payload) = encode_cursor_shape(&shape) {
                    shared.send(&payload)?;
                    sec.cursor_msgs += 1;
                    sec.bytes += payload.len() as u64 + 4;
                }
            }
        }
        Ok(())
    }
}

/// 组 CursorShape 消息。proto 里字段叫 rgba，但内容就是 capture 给的 BGRA，直接塞不转换。
fn encode_cursor_shape(shape: &lrc_capture::CursorShape) -> Option<Vec<u8>> {
    let expect = usize::try_from(shape.width)
        .ok()?
        .checked_mul(usize::try_from(shape.height).ok()?)?;
    if shape.width == 0 || shape.height == 0 || expect.checked_mul(4)? != shape.bgra.len() {
        log::warn!(
            "光标位图尺寸异常 {}x{} len={}",
            shape.width,
            shape.height,
            shape.bgra.len()
        );
        return None;
    }
    if shape.width > MAX_CURSOR_SHAPE_DIM || shape.height > MAX_CURSOR_SHAPE_DIM {
        log::warn!(
            "光标位图 {}x{} 超过协议上限 {}，只发位置不发形状",
            shape.width,
            shape.height,
            MAX_CURSOR_SHAPE_DIM
        );
        return None;
    }
    let msg = Message::CursorShape(CursorShape {
        width: u16::try_from(shape.width).ok()?,
        height: u16::try_from(shape.height).ok()?,
        hotspot_x: i16::try_from(shape.hotspot_x).unwrap_or(0),
        hotspot_y: i16::try_from(shape.hotspot_y).unwrap_or(0),
        rgba: shape.bgra.as_slice(),
    });
    Some(msg.encode())
}

/// 虚拟桌面的光标坐标 → 采集画面本地坐标 + 是否在画面内。
///
/// 在别的屏上时夹到本屏边缘（配合 visible=false，客户端不会画错位置）。
fn map_cursor_pos(
    desk: (i32, i32),
    origin: (i32, i32),
    native: (u32, u32),
    scale: f32,
) -> ((i32, i32), bool) {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    // 先按**原生像素**判断是否在采集区域内，再换算到缩放后的客户端坐标空间
    let raw_x = desk.0.saturating_sub(origin.0);
    let raw_y = desk.1.saturating_sub(origin.1);
    let (region_w, region_h) = scaled_dims(native.0, native.1, s);
    // 缩放后的尺寸会被吸附到块网格上，所以 x/y 各自的实际比例可能略有不同，分开换算
    let sx = region_w as f32 / native.0.max(1) as f32;
    let sy = region_h as f32 / native.1.max(1) as f32;
    let local_x = (raw_x as f32 * sx).round() as i32;
    let local_y = (raw_y as f32 * sy).round() as i32;
    let inside =
        raw_x >= 0 && raw_y >= 0 && raw_x < native.0 as i32 && raw_y < native.1 as i32;
    if inside {
        return ((local_x, local_y), true);
    }
    let max_x = region_w.saturating_sub(1) as i32;
    let max_y = region_h.saturating_sub(1) as i32;
    ((local_x.clamp(0, max_x), local_y.clamp(0, max_y)), false)
}

/// CursorPos 的 x/y 是 i16：本地坐标理论上在屏幕内，夹一下更安全。
fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_1080() -> Grid {
        Grid::new(64, 1920, 1080).expect("grid")
    }

    #[test]
    fn candidates_single_block() {
        let g = grid_1080();
        let mut out = Vec::new();
        candidates_for_rects(&g, &[Rect::new(0, 0, 1, 1)], &mut out);
        assert_eq!(out, vec![0]);
        candidates_for_rects(&g, &[Rect::new(64, 0, 1, 1)], &mut out);
        assert_eq!(out, vec![1]);
        // 跨 2x2 个块
        candidates_for_rects(&g, &[Rect::new(63, 63, 2, 2)], &mut out);
        assert_eq!(out, vec![0, 1, 30, 31]);
    }

    #[test]
    fn candidates_dedupe_overlapping_rects() {
        let g = grid_1080();
        let mut out = Vec::new();
        candidates_for_rects(
            &g,
            &[Rect::new(0, 0, 100, 100), Rect::new(10, 10, 20, 20)],
            &mut out,
        );
        assert_eq!(out, vec![0, 1, 30, 31]);
    }

    #[test]
    fn candidates_clip_to_screen() {
        let g = grid_1080();
        let mut out = Vec::new();
        candidates_for_rects(&g, &[Rect::new(1900, 1000, 500, 500)], &mut out);
        assert_eq!(out, vec![479, 509]);
        // 完全在屏幕外的矩形不产生候选块
        candidates_for_rects(&g, &[Rect::new(-50, -50, 10, 10)], &mut out);
        assert!(out.is_empty());
        // 空矩形
        candidates_for_rects(&g, &[Rect::new(0, 0, 0, 0)], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn candidates_empty_rect_list() {
        let g = grid_1080();
        let mut out = vec![7u32];
        candidates_for_rects(&g, &[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn norm_hash_keeps_zero_distinct() {
        assert_eq!(norm_hash(0), 1);
        assert_eq!(norm_hash(1), 1);
        assert_eq!(norm_hash(12345), 12345);
    }

    #[test]
    fn pixel_offset_math() {
        let stride = 1920 * 4;
        let r = Rect::new(64, 128, 32, 32);
        assert_eq!(pixel_offset(&r, stride), Some(128 * stride + 64 * 4));
        assert_eq!(pixel_offset(&Rect::new(-1, 0, 1, 1), stride), None);
    }

    #[test]
    fn cursor_mapping_single_screen() {
        // 单屏 1920x1080，origin=(0,0)
        let ((x, y), inside) = map_cursor_pos((100, 200), (0, 0), (1920, 1080), 1.0);
        assert_eq!((x, y), (100, 200));
        assert!(inside);
        // 右下角最后一个像素仍算在内
        let ((x, y), inside) = map_cursor_pos((1919, 1079), (0, 0), (1920, 1080), 1.0);
        assert_eq!((x, y), (1919, 1079));
        assert!(inside);
        // 刚好越界一个像素
        let ((x, _), inside) = map_cursor_pos((1920, 0), (0, 0), (1920, 1080), 1.0);
        assert_eq!(x, 1919);
        assert!(!inside);
        let ((_, y), inside) = map_cursor_pos((0, 1080), (0, 0), (1920, 1080), 1.0);
        assert_eq!(y, 1079);
        assert!(!inside);
    }

    #[test]
    fn cursor_mapping_second_screen_uses_origin() {
        // 双屏并排：采集第 2 块屏，origin=(1920,0)
        let origin = (1920, 0);
        let region = (1920u32, 1080u32);
        let ((x, y), inside) = map_cursor_pos((1920, 0), origin, region, 1.0);
        assert_eq!((x, y), (0, 0));
        assert!(inside);
        let ((x, y), inside) = map_cursor_pos((3839, 1079), origin, region, 1.0);
        assert_eq!((x, y), (1919, 1079));
        assert!(inside);
        // 光标跑到第 1 块屏 → 本地坐标为负、不在画面内
        let ((x, y), inside) = map_cursor_pos((1000, 500), origin, region, 1.0);
        assert_eq!((x, y), (0, 500));
        assert!(!inside);
        // 坐标负得更多也不会溢出
        let ((x, _), inside) = map_cursor_pos((i32::MIN, 0), origin, region, 1.0);
        assert_eq!(x, 0);
        assert!(!inside);
    }

    #[test]
    fn cursor_mapping_with_resolution_scale() {
        // 原生 1920x1080，自适应缩到 1600x896（scale≈0.833），客户端画面就是 1600x896
        let scale = 1600.0f32 / 1920.0;
        let native = (1920u32, 1080u32);
        let origin = (0, 0);
        // 原生左上角 → 缩放画面左上角
        let ((x, y), inside) = map_cursor_pos((0, 0), origin, native, scale);
        assert_eq!((x, y), (0, 0));
        assert!(inside);
        // 原生 (960, 540) 屏幕中心 → 缩放画面中心（y 轴比例略小：896/1080）
        let ((x, y), inside) = map_cursor_pos((960, 540), origin, native, scale);
        assert_eq!((x, y), (800, 448));
        assert!(inside);
        // 原生右下角前一像素仍在画面内，且坐标不超过缩放后尺寸
        let ((x, y), inside) = map_cursor_pos((1919, 1079), origin, native, scale);
        assert!(inside, "({x},{y})");
        assert!(x <= 1600 && y <= 896, "({x},{y})");
        // 原生越界（另一块屏）→ 不在画面内
        let ((_, _), inside) = map_cursor_pos((2000, 500), origin, native, scale);
        assert!(!inside);
        // 缩放 0.5（尺寸刚好是块倍数）时坐标严格减半
        let ((x, y), inside) = map_cursor_pos((800, 600), origin, (1920, 1024), 0.5);
        assert_eq!((x, y), (400, 300));
        assert!(inside);
    }

    #[test]
    fn encode_cursor_shape_checks_geometry() {
        let mk = |w: u32, h: u32| lrc_capture::CursorShape {
            width: w,
            height: h,
            hotspot_x: 1,
            hotspot_y: 2,
            bgra: vec![0u8; (w as usize) * (h as usize) * 4],
        };
        assert!(encode_cursor_shape(&mk(32, 32)).is_some());
        assert!(encode_cursor_shape(&mk(256, 256)).is_some());
        // 超过协议 256 上限：不发形状
        assert!(encode_cursor_shape(&mk(257, 257)).is_none());
        // 尺寸为 0 / 长度不匹配：不 panic、返回 None
        assert!(encode_cursor_shape(&mk(0, 0)).is_none());
        let mut bad = mk(16, 16);
        bad.bgra.truncate(4);
        assert!(encode_cursor_shape(&bad).is_none());
    }

    #[test]
    fn clamp_i16_range() {
        assert_eq!(clamp_i16(0), 0);
        assert_eq!(clamp_i16(-40000), i16::MIN);
        assert_eq!(clamp_i16(40000), i16::MAX);
        assert_eq!(clamp_i16(-1920), -1920);
    }

    // ---------------------------------------------------- 自适应控制器集成
    fn test_args() -> Args {
        Args {
            listen: "127.0.0.1:0".to_string(),
            password: "1".to_string(),
            display: 0,
            fps: 30,
            quality: 85,
            stats: false,
            video: crate::cli::VideoChoice::Auto,
            video_bitrate: 4_000_000,
            video_max_fps: 30,
            budget_bps: 2_000_000,
            audio: false,
            signal: None,
            room: 1,
            p2p: crate::cli::P2pMode::Auto,
        }
    }

    fn ctrl(budget: u32) -> Controller {
        Controller::new(ControllerConfig {
            link_budget_bps: budget,
            video_enabled: true,
            ..ControllerConfig::default()
        })
    }

    fn over_stats() -> LinkStats {
        // 8 Mbps（预算 2M 的 4 倍），高变化比例，无丢帧
        LinkStats {
            bytes_sent: 1_000_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.9,
            fps: 30.0,
        }
    }

    fn slack_stats() -> LinkStats {
        // 80 kbps，几乎静止
        LinkStats {
            bytes_sent: 10_000,
            inflight_bytes: 0,
            rtt_ms: 5,
            dropped_frames: 0,
            changed_ratio: 0.0,
            fps: 30.0,
        }
    }

    #[test]
    fn adapt_targets_degrade_monotonically_under_overload() {
        let args = test_args();
        let mut c = ctrl(args.budget_bps);
        let mut adapt = AdaptState::new(&args);
        let mut prev_fps = adapt.fps_limit;
        let mut prev_q = adapt.block_quality;
        let mut prev_scale = adapt.scale;
        for _ in 0..15 {
            let t = c.on_tick(&over_stats(), 1000);
            adapt.level = c.level();
            adapt.stage = c.stage();
            adapt.apply(&t, &args, true);
            assert!(adapt.fps_limit <= prev_fps, "fps 不应回升");
            assert!(adapt.block_quality <= prev_q, "质量不应回升");
            assert!(adapt.scale <= prev_scale + 1e-6, "scale 不应变大");
            prev_fps = adapt.fps_limit;
            prev_q = adapt.block_quality;
            prev_scale = adapt.scale;
        }
        assert!(
            adapt.level > 0,
            "持续超预算必须降档（level={}）",
            adapt.level
        );
        assert!(adapt.fps_limit < 30, "fps 必须降下来：{}", adapt.fps_limit);
        assert!(adapt.block_quality < 85, "质量必须降下来：{}", adapt.block_quality);
    }

    #[test]
    fn adapt_targets_recover_monotonically_under_slack() {
        let args = test_args();
        let mut c = ctrl(args.budget_bps);
        let mut adapt = AdaptState::new(&args);
        // 先压到低档
        for _ in 0..15 {
            let t = c.on_tick(&over_stats(), 1000);
            adapt.level = c.level();
            adapt.apply(&t, &args, true);
        }
        let low_level = adapt.level;
        assert!(low_level > 0);
        let mut prev_fps = adapt.fps_limit;
        let mut prev_q = adapt.block_quality;
        let mut prev_level = adapt.level;
        for _ in 0..60 {
            let t = c.on_tick(&slack_stats(), 1000);
            adapt.level = c.level();
            adapt.stage = c.stage();
            adapt.apply(&t, &args, true);
            assert!(adapt.fps_limit >= prev_fps, "fps 不应再降");
            assert!(adapt.block_quality >= prev_q, "质量不应再降");
            assert!(adapt.level <= prev_level, "档位不应再升（数值越小越好）");
            prev_fps = adapt.fps_limit;
            prev_q = adapt.block_quality;
            prev_level = adapt.level;
        }
        assert_eq!(adapt.level, 0, "链路富余 60 秒应回到 0 档");
        assert_eq!(adapt.fps_limit, 30);
        assert_eq!(adapt.block_quality, 85);
        assert!(!adapt.scale_active(), "应恢复原生分辨率");
    }

    #[test]
    fn adapt_never_exceeds_user_fps_cap() {
        let mut args = test_args();
        args.fps = 20; // 用户显式上限 20
        let mut adapt = AdaptState::new(&args);
        let target = Target::default_blocks(); // fps 30
        adapt.apply(&target, &args, true);
        assert_eq!(adapt.fps_limit, 20, "--fps 必须是硬上限");
    }

    #[test]
    fn adapt_disabled_keeps_original_params() {
        let mut args = test_args();
        args.budget_bps = 0;
        let adapt = AdaptState::new(&args);
        assert!(!adapt.enabled);
        assert_eq!(adapt.fps_limit, args.fps);
        assert_eq!(adapt.block_quality, args.quality);
        assert_eq!(adapt.scale, 1.0);
        assert_eq!(adapt.video_bitrate, args.video_bitrate);
    }

    fn screen(index_origin: (i32, i32)) -> ScreenInfo {
        ScreenInfo {
            width: 1920,
            height: 1080,
            rotation: 0,
            origin_x: index_origin.0,
            origin_y: index_origin.1,
        }
    }

    /// 把若干块长度按区间算成"这条消息的 payload 字节数"
    fn chunk_payload(len: &[usize], r: &std::ops::Range<usize>) -> usize {
        VIDEO_FRAME_HEAD_BYTES
            + len[r.clone()]
                .iter()
                .map(|l| BLOCK_ENTRY_BYTES + l)
                .sum::<usize>()
    }

    #[test]
    fn split_big_frame_into_bounded_chunks() {
        // 1080p 整帧的典型规模：500 块 × 8 KB ≈ 4 MB
        let lens = vec![8 * 1024usize; 500];
        let ranges = split_block_ranges(&lens, VIDEO_FRAME_CHUNK_BYTES);
        assert!(ranges.len() > 50, "4MB 应拆成多条，实际 {}", ranges.len());
        // 1) 区间连续、不重不漏：拼起来正好是 0..n（客户端按块覆盖写，拼回去就是整帧）
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[ranges.len() - 1].end, lens.len());
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start, "区间必须首尾相接");
        }
        let covered: usize = ranges.iter().map(|r| r.len()).sum();
        assert_eq!(covered, lens.len(), "块集合必须完整");
        // 2) 每条 payload 不超预算
        for r in &ranges {
            let bytes = chunk_payload(&lens, r);
            assert!(
                bytes <= VIDEO_FRAME_CHUNK_BYTES,
                "区间 {r:?} 的 payload {bytes} 超预算 {VIDEO_FRAME_CHUNK_BYTES}"
            );
        }
    }

    #[test]
    fn small_frame_is_a_single_chunk() {
        // 打字的日常规模：几块、每块几百字节 → 不该拆
        let lens = vec![120usize, 300, 80];
        let ranges = split_block_ranges(&lens, VIDEO_FRAME_CHUNK_BYTES);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..3);
    }

    #[test]
    fn oversized_block_gets_its_own_chunk() {
        // 单块就超过预算（理论上块最大 16KB，这里故意造 64KB 验证不会死循环/丢块）
        let lens = vec![100usize, 64 * 1024, 100];
        let ranges = split_block_ranges(&lens, VIDEO_FRAME_CHUNK_BYTES);
        assert_eq!(ranges.len(), 3, "超预算的块要独占一条: {ranges:?}");
        assert_eq!(ranges[0], 0..1);
        assert_eq!(ranges[1], 1..2);
        assert_eq!(ranges[2], 2..3);
    }

    #[test]
    fn full_flag_only_on_first_chunk() {
        let first = chunk_flags(0, true);
        assert_eq!(
            first,
            frame_flags::FULL | frame_flags::KEY,
            "首帧第一条必须带 FULL|KEY"
        );
        for i in 1..5 {
            assert_eq!(chunk_flags(i, true), 0, "第 {i} 条不能再带 FULL，否则客户端反复整屏重建");
        }
        // 非整帧：哪一条都不带 FULL
        for i in 0..3 {
            assert_eq!(chunk_flags(i, false), 0);
        }
    }

    #[test]
    fn display_switch_updates_origin_for_input_and_cursor() {
        // 双屏并排：display0 origin=(0,0)，display1 origin=(1920,0)
        let d0 = DisplayMapping::from_screen(0, &screen((0, 0)));
        let d1 = DisplayMapping::from_screen(1, &screen((1920, 0)));
        let vd = input::VirtualDesktop {
            x: 0,
            y: 0,
            w: 3840,
            h: 1080,
        };
        // 同一本地坐标在不同显示输出上落到不同桌面位置
        let (ax0, _) = d0.to_absolute((100, 50), vd);
        let (ax1, ay1) = d1.to_absolute((100, 50), vd);
        assert!(ax0 < 10_000, "display0 应在左半屏: ax0={ax0}");
        assert!(ax1 > 32_000, "display1 应在右半屏: ax1={ax1}");
        assert!(ay1 < 10_000);
        // 光标反算：桌面 (2020,50) 在 display1 上是本地 (100,50)，在 display0 上不在画面内
        let ((lx, ly), inside) = d1.to_local((2020, 50), 1.0);
        assert_eq!((lx, ly), (100, 50));
        assert!(inside);
        let ((_, _), inside) = d0.to_local((2020, 50), 1.0);
        assert!(!inside, "display0 上不应包含 display1 的坐标");
        // 切回 display0 也一致
        let ((lx, ly), inside) = d0.to_local((100, 50), 1.0);
        assert_eq!((lx, ly), (100, 50));
        assert!(inside);
        assert_eq!(d0.index, 0);
        assert_eq!(d1.index, 1);
        assert_eq!(d1.origin, (1920, 0));
        assert_eq!(d1.size, (1920, 1080));
    }

    #[test]
    fn block_bitrate_trigger_thresholds() {
        // 默认 4 Mbps 视频码率 → 阈值 max(6M, 6M) = 6 Mbps
        assert!(!block_bitrate_trigger(5_900.0, 4_000_000));
        assert!(block_bitrate_trigger(6_100.0, 4_000_000));
        // 视频码率配得很高（8 Mbps）→ 阈值抬到 12 Mbps
        assert!(!block_bitrate_trigger(8_000.0, 8_000_000));
        assert!(block_bitrate_trigger(12_100.0, 8_000_000));
        // 视频码率很低（1 Mbps）→ 阈值仍是 6 Mbps 底线
        assert!(block_bitrate_trigger(6_500.0, 1_000_000));
        assert!(!block_bitrate_trigger(5_000.0, 1_000_000));
    }

    #[test]
    fn force_video_switches_mode_and_respects_unavailable() {
        let mut sw = ModeSwitch::new(true);
        sw.force_video();
        assert_eq!(sw.mode(), Mode::Video);
        // 不可用时不允许强制
        let mut sw = ModeSwitch::new(false);
        sw.force_video();
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn scaled_dims_snap_to_block_grid() {
        assert_eq!(scaled_dims(1920, 1080, 1.0), (1920, 1080));
        let (w, h) = scaled_dims(1920, 1080, 0.83);
        assert_eq!(w % 64, 0);
        assert_eq!(h % 64, 0);
        assert_eq!((w, h), (1600, 896));
        let (w, h) = scaled_dims(1920, 1080, 0.67);
        assert_eq!((w, h), (1280, 704));
        // 极端缩放也不出 0
        let (w, h) = scaled_dims(1920, 1080, 0.0);
        assert!(w >= 64 && h >= 64);
    }

    #[test]
    fn downscale_box_filter_averages() {
        let (w, h) = (4u32, 4u32);
        let stride = w as usize * 4;
        let mut src = vec![0u8; stride * h as usize];
        for y in 0..4usize {
            for x in 0..4usize {
                let v: u8 = match (x < 2, y < 2) {
                    (true, true) => 100,
                    (false, true) => 200,
                    (true, false) => 50,
                    (false, false) => 0,
                };
                let o = y * stride + x * 4;
                src[o] = v;
                src[o + 1] = v;
                src[o + 2] = v;
                src[o + 3] = 255;
            }
        }
        let mut dst = Vec::new();
        assert!(downscale_bgra(&src, stride, 4, 4, &mut dst, 2, 2));
        assert_eq!(dst.len(), 2 * 2 * 4);
        // 2x2 -> 1x1：每块正好 4 个像素平均
        assert_eq!(&dst[0..4], &[100, 100, 100, 255]);
        assert_eq!(&dst[4..8], &[200, 200, 200, 255]);
        assert_eq!(&dst[8..12], &[50, 50, 50, 255]);
        assert_eq!(&dst[12..16], &[0, 0, 0, 255]);
        // 非法尺寸直接失败，不 panic
        assert!(!downscale_bgra(&src, stride, 4, 4, &mut dst, 0, 2));
    }

    #[test]
    fn stats_line_has_all_keys() {
        let zero = Stats::default();
        let line = stats::format_line(
            &zero,
            &zero,
            1.0,
            1.0,
            &stats::VideoLine::default(),
            &stats::PathLine::default(),
        );
        for key in [
            "t=",
            "fps=",
            "blocks=",
            "sent_frames=",
            "bytes=",
            "kbps=",
            "idle_frames=",
            "dropped=",
            "sec_bytes=",
            "level=",
            "stage=",
            "scale=",
            "path=",
            "udp_rtt=",
            "udp_loss=",
        ] {
            assert!(line.contains(key), "统计行缺少 {key}: {line}");
        }
    }

    #[test]
    fn track_grid_matches_1080p() {
        let t = Track::new(1920, 1080).expect("track");
        assert_eq!(t.grid.blocks_x, 30);
        assert_eq!(t.grid.blocks_y, 17);
        assert_eq!(t.hashes.len(), 510);
        assert!(!t.have_prev);
        assert!(t.scan_all);
        assert!(Track::new(0, 1080).is_none());
    }
}
