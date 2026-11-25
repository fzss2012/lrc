//! Linux 被控端：X11 采集 → 64×64 块哈希差分 → 编码 → 传输；输入回放。
//!
//! 用法（与 Windows 版一致）：
//!   lrc-host-linux --password 1234 [--display :1] [--listen 0.0.0.0:7800]
//!                  [--signal <vps>:7801 --room 1 --p2p auto|relay|direct|off]
//!                  [--monitor DP-1 | --region 1920x1080+0+0]
//!
//! 与 Windows 版 crates/lrc-host 的差别（平台相关部分）：
//! - 采集：lrc_linux::X11Capturer 每次抓整屏，没有脏区通知，靠块哈希自己算增量
//! - 输入：lrc_linux::input 的 XTEST 封装，键位需要把 Windows scancode 翻成 X11 keysym
//! - 多显示器：Windows 版按 DXGI 输出逐屏推；Linux 版是**单个 X root**，
//!   双屏会被合成一张（例如 3840x1080）。只想推一台屏时用 --monitor / --region
//!   裁出那块矩形，ServerHello 报的是这块的尺寸，输入坐标也按这块换算。
//! - 不做：视频通道、音频、光标形状、显示器切换（SelectDisplay 忽略）、自适应控制器

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use lrc_proto::{
    auth_reason, frame_flags, AuthResult, BlockEntry, ClientHello, Grid, InputEvent,
    Message, Pong, Rect, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::link::{self, LinkReader, LinkWriter};
use lrc_video::{FfmpegEncoder, VideoEncoder};
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
    /// 只推 root 上的一块区域（例如单台 1920x1080 显示器）
    region: Option<lrc_linux::Region>,
    /// 按 RandR 输出名选一台显示器（例如 DP-1）；与 --region 二选一
    monitor: Option<String>,
    /// 视频通道：off 只用块模式；auto 优先 hevc_nvenc；h264 指定 H.264
    video: VidChoice,
    /// 视频目标码率（bit/s）
    video_bitrate: u32,
    /// 视频模式帧率上限
    video_max_fps: u32,
    fps: u32,
    quality: u8,
    stats: bool,
    signal: Option<SocketAddr>,
    room: u32,
    p2p: P2pMode,
}

fn usage() -> &'static str {
    "用法: lrc-host-linux --password <PWD> [--listen IP:PORT] [--display :1] [--fps N] [--quality N]\n                         [--stats] [--signal IP:PORT] [--room N] [--p2p auto|off|direct|relay]\n                         [--monitor <输出名> | --region WxH+X+Y]\n                         [--video off|auto|h264] [--video-bitrate N] [--video-max-fps N]"
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
    let mut region: Option<lrc_linux::Region> = None;
    let mut monitor: Option<String> = None;
    let mut video = VidChoice::Auto;
    let mut video_bitrate: u32 = 4_000_000;
    let mut video_max_fps: u32 = 30;
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
            "--region" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                region = Some(lrc_linux::Region::parse(&v)?);
            }
            "--monitor" => monitor = Some(take_value(&mut it, inline_val.clone(), &name)?),
            "--video" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                video = match v.as_str() {
                    "off" | "none" | "0" => VidChoice::Off,
                    "auto" => VidChoice::Auto,
                    "h264" => VidChoice::H264,
                    other => {
                        return Err(format!("--video 只支持 off|auto|h264，收到 {other}"))
                    }
                };
            }
            "--video-bitrate" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                let n = v
                    .parse::<u32>()
                    .map_err(|_| format!("--video-bitrate 需要整数（bit/s），收到 {v}"))?;
                if n < 100_000 || n > 100_000_000 {
                    return Err(format!("--video-bitrate 应在 100k..=100M，收到 {n}"));
                }
                video_bitrate = n;
            }
            "--video-max-fps" => {
                let v = take_value(&mut it, inline_val.clone(), &name)?;
                let n = v
                    .parse::<u32>()
                    .map_err(|_| format!("--video-max-fps 需要整数，收到 {v}"))?;
                if n == 0 || n > 240 {
                    return Err(format!("--video-max-fps 应在 1..=240，收到 {n}"));
                }
                video_max_fps = n;
            }
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
    if region.is_some() && monitor.is_some() {
        return Err("--region 与 --monitor 只能给一个".to_string());
    }
    Ok(Args {
        password,
        listen,
        display,
        region,
        monitor,
        video,
        video_bitrate,
        video_max_fps,
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

    // 2.1) 只推一台显示器时的采集区域（--region / --monitor）
    let region = match resolve_region(args) {
        Ok(r) => r,
        Err(e) => {
            log::error!("采集区域解析失败：{e}");
            writer.shutdown();
            return Ok(());
        }
    };
    let (out_w, out_h) = match region {
        Some(r) => {
            log::info!("只推显示器区域：{r}（整屏 root 是 {info}）");
            (r.width, r.height)
        }
        None => (info.width, info.height),
    };

    let nonce_s = make_nonce(args.room as u64);
    let srv = Message::ServerHello(ServerHello {
        version: PROTO_VERSION,
        width: out_w,
        height: out_h,
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

    let result = capture_loop(args, info, region, Arc::clone(&shared), path);

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
    /// 本帧编码耗时累计（微秒），用来判断是不是编码卡住了
    encode_us_sum: u64,
    /// 单帧编码耗时峰值（微秒）
    encode_us_max: u64,
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

/// 解析 --region / --monitor：都没有就是整屏（None）。
fn resolve_region(args: &Args) -> Result<Option<lrc_linux::Region>, String> {
    if let Some(r) = args.region {
        return Ok(Some(r));
    }
    let Some(want) = args.monitor.as_deref() else {
        return Ok(None);
    };
    let monitors = lrc_linux::monitors().map_err(|e| format!("枚举显示器失败：{e}"))?;
    if monitors.is_empty() {
        return Err("--monitor：RandR 没报告任何已连接的显示器".to_string());
    }
    let listed = monitors
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join("，");
    match lrc_linux::pick_monitor(&monitors, Some(want)) {
        Some(m) => {
            log::info!("--monitor {want} → {m}（共 {list:?}）", list = listed);
            Ok(Some(m.region()))
        }
        None => Err(format!("--monitor {want} 没找到；已连接：{listed}")),
    }
}

// ─────────────────────────── 视频通道（NVENC） ───────────────────────────
//
// 背景：块模式是 intra-only（每块独立 JPEG）。全屏视频一帧有 3000+ 变化块，
// 实测要 27–30 Mbps，而这条链路（家用宽带上行 → VPS 出口）只有 ~5 Mbps，
// 于是 host 每帧都发不完、客户端只能收到几 fps。
//
// 视频通道把整帧交给 NVENC（HEVC/H.264）做帧间压缩，同样画面只要 ~4 Mbps，
// 链路吃得消就能稳定 30 fps。与 Windows 版 crates/lrc-host/src/video.rs 同构：
// - 编码器在会话建立时创建并探测一次，失败就永久留在块模式；
// - 编码跑独立线程，采集线程只往单槽邮箱丢最新帧（满了覆盖 = 丢旧帧，不排队）；
// - 切模式用世代号，切走后旧世代在途帧直接丢。

/// 进视频模式（判据①）：变化块比例 > ENTER_RATIO 连续 ENTER_FRAMES 帧
const VID_ENTER_RATIO: f32 = 0.45;
const VID_ENTER_FRAMES: u32 = 3;
/// 进视频模式（判据②）：块模式实际码率持续超过阈值——见 block_bitrate_trigger
const BLOCK_OVER_KBPS: f64 = 6_000.0;
/// 码率判据要连续超这么多秒才切（避免一阵突发就切走）
const VID_BITRATE_ENTER_SECS: u32 = 2;
/// 回块模式：变化块比例 < EXIT_RATIO 连续 EXIT_FRAMES 帧
const VID_EXIT_RATIO: f32 = 0.10;
const VID_EXIT_FRAMES: u32 = 30;

/// 判据②的判定：块模式实际码率是否明显超过"整帧视频"的代价。
///
/// 与 Windows 版 crates/lrc-host/src/session.rs 完全一致：
/// 阈值取 max(6 Mbps, --video-bitrate × 1.5)，这样即使视频码率配得很低，
/// 只要块模式码率远高于它，切视频依然划算。
fn block_bitrate_trigger(block_kbps: f64, video_bitrate_bps: u32) -> bool {
    let threshold = BLOCK_OVER_KBPS.max(f64::from(video_bitrate_bps) / 1000.0 * 1.5);
    block_kbps > threshold
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VidMode {
    Blocks,
    Video,
}

/// 块/视频模式判定（纯逻辑，便于单测）。
struct ModeSwitch {
    available: bool,
    mode: VidMode,
    high: u32,
    low: u32,
}

impl ModeSwitch {
    fn new(available: bool) -> Self {
        Self {
            available,
            mode: VidMode::Blocks,
            high: 0,
            low: 0,
        }
    }

    fn mode(&self) -> VidMode {
        self.mode
    }

    fn available(&self) -> bool {
        self.available
    }

    /// 观察一帧的 (变化块数, 总块数)，返回本帧是否刚切换模式。
    fn observe(&mut self, changed: usize, total: usize) -> Option<VidMode> {
        if !self.available || total == 0 {
            return None;
        }
        let ratio = changed as f32 / total as f32;
        match self.mode {
            VidMode::Blocks => {
                if ratio >= VID_ENTER_RATIO {
                    self.high = self.high.saturating_add(1);
                    if self.high >= VID_ENTER_FRAMES {
                        self.high = 0;
                        self.low = 0;
                        self.mode = VidMode::Video;
                        return Some(VidMode::Video);
                    }
                } else {
                    self.high = 0;
                }
                None
            }
            VidMode::Video => {
                if ratio <= VID_EXIT_RATIO {
                    self.low = self.low.saturating_add(1);
                    if self.low >= VID_EXIT_FRAMES {
                        self.high = 0;
                        self.low = 0;
                        self.mode = VidMode::Blocks;
                        return Some(VidMode::Blocks);
                    }
                } else {
                    self.low = 0;
                }
                None
            }
        }
    }

    /// 清空连续计数（整帧重发/尺寸变化等"非运动"帧到来时用）。
    fn reset_streaks(&mut self) {
        self.high = 0;
        self.low = 0;
    }

    /// 强制进视频模式（码率判据触发时用）。
    fn force_video(&mut self) {
        if !self.available {
            return;
        }
        self.mode = VidMode::Video;
        self.high = 0;
        self.low = 0;
    }

    /// 编码器挂了：永久回块模式。
    fn disable(&mut self) {
        self.available = false;
        self.mode = VidMode::Blocks;
    }
}

/// 码率判据的阈值（kbps），日志用。
fn block_bitrate_trigger_threshold_kbps(video_bitrate_bps: u32) -> f64 {
    BLOCK_OVER_KBPS.max(f64::from(video_bitrate_bps) / 1000.0 * 1.5)
}

/// 系统开机毫秒（u32 截断）。作为 VideoAu.ts_ms 的时间基准：
/// 同机 host/client 共享系统时钟时是真延迟，跨机只能当参考（与 Windows 版口径一致）。
fn uptime_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

/// 编码线程的一帧任务。
struct VidJob {
    frame: Arc<lrc_linux::Frame>,
    seq: u32,
    generation: u32,
    ts_ms: u32,
}

struct VidPipe {
    slot: Mutex<Option<VidJob>>,
    cv: Condvar,
    stop: AtomicBool,
    generation: AtomicU32,
    keyframe_req: AtomicBool,
    broken: AtomicBool,
    send_failed: AtomicBool,
    aus: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
}

impl VidPipe {
    fn lock_slot(&self) -> std::sync::MutexGuard<'_, Option<VidJob>> {
        match self.slot.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }
}

struct VideoPipe {
    pipe: Arc<VidPipe>,
    handle: Option<std::thread::JoinHandle<()>>,
    codec_name: String,
    frame_size: (u32, u32),
}

impl VideoPipe {
    fn codec_name(&self) -> &str {
        &self.codec_name
    }

    fn frame_size(&self) -> (u32, u32) {
        self.frame_size
    }

    fn new_generation(&self) -> u32 {
        self.pipe
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// 把整帧交给编码线程（Arc 复用，不拷贝像素）。邮箱里已有的旧帧会被丢掉。
    fn submit(&self, frame: Arc<lrc_linux::Frame>, seq: u32, generation: u32) {
        {
            let mut slot = self.pipe.lock_slot();
            if slot.is_some() {
                self.pipe.dropped.fetch_add(1, Ordering::Relaxed);
            }
            *slot = Some(VidJob {
                frame,
                seq,
                generation,
                ts_ms: uptime_ms(),
            });
        }
        self.pipe.cv.notify_one();
    }

    fn is_broken(&self) -> bool {
        self.pipe.broken.load(Ordering::Relaxed)
    }

    fn send_failed(&self) -> bool {
        self.pipe.send_failed.load(Ordering::Relaxed)
    }

    fn dropped(&self) -> u64 {
        self.pipe.dropped.load(Ordering::Relaxed)
    }

    /// 请求下一个编码帧为 IDR（切进视频模式时用）。
    fn request_keyframe(&self) {
        self.pipe.keyframe_req.store(true, Ordering::Relaxed);
    }

    fn counters(&self) -> (u64, u64) {
        (
            self.pipe.aus.load(Ordering::Relaxed),
            self.pipe.bytes.load(Ordering::Relaxed),
        )
    }

    fn stop(&mut self) {
        self.pipe.stop.store(true, Ordering::Relaxed);
        self.pipe.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                log::warn!("视频编码线程 panic");
            }
        }
    }
}

impl Drop for VideoPipe {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 视频编码器选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VidChoice {
    Off,
    Auto,
    H264,
}

impl VidChoice {
    fn label(self) -> &'static str {
        match self {
            VidChoice::Off => "off",
            VidChoice::Auto => "auto",
            VidChoice::H264 => "h264",
        }
    }
}

/// 建编码器 + 起编码线程。返回 None = 视频通道不可用（永久留在块模式）。
fn start_video(
    args: &Args,
    width: u32,
    height: u32,
    shared: Arc<Shared>,
) -> Option<VideoPipe> {
    if args.video == VidChoice::Off {
        log::info!("视频通道：--video off，只用块模式");
        return None;
    }
    // auto：优先 hevc_nvenc（同画质码率更低），不可用再试 h264_nvenc
    let candidates: &[&str] = match args.video {
        VidChoice::H264 => &["h264_nvenc"],
        _ => &["hevc_nvenc", "h264_nvenc"],
    };
    let fps = args.video_max_fps.max(1);
    let mut created: Option<(FfmpegEncoder, String, u8)> = None;
    for codec in candidates {
        let Some(tag) = lrc_video::codec_tag_of(codec) else {
            continue;
        };
        let cfg = lrc_video::EncoderConfig {
            width,
            height,
            bitrate_bps: args.video_bitrate,
            // 2 秒一个 IDR：既保证可恢复，又不吃太多码率
            gop: fps.saturating_mul(2).max(1),
            fps,
            codec: (*codec).to_string(),
            ffmpeg_path: None,
        };
        let t = Instant::now();
        match lrc_video::FfmpegEncoder::new(cfg) {
            Ok(enc) => {
                log::info!(
                    "视频通道就绪：{} {}x{} @{}fps {}kbps（探测 {} ms）",
                    codec,
                    width,
                    height,
                    fps,
                    args.video_bitrate / 1000,
                    t.elapsed().as_millis()
                );
                created = Some((enc, (*codec).to_string(), tag));
                break;
            }
            Err(e) => log::warn!("视频编码器 {codec} 不可用：{e}"),
        }
    }
    let (encoder, codec_name, tag) = created?;

    let pipe = Arc::new(VidPipe {
        slot: Mutex::new(None),
        cv: Condvar::new(),
        stop: AtomicBool::new(false),
        generation: AtomicU32::new(0),
        keyframe_req: AtomicBool::new(false),
        broken: AtomicBool::new(false),
        send_failed: AtomicBool::new(false),
        aus: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    let spawned = {
        let pipe = Arc::clone(&pipe);
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("lrc-host-linux-video".to_string())
            .spawn(move || video_encode_loop(encoder, pipe, shared, tag))
    };
    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            log::warn!("视频通道不可用（启动编码线程失败）：{e}");
            return None;
        }
    };
    Some(VideoPipe {
        pipe,
        handle: Some(handle),
        codec_name,
        frame_size: (width, height),
    })
}

fn video_encode_loop(
    mut encoder: FfmpegEncoder,
    pipe: Arc<VidPipe>,
    shared: Arc<Shared>,
    codec: u8,
) {
    loop {
        let job = {
            let mut slot = pipe.lock_slot();
            loop {
                if pipe.stop.load(Ordering::Relaxed) {
                    encoder.shutdown();
                    return;
                }
                if let Some(job) = slot.take() {
                    break job;
                }
                slot = match pipe.cv.wait(slot) {
                    Ok(g) => g,
                    Err(poison) => poison.into_inner(),
                };
            }
        };

        // 切模式之后剩下的旧帧直接丢
        if job.generation != pipe.generation.load(Ordering::Relaxed) {
            continue;
        }
        if pipe.keyframe_req.swap(false, Ordering::Relaxed) {
            if let Err(e) = encoder.request_keyframe() {
                log::warn!("视频：请求 IDR 失败：{e}");
            }
        }

        let aus: Vec<Vec<u8>> = match encoder.encode(&job.frame.bgra, job.frame.stride) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("视频编码失败，永久回退块模式：{e}");
                pipe.broken.store(true, Ordering::Relaxed);
                encoder.shutdown();
                return;
            }
        };
        let key = encoder.last_au_was_keyframe();
        for au in aus {
            if job.generation != pipe.generation.load(Ordering::Relaxed) {
                break;
            }
            if au.len() > lrc_proto::message::MAX_VIDEO_AU {
                log::warn!(
                    "视频 AU 过大 {} > {}，丢弃",
                    au.len(),
                    lrc_proto::message::MAX_VIDEO_AU
                );
                continue;
            }
            let msg = Message::VideoAu(lrc_proto::VideoAu {
                ts_ms: job.ts_ms,
                seq: job.seq,
                codec,
                flags: if key { 1 } else { 0 },
                data: &au,
            });
            let payload = msg.encode();
            if let Err(e) = shared.send(&payload) {
                log::warn!("视频：发送码流失败：{e}");
                pipe.send_failed.store(true, Ordering::Relaxed);
                encoder.shutdown();
                return;
            }
            pipe.aus.fetch_add(1, Ordering::Relaxed);
            pipe.bytes
                .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        }
    }
}

/// 编码块的工作切片：每个工作线程按 (start, len) 认领一段连续区间。
///
/// 全屏视频时一帧可能有 500+ 个变化块，单线程 JPEG 只有 ~5 fps；
/// 块之间完全独立（只读同一份帧缓冲），所以按区间并行是安全的。
#[derive(Clone, Copy)]
struct EncodeJob {
    start: usize,
    len: usize,
}

/// 线程数：默认 = CPU 核数，上限 16（再多人抢内存带宽，收益递减）。
/// 环境变量 LRC_ENCODE_THREADS 可覆盖（1 = 单线程）。
fn encode_threads() -> usize {
    if let Ok(v) = std::env::var("LRC_ENCODE_THREADS") {
        if let Ok(n) = v.parse::<usize>() {
            return n.clamp(1, 64);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

/// 把 n 个元素切成尽量均分的 workers 段（空输入返回空表）。
fn split_jobs(n: usize, workers: usize) -> Vec<EncodeJob> {
    if n == 0 {
        return Vec::new();
    }
    let workers = workers.clamp(1, n);
    let base = n / workers;
    let extra = n % workers;
    let mut jobs = Vec::with_capacity(workers);
    let mut start = 0usize;
    for i in 0..workers {
        let len = base + usize::from(i < extra);
        if len == 0 {
            continue;
        }
        jobs.push(EncodeJob { start, len });
        start += len;
    }
    jobs
}

/// 编码单个块到 slot；成功返回 true。
fn encode_one(
    idx: u32,
    hash: u64,
    src: &[u8],
    stride: usize,
    grid: &Grid,
    frame_size: (u32, u32),
    opts: &lrc_codec::Options,
    slot: &mut BlockSlot,
) -> bool {
    let Some(rect) = grid.block_rect(idx, frame_size.0, frame_size.1) else {
        return false;
    };
    let Some(offset) = pixel_offset(&rect, stride) else {
        return false;
    };
    if offset >= src.len() {
        return false;
    }
    match lrc_codec::encode_block(&src[offset..], stride, rect.w, rect.h, opts, &mut slot.data) {
        Ok(codec) => {
            slot.idx = idx;
            slot.codec = codec;
            slot.hash = hash;
            true
        }
        Err(e) => {
            log::warn!("块 {idx} 编码失败：{e}");
            false
        }
    }
}

/// 并行编码一帧的所有变化块。
///
/// 每个 worker 只往 slots[start..start+len] 写自己那段（区间互不重叠，无需加锁）；
/// 若某个 worker 有块编码失败，退回单线程重编这一帧，保证 slots 紧凑无空洞。
/// 返回真正写好的块数。
fn encode_blocks_parallel(
    changed: &[(u32, u64)],
    src: &[u8],
    stride: usize,
    grid: &Grid,
    frame_size: (u32, u32),
    opts: &lrc_codec::Options,
    threads: usize,
    slots: &mut [BlockSlot],
) -> usize {
    if changed.is_empty() {
        return 0;
    }
    debug_assert!(slots.len() >= changed.len());
    let jobs = split_jobs(changed.len(), threads);
    if jobs.len() > 1 {
        // 先把 slots 切成和 jobs 一一对应的不相交可变切片（都是连续区间）
        let mut rest: &mut [BlockSlot] = &mut slots[..changed.len()];
        let mut segs: Vec<&mut [BlockSlot]> = Vec::with_capacity(jobs.len());
        for job in &jobs {
            let (head, tail) = rest.split_at_mut(job.len);
            segs.push(head);
            rest = tail;
        }
        let all_ok = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(jobs.len());
            for (job, out) in jobs.iter().zip(segs) {
                let changed_seg = &changed[job.start..job.start + job.len];
                handles.push(scope.spawn(move || {
                    let mut ok = true;
                    for (i, &(idx, hash)) in changed_seg.iter().enumerate() {
                        if !encode_one(idx, hash, src, stride, grid, frame_size, opts, &mut out[i]) {
                            ok = false;
                        }
                    }
                    ok
                }));
            }
            handles.into_iter().all(|h| h.join().unwrap_or(false))
        });
        if all_ok {
            return changed.len();
        }
        log::warn!("并行编码有块失败，本帧改单线程重编");
    }
    // 单线程路径（也是上面失败后的兜底）
    let mut used = 0usize;
    for &(idx, hash) in changed {
        if encode_one(idx, hash, src, stride, grid, frame_size, opts, &mut slots[used]) {
            used += 1;
        }
    }
    used
}
fn capture_loop(
    args: &Args,
    info: lrc_linux::DisplayInfo,
    region: Option<lrc_linux::Region>,
    shared: Arc<Shared>,
    path: &'static str,
) -> Result<(), String> {
    let mut capturer = lrc_linux::X11Capturer::new_with_region(info, region)
        .map_err(|e| format!("创建 X11 采集器失败：{e}"))?;
    // 鼠标注入要加采集区域原点，否则单屏模式下点击整体偏移
    let (origin_x, origin_y) = capturer.origin();
    lrc_linux::set_pointer_origin(origin_x, origin_y);
    let mut grid = Grid::new(BLOCK_SIZE, capturer.size().0, capturer.size().1)
        .ok_or_else(|| format!("网格非法 {}x{}", capturer.size().0, capturer.size().1))?;
    let (out_w, out_h) = capturer.size();
    log::info!("采集路径：{}", capturer.path_name());
    let mut hashes: Vec<u64> = vec![0; grid.count() as usize];
    let mut have_prev = false;
    let mut force_full = true;

    let opts = lrc_codec::Options {
        jpeg_quality: args.quality,
        ..lrc_codec::Options::default()
    };
    // 编码并行度（全屏视频时单线程只有几 fps）
    let encode_workers = encode_threads();
    log::info!("编码并行度：{encode_workers} 线程（LRC_ENCODE_THREADS 可覆盖）");
    // 视频通道（NVENC）：全屏运动内容走它，否则块模式 intra-only 会把链路打爆
    let mut video = start_video(args, out_w, out_h, Arc::clone(&shared));
    let mut mode_switch = ModeSwitch::new(video.is_some());
    let mut total_video_dropped: u64 = 0;
    match video.as_ref() {
        Some(v) => log::info!(
            "视频通道就绪：{}（块模式仍是默认，运动度高才切；--video {}）",
            v.codec_name(),
            args.video.label()
        ),
        None => log::info!("视频通道：不可用，只用块模式（--video {}）", args.video.label()),
    }
    let video_interval = Duration::from_secs_f64(1.0 / f64::from(args.video_max_fps.max(1)));
    let mut video_gen: u32 = 0;
    let mut last_video_counters = (0u64, 0u64);
    // 连续多少秒块模式码率超标（判据②用）
    let mut over_budget_secs: u32 = 0;
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
    let mut cur_size = (out_w, out_h);
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
        "推流 {}x{}（整屏 {}x{}） = {}x{} 块（{}px），上限 {}fps，JPEG q{}，视频 {}kbps@{}fps",
        out_w,
        out_h,
        info.width,
        info.height,
        grid.blocks_x,
        grid.blocks_y,
        BLOCK_SIZE,
        args.fps,
        args.quality,
        args.video_bitrate / 1000,
        args.video_max_fps
    );

    loop {
        if shared.stop() {
            break;
        }

        // 统计行（每秒一行）。注意：判定/统计都必须每秒跑，--stats 只控制打印，
        // 否则不开 --stats 时判据②永远不生效。
        {
            let elapsed = last_report.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let d = elapsed.as_secs_f64().max(0.001);
                // 视频通道累计（这一秒的增量）
                let (vaus, vbytes) = video
                    .as_ref()
                    .map(|v| v.counters())
                    .unwrap_or((0, 0));
                let dvaus = vaus.saturating_sub(last_video_counters.0);
                let dvbytes = vbytes.saturating_sub(last_video_counters.1);
                last_video_counters = (vaus, vbytes);
                let mode_label = match mode_switch.mode() {
                    VidMode::Video => "video",
                    VidMode::Blocks => "blocks",
                };
                // 视频邮箱丢弃（编码跟不上采集时才会涨）
                if let Some(v) = video.as_ref() {
                    let d = v.dropped();
                    if d > total_video_dropped {
                        total_video_dropped = d;
                    }
                }
                let sec_kbps = (sec.bytes as f64 * 8.0 / 1000.0) / d;
                // 判据②：块模式码率持续超过"整帧视频的代价"就切视频（与 Windows 版一致）
                if mode_switch.mode() == VidMode::Blocks
                    && mode_switch.available()
                    && block_bitrate_trigger(sec_kbps, args.video_bitrate)
                {
                    over_budget_secs = over_budget_secs.saturating_add(1);
                    if over_budget_secs >= VID_BITRATE_ENTER_SECS {
                        log::info!(
                            "切视频模式（码率判据）：块模式 {sec_kbps:.0} kbps > 阈值 {} kbps，连续 {} 秒",
                            block_bitrate_trigger_threshold_kbps(args.video_bitrate),
                            over_budget_secs
                        );
                        if let Some(v) = video.as_ref() {
                            video_gen = v.new_generation();
                            v.request_keyframe();
                        }
                        mode_switch.force_video();
                        force_full = false;
                        over_budget_secs = 0;
                    }
                } else if mode_switch.mode() == VidMode::Blocks {
                    over_budget_secs = 0;
                }
                if args.stats {
                println!(
                    "t={:.1} fps={:.1} blocks={} sent_frames={} bytes={} kbps={:.1} idle_frames={} dropped={} sec_bytes={} cursor_msgs={} full_resends={} mode={} video_kbps={:.1} video_fps={:.1} video_drop={} video_avail={} level=0 stage=原生 scale=1.00 path={} udp_rtt={} udp_loss={} enc_us_max={}",
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
                    mode_label,
                    (dvbytes as f64 * 8.0 / 1000.0) / d,
                    dvaus as f64 / d,
                    total_video_dropped,
                    mode_switch.available(),
                    path,
                    shared.udp_rtt_ms(),
                    shared.udp_loss_permille(),
                    sec.encode_us_max
                );
                let _ = std::io::stdout().flush();
                }
                total.frames = total.frames.saturating_add(sec.frames);
                total.blocks = total.blocks.saturating_add(sec.blocks);
                total.bytes = total.bytes.saturating_add(sec.bytes);
                total.idle = total.idle.saturating_add(sec.idle);
                total.dropped = total.dropped.saturating_add(sec.dropped);
                total.full_resends = total.full_resends.saturating_add(sec.full_resends);
                total.cursor = total.cursor.saturating_add(sec.cursor);
                total.encode_us_sum = total.encode_us_sum.saturating_add(sec.encode_us_sum);
                total.encode_us_max = total.encode_us_max.max(sec.encode_us_max);
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
                    // 单屏模式下光标坐标要换算成采集区域内的相对坐标，
                    // 跑到区域外就当作不可见（客户端不该在别的屏上画光标）。
                    let rel_x = st.x - origin_x;
                    let rel_y = st.y - origin_y;
                    let inside = rel_x >= 0
                        && rel_y >= 0
                        && u32::try_from(rel_x).map_or(false, |v| v < out_w)
                        && u32::try_from(rel_y).map_or(false, |v| v < out_h);
                    let pos = Message::CursorPos(lrc_proto::CursorPos {
                        x: clamp_i16(rel_x.clamp(0, i32::from(i16::MAX))),
                        y: clamp_i16(rel_y.clamp(0, i32::from(i16::MAX))),
                        visible: st.visible && inside,
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

        // 模式判定：运动度高就切视频通道（块模式 intra-only 撑不住全屏视频）。
        // ⚠ 整帧重发（force_full / 首帧）时 changed 必然是 100%，那是"补帧"不是"运动"，
        // 喂给判定器会造成"刚切回块模式、下一帧又被判成高运动"的来回抖动。
        if send_full {
            mode_switch.reset_streaks();
        } else if let Some(new_mode) = mode_switch.observe(changed.len(), grid.count() as usize) {
            match new_mode {
                VidMode::Video => {
                    if let Some(v) = video.as_ref() {
                        log::info!(
                            "切视频模式：本帧 {} / {} 块变化（阈值 {:.2}）",
                            changed.len(),
                            grid.count(),
                            VID_ENTER_RATIO
                        );
                        video_gen = v.new_generation();
                        // 新世代第一帧必须是 IDR，客户端才能立刻接上
                        v.request_keyframe();
                        // 视频模式不发块帧，这个标志会一直挂着并让判定器误判"整帧重发"，
                        // 必须在切进来的这一刻清掉，否则永远切不回块模式。
                        force_full = false;
                    }
                }
                VidMode::Blocks => {
                    log::info!("切回块模式（运动消退），发一整帧干净画面");
                    if let Some(v) = video.as_ref() {
                        video_gen = v.new_generation();
                    }
                    // 回块模式：客户端需要一张完整底图
                    force_full = true;
                    have_prev = false;
                }
            }
        }

        // 视频模式：整帧交给编码线程（单槽邮箱，不排队），块模式不再逐块编码
        if mode_switch.mode() == VidMode::Video {
            match video.as_mut() {
                Some(v) if !v.is_broken() && !v.send_failed() => {
                    if frame.width != v.frame_size().0 || frame.height != v.frame_size().1 {
                        log::warn!(
                            "画面尺寸变了（{}x{} != 编码器 {}x{}），回块模式",
                            frame.width,
                            frame.height,
                            v.frame_size().0,
                            v.frame_size().1
                        );
                        mode_switch.disable();
                        video = None;
                        force_full = true;
                        have_prev = false;
                        next_grab_at = grab_done + frame_interval;
                        continue;
                    }
                    // 视频模式下也要把本帧哈希记为"已知"，否则下一帧的差分
                    // 永远和进视频模式前那一帧比，changed_ratio 降不下来、切不回块模式。
                    for &(idx, h) in &changed {
                        if let Some(slot) = hashes.get_mut(idx as usize) {
                            *slot = h;
                        }
                    }
                    have_prev = true;
                    v.submit(Arc::new(frame), seq, video_gen);
                    sec.frames += 1;
                    next_grab_at = grab_done + video_interval;
                    continue;
                }
                _ => {
                    // 编码器挂了：永久回块模式
                    log::warn!("视频通道不可用，永久回块模式");
                    mode_switch.disable();
                    video = None;
                    force_full = true;
                    have_prev = false;
                    next_grab_at = grab_done + frame_interval;
                    continue;
                }
            }
        }

        if changed.is_empty() {
            // 零变化零发送
            sec.idle += 1;
            next_grab_at = grab_done + frame_interval;
            continue;
        }

        // 逐块编码（文字无损优先，图像走 JPEG，取小者）。
        // 全屏视频一帧几百个块，单线程只有几 fps，所以按区间并行编码。
        while slots.len() < changed.len() {
            slots.push(BlockSlot::default());
        }
        let encode_started = Instant::now();
        let used = encode_blocks_parallel(
            &changed,
            &frame.bgra,
            frame.stride,
            &grid,
            (frame.width, frame.height),
            &opts,
            encode_workers,
            &mut slots,
        );
        let encode_us = encode_started.elapsed().as_micros() as u64;
        sec.encode_us_max = sec.encode_us_max.max(encode_us);
        sec.encode_us_sum = sec.encode_us_sum.saturating_add(encode_us);
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
        assert_eq!(a.region, None);
        assert_eq!(a.monitor, None);
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
            "--region",
            "1920x1080+1920+0",
        ]))
        .expect("parse");
        assert_eq!(a.password, "secret");
        assert_eq!(a.listen, "127.0.0.1:9000");
        assert_eq!(a.display.as_deref(), Some(":1"));
        assert_eq!(
            a.region,
            Some(lrc_linux::Region {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
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
        assert!(parse_args(argv(&["--password", "1", "--region", "1920"])).is_err());
    }

    #[test]
    fn video_cli_flags() {
        let a = parse_args(argv(&["--password", "1"])).expect("parse");
        assert_eq!(a.video, VidChoice::Auto);
        assert_eq!(a.video_bitrate, 4_000_000);
        assert_eq!(a.video_max_fps, 30);

        let b = parse_args(argv(&[
            "--password",
            "1",
            "--video",
            "h264",
            "--video-bitrate",
            "2500000",
            "--video-max-fps",
            "20",
        ]))
        .expect("parse");
        assert_eq!(b.video, VidChoice::H264);
        assert_eq!(b.video_bitrate, 2_500_000);
        assert_eq!(b.video_max_fps, 20);

        let off = parse_args(argv(&["--password", "1", "--video", "off"])).expect("parse");
        assert_eq!(off.video, VidChoice::Off);

        assert!(parse_args(argv(&["--password", "1", "--video", "vp9"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--video-bitrate", "1"])).is_err());
        assert!(parse_args(argv(&["--password", "1", "--video-max-fps", "0"])).is_err());
    }

    #[test]
    fn video_mode_needs_sustained_motion() {
        let total = 510;
        let mut sw = ModeSwitch::new(true);
        assert_eq!(sw.mode(), VidMode::Blocks);
        // 单帧高运动不够（要连续 3 帧）
        assert_eq!(sw.observe(400, total), None);
        assert_eq!(sw.observe(400, total), None);
        assert_eq!(sw.observe(400, total), Some(VidMode::Video));
        assert_eq!(sw.mode(), VidMode::Video);
        // 回到块模式要连续 30 帧低运动
        for _ in 0..(VID_EXIT_FRAMES - 1) {
            assert_eq!(sw.observe(10, total), None);
        }
        assert_eq!(sw.observe(10, total), Some(VidMode::Blocks));
        assert_eq!(sw.mode(), VidMode::Blocks);
    }

    #[test]
    fn video_mode_requires_encoder() {
        let mut sw = ModeSwitch::new(false);
        for _ in 0..100 {
            assert_eq!(sw.observe(400, 510), None);
        }
        assert_eq!(sw.mode(), VidMode::Blocks);
        // 编码器挂掉后永久禁用
        sw.disable();
        assert!(!sw.available());
        assert_eq!(sw.observe(400, 510), None);
    }

    #[test]
    fn monitor_and_region_are_exclusive() {
        let a = parse_args(argv(&["--password", "1", "--monitor", "DP-1"])).expect("parse");
        assert_eq!(a.monitor.as_deref(), Some("DP-1"));
        assert_eq!(a.region, None);
        assert!(parse_args(argv(&[
            "--password",
            "1",
            "--monitor",
            "DP-1",
            "--region",
            "1920x1080"
        ]))
        .is_err());
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
