//! Linux 被控端：X11 采集 → 64×64 块哈希差分 → 编码 → 传输；输入回放。
//!
//! 用法（与 Windows 版一致）：
//!   lrc-host-linux --password 1234 [--display :1] [--listen 0.0.0.0:7800]
//!                  [--signal <vps>:7801 --room 1 --p2p auto|relay|direct|off]
//!
//! 与 Windows 版 crates/lrc-host 的差别（平台相关部分）：
//! - 采集：lrc_linux::X11Capturer 每次抓整屏，没有脏区通知，靠块哈希自己算增量
//! - 输入：lrc_linux::input 的 XTEST 封装，键位需要把 Windows scancode 翻成 X11 keysym
//! - 不做：视频通道、音频、光标形状、显示器切换、自适应控制器（后续再加）

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use lrc_proto::{
    auth_reason, frame_flags, AuthResult, BlockEntry, ClientHello, Grid, InputEvent,
    Message, Pong, Rect, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::link::{self, LinkReader, LinkWriter};
use lrc_udp::UdpConn;
use sha2::{Digest, Sha256};


// ─────────────────────────── 常量 ───────────────────────────

const BLOCK_SIZE: u16 = 64;
/// 握手阶段单条消息的读超时
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 写超时：对端不读时不能永久阻塞
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// 未确认字节上限（流控）
const MAX_INFLIGHT_BYTES: u64 = 8 * 1024 * 1024;
/// 读线程轮询粒度（TCP/UDP 统一）
const READ_POLL: Duration = Duration::from_millis(200);
/// 节流时单次睡眠上限
const MAX_THROTTLE_SLEEP: Duration = Duration::from_millis(20);
/// 采集连续失败多少次后放弃会话
const MAX_CAPTURE_FAILURES: u32 = 20;
/// 单条 VideoFrame 的 payload 预算（UDP 一个分片约 1.2 KB，32 KB ≈ 26 个分片）
const VIDEO_FRAME_CHUNK_BYTES: usize = 32 * 1024;
/// VideoFrame 固定头：type(1)+seq(4)+flags(1)+base_seq(4)+block_size(2)+blocks_x(2)+blocks_y(2)+count(2)
const VIDEO_FRAME_HEAD_BYTES: usize = 18;
/// 每个块条目的固定开销：idx(4)+codec(1)+len(4)
const BLOCK_ENTRY_BYTES: usize = 9;
/// nonce 长度
const NONCE_LEN: usize = 16;
/// P2P 等对端时间上限
const PEER_WAIT: Duration = Duration::from_secs(60);
/// 打洞超时
const PUNCH_TIMEOUT: Duration = Duration::from_secs(2);

// ─────────────────────────── CLI ───────────────────────────

/// P2P 路径选择（与 Windows 版 path.rs 完全一致的语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum P2pMode {
    Auto,
    Off,
    Direct,
    Relay,
}

impl P2pMode {
    fn label(self) -> &'static str {
        match self {
            P2pMode::Auto => "auto",
            P2pMode::Off => "off",
            P2pMode::Direct => "direct",
            P2pMode::Relay => "relay",
        }
    }
}

#[derive(Debug, Clone)]
struct Args {
    password: String,
    listen: String,
    /// X11 display 名（例如 ":1"）；None = 用环境变量 DISPLAY
    display: Option<String>,
    fps: u32,
    quality: u8,
    stats: bool,
    signal: Option<SocketAddr>,
    room: u32,
    p2p: P2pMode,
}

fn usage() -> &'static str {
    "用法: lrc-host-linux --password <PWD> [--listen IP:PORT] [--display :1] [--fps N] [--quality N]\n                         [--stats] [--signal IP:PORT] [--room N] [--p2p auto|off|direct|relay]"
}

/// 取一个参数值：支持 --flag value 与 --flag=value 两种写法。
fn take_value(
    it: &mut std::vec::IntoIter<String>,
    inline: Option<String>,
    name: &str,
) -> Result<String, String> {
    if let Some(v) = inline {
        return Ok(v);
    }
    it.next().ok_or_else(|| format!("{name} 需要一个值"))
}

fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let mut password: Option<String> = None;
    let mut listen = String::from("0.0.0.0:7800");
    let mut display: Option<String> = None;
    let mut fps: u32 = 30;
    let mut quality: u8 = 85;
    let mut stats = false;
    let mut signal: Option<SocketAddr> = None;
    let mut room: u32 = 1;
    let mut p2p = P2pMode::Auto;

    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg, None),
        };
        let inline_val = inline.clone();
        match name.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--stats" => stats = !matches!(inline.as_deref(), Some("0") | Some("false") | Some("no")),
            "--no-stats" => stats = false,
            "--password" => password = Some(take_value(&mut it, inline_val.clone(), &name)?),
            "--listen" => listen = take_value(&mut it, inline_val.clone(), &name)?,
            "--display" => display = Some(normalize_display(&take_value(&mut it, inline_val.clone(), &name)?)),
            "--fps" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                fps = v.parse::<u32>().map_err(|_| format!("--fps 需要整数，收到 {v}"))?;
                if fps == 0 || fps > 240 {
                    return Err(format!("--fps 应在 1..=240，收到 {fps}"));
                }
            }
            "--quality" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                let q = v.parse::<u32>().map_err(|_| format!("--quality 需要整数，收到 {v}"))?;
                if q == 0 || q > 100 {
                    return Err(format!("--quality 应在 1..=100，收到 {q}"));
                }
                quality = q as u8;
            }
            "--signal" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                signal = Some(
                    v.parse::<SocketAddr>()
                        .map_err(|_| format!("--signal 需要 IP:PORT，收到 {v}"))?,
                );
            }
            "--room" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                room = v.parse::<u32>().map_err(|_| format!("--room 需要整数，收到 {v}"))?;
            }
            "--p2p" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                p2p = match v.as_str() {
                    "auto" => P2pMode::Auto,
                    "off" | "none" | "0" => P2pMode::Off,
                    "direct" => P2pMode::Direct,
                    "relay" => P2pMode::Relay,
                    other => return Err(format!("--p2p 只支持 auto|off|direct|relay，收到 {other}")),
                };
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    let password = password.ok_or_else(|| "--password 必填（或用环境变量 LRC_PASSWORD）")?;
    if password.is_empty() {
        return Err("--password 不能为空".to_string());
    }
    Ok(Args {
        password,
        listen,
        display,
        fps,
        quality,
        stats,
        signal,
        room,
        p2p,
    })
}

/// ":1" / "1" / ":1.0" → 统一成 X11 能用的名字。
fn normalize_display(raw: &str) -> String {
    let s = raw.trim();
    if s.starts_with(':') {
        s.to_string()
    } else if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.') {
        format!(":{s}")
    } else {
        s.to_string()
    }
}

// ─────────────────────────── 入口 ───────────────────────────

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) if e.is_empty() => {
            println!("{}", usage());
            return std::process::ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("lrc-host-linux: {e}");
            eprintln!("{}", usage());
            return std::process::ExitCode::from(2);
        }
    };
    if let Some(d) = args.display.as_ref() {
        std::env::set_var("DISPLAY", d);
    }
    match serve(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            log::error!("lrc-host-linux: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

// ─────────────────────────── 链路接入（P2P → TCP） ───────────────────────────

struct AcquiredLink {
    writer: LinkWriter,
    reader: LinkReader,
    path: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    Tcp,
    Fatal,
}

fn should_try_p2p(mode: P2pMode, signal: Option<SocketAddr>) -> bool {
    signal.is_some() && mode != P2pMode::Off
}

fn fallback_on_failure(mode: P2pMode) -> Fallback {
    match mode {
        P2pMode::Auto | P2pMode::Off => Fallback::Tcp,
        P2pMode::Direct | P2pMode::Relay => Fallback::Fatal,
    }
}

fn punch_timeout(mode: P2pMode) -> Duration {
    match mode {
        P2pMode::Relay => Duration::ZERO,
        _ => PUNCH_TIMEOUT,
    }
}

fn need_tcp_listener(mode: P2pMode, signal: Option<SocketAddr>) -> bool {
    if !should_try_p2p(mode, signal) {
        return true;
    }
    fallback_on_failure(mode) == Fallback::Tcp
}

fn can_retry(mode: P2pMode) -> bool {
    fallback_on_failure(mode) == Fallback::Tcp
}

fn acquire_link(listener: Option<&TcpListener>, args: &Args) -> Result<AcquiredLink, String> {
    if should_try_p2p(args.p2p, args.signal) {
        if let Some(relay) = args.signal {
            match establish_udp(relay, args.room, args.p2p) {
                Ok(l) => return Ok(l),
                Err(e) => match fallback_on_failure(args.p2p) {
                    Fallback::Tcp => log::warn!("P2P/relay 不可用（{e}），回退 TCP"),
                    Fallback::Fatal => {
                        return Err(format!(
                            "P2P/relay 不可用（{e}），--p2p {} 不降级",
                            args.p2p.label()
                        ))
                    }
                },
            }
        }
    }
    accept_tcp(listener)
}

fn establish_udp(relay: SocketAddr, room: u32, p2p: P2pMode) -> Result<AcquiredLink, String> {
    log::info!(
        "P2P：向 relay {relay} 登记房间 {room}（--p2p {}，等客户端 {}s，打洞超时 {}ms）",
        p2p.label(),
        PEER_WAIT.as_secs(),
        punch_timeout(p2p).as_millis()
    );
    let established = lrc_relay::establish(true, relay, room, None, PEER_WAIT, punch_timeout(p2p))
        .map_err(|e| format!("relay 建立失败：{e}"))?;
    let peer = established.conn_peer();
    let lrc_relay::Established { socket, mode, .. } = established;
    let path = mode.kind();
    let conn = UdpConn::new(socket, peer, mode).map_err(|e| format!("UdpConn::new 失败：{e}"))?;
    log::info!("P2P 链路就绪：path={path} peer={peer}");
    let udp = Arc::new(conn);
    Ok(AcquiredLink {
        writer: LinkWriter::Udp(Arc::clone(&udp)),
        reader: LinkReader::Udp(udp),
        path,
    })
}

fn accept_tcp(listener: Option<&TcpListener>) -> Result<AcquiredLink, String> {
    let Some(listener) = listener else {
        return Err("需要 TCP 兜底，但本次进程没有绑定监听套接字".to_string());
    };
    log::info!("等待 TCP 客户端连接……");
    let conn = lrc_transport::Conn::accept(listener).map_err(|e| format!("accept 失败：{e}"))?;
    let (writer, reader) = link::from_tcp(conn).map_err(|e| format!("拆分流失败：{e}"))?;
    log::info!("TCP 链路就绪：path={}", link::PATH_TCP);
    Ok(AcquiredLink {
        writer,
        reader,
        path: link::PATH_TCP,
    })
}

fn serve(args: &Args) -> Result<(), String> {
    let need_tcp = need_tcp_listener(args.p2p, args.signal);
    let listener = if need_tcp {
        Some(
            TcpListener::bind(args.listen.as_str())
                .map_err(|e| format!("绑定 {} 失败：{e}", args.listen))?,
        )
    } else {
        None
    };
    match listener.as_ref() {
        Some(l) => {
            let local = l
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| args.listen.clone());
            log::info!(
                "lrc-host-linux 监听 TCP {local}（fps={} quality={} stats={}）",
                args.fps,
                args.quality,
                args.stats
            );
        }
        None => log::info!("lrc-host-linux 不监听 TCP（--p2p {}）", args.p2p.label()),
    }
    match args.signal {
        Some(sig) => log::info!(
            "P2P 打开：信令 {sig}，房间 {}，模式 {}",
            args.room,
            args.p2p.label()
        ),
        None => log::info!("未配置 --signal：只走 TCP"),
    }

    let mut served: u64 = 0;
    loop {
        let link = match acquire_link(listener.as_ref(), args) {
            Ok(l) => l,
            Err(e) => {
                if can_retry(args.p2p) && listener.is_some() {
                    log::warn!("接入失败（{e}），200ms 后重试");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                return Err(e);
            }
        };
        served += 1;
        let path = link.path;
        log::info!("第 {served} 条链路接入：path={path}");
        if let Err(e) = run(link, args) {
            log::error!("会话异常结束：{e}");
        }
        log::info!("第 {served} 条链路结束（path={path}），继续接入");
    }
}

// ─────────────────────────── 鉴权 ───────────────────────────

type HmacSha256 = Hmac<Sha256>;

fn compute_mac(password: &str, nonce_c: &[u8; NONCE_LEN], nonce_s: &[u8; NONCE_LEN]) -> [u8; 32] {
    let mut mac = match HmacSha256::new_from_slice(password.as_bytes()) {
        Ok(m) => m,
        // HMAC 接受任意长度 key；这里不会失败，兜底成零 MAC（必然鉴权失败）
        Err(_) => return [0u8; 32],
    };
    mac.update(nonce_c);
    mac.update(nonce_s);
    let out = mac.finalize().into_bytes();
    let mut mac_bytes = [0u8; 32];
    mac_bytes.copy_from_slice(&out[..32]);
    mac_bytes
}

/// 定时安全比较（避免按字节提前返回泄露信息）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 生成 server nonce：进程 pid + 纳秒时间 + 随机化哈希种子 + 计数器，混进 SHA-256。
fn make_nonce(seq: u64) -> [u8; NONCE_LEN] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let seed = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(seq);
        h.write_u128(nanos);
        h.finish()
    };
    let mut buf = [0u8; NONCE_LEN];
    let mut hasher = Sha256::new();
    hasher.update(pid.to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    hasher.update(seed.to_le_bytes());
    let digest = hasher.finalize();
    buf.copy_from_slice(&digest[..NONCE_LEN]);
    buf
}

// ─────────────────────────── 会话共享状态 ───────────────────────────

struct Shared {
    writer: Mutex<LinkWriter>,
    stop: AtomicBool,
    peer_gone: AtomicBool,
    /// 客户端最近确认的 seq / 已收字节 / 是否有过 ack
    ack_seq: AtomicU32,
    ack_bytes: AtomicU64,
    ack_seen: AtomicBool,
    /// 累计发出的线上字节（含 4 字节长度前缀）
    sent_bytes: AtomicU64,
    /// 客户端请求整帧
    request_full: AtomicBool,
    /// UDP 链路的 RTT（毫秒 ×10）与丢包（千分比），由读线程发布
    udp_rtt_x10: AtomicU32,
    udp_loss_permille: AtomicU32,
}

impl Shared {
    fn new(writer: LinkWriter) -> Self {
        Self {
            writer: Mutex::new(writer),
            stop: AtomicBool::new(false),
            peer_gone: AtomicBool::new(false),
            ack_seq: AtomicU32::new(0),
            ack_bytes: AtomicU64::new(0),
            ack_seen: AtomicBool::new(false),
            sent_bytes: AtomicU64::new(0),
            request_full: AtomicBool::new(false),
            udp_rtt_x10: AtomicU32::new(0),
            udp_loss_permille: AtomicU32::new(0),
        }
    }

    fn stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn stop_now(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn shutdown(&self) {
        match self.writer.lock() {
            Ok(w) => w.shutdown(),
            Err(p) => p.into_inner().shutdown(),
        }
    }

    fn writer_guard(&self) -> std::sync::MutexGuard<'_, LinkWriter> {
        match self.writer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn send(&self, payload: &[u8]) -> std::io::Result<()> {
        self.writer_guard().send(payload)?;
        self.sent_bytes
            .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        Ok(())
    }

    /// 连续发多条消息（一次持锁，中间不会插进别的消息）。
    fn send_batch(&self, payloads: &[Vec<u8>]) -> std::io::Result<()> {
        let mut w = self.writer_guard();
        let mut total = 0u64;
        for p in payloads {
            w.send(p)?;
            total = total.saturating_add(p.len() as u64 + 4);
        }
        self.sent_bytes.fetch_add(total, Ordering::Relaxed);
        Ok(())
    }

    fn ack_seq(&self) -> u32 {
        self.ack_seq.load(Ordering::Relaxed)
    }

    fn ack_fresh(&self) -> bool {
        self.ack_seen.load(Ordering::Relaxed)
    }

    fn inflight_bytes(&self) -> u64 {
        self.sent_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(self.ack_bytes.load(Ordering::Relaxed))
    }

    fn note_ack(&self, seq: u32, recv_bytes: u64) {
        self.ack_seq.store(seq, Ordering::Relaxed);
        self.ack_bytes.store(recv_bytes, Ordering::Relaxed);
        self.ack_seen.store(true, Ordering::Relaxed);
    }

    fn set_udp_stats(&self, st: lrc_udp::UdpStats) {
        let rtt = if st.rtt_ms.is_finite() && st.rtt_ms > 0.0 {
            (st.rtt_ms * 10.0).min(60_000.0) as u32
        } else {
            0
        };
        self.udp_rtt_x10.store(rtt, Ordering::Relaxed);
        self.udp_loss_permille
            .store(u32::from(st.loss_permille), Ordering::Relaxed);
    }

    fn udp_rtt_ms(&self) -> u32 {
        self.udp_rtt_x10.load(Ordering::Relaxed) / 10
    }

    fn udp_loss_permille(&self) -> u16 {
        u16::try_from(self.udp_loss_permille.load(Ordering::Relaxed)).unwrap_or(u16::MAX)
    }
}

// ─────────────────────────── 会话 ───────────────────────────

fn run(link: AcquiredLink, args: &Args) -> Result<(), String> {
    let AcquiredLink {
        writer,
        reader,
        path,
    } = link;
    let (mut writer, mut reader) = (writer, reader);
    log::info!("会话建立：path={path}");
    if let Err(e) = writer.set_write_timeout(Some(HANDSHAKE_TIMEOUT)) {
        log::warn!("设置握手写超时失败：{e}");
    }

    // 1) ClientHello
    let hello = match recv_client_hello(&mut reader) {
        Ok(h) => h,
        Err(e) => {
            log::warn!("握手失败：{e}");
            writer.shutdown();
            return Ok(());
        }
    };
    if hello.version != PROTO_VERSION {
        log::warn!(
            "拒绝：客户端协议 {}，本机只支持 {PROTO_VERSION}",
            hello.version
        );
        let msg = Message::AuthResult(AuthResult {
            ok: false,
            reason: auth_reason::VERSION_MISMATCH,
        })
        .encode();
        let _ = writer.send(&msg);
        writer.shutdown();
        return Ok(());
    }

    // 2) 显示尺寸（ServerHello 必须来自真实屏幕）
    let info = match lrc_linux::display::query() {
        Ok(i) => i,
        Err(e) => {
            log::error!(
                "查询 X11 屏幕失败：{e}（DISPLAY={:?}）",
                std::env::var("DISPLAY").ok()
            );
            writer.shutdown();
            return Ok(());
        }
    };
    log::info!("采集屏幕：{info}");

    let nonce_s = make_nonce(args.room as u64);
    let srv = Message::ServerHello(ServerHello {
        version: PROTO_VERSION,
        width: info.width,
        height: info.height,
        rotation: 0,
        nonce_s,
    });
    if let Err(e) = writer.send(&srv.encode()) {
        log::warn!("发送 ServerHello 失败：{e}");
        writer.shutdown();
        return Ok(());
    }

    // 3) 鉴权
    if let Err(e) = await_auth(&mut reader, &mut writer, &args.password, &hello.nonce_c, &nonce_s) {
        log::warn!("鉴权失败：{e}");
        writer.shutdown();
        return Ok(());
    }
    log::info!(
        "客户端已鉴权，开始推流（块模式，--fps {}，q{}）",
        args.fps,
        args.quality
    );

    if let Err(e) = writer.set_write_timeout(Some(SEND_TIMEOUT)) {
        log::warn!("设置写超时失败：{e}");
    }

    let shared = Arc::new(Shared::new(writer));
    let reader_shared = Arc::clone(&shared);
    let reader_handle = match std::thread::Builder::new()
        .name("lrc-host-linux-reader".to_string())
        .spawn(move || reader_loop(reader, reader_shared))
    {
        Ok(h) => h,
        Err(e) => {
            shared.shutdown();
            return Err(format!("启动读线程失败：{e}"));
        }
    };

    let result = capture_loop(args, info, Arc::clone(&shared), path);

    shared.stop_now();
    shared.shutdown();
    let _ = reader_handle.join();
    if shared.peer_gone.load(Ordering::Relaxed) {
        log::info!("会话结束：客户端已断开");
    }
    result
}

/// 握手阶段带超时的读。
fn recv_msg_timeout(
    reader: &mut LinkReader,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<usize, String> {
    match reader.recv_timeout(buf, timeout) {
        Ok(Some(n)) => Ok(n),
        Ok(None) => Err("握手超时/对端未发消息".to_string()),
        Err(e) => Err(format!("读失败：{e}")),
    }
}

fn recv_client_hello(reader: &mut LinkReader) -> Result<ClientHello, String> {
    let mut buf = Vec::with_capacity(256);
    recv_msg_timeout(reader, &mut buf, HANDSHAKE_TIMEOUT)?;
    match Message::decode(&buf) {
        Ok(Message::ClientHello(h)) => Ok(h),
        Ok(other) => Err(format!("握手顺序错误：先收到 {other:?}")),
        Err(e) => Err(format!("ClientHello 解析失败：{e}")),
    }
}

fn await_auth(
    reader: &mut LinkReader,
    writer: &mut LinkWriter,
    password: &str,
    nonce_c: &[u8; NONCE_LEN],
    nonce_s: &[u8; NONCE_LEN],
) -> Result<(), String> {
    let mut buf = Vec::with_capacity(128);
    recv_msg_timeout(reader, &mut buf, HANDSHAKE_TIMEOUT)?;
    let req = match Message::decode(&buf) {
        Ok(Message::AuthRequest(r)) => r,
        Ok(other) => {
            let _ = writer.send(
                &Message::AuthResult(AuthResult {
                    ok: false,
                    reason: auth_reason::BAD_PASSWORD,
                })
                .encode(),
            );
            return Err(format!("握手顺序错误：收到 {other:?}"));
        }
        Err(e) => return Err(format!("AuthRequest 解析失败：{e}")),
    };
    let expect = compute_mac(password, nonce_c, nonce_s);
    let ok = ct_eq(&req.mac, &expect);
    let verdict = Message::AuthResult(AuthResult {
        ok,
        reason: if ok {
            auth_reason::OK
        } else {
            auth_reason::BAD_PASSWORD
        },
    });
    writer
        .send(&verdict.encode())
        .map_err(|e| format!("发送 AuthResult 失败：{e}"))?;
    if ok {
        Ok(())
    } else {
        Err("密码错误".to_string())
    }
}

fn reader_loop(mut reader: LinkReader, shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    loop {
        if shared.stop() {
            break;
        }
        match reader.recv_timeout(&mut buf, READ_POLL) {
            Ok(Some(n)) => {
                if let Some(st) = reader.udp_stats() {
                    shared.set_udp_stats(st);
                }
                handle_incoming(&buf[..n], &shared);
            }
            Ok(None) => {
                if let Some(st) = reader.udp_stats() {
                    shared.set_udp_stats(st);
                }
                continue;
            }
            Err(e) => {
                if !shared.stop() {
                    log::info!("客户端连接结束（{e}）");
                }
                break;
            }
        }
    }
    shared.peer_gone.store(true, Ordering::Relaxed);
    shared.stop_now();
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
        Message::Input(ev) => apply_input(ev),
        Message::VideoAck(ack) => {
            shared.note_ack(ack.seq, ack.recv_bytes);
            log::trace!("ack seq={} recv_bytes={}", ack.seq, ack.recv_bytes);
        }
        Message::RequestFull => {
            shared.request_full.store(true, Ordering::Relaxed);
            log::info!("客户端请求整帧");
        }
        Message::Ping(p) => {
            let pong = Message::Pong(Pong { ts_ms: p.ts_ms }).encode();
            if let Err(e) = shared.send(&pong) {
                log::warn!("回 Pong 失败：{e}");
            }
        }
        Message::SelectDisplay(sel) => {
            log::info!(
                "忽略 SelectDisplay({})（Linux 版不做显示器切换）",
                sel.index
            );
        }
        other => log::debug!("忽略消息 {other:?}"),
    }
}

// ─────────────────────────── 输入回放 ───────────────────────────

/// 鼠标按键：协议编号 → X11 按钮号。
fn x11_button(button: u8) -> u8 {
    match button {
        lrc_proto::input::mouse_button::LEFT => 1,
        lrc_proto::input::mouse_button::MIDDLE => 2,
        lrc_proto::input::mouse_button::RIGHT => 3,
        // X1/X2：X11 里是 8/9
        lrc_proto::input::mouse_button::X1 => 8,
        lrc_proto::input::mouse_button::X2 => 9,
        other => other.saturating_add(1),
    }
}

fn apply_input(ev: InputEvent) {
    log::debug!("输入注入 {ev:?}");
    match ev {
        InputEvent::MouseMove { x, y } => lrc_linux::send_mouse_move(x, y),
        InputEvent::MouseButton {
            button,
            down,
            x,
            y,
        } => {
            lrc_linux::send_mouse_move(x, y);
            lrc_linux::send_mouse_button(x11_button(button), down);
        }
        InputEvent::Wheel { dx, dy, x, y } => {
            lrc_linux::send_mouse_move(x, y);
            if dy != 0 {
                lrc_linux::send_wheel(wheel_dy_to_x11(dy));
            }
            if dx != 0 {
                log::debug!("忽略水平滚轮 dx={dx}（X11 侧只注入垂直滚轮）");
            }
        }
        InputEvent::Key {
            scancode,
            down,
            extended,
        } => match scancode_to_keysym(scancode, extended) {
            Some(keysym) => lrc_linux::send_key(keysym, down),
            None => log::debug!("未映射的 scancode=0x{scancode:02x} extended={extended}"),
        },
        InputEvent::KeyUnicode { codepoint, down } => {
            lrc_linux::send_key(unicode_keysym(codepoint), down)
        }
    }
}

/// 协议里的滚轮 dy **已经是"格"**（客户端把 WHEEL_DELTA=120 除过，正数=向上）；
/// lrc-linux 的 send_wheel 约定"正数向下、负数向上"，所以取反。
fn wheel_dy_to_x11(dy: i16) -> i32 {
    -i32::from(dy)
}

/// Unicode 码点 → X11 keysym（ASCII 直接用码点，其余用 0x01000000 | cp）。
fn unicode_keysym(codepoint: u32) -> u32 {
    if (0x20..=0x7e).contains(&codepoint) {
        codepoint
    } else {
        0x0100_0000 | (codepoint & 0x001f_ffff)
    }
}

/// Windows scancode（set 1）→ X11 keysym。扩展键用 extended 区分。
fn scancode_to_keysym(scancode: u16, extended: bool) -> Option<u32> {
    const XK_BACKSPACE: u32 = 0xff08;
    const XK_TAB: u32 = 0xff09;
    const XK_RETURN: u32 = 0xff0d;
    const XK_ESCAPE: u32 = 0xff1b;
    const XK_HOME: u32 = 0xff50;
    const XK_LEFT: u32 = 0xff51;
    const XK_UP: u32 = 0xff52;
    const XK_RIGHT: u32 = 0xff53;
    const XK_DOWN: u32 = 0xff54;
    const XK_PAGE_UP: u32 = 0xff55;
    const XK_PAGE_DOWN: u32 = 0xff56;
    const XK_END: u32 = 0xff57;
    const XK_INSERT: u32 = 0xff63;
    const XK_DELETE: u32 = 0xffff;
    const XK_SHIFT_L: u32 = 0xffe1;
    const XK_SHIFT_R: u32 = 0xffe2;
    const XK_CONTROL_L: u32 = 0xffe3;
    const XK_CONTROL_R: u32 = 0xffe4;
    const XK_CAPS_LOCK: u32 = 0xffe5;
    const XK_META_L: u32 = 0xffe7;
    const XK_ALT_L: u32 = 0xffe9;
    const XK_ALT_R: u32 = 0xffea;
    const XK_SUPER_L: u32 = 0xffeb;
    const XK_SUPER_R: u32 = 0xffec;
    const XK_MENU: u32 = 0xff67;
    const XK_PRINT: u32 = 0xff61;
    const XK_NUM_LOCK: u32 = 0xff7f;
    const XK_SCROLL_LOCK: u32 = 0xff14;
    const XK_KP_ENTER: u32 = 0xff8d;
    const XK_KP_ADD: u32 = 0xffab;
    const XK_KP_SUBTRACT: u32 = 0xffad;
    const XK_KP_DIVIDE: u32 = 0xffaf;
    const XK_KP_DECIMAL: u32 = 0xffae;
    const XK_KP_0: u32 = 0xffb0;
    /// 反引号/波浪号键（0x60）
    const ASCII_GRAVE: u32 = 0x60;

    if extended {
        return Some(match scancode {
            0x1c => XK_KP_ENTER,
            0x1d => XK_CONTROL_R,
            0x35 => XK_KP_DIVIDE,
            0x37 => XK_PRINT,
            0x38 => XK_ALT_R,
            0x47 => XK_HOME,
            0x48 => XK_UP,
            0x49 => XK_PAGE_UP,
            0x4b => XK_LEFT,
            0x4d => XK_RIGHT,
            0x4f => XK_END,
            0x50 => XK_DOWN,
            0x51 => XK_PAGE_DOWN,
            0x52 => XK_INSERT,
            0x53 => XK_DELETE,
            0x5b => XK_SUPER_L,
            0x5c => XK_SUPER_R,
            0x5d => XK_MENU,
            _ => return None,
        });
    }

    Some(match scancode {
        0x01 => XK_ESCAPE,
        0x0e => XK_BACKSPACE,
        0x0f => XK_TAB,
        0x1c => XK_RETURN,
        0x1d => XK_CONTROL_L,
        0x2a => XK_SHIFT_L,
        0x36 => XK_SHIFT_R,
        0x37 => XK_PRINT,
        0x38 => XK_ALT_L,
        0x39 => 0x20,
        0x3a => XK_CAPS_LOCK,
        0x3b..=0x44 => 0xffbe + u32::from(scancode - 0x3b),
        0x45 => XK_NUM_LOCK,
        0x46 => XK_SCROLL_LOCK,
        0x47 => XK_KP_0 + 7,
        0x48 => XK_KP_0 + 8,
        0x49 => XK_KP_0 + 9,
        0x4a => XK_KP_SUBTRACT,
        0x4b => XK_KP_0 + 4,
        0x4c => XK_KP_0 + 5,
        0x4d => XK_KP_0 + 6,
        0x4e => XK_KP_ADD,
        0x4f => XK_KP_0 + 1,
        0x50 => XK_KP_0 + 2,
        0x51 => XK_KP_0 + 3,
        0x52 => XK_KP_0,
        0x53 => XK_KP_DECIMAL,
        0x57 => 0xffc8,
        0x58 => 0xffc9,
        0x5b => XK_META_L,
        0x02..=0x0b => u32::from(b"1234567890"[usize::from(scancode - 0x02)]),
        0x0c => u32::from(b'-'),
        0x0d => u32::from(b'='),
        0x10 => u32::from(b'q'),
        0x11 => u32::from(b'w'),
        0x12 => u32::from(b'e'),
        0x13 => u32::from(b'r'),
        0x14 => u32::from(b't'),
        0x15 => u32::from(b'y'),
        0x16 => u32::from(b'u'),
        0x17 => u32::from(b'i'),
        0x18 => u32::from(b'o'),
        0x19 => u32::from(b'p'),
        0x1a => u32::from(b'['),
        0x1b => u32::from(b']'),
        0x1e => u32::from(b'a'),
        0x1f => u32::from(b's'),
        0x20 => u32::from(b'd'),
        0x21 => u32::from(b'f'),
        0x22 => u32::from(b'g'),
        0x23 => u32::from(b'h'),
        0x24 => u32::from(b'j'),
        0x25 => u32::from(b'k'),
        0x26 => u32::from(b'l'),
        0x27 => u32::from(b';'),
        0x28 => u32::from(b'\''),
        0x29 => ASCII_GRAVE,
        0x2b => u32::from(b'\\'),
        0x2c => u32::from(b'z'),
        0x2d => u32::from(b'x'),
        0x2e => u32::from(b'c'),
        0x2f => u32::from(b'v'),
        0x30 => u32::from(b'b'),
        0x31 => u32::from(b'n'),
        0x32 => u32::from(b'm'),
        0x33 => u32::from(b','),
        0x34 => u32::from(b'.'),
        0x35 => u32::from(b'/'),
        _ => return None,
    })
}

// ─────────────────────────── 光标位置 ───────────────────────────

/// CursorShape 消息的边长上限：lrc-proto 的 MAX_CURSOR_BYTES = 256*256*4。
const MAX_CURSOR_DIM: u32 = 256;

/// 组 CursorShape 消息。proto 字段名叫 rgba，但内容就是采集给的 BGRA，**不转换**（与 Windows 版一致）。
fn encode_cursor_shape(shape: &lrc_linux::CursorShape) -> Option<Vec<u8>> {
    if shape.width == 0 || shape.height == 0 {
        return None;
    }
    if shape.width > MAX_CURSOR_DIM || shape.height > MAX_CURSOR_DIM {
        log::debug!(
            "光标位图 {}x{} 超过协议上限 {}，本次不上报形状",
            shape.width,
            shape.height,
            MAX_CURSOR_DIM
        );
        return None;
    }
    let need = 4usize
        .checked_mul(shape.width as usize)?
        .checked_mul(shape.height as usize)?;
    if shape.bgra.len() < need {
        return None;
    }
    let msg = Message::CursorShape(lrc_proto::CursorShape {
        width: u16::try_from(shape.width).ok()?,
        height: u16::try_from(shape.height).ok()?,
        hotspot_x: clamp_i16(shape.hotspot_x),
        hotspot_y: clamp_i16(shape.hotspot_y),
        rgba: &shape.bgra[..need],
    });
    Some(msg.encode())
}

// ─────────────────────────── 采集/编码/发送 ───────────────────────────

#[derive(Default, Clone, Copy)]
struct Stats {
    frames: u64,
    blocks: u64,
    bytes: u64,
    idle: u64,
    dropped: u64,
    full_resends: u64,
    cursor: u64,
}

#[derive(Default)]
struct BlockSlot {
    idx: u32,
    codec: u8,
    hash: u64,
    data: Vec<u8>,
}

/// 光标坐标是 i16（协议限制）：超出范围就夹到边界。
fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// 块左上角在整帧 BGRA 缓冲里的字节偏移。
fn pixel_offset(rect: &Rect, stride: usize) -> Option<usize> {
    if rect.x < 0 || rect.y < 0 {
        return None;
    }
    let x = usize::try_from(rect.x).ok()?;
    let y = usize::try_from(rect.y).ok()?;
    x.checked_mul(4)?.checked_add(y.checked_mul(stride)?)
}

/// 块内容哈希。算法只在本机用（客户端不需要），要求快且碰撞足够少。
fn hash_block(src: &[u8], stride: usize, w: u32, h: u32) -> u64 {
    const SEED: u64 = 0x517c_c1b7_2722_0a95;
    const MUL: u64 = 0x9E37_79B9_7F4A_7C15;
    let row_bytes = match usize::try_from(w).ok().and_then(|v| v.checked_mul(4)) {
        Some(v) => v,
        None => return 0,
    };
    let mut hash = SEED;
    let height = match usize::try_from(h) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    for y in 0..height {
        let start = y.saturating_mul(stride);
        let end = start.saturating_add(row_bytes);
        if end > src.len() {
            break;
        }
        let row = &src[start..end];
        let mut chunks = row.chunks_exact(8);
        for c in &mut chunks {
            let mut word = [0u8; 8];
            word.copy_from_slice(c);
            hash = (hash ^ u64::from_le_bytes(word))
                .wrapping_mul(MUL)
                .rotate_left(29);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(MUL);
        }
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^ (hash >> 33)
}

/// 把已编码块按"单条消息 payload ≤ budget"切成若干组（返回 blocks 的下标区间）。
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

/// 只有第一条分片消息带 FULL|KEY（否则客户端会反复整屏重建）。
fn chunk_flags(index: usize, send_full: bool) -> u8 {
    if index == 0 && send_full {
        frame_flags::FULL | frame_flags::KEY
    } else {
        0
    }
}

fn capture_loop(
    args: &Args,
    info: lrc_linux::DisplayInfo,
    shared: Arc<Shared>,
    path: &'static str,
) -> Result<(), String> {
    let mut capturer =
        lrc_linux::X11Capturer::new(info).map_err(|e| format!("创建 X11 采集器失败：{e}"))?;
    let mut grid = Grid::new(BLOCK_SIZE, info.width, info.height)
        .ok_or_else(|| format!("网格非法 {}x{}", info.width, info.height))?;
    log::info!("采集路径：{}", capturer.path_name());
    let mut hashes: Vec<u64> = vec![0; grid.count() as usize];
    let mut have_prev = false;
    let mut force_full = true;

    let opts = lrc_codec::Options {
        jpeg_quality: args.quality,
        ..lrc_codec::Options::default()
    };
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    let mut next_grab_at = Instant::now();
    let session_start = Instant::now();

    let mut seq: u32 = 0;
    let mut slots: Vec<BlockSlot> = Vec::new();
    let mut changed: Vec<(u32, u64)> = Vec::new();
    let mut frame_chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = Stats::default();
    let mut sec = Stats::default();
    let mut last_report = Instant::now();
    let mut capture_failures: u32 = 0;
    let mut cur_size = (info.width, info.height);
    // 光标：X11 的 GetImage 不含光标，必须走 XFixes 单独抓（位置 + 位图）
    let mut cursor_watcher = match lrc_linux::CursorWatcher::new(None) {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!("光标采集不可用（客户端将看不到鼠标）：{e}");
            None
        }
    };
    let mut last_cursor_poll = Instant::now() - Duration::from_millis(100);
    let cursor_poll_interval = Duration::from_millis(16);

    log::info!(
        "推流 {}x{} = {}x{} 块（{}px），上限 {}fps，JPEG q{}",
        info.width,
        info.height,
        grid.blocks_x,
        grid.blocks_y,
        BLOCK_SIZE,
        args.fps,
        args.quality
    );

    loop {
        if shared.stop() {
            break;
        }

        // 统计行（每秒一行）
        if args.stats {
            let elapsed = last_report.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let d = elapsed.as_secs_f64().max(0.001);
                println!(
                    "t={:.1} fps={:.1} blocks={} sent_frames={} bytes={} kbps={:.1} idle_frames={} dropped={} sec_bytes={} cursor_msgs={} full_resends={} mode=blocks video_kbps=0.0 video_fps=0.0 level=0 stage=原生 scale=1.00 path={} udp_rtt={} udp_loss={}",
                    session_start.elapsed().as_secs_f64(),
                    sec.frames as f64 / d,
                    sec.blocks,
                    total.frames,
                    total.bytes,
                    (sec.bytes as f64 * 8.0 / 1000.0) / d,
                    sec.idle,
                    sec.dropped,
                    sec.bytes,
                    sec.cursor,
                    sec.full_resends,
                    path,
                    shared.udp_rtt_ms(),
                    shared.udp_loss_permille()
                );
                let _ = std::io::stdout().flush();
                total.frames = total.frames.saturating_add(sec.frames);
                total.blocks = total.blocks.saturating_add(sec.blocks);
                total.bytes = total.bytes.saturating_add(sec.bytes);
                total.idle = total.idle.saturating_add(sec.idle);
                total.dropped = total.dropped.saturating_add(sec.dropped);
                total.full_resends = total.full_resends.saturating_add(sec.full_resends);
                total.cursor = total.cursor.saturating_add(sec.cursor);
                sec = Stats::default();
                last_report = Instant::now();
            }
        }

        // 光标：位置变了发 CursorPos，形状变了再补一条 CursorShape（客户端缓存位图）
        if last_cursor_poll.elapsed() >= cursor_poll_interval {
            last_cursor_poll = Instant::now();
            if let Some(cw) = cursor_watcher.as_mut() {
                if let Some(st) = cw.poll() {
                    if let Some(shape) = st.shape.as_ref() {
                        if let Some(payload) = encode_cursor_shape(shape) {
                            if let Err(e) = shared.send(&payload) {
                                log::warn!("发送光标形状失败：{e}");
                                break;
                            }
                            sec.cursor += 1;
                        }
                    }
                    let pos = Message::CursorPos(lrc_proto::CursorPos {
                        x: clamp_i16(st.x),
                        y: clamp_i16(st.y),
                        visible: st.visible,
                    })
                    .encode();
                    if let Err(e) = shared.send(&pos) {
                        log::warn!("发送光标位置失败：{e}");
                        break;
                    }
                    sec.cursor += 1;
                }
            }
        }

        // 帧率节流
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

        // 抓整帧（X11 没有脏区通知）
        let frame = match capturer.capture() {
            Ok(f) => {
                capture_failures = 0;
                f
            }
            Err(e) => {
                capture_failures = capture_failures.saturating_add(1);
                log::warn!("采集失败（第 {capture_failures} 次）：{e}");
                if capture_failures > MAX_CAPTURE_FAILURES {
                    return Err(format!("采集连续失败 {capture_failures} 次，放弃会话"));
                }
                std::thread::sleep(Duration::from_millis(50));
                next_grab_at = Instant::now();
                continue;
            }
        };
        let grab_done = Instant::now();

        // 尺寸变化（分辨率/旋转）：重建网格
        if (frame.width, frame.height) != cur_size {
            match Grid::new(BLOCK_SIZE, frame.width, frame.height) {
                Some(g) => {
                    log::info!(
                        "屏幕尺寸变化 {}x{}，重建块网格 {}x{}",
                        frame.width,
                        frame.height,
                        g.blocks_x,
                        g.blocks_y
                    );
                    hashes = vec![0; g.count() as usize];
                    grid = g;
                    cur_size = (frame.width, frame.height);
                    have_prev = false;
                    force_full = true;
                }
                None => {
                    log::warn!("帧尺寸非法 {}x{}，跳过", frame.width, frame.height);
                    next_grab_at = grab_done + frame_interval;
                    continue;
                }
            }
        }

        // 流控：未确认字节太多就丢这一帧
        if shared.ack_fresh() && shared.inflight_bytes() > MAX_INFLIGHT_BYTES {
            sec.dropped += 1;
            force_full = true;
            next_grab_at = Instant::now() + frame_interval;
            log::debug!("流控丢帧：未确认字节 {}", shared.inflight_bytes());
            continue;
        }

        let request_full = shared.request_full.swap(false, Ordering::Relaxed);
        let send_full = force_full || !have_prev || request_full;

        // 全网格哈希 + 差分（Linux 只能全扫）
        changed.clear();
        for idx in 0..grid.count() {
            let Some(rect) = grid.block_rect(idx, frame.width, frame.height) else {
                continue;
            };
            let Some(offset) = pixel_offset(&rect, frame.stride) else {
                continue;
            };
            if offset >= frame.bgra.len() {
                continue;
            }
            let h = hash_block(&frame.bgra[offset..], frame.stride, rect.w, rect.h);
            let known = hashes.get(idx as usize).copied().unwrap_or(0);
            if send_full || !have_prev || known != h {
                changed.push((idx, h));
            }
        }

        if changed.is_empty() {
            // 零变化零发送
            sec.idle += 1;
            next_grab_at = grab_done + frame_interval;
            continue;
        }

        // 逐块编码（文字无损优先，图像走 JPEG，取小者）
        let mut used = 0usize;
        for &(idx, hash) in &changed {
            let Some(rect) = grid.block_rect(idx, frame.width, frame.height) else {
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
            next_grab_at = grab_done + frame_interval;
            continue;
        }

        // 大帧分片：UDP 有序交付下，一条大消息丢一个分片就要等整条补齐
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
                blocks_x: grid.blocks_x,
                blocks_y: grid.blocks_y,
                entries,
            });
            let payload = msg.encode();
            if payload.len() > lrc_proto::MAX_PAYLOAD {
                log::warn!(
                    "分片过大 {} > {}，本帧丢弃",
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
        if frame_ok {
            if let Err(e) = shared.send_batch(&frame_chunks) {
                log::warn!("发送帧失败：{e}");
                break;
            }
        }
        if !frame_ok {
            force_full = true;
            next_grab_at = grab_done + frame_interval;
            continue;
        }

        // 只有真正发出去，才把"客户端已知状态"推进到当前
        for slot in &slots[..used] {
            if let Some(h) = hashes.get_mut(slot.idx as usize) {
                *h = slot.hash;
            }
        }
        have_prev = true;
        force_full = false;
        seq = seq.wrapping_add(1);
        sec.frames += 1;
        sec.blocks += used as u64;
        sec.bytes += payload_total as u64 + 4 * chunks_sent as u64;
        if request_full {
            sec.full_resends += 1;
        }
        if chunks_sent > 1 {
            log::debug!("大帧拆成 {chunks_sent} 条发（{used} 块 / {payload_total} 字节）");
        }
        next_grab_at = grab_done + frame_interval;
    }

    let session_secs = session_start.elapsed().as_secs_f64().max(0.001);
    log::info!(
        "会话结束，总计 t={:.1} fps={:.1} sent_frames={} blocks={} bytes={} kbps={:.1} idle_frames={} dropped={} full_resends={} path={}",
        session_secs,
        total.frames as f64 / session_secs,
        total.frames,
        total.blocks,
        total.bytes,
        (total.bytes as f64 * 8.0 / 1000.0) / session_secs,
        total.idle,
        total.dropped,
        total.full_resends,
        path
    );
    Ok(())
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn cli_defaults() {
        let a = parse_args(argv(&["--password", "1234"])).expect("parse");
        assert_eq!(a.password, "1234");
        assert_eq!(a.listen, "0.0.0.0:7800");
        assert_eq!(a.display, None);
        assert_eq!(a.fps, 30);
        assert_eq!(a.quality, 85);
        assert!(!a.stats);
        assert_eq!(a.signal, None);
        assert_eq!(a.room, 1);
        assert_eq!(a.p2p, P2pMode::Auto);
    }

    #[test]
    fn cli_all_flags() {
        let a = parse_args(argv(&[
            "--password=secret",
            "--listen",
            "127.0.0.1:9000",
            "--display",
            ":1",
            "--fps",
            "24",
            "--quality",
            "70",
            "--stats",
            "--signal",
            "1.2.3.4:7801",
            "--room",
            "9",
            "--p2p",
            "relay",
        ]))
        .expect("parse");
        assert_eq!(a.password, "secret");
        assert_eq!(a.listen, "127.0.0.1:9000");
        assert_eq!(a.display.as_deref(), Some(":1"));
        assert_eq!(a.fps, 24);
        assert_eq!(a.quality, 70);
        assert!(a.stats);
        assert_eq!(a.signal, Some("1.2.3.4:7801".parse().expect("addr")));
        assert_eq!(a.room, 9);
        assert_eq!(a.p2p, P2pMode::Relay);
    }

    #[test]
    fn cli_rejects_bad_values() {
        assert!(parse_args(argv(&[])).is_err(), "缺 --password");
        assert!(parse_args(argv(&["--password", ""])).is_err(), "空密码");
        assert!(parse_args(argv(&["--password", "1", "--fps", "0"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--fps", "999"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--quality", "0"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--p2p", "hole"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--signal", "nope"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--bogus"])).is_err());
    }

    #[test]
    fn display_normalization() {
        assert_eq!(normalize_display(":1"), ":1");
        assert_eq!(normalize_display("1"), ":1");
        assert_eq!(normalize_display(":1.0"), ":1.0");
        assert_eq!(normalize_display("host:1"), "host:1");
    }

    #[test]
    fn p2p_path_selection() {
        let sig = Some("127.0.0.1:7801".parse().expect("addr"));
        assert!(!should_try_p2p(P2pMode::Off, sig));
        assert!(!should_try_p2p(P2pMode::Auto, None));
        assert!(need_tcp_listener(P2pMode::Off, sig));
        assert!(should_try_p2p(P2pMode::Auto, sig));
        assert_eq!(fallback_on_failure(P2pMode::Auto), Fallback::Tcp);
        assert!(need_tcp_listener(P2pMode::Auto, sig));
        assert!(can_retry(P2pMode::Auto));
        assert_eq!(fallback_on_failure(P2pMode::Direct), Fallback::Fatal);
        assert!(!need_tcp_listener(P2pMode::Direct, sig));
        assert!(!can_retry(P2pMode::Direct));
        assert_eq!(punch_timeout(P2pMode::Relay), Duration::ZERO);
        assert_eq!(punch_timeout(P2pMode::Auto), PUNCH_TIMEOUT);
        assert!(!need_tcp_listener(P2pMode::Relay, sig));
    }

    #[test]
    fn block_geometry_1080p() {
        let g = Grid::new(BLOCK_SIZE, 1920, 1080).expect("grid");
        assert_eq!((g.blocks_x, g.blocks_y), (30, 17));
        let r0 = g.block_rect(0, 1920, 1080).expect("rect");
        assert_eq!(pixel_offset(&r0, 1920 * 4), Some(0));
        let r1 = g.block_rect(1, 1920, 1080).expect("rect");
        assert_eq!(pixel_offset(&r1, 1920 * 4), Some(64 * 4));
    }

    #[test]
    fn hash_detects_change() {
        let stride = 64 * 4;
        let mut block = vec![0u8; stride * 64];
        let h1 = hash_block(&block, stride, 64, 64);
        assert_eq!(h1, hash_block(&block, stride, 64, 64));
        block[100] = 1;
        let h2 = hash_block(&block, stride, 64, 64);
        assert_ne!(h1, h2, "改一个字节必须改变哈希");
        let big_stride = 64 * 4 + 16;
        let big = vec![0u8; big_stride * 64];
        let _ = hash_block(&big, big_stride, 64, 64);
    }

    #[test]
    fn chunking_covers_all_blocks() {
        let lens = vec![8 * 1024usize; 500];
        let ranges = split_block_ranges(&lens, VIDEO_FRAME_CHUNK_BYTES);
        assert!(ranges.len() > 50, "4MB 应拆成多条");
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[ranges.len() - 1].end, lens.len());
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        for r in &ranges {
            let bytes = VIDEO_FRAME_HEAD_BYTES
                + lens[r.clone()]
                    .iter()
                    .map(|l| BLOCK_ENTRY_BYTES + l)
                    .sum::<usize>();
            assert!(bytes <= VIDEO_FRAME_CHUNK_BYTES);
        }
        assert_eq!(
            split_block_ranges(&[100usize, 200], VIDEO_FRAME_CHUNK_BYTES).len(),
            1
        );
        assert_eq!(chunk_flags(0, true), frame_flags::FULL | frame_flags::KEY);
        assert_eq!(chunk_flags(1, true), 0);
        assert_eq!(chunk_flags(0, false), 0);
    }

    #[test]
    fn scancode_mapping() {
        assert_eq!(scancode_to_keysym(0x1e, false), Some(0x61));
        assert_eq!(scancode_to_keysym(0x02, false), Some(0x31));
        assert_eq!(scancode_to_keysym(0x1c, false), Some(0xff0d));
        assert_eq!(scancode_to_keysym(0x1c, true), Some(0xff8d));
        assert_eq!(scancode_to_keysym(0x2a, false), Some(0xffe1));
        assert_eq!(scancode_to_keysym(0x3b, false), Some(0xffbe));
        assert_eq!(scancode_to_keysym(0x44, false), Some(0xffc7));
        assert_eq!(scancode_to_keysym(0x47, true), Some(0xff50));
        assert_eq!(scancode_to_keysym(0x48, false), Some(0xffb8));
        assert_eq!(scancode_to_keysym(0x29, false), Some(0x60));
        assert_eq!(scancode_to_keysym(0x0200, false), None);
        assert_eq!(x11_button(lrc_proto::input::mouse_button::LEFT), 1);
        assert_eq!(x11_button(lrc_proto::input::mouse_button::RIGHT), 3);
        assert_eq!(x11_button(lrc_proto::input::mouse_button::MIDDLE), 2);
        // 滚轮：dy 已经是格数，正=向上 → X11 负数=向上
        assert_eq!(wheel_dy_to_x11(1), -1);
        assert_eq!(wheel_dy_to_x11(-1), 1);
        assert_eq!(wheel_dy_to_x11(3), -3);
        assert_eq!(unicode_keysym(0x41), 0x41);
        assert_eq!(unicode_keysym(0x4e2d), 0x0100_4e2d);
    }

    #[test]
    fn auth_mac_is_stable() {
        let nc = [7u8; NONCE_LEN];
        let ns = [9u8; NONCE_LEN];
        let m1 = compute_mac("1234", &nc, &ns);
        let m2 = compute_mac("1234", &nc, &ns);
        assert_eq!(m1, m2);
        assert_ne!(m1, compute_mac("12345", &nc, &ns));
        assert_ne!(m1, compute_mac("1234", &ns, &nc));
        assert!(ct_eq(&m1, &m2));
        assert!(!ct_eq(&m1, &[0u8; 32]));
        assert!(!ct_eq(&m1, &[0u8; 31]));
    }

    #[test]
    fn nonce_is_not_constant() {
        let a = make_nonce(1);
        let b = make_nonce(2);
        assert_ne!(a, b);
        assert_eq!(a.len(), NONCE_LEN);
    }
}
