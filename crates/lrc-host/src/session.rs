//! 一条被控会话：握手鉴权 → 采集/差分/编码/发送 + 接收线程（输入 / ack / ping）。

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lrc_capture::{CaptureError, Capturer, ScreenInfo};
use lrc_codec::{self, Options};
use lrc_proto::{
    auth_reason, frame_flags, AuthResult, BlockEntry, ClientHello, CursorPos, Grid, Message, Pong,
    Rect, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::{Conn, ConnReader, ConnWriter};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, CURSORINFO, CURSOR_SHOWING,
};

use crate::auth;
use crate::cli::Args;
use crate::input;
use crate::stats::Stats;

/// 块边长（DESIGN §0：64×64 块）。
const BLOCK_SIZE: u16 = 64;
/// 握手阶段单条消息的读/写超时，防止"连上不说话"的连接占住单流。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 会话期间单次写的超时：对端完全不读时不能永久阻塞。
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// 未确认字节上限（DESIGN §1 的滑动窗口）：超过就丢帧、不排队。
const MAX_INFLIGHT_BYTES: u64 = 8 * 1024 * 1024;
/// 没有新帧时的轮询间隔：这是静止时线上 0 字节、CPU 接近 0 的来源。
const IDLE_POLL: Duration = Duration::from_millis(2);
/// 帧率节流时单次睡眠的上限，保证停止标志/光标能被及时处理。
const MAX_THROTTLE_SLEEP: Duration = Duration::from_millis(20);
/// 采集器连续重建失败的次数上限，超过就结束这条会话。
const CAPTURE_REOPEN_TRIES: u32 = 20;

/// 会话期的共享状态：写端 + 客户端反馈 + 停止标志。
struct Shared {
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
    fn send(&self, payload: &[u8]) -> io::Result<()> {
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
    log::info!("客户端已鉴权，开始推流");

    // 会话期间：读端永久阻塞（靠 shutdown 唤醒），写端保留超时
    if let Err(e) = reader.set_read_timeout(None) {
        log::warn!("清除读超时失败：{e}");
    }
    if let Err(e) = writer.set_write_timeout(Some(SEND_TIMEOUT)) {
        log::warn!("设置写超时失败：{e}");
    }

    let shared = Arc::new(Shared::new(writer, (info.origin_x, info.origin_y)));
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

    let result = capture_loop(args, &mut capturer, &mut info, Arc::clone(&shared));

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
                if !shared.stop() {
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
) -> Result<(), String> {
    let mut track = Track::new(info.width, info.height)
        .ok_or_else(|| format!("采集尺寸非法 {}x{}", info.width, info.height))?;
    let opts = Options {
        jpeg_quality: args.quality,
        ..Options::default()
    };
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    let session_start = Instant::now();

    let mut seq: u32 = 0;
    let mut force_full = true;
    let mut next_grab_at = Instant::now();
    let mut last_cursor: Option<(i16, i16, bool)> = None;
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
        report_if_due(args, &mut sec, &mut total, &mut last_report);

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

        let frame = match grabbed {
            Ok(Some(f)) => f,
            Ok(None) => {
                // 没有任何像素变化：一个字节都不发（零变化零发送的源头）
                sec.idle += 1;
                if let Err(e) = maybe_send_cursor(&shared, &mut last_cursor, &mut sec) {
                    log_send_failure(&shared, "发送光标", &e);
                    break;
                }
                std::thread::sleep(IDLE_POLL);
                continue;
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

        // 流控：客户端没确认的字节太多 → 丢这一帧、不排队。
        // 这一帧的 dirty rect 已经消费掉了，所以下一帧必须全盘重扫（否则变化块永久丢失）。
        if shared.ack_seen() && shared.inflight_bytes() > MAX_INFLIGHT_BYTES {
            sec.dropped += 1;
            track.scan_all = true;
            next_grab_at = Instant::now() + frame_interval;
            log::debug!("流控丢帧：未确认字节 {} ", shared.inflight_bytes());
            continue;
        }

        // 分辨率 / 显示切换
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
        }

        if shared.request_full.swap(false, Ordering::Relaxed) {
            force_full = true;
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

        if changed.is_empty() {
            // 采集到了帧，但候选块内容没变：依然一个字节都不发
            sec.idle += 1;
            if let Err(e) = maybe_send_cursor(&shared, &mut last_cursor, &mut sec) {
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
            }
            Err(e) => {
                log_send_failure(&shared, "发送帧", &e);
                break;
            }
        }

        if let Err(e) = maybe_send_cursor(&shared, &mut last_cursor, &mut sec) {
            log_send_failure(&shared, "发送光标", &e);
            break;
        }
        next_grab_at = t_cap + frame_interval;
    }

    total.add(&sec);
    log::info!(
        "会话结束，总计 {}",
        total.format_line(session_start.elapsed().as_secs_f64())
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

/// 每秒一行统计（--stats）。
fn report_if_due(args: &Args, sec: &mut Stats, total: &mut Stats, last_report: &mut Instant) {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(*last_report);
    if elapsed < Duration::from_secs(1) {
        return;
    }
    if args.stats {
        println!("{}", sec.format_line(elapsed.as_secs_f64()));
        let _ = io::stdout().flush();
    }
    total.add(sec);
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

/// 光标位置变化才发一条 CursorPos（光标不进码流，DESIGN §2.4）。
/// 光标位图（Message::CursorShape）按计划留到 M1.5，M1 只用系统光标。
fn maybe_send_cursor(
    shared: &Shared,
    last: &mut Option<(i16, i16, bool)>,
    sec: &mut Stats,
) -> io::Result<()> {
    let Some(cur) = poll_cursor() else {
        return Ok(());
    };
    if *last == Some(cur) {
        return Ok(());
    }
    let payload = Message::CursorPos(CursorPos {
        x: cur.0,
        y: cur.1,
        visible: cur.2,
    })
    .encode();
    shared.send(&payload)?;
    *last = Some(cur);
    sec.bytes += payload.len() as u64 + 4;
    Ok(())
}

/// 读物理像素的光标位置与可见性（main 里已声明 PER_MONITOR_AWARE_V2）。
fn poll_cursor() -> Option<(i16, i16, bool)> {
    // SAFETY: 两个调用只写本栈上的结构体，指针来自 &mut。
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let visible = match GetCursorInfo(&mut ci) {
            Ok(()) => (ci.flags.0 & CURSOR_SHOWING.0) != 0 && !ci.hCursor.is_invalid(),
            Err(_) => true,
        };
        Some((clamp_i16(pt.x), clamp_i16(pt.y), visible))
    }
}

/// CursorPos 的 x/y 是 i16：虚拟桌面坐标可能为负，超出就夹紧。
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
    fn clamp_i16_range() {
        assert_eq!(clamp_i16(0), 0);
        assert_eq!(clamp_i16(-40000), i16::MIN);
        assert_eq!(clamp_i16(40000), i16::MAX);
        assert_eq!(clamp_i16(-1920), -1920);
    }

    #[test]
    fn stats_line_has_all_keys() {
        let line = Stats::default().format_line(1.0);
        for key in [
            "fps=",
            "capture_ms_avg=",
            "blocks_changed=",
            "bytes_sent=",
            "kbps=",
            "idle_frames=",
            "dropped=",
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
