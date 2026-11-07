//! 一条被控会话：握手鉴权 → 采集/差分/编码/发送 + 接收线程（输入 / ack / ping）。

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lrc_capture::cursor::CursorWatcher;
use lrc_capture::{CaptureError, Capturer, Frame, ScreenInfo};
use lrc_codec::{self, Options};
use lrc_proto::{
    auth_reason, frame_flags, AuthResult, BlockEntry, ClientHello, CursorPos, CursorShape, Grid,
    Message, Pong, Rect, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::{Conn, ConnReader, ConnWriter};
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
/// 帧率节流时单次睡眠的上限，保证停止标志/光标能被及时处理。
const MAX_THROTTLE_SLEEP: Duration = Duration::from_millis(20);
/// 采集器连续重建失败的次数上限，超过就结束这条会话。
const CAPTURE_REOPEN_TRIES: u32 = 20;
/// CursorShape 消息的边长上限：lrc-proto 的 MAX_CURSOR_BYTES = 256*256*4，
/// 超过客户端会解不动（更大会话就断），所以宁可不发形状只发位置。
const MAX_CURSOR_SHAPE_DIM: u32 = 256;

/// 会话期的共享状态：写端 + 客户端反馈 + 停止标志。
///
/// pub(crate) 是因为视频编码线程（crate::video）也要通过它写 socket。
pub(crate) struct Shared {
    /// 收发线程共享同一个写端；一帧必须整条原子写入。
    writer: Mutex<ConnWriter>,
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
    /// 客户端请求整帧。
    request_full: AtomicBool,
    /// 当前采集输出在虚拟桌面里的原点（输入坐标换算用）。
    origin_x: AtomicI32,
    origin_y: AtomicI32,
}

impl Shared {
    fn new(writer: ConnWriter, origin: (i32, i32)) -> Self {
        Self {
            writer: Mutex::new(writer),
            stop: AtomicBool::new(false),
            peer_gone: AtomicBool::new(false),
            ack_seq: AtomicU32::new(0),
            ack_seen: AtomicBool::new(false),
            ack_bytes: AtomicU64::new(0),
            request_full: AtomicBool::new(false),
            origin_x: AtomicI32::new(origin.0),
            origin_y: AtomicI32::new(origin.1),
        }
    }

    /// 锁中毒时照常取回：socket 还能用，没有理由因此断流。
    fn writer_guard(&self) -> std::sync::MutexGuard<'_, ConnWriter> {
        match self.writer.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }

    /// 写一条已编码消息。
    pub(crate) fn send(&self, payload: &[u8]) -> io::Result<()> {
        self.writer_guard().send(payload)
    }

    fn shutdown(&self) {
        self.writer_guard().shutdown();
    }

    fn stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 已发出但客户端还没确认的字节数（估算，用于流控）。
    fn inflight_bytes(&self) -> u64 {
        let sent = self.writer_guard().sent_bytes();
        sent.saturating_sub(self.ack_bytes.load(Ordering::Relaxed))
    }

    fn ack_seq(&self) -> u32 {
        self.ack_seq.load(Ordering::Relaxed)
    }

    /// 已经收到过 ack，才启用基于未确认字节的流控。
    fn ack_seen(&self) -> bool {
        self.ack_seen.load(Ordering::Relaxed)
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
}

/// 握手期的拒绝原因。
#[derive(Debug)]
enum Reject {
    Io(io::Error),
    Closed,
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
            Reject::Closed => write!(f, "对端直接关闭"),
            Reject::Version(v) => write!(f, "协议版本不匹配 {v} != {PROTO_VERSION}"),
            Reject::Proto(m) => write!(f, "消息解析失败 {m}"),
            Reject::BadMessage(m) => write!(f, "握手顺序错误 {m}"),
            Reject::BadPassword => write!(f, "密码错误"),
            Reject::Auth(e) => write!(f, "鉴权计算失败 {e}"),
        }
    }
}

impl std::error::Error for Reject {}

/// 处理一条已接受的连接，直到会话结束。返回 Ok(()) 表示可以继续 accept 下一条。
pub fn run(conn: Conn, args: &Args, nonce_seq: u64) -> Result<(), String> {
    let (mut writer, mut reader) = conn.split().map_err(|e| format!("拆分流失败: {e}"))?;
    set_timeouts(&reader, &writer, Some(HANDSHAKE_TIMEOUT));

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

    // 会话期间：读端永久阻塞（靠 shutdown 唤醒），写端保留超时
    if let Err(e) = reader.set_read_timeout(None) {
        log::warn!("清除读超时失败：{e}");
    }
    if let Err(e) = writer.set_write_timeout(Some(SEND_TIMEOUT)) {
        log::warn!("设置写超时失败：{e}");
    }

    let shared = Arc::new(Shared::new(writer, (info.origin_x, info.origin_y)));
    // 视频通道：会话建立时创建编码器 + 预热探测（失败就一直用块模式，不重试）
    let pipeline = crate::video::start(args, info.width, info.height, Arc::clone(&shared));

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

    let result = capture_loop(args, &mut capturer, &mut info, Arc::clone(&shared), pipeline);

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

fn set_timeouts(reader: &ConnReader, writer: &ConnWriter, timeout: Option<Duration>) {
    if let Err(e) = reader.set_read_timeout(timeout) {
        log::warn!("设置读超时失败：{e}");
    }
    if let Err(e) = writer.set_write_timeout(timeout) {
        log::warn!("设置写超时失败：{e}");
    }
}

fn open_capturer(display: usize) -> Result<(Capturer, ScreenInfo), CaptureError> {
    // timeout_ms = 0：非阻塞轮询。没有变化时 grab() 返回 Ok(None)，一个字节都不发。
    let capturer = Capturer::new(display, 0)?;
    let info = capturer.info();
    Ok((capturer, info))
}

/// 等 ClientHello 并检查协议版本。
fn recv_client_hello(reader: &mut ConnReader) -> Result<ClientHello, Reject> {
    let mut buf = Vec::with_capacity(256);
    reader
        .recv(&mut buf)
        .map_err(Reject::Io)?
        .ok_or(Reject::Closed)?;
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
    reader: &mut ConnReader,
    writer: &mut ConnWriter,
    password: &str,
    nonce_c: &[u8; auth::NONCE_LEN],
    nonce_s: &[u8; auth::NONCE_LEN],
) -> Result<(), Reject> {
    let mut buf = Vec::with_capacity(128);
    reader
        .recv(&mut buf)
        .map_err(Reject::Io)?
        .ok_or(Reject::Closed)?;
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
fn reader_loop(mut reader: ConnReader, shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    loop {
        if shared.stop() {
            break;
        }
        match reader.recv(&mut buf) {
            Ok(Some(n)) => handle_incoming(&buf[..n], &shared),
            Ok(None) => {
                log::info!("客户端关闭了连接");
                break;
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
            shared.ack_seq.store(ack.seq, Ordering::Relaxed);
            shared.ack_seen.store(true, Ordering::Relaxed);
            shared.ack_bytes.store(ack.recv_bytes, Ordering::Relaxed);
            log::trace!("ack seq={} recv_bytes={}", ack.seq, ack.recv_bytes);
        }
        Message::RequestFull => {
            shared.request_full.store(true, Ordering::Relaxed);
            log::info!("客户端请求整帧");
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

/// 采集/差分/编码/发送主循环。
fn capture_loop(
    args: &Args,
    capturer: &mut Capturer,
    info: &mut ScreenInfo,
    shared: Arc<Shared>,
    pipeline: Option<VideoPipeline>,
) -> Result<(), String> {
    let mut track = Track::new(info.width, info.height)
        .ok_or_else(|| format!("采集尺寸非法 {}x{}", info.width, info.height))?;
    let opts = Options {
        jpeg_quality: args.quality,
        ..Options::default()
    };
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    // 视频模式不跟随 --fps，单独按 --video-max-fps 采集
    let video_interval = Duration::from_secs_f64(1.0 / f64::from(args.video_max_fps.max(1)));
    let session_start = Instant::now();

    let mut seq: u32 = 0;
    let mut force_full = true;
    let mut next_grab_at = Instant::now();
    let mut cursor = CursorTracker::new((info.origin_x, info.origin_y), (info.width, info.height));
    // 最近一帧像素：RequestFull 在静止桌面时靠它补整帧。
    // 用 Arc 是为了视频模式能零拷贝地把同一帧交给编码线程（cached 自己还要留着补帧）。
    let mut cached: Option<Arc<Frame>> = None;
    // 块/视频模式判定
    let mut mode_switch = ModeSwitch::new(pipeline.is_some());
    let mut video_gen: u32 = 0;
    // 一次性标志：本轮若没有新帧，就用缓存帧发一帧整帧（RequestFull / 模式退出）
    let mut force_cache_resend = false;
    let mut last_video = VideoCounters::default();
    let mut slots: Vec<BlockSlot> = Vec::new();
    let mut cand: Vec<u32> = Vec::new();
    let mut rects: Vec<Rect> = Vec::new();
    let mut changed: Vec<(u32, u64)> = Vec::new();
    let mut total = Stats::default();
    let mut sec = Stats::default();
    let mut last_report = Instant::now();
    let mut reopen_failures: u32 = 0;

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
        let video_now = pipeline
            .as_ref()
            .map(|p| p.counters())
            .unwrap_or_default();
        report_if_due(
            args,
            &mut sec,
            &mut total,
            &mut last_report,
            session_start,
            mode_switch.mode().label(),
            video_now,
            &mut last_video,
        );

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
        let grabbed = capturer.grab();
        let capture_ms = t_cap.elapsed().as_secs_f64() * 1000.0;

        let mut has_new_frame = false;
        // 抓帧完成时刻（视频 AU 的 ts_ms 用它）：GetTickCount64 低 32 位。
        // 和客户端 lat_ms 的 now 同源，**只在同机测试时有效**，跨机只能作参考。
        let capture_tick = unsafe { GetTickCount64() } as u32;
        match grabbed {
            Ok(Some(f)) => {
                has_new_frame = true;
                // 缓存最近一帧像素（整体换出，不 clone）；RequestFull 静止时用它重发
                cached = Some(Arc::new(f));
            }
            Ok(None) => {
                // 没有任何像素变化：一个字节的画面数据都不发（零变化零发送的源头）
                sec.idle += 1;
            }
            Err(e @ (CaptureError::Lost | CaptureError::Runtime(_))) => {
                log::warn!("采集出错：{e}");
                reopen_failures += 1;
                if reopen_failures > CAPTURE_REOPEN_TRIES {
                    return Err(format!("采集器连续 {reopen_failures} 次重建失败，放弃会话"));
                }
                let backoff = u64::from(reopen_failures).min(10) * 100;
                std::thread::sleep(Duration::from_millis(backoff));
                match open_capturer(args.display) {
                    Ok((fresh, fresh_info)) => {
                        *capturer = fresh;
                        *info = fresh_info;
                        track = Track::new(fresh_info.width, fresh_info.height).ok_or_else(|| {
                            format!("重建后尺寸非法 {}x{}", fresh_info.width, fresh_info.height)
                        })?;
                        cached = None;
                        cursor.set_mapping(
                            (fresh_info.origin_x, fresh_info.origin_y),
                            (fresh_info.width, fresh_info.height),
                        );
                        shared.set_origin((fresh_info.origin_x, fresh_info.origin_y));
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
            Err(CaptureError::Unsupported(m)) => return Err(format!("采集不支持：{m}")),
            Err(CaptureError::Init(m)) => return Err(format!("采集初始化失败：{m}")),
        };

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
                                    if shared.ack_seen()
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

        let Some(frame) = cached.as_deref() else {
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

        // 流控：客户端没确认的字节太多 → 丢这一帧、不排队。
        // 这一帧的 dirty rect 已经消费掉了，所以下一帧必须全盘重扫（否则变化块永久丢失）。
        if shared.ack_seen() && shared.inflight_bytes() > MAX_INFLIGHT_BYTES {
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
        if frame.width != track.width || frame.height != track.height {
            log::info!(
                "采集尺寸变化 {}x{} → {}x{}，重建块网格",
                track.width,
                track.height,
                frame.width,
                frame.height
            );
            track = Track::new(frame.width, frame.height)
                .ok_or_else(|| format!("帧尺寸非法 {}x{}", frame.width, frame.height))?;
            cursor.set_mapping((info.origin_x, info.origin_y), (frame.width, frame.height));
        }

        // 首帧 / 客户端请求整帧 / 刚重建过 → 整帧发，flags 带 FULL|KEY
        let send_full = force_full || !track.have_prev;
        let scan_all = send_full || track.scan_all || frame.full;

        if scan_all {
            cand.clear();
            cand.extend(0..track.grid.count());
        } else {
            rects.clear();
            rects.extend_from_slice(&frame.dirty);
            for mv in &frame.moves {
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
            let known = track.hashes.get(idx as usize).copied();
            if send_full || !track.have_prev || known != Some(hash) {
                changed.push((idx, hash));
            }
        }

        // 块模式下也观察变化比例：连续 3 帧 > 45% 就切视频通道
        match mode_switch.observe(changed.len(), track.grid.count() as usize) {
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
            let Some(offset) = pixel_offset(&rect, frame.stride) else {
                continue;
            };
            if offset >= frame.bgra.len() {
                continue;
            }
            while slots.len() <= used {
                slots.push(BlockSlot::default());
            }
            let slot = &mut slots[used];
            match lrc_codec::encode_block(
                &frame.bgra[offset..],
                frame.stride,
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

        let entries: Vec<BlockEntry<'_>> = slots[..used]
            .iter()
            .map(|s| BlockEntry {
                idx: s.idx,
                codec: s.codec,
                data: s.data.as_slice(),
            })
            .collect();
        let flags = if send_full {
            frame_flags::FULL | frame_flags::KEY
        } else {
            0
        };
        let msg = Message::VideoFrame(VideoFrame {
            seq,
            flags,
            base_seq: shared.ack_seq(),
            block_size: BLOCK_SIZE,
            blocks_x: track.grid.blocks_x,
            blocks_y: track.grid.blocks_y,
            entries,
        });
        let payload = msg.encode();
        // 传输层单条消息硬上限 32 MiB（4K 全屏 + 不可压块才可能撞上）：宁可整帧不发也不能发半条
        if payload.len() > lrc_proto::MAX_PAYLOAD {
            log::warn!(
                "整帧过大 {} 字节 > 上限 {}，本帧丢弃（降低 --quality 或换更小分辨率）",
                payload.len(),
                lrc_proto::MAX_PAYLOAD
            );
            track.scan_all = true;
            next_grab_at = t_cap + frame_interval;
            continue;
        }
        match shared.send(&payload) {
            Ok(()) => {
                seq = seq.wrapping_add(1);
                sec.frames += 1;
                sec.blocks += used as u64;
                sec.bytes += payload.len() as u64 + 4;
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
                if request_full_now {
                    sec.full_resends += 1;
                    log::debug!(
                        "已响应整帧请求（{}，{} 块）",
                        if resend_from_cache {
                            "静止桌面用缓存帧"
                        } else {
                            "新帧"
                        },
                        used
                    );
                }
            }
            Err(e) => {
                log_send_failure(&shared, "发送帧", &e);
                break;
            }
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
    log::info!(
        "会话结束，总计 {} capture_ms_avg={capture_avg:.2}",
        stats::format_summary(&total, session_secs, &video_summary)
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
    video_now: VideoCounters,
    last_video: &mut VideoCounters,
) {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(*last_report);
    if elapsed < Duration::from_secs(1) {
        return;
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
    };

    total.add(sec);
    if args.stats {
        let session_secs = now.saturating_duration_since(session_start).as_secs_f64();
        println!(
            "{}",
            stats::format_line(total, sec, session_secs, interval, &video_line)
        );
        let _ = io::stdout().flush();
    }
    sec.reset();
    *last_report = now;
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
    /// 采集输出在虚拟桌面里的原点
    origin: (i32, i32),
    /// 采集画面尺寸
    region: (u32, u32),
}

impl CursorTracker {
    fn new(origin: (i32, i32), region: (u32, u32)) -> Self {
        Self {
            watcher: CursorWatcher::new(),
            last_pos: None,
            last_shape: None,
            origin,
            region,
        }
    }

    fn stats(&self) -> lrc_capture::CursorWatcherStats {
        self.watcher.stats()
    }

    /// 采集器重建 / 分辨率变化时更新映射口径。
    fn set_mapping(&mut self, origin: (i32, i32), region: (u32, u32)) {
        self.origin = origin;
        self.region = region;
    }

    /// 主循环每轮调一次；只有真的有变化才发消息（静止时 0 字节）。
    fn poll_and_send(&mut self, shared: &Shared, sec: &mut Stats) -> io::Result<()> {
        let Some(state) = self.watcher.poll() else {
            return Ok(());
        };

        let ((sx, sy), inside) = map_cursor_pos((state.x, state.y), self.origin, self.region);
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
    region: (u32, u32),
) -> ((i32, i32), bool) {
    let local_x = desk.0.saturating_sub(origin.0);
    let local_y = desk.1.saturating_sub(origin.1);
    let inside = local_x >= 0
        && local_y >= 0
        && (local_x as u32) < region.0
        && (local_y as u32) < region.1;
    if inside {
        return ((local_x, local_y), true);
    }
    let max_x = region.0.saturating_sub(1) as i32;
    let max_y = region.1.saturating_sub(1) as i32;
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
        let ((x, y), inside) = map_cursor_pos((100, 200), (0, 0), (1920, 1080));
        assert_eq!((x, y), (100, 200));
        assert!(inside);
        // 右下角最后一个像素仍算在内
        let ((x, y), inside) = map_cursor_pos((1919, 1079), (0, 0), (1920, 1080));
        assert_eq!((x, y), (1919, 1079));
        assert!(inside);
        // 刚好越界一个像素
        let ((x, _), inside) = map_cursor_pos((1920, 0), (0, 0), (1920, 1080));
        assert_eq!(x, 1919);
        assert!(!inside);
        let ((_, y), inside) = map_cursor_pos((0, 1080), (0, 0), (1920, 1080));
        assert_eq!(y, 1079);
        assert!(!inside);
    }

    #[test]
    fn cursor_mapping_second_screen_uses_origin() {
        // 双屏并排：采集第 2 块屏，origin=(1920,0)
        let origin = (1920, 0);
        let region = (1920u32, 1080u32);
        let ((x, y), inside) = map_cursor_pos((1920, 0), origin, region);
        assert_eq!((x, y), (0, 0));
        assert!(inside);
        let ((x, y), inside) = map_cursor_pos((3839, 1079), origin, region);
        assert_eq!((x, y), (1919, 1079));
        assert!(inside);
        // 光标跑到第 1 块屏 → 本地坐标为负、不在画面内
        let ((x, y), inside) = map_cursor_pos((1000, 500), origin, region);
        assert_eq!((x, y), (0, 500));
        assert!(!inside);
        // 坐标负得更多也不会溢出
        let ((x, _), inside) = map_cursor_pos((i32::MIN, 0), origin, region);
        assert_eq!(x, 0);
        assert!(!inside);
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

    #[test]
    fn stats_line_has_all_keys() {
        let zero = Stats::default();
        let line = stats::format_line(&zero, &zero, 1.0, 1.0, &stats::VideoLine::default());
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
