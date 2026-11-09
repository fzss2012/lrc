//! Linux 控制端：X11 窗口 + 收流解码 + 呈现 + 输入转发。
//!
//! 用法：
//!   lrc-client-linux --password 1234 --connect 127.0.0.1:7800
//!                    [--signal <vps>:7801 --room 1 --p2p auto]
//!                    [--title "lrc viewer"] [--stats] [--snapshot out.ppm]
//!
//! 结构：
//! - 网络线程：建连（TCP / P2P）+ 握手 + 收消息，块/视频帧解到共享画布；
//! - 主线程：X11 事件循环（输入转发）+ 脏帧缩放后 put_image 到窗口。
//!
//! 协议/编码/解码与 Windows 版一致（复用 lrc-proto / lrc-codec / lrc-video）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use lrc_codec as codec;
use lrc_proto::{
    auth_reason, codec_tag, ClientHello, Grid, InputEvent, Message, Pong, VideoAu, VideoFrame,
    PROTO_VERSION,
};
use lrc_transport::link::{self, LinkReader, LinkWriter};
use lrc_transport::Conn;
use lrc_udp::{PeerMode, UdpStats};
use lrc_video::{DecoderConfig, FfmpegDecoder, VideoDecoder};
use sha2::Sha256;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, PropMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------- 参数

/// P2P 模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum P2pMode {
    Auto,
    Off,
    Direct,
    Relay,
}

impl P2pMode {
    fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

struct Config {
    password: String,
    connect: Option<String>,
    signal: Option<String>,
    room: u32,
    p2p: P2pMode,
    title: String,
    stats: bool,
    /// 渲染出一帧后把窗口内容写成 PPM（验收/排障用）
    snapshot: Option<String>,
    width: u32,
    height: u32,
}

const USAGE: &str = "\
用法: lrc-client-linux --password <密码> [--connect <host:port> | --signal <IP:PORT>]
                       [--room N] [--p2p auto|off|direct|relay] [--title <text>]
                       [--stats] [--snapshot <out.ppm>] [--width N] [--height N]
";

fn parse_args() -> Result<Config, String> {
    let mut password: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut signal: Option<String> = None;
    let mut room: u32 = 1;
    let mut p2p = P2pMode::Auto;
    let mut title = String::from("lrc viewer");
    let mut stats = false;
    let mut snapshot: Option<String> = None;
    let mut width: u32 = 1280;
    let mut height: u32 = 720;

    let mut it = std::env::args().skip(1).collect::<Vec<String>>().into_iter().peekable();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg, None),
        };
        let value = |it: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
                     inline: &Option<String>,
                     flag: &str|
         -> Result<String, String> {
            match inline {
                Some(v) => Ok(v.clone()),
                None => it.next().ok_or_else(|| format!("{flag} 缺少取值")),
            }
        };
        match flag.as_str() {
            "--password" => password = Some(value(&mut it, &inline, "--password")?),
            "--connect" => connect = Some(value(&mut it, &inline, "--connect")?),
            "--signal" => signal = Some(value(&mut it, &inline, "--signal")?),
            "--room" => {
                room = value(&mut it, &inline, "--room")?
                    .parse()
                    .map_err(|e| format!("--room: {e}"))?
            }
            "--p2p" => {
                p2p = match value(&mut it, &inline, "--p2p")?.to_ascii_lowercase().as_str() {
                    "auto" => P2pMode::Auto,
                    "off" => P2pMode::Off,
                    "direct" => P2pMode::Direct,
                    "relay" => P2pMode::Relay,
                    other => {
                        return Err(format!(
                            "--p2p 只接受 auto|off|direct|relay（收到 {other}）"
                        ))
                    }
                }
            }
            "--title" => title = value(&mut it, &inline, "--title")?,
            "--stats" => stats = true,
            "--snapshot" => snapshot = Some(value(&mut it, &inline, "--snapshot")?),
            "--width" => {
                width = value(&mut it, &inline, "--width")?
                    .parse()
                    .map_err(|e| format!("--width: {e}"))?
            }
            "--height" => {
                height = value(&mut it, &inline, "--height")?
                    .parse()
                    .map_err(|e| format!("--height: {e}"))?
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }

    let password = password.ok_or_else(|| String::from("缺少 --password <密码>"))?;
    let p2p_available = signal.is_some() && p2p.enabled();
    if connect.is_none() && !p2p_available {
        return Err(String::from(
            "缺少 --connect <host:port>（没给 --signal 时必须提供 TCP 直连地址）",
        ));
    }
    if width == 0 || height == 0 {
        return Err(String::from("--width/--height 必须大于 0"));
    }
    Ok(Config {
        password,
        connect,
        signal,
        room,
        p2p,
        title,
        stats,
        snapshot,
        width,
        height,
    })
}

// ---------------------------------------------------------------- 共享状态

/// 远端画布（被控端像素坐标系 BGRA，紧凑行）。
struct Canvas {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    /// 自上次渲染后有没有新数据
    dirty: bool,
}

struct Shared {
    canvas: Mutex<Canvas>,
    /// 发送通道（会话期间有值；断线时为 None）
    out: Mutex<Option<Sender<Vec<u8>>>>,
    /// 被控端画面尺寸（ServerHello / FULL 帧校正）
    remote: Mutex<Option<(u32, u32)>>,
    /// 标题（主线程负责写 WM_NAME）
    title: Mutex<String>,
    quit: AtomicBool,
    frames: AtomicU64,
    decoded_blocks: AtomicU64,
    recv_bytes: AtomicU64,
    video_frames: AtomicU64,
    video_mode: AtomicBool,
    reconnect_count: AtomicU64,
    decode_us: AtomicU64,
    decode_n: AtomicU64,
    render_us: AtomicU64,
    render_n: AtomicU64,
    rtt_micros: AtomicU64,
    udp_rtt_x10: AtomicU64,
    udp_loss: AtomicU64,
    path: Mutex<&'static str>,
    ping_at: Mutex<Option<Instant>>,
    /// 单调时钟起点（Ping 用）
    clock: Instant,
}

impl Shared {
    fn new(title: String) -> Self {
        Self {
            canvas: Mutex::new(Canvas {
                width: 0,
                height: 0,
                bgra: Vec::new(),
                dirty: false,
            }),
            out: Mutex::new(None),
            remote: Mutex::new(None),
            title: Mutex::new(title),
            quit: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            decoded_blocks: AtomicU64::new(0),
            recv_bytes: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_mode: AtomicBool::new(false),
            reconnect_count: AtomicU64::new(0),
            decode_us: AtomicU64::new(0),
            decode_n: AtomicU64::new(0),
            render_us: AtomicU64::new(0),
            render_n: AtomicU64::new(0),
            rtt_micros: AtomicU64::new(0),
            udp_rtt_x10: AtomicU64::new(0),
            udp_loss: AtomicU64::new(0),
            path: Mutex::new("tcp"),
            ping_at: Mutex::new(None),
            clock: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    fn send(&self, payload: Vec<u8>) -> bool {
        let guard = lock(&self.out);
        match guard.as_ref() {
            Some(tx) => tx.send(payload).is_ok(),
            None => false,
        }
    }

    fn remote_size(&self) -> Option<(u32, u32)> {
        *lock(&self.remote)
    }

    /// 确保画布和远端尺寸一致；尺寸变了就重建（内容清空，等下一帧）。
    fn ensure_canvas(&self, width: u32, height: u32) {
        let mut canvas = lock(&self.canvas);
        if canvas.width == width && canvas.height == height && !canvas.bgra.is_empty() {
            return;
        }
        let Some(len) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
        else {
            return;
        };
        canvas.width = width;
        canvas.height = height;
        canvas.bgra = vec![0u8; len];
        canvas.dirty = true;
    }

    fn set_path(&self, path: &'static str) {
        *lock(&self.path) = path;
    }

    fn path(&self) -> &'static str {
        *lock(&self.path)
    }

    fn set_udp_stats(&self, stats: UdpStats) {
        self.udp_rtt_x10
            .store((f64::from(stats.rtt_ms) * 10.0) as u64, Ordering::Relaxed);
        self.udp_loss
            .store(u64::from(stats.loss_permille), Ordering::Relaxed);
    }

    fn note_ping(&self) {
        *lock(&self.ping_at) = Some(Instant::now());
    }

    fn note_pong(&self) {
        if let Some(at) = lock(&self.ping_at).take() {
            self.rtt_micros
                .store(at.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ---------------------------------------------------------------- 网络线程

enum SessionEnd {
    Quit,
    Disconnected(String),
}

enum HandshakeError {
    Terminal(String),
    Transient(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    Udp,
    Tcp,
}

/// 本次连接尝试顺序：上次成功的路径优先，失败再换另一条。
fn attempt_order(p2p_enabled: bool, preferred: PathKind) -> Vec<PathKind> {
    if !p2p_enabled {
        return vec![PathKind::Tcp];
    }
    match preferred {
        PathKind::Udp => vec![PathKind::Udp, PathKind::Tcp],
        PathKind::Tcp => vec![PathKind::Tcp, PathKind::Udp],
    }
}

fn backoff_delay(attempt: u32) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(4),
        4 => Duration::from_secs(8),
        _ => Duration::from_secs(10),
    }
}

fn sleep_backoff(shared: &Shared, attempt: u32) -> bool {
    let mut left = backoff_delay(attempt);
    while !left.is_zero() {
        if shared.quit.load(Ordering::Relaxed) {
            return false;
        }
        let step = left.min(Duration::from_millis(100));
        std::thread::sleep(step);
        left -= step;
    }
    !shared.quit.load(Ordering::Relaxed)
}

/// 连接监督循环：断线退避重连；认证失败才终止（窗口保留）。
fn supervise(cfg: Config, shared: Arc<Shared>) {
    let p2p_enabled = cfg.signal.is_some() && cfg.p2p.enabled();
    let has_tcp = cfg.connect.is_some();
    if !has_tcp {
        log::warn!("未给 --connect：P2P 失败后无法回退 TCP");
    }
    let mut preferred = if p2p_enabled {
        PathKind::Udp
    } else {
        PathKind::Tcp
    };
    let mut attempt: u32 = 0;
    loop {
        if shared.quit.load(Ordering::Relaxed) {
            return;
        }

        // ---- 1) 建连 ----
        let mut opened: Option<(LinkWriter, LinkReader, &'static str)> = None;
        let mut last_error = String::new();
        let mut order = attempt_order(p2p_enabled, preferred);
        if !has_tcp {
            order.retain(|k| *k != PathKind::Tcp);
        }
        for kind in order {
            if shared.quit.load(Ordering::Relaxed) {
                return;
            }
            let result = match kind {
                PathKind::Udp => match cfg.signal.as_deref() {
                    Some(signal) => open_udp(signal, cfg.room, cfg.p2p),
                    None => Err(String::from("未配置 --signal")),
                },
                PathKind::Tcp => match cfg.connect.as_deref() {
                    Some(addr) => match Conn::connect(addr) {
                        Ok(conn) => match link::from_tcp(conn) {
                            Ok(link) => Ok(link),
                            Err(e) => Err(format!("TCP split 失败：{e}")),
                        },
                        Err(e) => Err(format!("TCP 连接 {addr} 失败：{e}")),
                    },
                    None => Err(String::from("未给 --connect：P2P 失败后无法回退 TCP")),
                },
            };
            match result {
                Ok((writer, reader)) => {
                    let path = writer.path();
                    preferred = kind;
                    opened = Some((writer, reader, path));
                    break;
                }
                Err(e) => {
                    log::warn!("路径 {kind:?} 不可用：{e}");
                    last_error = e;
                }
            }
        }

        let Some((writer, reader, path)) = opened else {
            attempt = attempt.saturating_add(1);
            let delay = backoff_delay(attempt);
            log::warn!("连接失败（第 {attempt} 次）：{last_error}；{}s 后重试", delay.as_secs());
            if !sleep_backoff(&shared, attempt) {
                return;
            }
            continue;
        };
        shared.set_path(path);

        // ---- 2) 握手 ----
        match handshake(writer, reader, &cfg.password, &shared) {
            Ok((writer, mut reader)) => {
                if attempt > 0 {
                    shared.reconnect_count.fetch_add(1, Ordering::Relaxed);
                }
                attempt = 0;
                let (tx, rx) = channel::<Vec<u8>>();
                spawn_writer(writer, rx);
                *lock(&shared.out) = Some(tx.clone());
                log::info!("已连接（{path}），开始接收");
                shared.send(Message::RequestFull.encode());
                spawn_pinger(tx.clone(), shared.clone());

                let end = session_loop(&mut reader, &shared, &tx);
                *lock(&shared.out) = None;
                drop(tx);
                match end {
                    SessionEnd::Quit => return,
                    SessionEnd::Disconnected(reason) => {
                        if shared.quit.load(Ordering::Relaxed) {
                            return;
                        }
                        attempt = attempt.saturating_add(1);
                        shared.reconnect_count.fetch_add(1, Ordering::Relaxed);
                        let delay = backoff_delay(attempt);
                        log::warn!(
                            "连接断开（{reason}）；第 {attempt} 次重连，{}s 后重试",
                            delay.as_secs()
                        );
                        if !sleep_backoff(&shared, attempt) {
                            return;
                        }
                    }
                }
            }
            Err(HandshakeError::Terminal(message)) => {
                log::error!("{message}（不再重连）");
                *lock(&shared.out) = None;
                let mut title = lock(&shared.title);
                *title = format!("{} — {message}", title.clone());
                return;
            }
            Err(HandshakeError::Transient(e)) => {
                attempt = attempt.saturating_add(1);
                let delay = backoff_delay(attempt);
                log::warn!("握手失败（第 {attempt} 次）：{e}；{}s 后重试", delay.as_secs());
                if !sleep_backoff(&shared, attempt) {
                    return;
                }
            }
        }
    }
}

/// P2P：打洞成功 = direct_udp，失败自动 relay_udp。
fn open_udp(signal: &str, room: u32, mode: P2pMode) -> Result<(LinkWriter, LinkReader), String> {
    let relay: SocketAddr = signal
        .parse()
        .map_err(|e| format!("--signal 地址非法（{signal}）：{e}"))?;
    let punch = match mode {
        P2pMode::Relay => Duration::from_millis(0),
        _ => Duration::from_secs(2),
    };
    let established =
        lrc_relay::establish(false, relay, room, None, Duration::from_secs(2), punch)
            .map_err(|e| format!("P2P 建立失败：{e}"))?;
    if mode == P2pMode::Direct && established.mode != PeerMode::Direct {
        return Err(String::from("--p2p direct 要求打洞成功，但已降级 relay"));
    }
    let peer = established.conn_peer();
    let peer_mode = established.mode;
    log::info!("UDP 链路建立：{}（peer={peer}）", peer_mode.kind());
    let conn = lrc_udp::UdpConn::new(established.socket, peer, peer_mode)
        .map_err(|e| format!("UdpConn 初始化失败：{e}"))?;
    Ok(link::from_udp(conn))
}

fn spawn_writer(mut writer: LinkWriter, rx: Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        let _ = writer.set_write_timeout(Some(Duration::from_secs(5)));
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(payload) => {
                    if let Err(e) = writer.send(&payload) {
                        log::warn!("发送失败：{e}");
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// 每秒 Ping（RTT + 保活）。发送通道断开（会话结束）就退出。
fn spawn_pinger(tx: Sender<Vec<u8>>, shared: Arc<Shared>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if shared.quit.load(Ordering::Relaxed) {
            break;
        }
        shared.note_ping();
        let ping = Message::Ping(lrc_proto::Ping {
            ts_ms: shared.now_ms(),
        });
        if tx.send(ping.encode()).is_err() {
            break;
        }
    });
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn send_link(writer: &mut LinkWriter, payload: &[u8]) -> Result<(), HandshakeError> {
    writer
        .send(payload)
        .map_err(|e| HandshakeError::Transient(format!("发送失败: {e}")))
}

/// 统一链路上读一条握手消息；超时返回 Ok(None)。
fn recv_link_msg<'a>(
    reader: &mut LinkReader,
    buf: &'a mut Vec<u8>,
    timeout: Duration,
) -> Result<Option<Message<'a>>, HandshakeError> {
    let is_udp = matches!(reader, LinkReader::Udp(_));
    let read = if is_udp {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                break Ok(None);
            }
            match reader.recv_timeout(buf, Duration::from_millis(200)) {
                Ok(Some(_)) => break Ok(Some(())),
                Ok(None) => continue,
                Err(e) => break Err(e),
            }
        }
    } else {
        match reader.recv(buf) {
            Ok(Some(_)) => Ok(Some(())),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    };
    match read {
        Ok(Some(())) => Message::decode(buf)
            .map(Some)
            .map_err(|e| HandshakeError::Transient(format!("握手消息解码失败: {e}"))),
        Ok(None) => Ok(None),
        Err(e) => Err(HandshakeError::Transient(format!("读握手消息失败: {e}"))),
    }
}

fn handshake(
    mut writer: LinkWriter,
    mut reader: LinkReader,
    password: &str,
    shared: &Arc<Shared>,
) -> Result<(LinkWriter, LinkReader), HandshakeError> {
    let client_id = random_id().map_err(|e| HandshakeError::Transient(format!("{e}")))?;
    let nonce_c = random_id().map_err(|e| HandshakeError::Transient(format!("{e}")))?;
    send_link(
        &mut writer,
        &Message::ClientHello(ClientHello {
            version: PROTO_VERSION,
            client_id,
            nonce_c,
        })
        .encode(),
    )?;

    let mut buf = Vec::with_capacity(4096);
    let server = match recv_link_msg(&mut reader, &mut buf, HANDSHAKE_TIMEOUT) {
        Ok(Some(Message::ServerHello(hello))) => hello,
        Ok(Some(other)) => {
            return Err(HandshakeError::Transient(format!(
                "期望 ServerHello，收到消息类型 {:#x}",
                other.msg_type()
            )))
        }
        Ok(None) => {
            return Err(HandshakeError::Transient(String::from(
                "对端在 ServerHello 之前关闭连接/超时",
            )))
        }
        Err(e) => return Err(e),
    };
    if server.version != PROTO_VERSION {
        return Err(HandshakeError::Terminal(format!(
            "协议版本不匹配：对端 v{}，本地 v{}",
            server.version, PROTO_VERSION
        )));
    }
    *lock(&shared.remote) = Some((server.width, server.height));
    log::info!(
        "ServerHello: {}x{} rotation={} 协议 v{}",
        server.width,
        server.height,
        server.rotation,
        server.version
    );

    let mut mac_input = [0u8; 32];
    mac_input[..16].copy_from_slice(&nonce_c);
    mac_input[16..].copy_from_slice(&server.nonce_s);
    let mac = hmac_sha256(password.as_bytes(), &mac_input)
        .map_err(|e| HandshakeError::Transient(format!("{e}")))?;
    send_link(
        &mut writer,
        &Message::AuthRequest(lrc_proto::AuthRequest { mac }).encode(),
    )?;

    match recv_link_msg(&mut reader, &mut buf, HANDSHAKE_TIMEOUT) {
        Ok(Some(Message::AuthResult(result))) => {
            if !result.ok {
                return Err(HandshakeError::Terminal(format!(
                    "认证失败：{}",
                    auth_reason_text(result.reason)
                )));
            }
        }
        Ok(Some(other)) => {
            return Err(HandshakeError::Transient(format!(
                "期望 AuthResult，收到消息类型 {:#x}",
                other.msg_type()
            )))
        }
        Ok(None) => {
            return Err(HandshakeError::Transient(String::from(
                "对端在 AuthResult 之前关闭连接",
            )))
        }
        Err(e) => return Err(e),
    }
    log::info!("认证通过");
    Ok((writer, reader))
}

fn auth_reason_text(reason: u8) -> &'static str {
    match reason {
        auth_reason::OK => "ok",
        auth_reason::BAD_PASSWORD => "密码错误",
        auth_reason::VERSION_MISMATCH => "协议版本不匹配",
        auth_reason::RATE_LIMITED => "尝试过于频繁，被限流",
        _ => "未知原因",
    }
}

fn session_loop(reader: &mut LinkReader, shared: &Arc<Shared>, tx: &Sender<Vec<u8>>) -> SessionEnd {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    let mut decoder: Option<(u8, FfmpegDecoder)> = None;
    let mut seen_key = false;
    loop {
        if shared.quit.load(Ordering::Relaxed) {
            return SessionEnd::Quit;
        }
        let n = match reader.recv(&mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => {
                if shared.quit.load(Ordering::Relaxed) {
                    return SessionEnd::Quit;
                }
                return SessionEnd::Disconnected(String::from("对端关闭了连接"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if shared.quit.load(Ordering::Relaxed) {
                    return SessionEnd::Quit;
                }
                return SessionEnd::Disconnected(format!("收包失败: {e}"));
            }
        };
        shared.recv_bytes.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(stats) = reader.udp_stats() {
            shared.set_udp_stats(stats);
        }

        match Message::decode(&buf) {
            Ok(Message::VideoFrame(frame)) => {
                shared.video_mode.store(false, Ordering::Relaxed);
                handle_frame(&frame, shared);
            }
            Ok(Message::VideoAu(au)) => {
                handle_au(&au, shared, tx, &mut decoder, &mut seen_key);
            }
            Ok(Message::Ping(ping)) => {
                let _ = tx.send(Message::Pong(Pong { ts_ms: ping.ts_ms }).encode());
            }
            Ok(Message::Pong(_)) => shared.note_pong(),
            Ok(Message::ServerHello(hello)) => {
                let changed = {
                    let mut remote = lock(&shared.remote);
                    if *remote != Some((hello.width, hello.height)) {
                        *remote = Some((hello.width, hello.height));
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    log::info!("被控端尺寸变化: {}x{}", hello.width, hello.height);
                    let _ = tx.send(Message::RequestFull.encode());
                }
            }
            // 本版不自绘光标：X11 窗口有自己的指针，避免画两遍
            Ok(Message::CursorPos(_)) | Ok(Message::CursorShape(_)) => {}
            Ok(Message::DisplayList(list)) => {
                let summary: Vec<String> = list
                    .iter()
                    .map(|d| format!("#{} {}x{}", d.index, d.width, d.height))
                    .collect();
                log::info!("可用显示输出 {} 个：{}", list.len(), summary.join("，"));
            }
            Ok(Message::SelectDisplay(_)) => {}
            Ok(other) => log::debug!("忽略消息类型 {:#x}", other.msg_type()),
            Err(e) => log::warn!("消息解码失败，丢弃: {e}"),
        }
    }
}

fn codec_name(tag: u8) -> Option<&'static str> {
    match tag {
        codec_tag::H264 => Some("h264"),
        codec_tag::HEVC => Some("hevc"),
        _ => None,
    }
}

/// 视频访问单元 → 解码 → 覆盖画布。
fn handle_au(
    au: &VideoAu<'_>,
    shared: &Arc<Shared>,
    tx: &Sender<Vec<u8>>,
    decoder: &mut Option<(u8, FfmpegDecoder)>,
    seen_key: &mut bool,
) {
    let Some(name) = codec_name(au.codec) else {
        log::warn!("未知视频码流 codec={}", au.codec);
        return;
    };
    let Some((width, height)) = shared.remote_size() else {
        return;
    };
    let need_decoder = match decoder {
        None => true,
        Some((codec, _)) => *codec != au.codec && au.is_key(),
    };
    if need_decoder {
        let cfg = DecoderConfig {
            width,
            height,
            codec: String::from(name),
            ffmpeg_path: None,
        };
        match FfmpegDecoder::new(&cfg) {
            Ok(dec) => {
                log::info!("视频解码器已建立：{} {}x{}", cfg.codec, width, height);
                *decoder = Some((au.codec, dec));
                *seen_key = false;
            }
            Err(e) => {
                log::error!("视频解码器创建失败: {e}");
                let _ = tx.send(Message::RequestFull.encode());
                return;
            }
        }
    }
    if !*seen_key && !au.is_key() {
        return;
    }
    let Some((_, dec)) = decoder.as_mut() else {
        return;
    };
    let started = Instant::now();
    match dec.decode(au.data) {
        Ok(Some(bgra)) => {
            let need = width as usize * height as usize * 4;
            if bgra.len() < need {
                log::warn!("解码帧字节数不足: {} < {}", bgra.len(), need);
                return;
            }
            if au.is_key() {
                *seen_key = true;
            }
            shared
                .decode_us
                .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            shared.decode_n.fetch_add(1, Ordering::Relaxed);
            shared.video_frames.fetch_add(1, Ordering::Relaxed);
            shared.video_mode.store(true, Ordering::Relaxed);
            let mut canvas = lock(&shared.canvas);
            if canvas.width != width || canvas.height != height {
                canvas.width = width;
                canvas.height = height;
                canvas.bgra = vec![0u8; need];
            }
            canvas.bgra[..need].copy_from_slice(&bgra[..need]);
            canvas.dirty = true;
        }
        Ok(None) => {}
        Err(e) => {
            log::warn!("视频解码失败 seq={} key={}: {e}", au.seq, au.is_key());
            *seen_key = false;
            *decoder = None;
            let _ = tx.send(Message::RequestFull.encode());
        }
    }
}

/// 块帧：逐个解到画布对应位置。
fn handle_frame(frame: &VideoFrame<'_>, shared: &Arc<Shared>) {
    let Some(grid) = frame.grid() else {
        log::warn!("帧几何非法：block_size/blocks 为 0");
        return;
    };
    // 服务端降分辨率时 ServerHello 的宽高不再等于实际帧：FULL 帧网格为准
    if frame.is_full() {
        let grid_w = u32::from(grid.blocks_x) * u32::from(grid.block_size);
        let grid_h = u32::from(grid.blocks_y) * u32::from(grid.block_size);
        if grid_w > 0 && grid_h > 0 {
            if let Some((sw, sh)) = shared.remote_size() {
                if grid_w < sw || grid_h < sh {
                    log::info!(
                        "按 FULL 帧网格校正画布 → {grid_w}x{grid_h}（ServerHello {sw}x{sh}）"
                    );
                    *lock(&shared.remote) = Some((grid_w, grid_h));
                }
            }
        }
    }
    let Some((screen_w, screen_h)) = shared.remote_size() else {
        return;
    };
    if screen_w == 0 || screen_h == 0 {
        return;
    }
    shared.ensure_canvas(screen_w, screen_h);

    let mut canvas = lock(&shared.canvas);
    let stride = canvas.width as usize * 4;
    let mut decoded = 0u64;
    for entry in &frame.entries {
        let Some(rect) = block_rect(&grid, entry.idx, screen_w, screen_h) else {
            continue;
        };
        if codec::row_bytes(rect.w).is_none() {
            continue;
        }
        let offset = rect.y as usize * stride + rect.x as usize * 4;
        if offset >= canvas.bgra.len() {
            continue;
        }
        match codec::decode_block(
            entry.codec,
            entry.data,
            rect.w,
            rect.h,
            &mut canvas.bgra[offset..],
            stride,
        ) {
            Ok(()) => decoded = decoded.saturating_add(1),
            Err(e) => log::warn!(
                "块解码失败 idx={} codec={} {}x{}: {e}",
                entry.idx,
                entry.codec,
                rect.w,
                rect.h
            ),
        }
    }
    canvas.dirty = true;
    drop(canvas);
    shared.decoded_blocks.fetch_add(decoded, Ordering::Relaxed);
    shared.frames.fetch_add(1, Ordering::Relaxed);
    log::trace!(
        "帧 seq={} flags={:#x} 解出 {}/{} 块",
        frame.seq,
        frame.flags,
        decoded,
        frame.entries.len()
    );
}

/// 块矩形（远端像素坐标，底部/右侧自动裁剪）。
fn block_rect(grid: &Grid, idx: u32, screen_w: u32, screen_h: u32) -> Option<Rect> {
    let bx = u32::from(grid.blocks_x);
    let bs = u32::from(grid.block_size);
    if bx == 0 || bs == 0 {
        return None;
    }
    let x = idx % bx;
    let y = idx / bx;
    if y >= u32::from(grid.blocks_y) {
        return None;
    }
    let px = x * bs;
    let py = y * bs;
    if px >= screen_w || py >= screen_h {
        return None;
    }
    Some(Rect {
        x: px,
        y: py,
        w: bs.min(screen_w - px),
        h: bs.min(screen_h - py),
    })
}

struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn random_id() -> std::io::Result<[u8; 16]> {
    let mut id = [0u8; 16];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut id)?;
    Ok(id)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Result<[u8; 32], String> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| format!("HMAC 初始化失败: {e}"))?;
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag[..]);
    Ok(out)
}

// ---------------------------------------------------------------- X11 / UI

struct Ui {
    conn: RustConnection,
    window: x11rb::protocol::xproto::Window,
    gc: x11rb::protocol::xproto::Gcontext,
    win_w: u16,
    win_h: u16,
    view: Vec<u8>,
    keymap: HashMap<u8, (u16, bool)>,
    mapped: (u32, u32, u16, u16),
    xmap: Vec<u32>,
    shown_path: &'static str,
    wm_protocols: u32,
    wm_delete: u32,
}

impl Ui {
    fn rebuild_xmap(&mut self, canvas_w: u32, canvas_h: u32, win_w: u16, win_h: u16) {
        if win_w == 0 || win_h == 0 {
            return;
        }
        self.xmap.clear();
        self.xmap.reserve(usize::from(win_w));
        for x in 0..u32::from(win_w) {
            let sx = (u64::from(x) * u64::from(canvas_w) / u64::from(win_w)) as u32;
            self.xmap.push(sx.min(canvas_w.saturating_sub(1)));
        }
        self.mapped = (canvas_w, canvas_h, win_w, win_h);
    }

    /// 脏帧：画布 → 窗口缓冲 → put_image。
    fn render_if_dirty(&mut self, shared: &Arc<Shared>) -> Result<bool, String> {
        let started = Instant::now();
        let rendered = {
            let mut canvas = lock(&shared.canvas);
            if !canvas.dirty || canvas.width == 0 || canvas.height == 0 {
                false
            } else {
                let (cw, ch) = (canvas.width, canvas.height);
                let need = usize::from(self.win_w) * usize::from(self.win_h) * 4;
                if self.view.len() != need {
                    self.view = vec![0u8; need];
                }
                if self.mapped.0 != cw
                    || self.mapped.1 != ch
                    || self.mapped.2 != self.win_w
                    || self.mapped.3 != self.win_h
                {
                    self.rebuild_xmap(cw, ch, self.win_w, self.win_h);
                }
                if self.xmap.len() != usize::from(self.win_w) {
                    self.rebuild_xmap(cw, ch, self.win_w, self.win_h);
                }
                let stride = cw as usize * 4;
                let win_w = usize::from(self.win_w);
                let win_h = usize::from(self.win_h);
                for y in 0..win_h {
                    let sy = (y as u64 * u64::from(ch) / u64::from(self.win_h.max(1))) as usize;
                    let sy = sy.min(ch as usize - 1);
                    let src_row = &canvas.bgra[sy * stride..];
                    let dst_row = &mut self.view[y * win_w * 4..(y + 1) * win_w * 4];
                    for x in 0..win_w {
                        let s = self.xmap[x] as usize * 4;
                        let d = x * 4;
                        dst_row[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
                    }
                }
                canvas.dirty = false;
                true
            }
        };
        if !rendered {
            return Ok(false);
        }
        self.put_view()?;
        shared
            .render_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        shared.render_n.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    fn put_view(&mut self) -> Result<(), String> {
        if self.win_w == 0 || self.win_h == 0 {
            return Ok(());
        }
        self.conn
            .put_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                self.gc,
                self.win_w,
                self.win_h,
                0,
                0,
                0,
                24,
                &self.view,
            )
            .map_err(|e| format!("put_image: {e}"))?;
        self.conn.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    fn draw_background(&mut self) -> Result<(), String> {
        for b in self.view.iter_mut() {
            *b = 0;
        }
        self.put_view()
    }

    fn set_window_size(&mut self, w: u16, h: u16) {
        if w == 0 || h == 0 {
            return;
        }
        self.win_w = w;
        self.win_h = h;
        self.view = vec![0u8; usize::from(w) * usize::from(h) * 4];
        self.mapped = (0, 0, 0, 0);
    }
}

fn run_ui(cfg: Config, shared: Arc<Shared>) -> Result<(), String> {
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| format!("X11 连接失败（DISPLAY 设了吗？）: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let depth = screen.root_depth;
    let visual = screen.root_visual;

    let window = conn.generate_id().map_err(|e| format!("generate_id: {e}"))?;
    let event_mask = EventMask::EXPOSURE
        | EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::FOCUS_CHANGE;
    let aux = CreateWindowAux::new()
        .background_pixel(screen.black_pixel)
        .event_mask(event_mask);
    conn.create_window(
        depth,
        window,
        root,
        0,
        0,
        cfg.width as u16,
        cfg.height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &aux,
    )
    .map_err(|e| format!("create_window: {e}"))?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        cfg.title.as_bytes(),
    )
    .map_err(|e| format!("WM_NAME: {e}"))?;
    let wm_protocols = conn
        .intern_atom(false, b"WM_PROTOCOLS")
        .map_err(|e| format!("intern WM_PROTOCOLS: {e}"))?
        .reply()
        .map_err(|e| format!("intern WM_PROTOCOLS reply: {e}"))?
        .atom;
    let wm_delete = conn
        .intern_atom(false, b"WM_DELETE_WINDOW")
        .map_err(|e| format!("intern WM_DELETE_WINDOW: {e}"))?
        .reply()
        .map_err(|e| format!("intern WM_DELETE_WINDOW reply: {e}"))?
        .atom;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete],
    )
    .map_err(|e| format!("WM_PROTOCOLS: {e}"))?;
    conn.map_window(window)
        .map_err(|e| format!("map_window: {e}"))?;
    let gc = conn.generate_id().map_err(|e| format!("gc id: {e}"))?;
    conn.create_gc(gc, window, &CreateGCAux::new())
        .map_err(|e| format!("create_gc: {e}"))?;
    conn.flush().map_err(|e| format!("flush: {e}"))?;

    let keymap = build_keymap(&conn)?;
    log::info!("键盘映射就绪：{} 个 keycode", keymap.len());

    let mut ui = Ui {
        conn,
        window,
        gc,
        win_w: cfg.width as u16,
        win_h: cfg.height as u16,
        view: vec![0u8; cfg.width as usize * cfg.height as usize * 4],
        keymap,
        mapped: (0, 0, 0, 0),
        xmap: Vec::new(),
        shown_path: "tcp",
        wm_protocols,
        wm_delete,
    };
    ui.draw_background()?;
    run_loop(&mut ui, &cfg, &shared)
}

fn run_loop(ui: &mut Ui, cfg: &Config, shared: &Arc<Shared>) -> Result<(), String> {
    let mut last_stats = Instant::now();
    let mut stat_frames = 0u64;
    let mut stat_blocks = 0u64;
    let mut stat_video = 0u64;
    let mut stat_bytes = 0u64;
    let mut snapshot_done = false;
    let mut first_render: Option<Instant> = None;
    let mut resized = false;

    while !shared.quit.load(Ordering::Relaxed) {
        loop {
            let event = match ui.conn.poll_for_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => return Err(format!("poll_for_event: {e}")),
            };
            if handle_event(ui, &event, shared)? {
                shared.quit.store(true, Ordering::Relaxed);
            }
        }

        // 远端尺寸已知后按宽高比调一次窗口
        if !resized {
            if let Some((rw, rh)) = shared.remote_size() {
                if rw > 0 && rh > 0 {
                    let want_h = (u32::from(cfg.width) * rh / rw).max(1) as u16;
                    if want_h != ui.win_h {
                        let aux = ConfigureWindowAux::new()
                            .width(u32::from(cfg.width))
                            .height(u32::from(want_h));
                        let _ = ui.conn.configure_window(ui.window, &aux);
                        let _ = ui.conn.flush();
                        ui.set_window_size(cfg.width as u16, want_h);
                    }
                    resized = true;
                    log::info!("窗口按远端比例调整为 {}x{}", cfg.width, want_h);
                }
            }
        }

        // 脏帧渲染
        if ui.render_if_dirty(shared)? {
            let first = *first_render.get_or_insert_with(Instant::now);
            // 整帧是分多条 VideoFrame 发的，第一块到手就截图会只截到一条；
            // 等 1.5 秒（拿到完整画面）再写快照。
            if let Some(path) = cfg.snapshot.as_deref() {
                if !snapshot_done && first.elapsed() >= Duration::from_millis(1500) {
                    snapshot_done = true;
                    match write_ppm(path, &ui.view, ui.win_w, ui.win_h) {
                        Ok(()) => log::info!("窗口内容已写到 {path}"),
                        Err(e) => log::warn!("写快照失败: {e}"),
                    }
                }
            }
        }

        // 标题里显示当前路径
        let path = shared.path();
        if path != ui.shown_path {
            ui.shown_path = path;
            let title = format!("{} — {}", cfg.title, path);
            let _ = ui.conn.change_property8(
                PropMode::REPLACE,
                ui.window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                title.as_bytes(),
            );
            let _ = ui.conn.flush();
        }

        // 每秒 stats
        if cfg.stats && last_stats.elapsed() >= Duration::from_secs(1) {
            let frames = shared.frames.load(Ordering::Relaxed);
            let blocks = shared.decoded_blocks.load(Ordering::Relaxed);
            let video = shared.video_frames.load(Ordering::Relaxed);
            let bytes = shared.recv_bytes.load(Ordering::Relaxed);
            let dt = last_stats.elapsed().as_secs_f64();
            let dframes = frames.saturating_sub(stat_frames);
            let dblocks = blocks.saturating_sub(stat_blocks);
            let dvideo = video.saturating_sub(stat_video);
            let dbytes = bytes.saturating_sub(stat_bytes);
            let decode_n = shared.decode_n.swap(0, Ordering::Relaxed);
            let decode_us = shared.decode_us.swap(0, Ordering::Relaxed);
            let render_n = shared.render_n.swap(0, Ordering::Relaxed);
            let render_us = shared.render_us.swap(0, Ordering::Relaxed);
            let decode_ms = if decode_n > 0 {
                decode_us as f64 / decode_n as f64 / 1000.0
            } else {
                0.0
            };
            let render_ms = if render_n > 0 {
                render_us as f64 / render_n as f64 / 1000.0
            } else {
                0.0
            };
            let mode = if shared.video_mode.load(Ordering::Relaxed) {
                "video"
            } else {
                "blocks"
            };
            println!(
                "fps={:.0} recv_kbps={:.1} decoded_blocks={} mode={} video_fps={:.1} rtt_ms={:.2} path={} udp_rtt={:.1} udp_loss={} window={}x{} decode_ms={:.2} render_ms={:.2} reconnects={}",
                dframes as f64 / dt,
                dbytes as f64 * 8.0 / 1000.0 / dt,
                dblocks,
                mode,
                dvideo as f64 / dt,
                shared.rtt_micros.load(Ordering::Relaxed) as f64 / 1000.0,
                shared.path(),
                shared.udp_rtt_x10.load(Ordering::Relaxed) as f64 / 10.0,
                shared.udp_loss.load(Ordering::Relaxed),
                ui.win_w,
                ui.win_h,
                decode_ms,
                render_ms,
                shared.reconnect_count.load(Ordering::Relaxed)
            );
            let _ = std::io::stdout().flush();
            last_stats = Instant::now();
            stat_frames = frames;
            stat_blocks = blocks;
            stat_video = video;
            stat_bytes = bytes;
        }

        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// 处理一个 X11 事件；返回 true 表示要关窗口。
fn handle_event(ui: &mut Ui, event: &Event, shared: &Arc<Shared>) -> Result<bool, String> {
    match event {
        Event::Expose(ev) => {
            if ev.count == 0 {
                ui.put_view()?;
            }
        }
        Event::ConfigureNotify(ev) => {
            if ev.width != ui.win_w || ev.height != ui.win_h {
                ui.set_window_size(ev.width, ev.height);
                ui.conn
                    .put_image(
                        ImageFormat::Z_PIXMAP,
                        ui.window,
                        ui.gc,
                        ev.width,
                        ev.height,
                        0,
                        0,
                        0,
                        24,
                        &ui.view,
                    )
                    .map_err(|e| format!("put_image(configure): {e}"))?;
                ui.conn.flush().map_err(|e| format!("flush: {e}"))?;
            }
        }
        Event::KeyPress(ev) => {
            send_key(ui, shared, ev.detail, true)?;
        }
        Event::KeyRelease(ev) => {
            send_key(ui, shared, ev.detail, false)?;
        }
        Event::ButtonPress(ev) => {
            send_button(ui, shared, ev.detail, ev.event_x, ev.event_y, true)?;
        }
        Event::ButtonRelease(ev) => {
            send_button(ui, shared, ev.detail, ev.event_x, ev.event_y, false)?;
        }
        Event::MotionNotify(ev) => {
            let (rx, ry) = map_pointer(ui, shared, i32::from(ev.event_x), i32::from(ev.event_y));
            shared.send(Message::Input(InputEvent::MouseMove { x: rx, y: ry }).encode());
        }
        Event::ClientMessage(ev) => {
            if ev.type_ == ui.wm_protocols && ev.data.as_data32()[0] == ui.wm_delete {
                log::info!("窗口关闭请求");
                return Ok(true);
            }
        }
        _ => {}
    }
    Ok(false)
}

/// 窗口坐标 → 被控端像素（等比映射 + clamp）。
fn map_pointer(ui: &Ui, shared: &Arc<Shared>, x: i32, y: i32) -> (i32, i32) {
    let Some((rw, rh)) = shared.remote_size() else {
        return (0, 0);
    };
    let ww = i64::from(ui.win_w.max(1));
    let wh = i64::from(ui.win_h.max(1));
    let rx = (i64::from(x) * i64::from(rw) / ww).clamp(0, i64::from(rw.saturating_sub(1)));
    let ry = (i64::from(y) * i64::from(rh) / wh).clamp(0, i64::from(rh.saturating_sub(1)));
    (rx as i32, ry as i32)
}

fn send_key(ui: &mut Ui, shared: &Arc<Shared>, keycode: u8, down: bool) -> Result<bool, String> {
    let Some((scancode, extended)) = ui.keymap.get(&keycode).copied() else {
        log::debug!("未知 keycode={keycode}，忽略");
        return Ok(false);
    };
    shared.send(
        Message::Input(InputEvent::Key {
            scancode,
            down,
            extended,
        })
        .encode(),
    );
    Ok(false)
}

fn send_button(
    ui: &mut Ui,
    shared: &Arc<Shared>,
    button: u8,
    x: i16,
    y: i16,
    down: bool,
) -> Result<bool, String> {
    let (rx, ry) = map_pointer(ui, shared, i32::from(x), i32::from(y));
    match button {
        1 | 2 | 3 => {
            // proto：LEFT=0 / RIGHT=1 / MIDDLE=2
            let proto_button = match button {
                1 => 0,
                3 => 1,
                _ => 2,
            };
            shared.send(
                Message::Input(InputEvent::MouseButton {
                    button: proto_button,
                    down,
                    x: rx,
                    y: ry,
                })
                .encode(),
            );
        }
        4 | 5 => {
            if down {
                let dy = if button == 4 { 1 } else { -1 };
                shared.send(
                    Message::Input(InputEvent::Wheel { dx: 0, dy, x: rx, y: ry }).encode(),
                );
            }
        }
        6 | 7 => {
            if down {
                let dx = if button == 7 { 1 } else { -1 };
                shared.send(
                    Message::Input(InputEvent::Wheel { dx, dy: 0, x: rx, y: ry }).encode(),
                );
            }
        }
        other => log::debug!("忽略鼠标按键 {other}"),
    }
    Ok(false)
}

/// keycode → (scancode, extended)：keysym 查表，查不到退回 keycode-8。
fn build_keymap(conn: &RustConnection) -> Result<HashMap<u8, (u16, bool)>, String> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
    if count == 0 {
        return Ok(HashMap::new());
    }
    let reply = conn
        .get_keyboard_mapping(first, count)
        .map_err(|e| format!("get_keyboard_mapping: {e}"))?
        .reply()
        .map_err(|e| format!("get_keyboard_mapping reply: {e}"))?;
    let per = usize::from(reply.keysyms_per_keycode.max(1));
    let mut map = HashMap::new();
    for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
        let keycode = first.wrapping_add(i as u8);
        let Some(keysym) = chunk.iter().copied().find(|k| *k != 0) else {
            continue;
        };
        if let Some(mapped) = keysym_to_scancode(keysym) {
            map.insert(keycode, mapped);
        } else if (8..=0x58).contains(&keycode) {
            map.insert(keycode, (u16::from(keycode) - 8, false));
        }
    }
    Ok(map)
}

/// X11 keysym → PC（set 1）scancode + 是否 extended。
fn keysym_to_scancode(keysym: u32) -> Option<(u16, bool)> {
    if let Ok(ch) = u8::try_from(keysym) {
        if let Some(sc) = ascii_scancode(ch) {
            return Some((sc, false));
        }
    }
    let mapped = match keysym {
        0xff08 => (0x0E_u16, false),
        0xff09 => (0x0F, false),
        0xff0d => (0x1C, false),
        0xff1b => (0x01, false),
        0xffff => (0x53, true),
        0xff50 => (0x47, true),
        0xff51 => (0x4B, true),
        0xff52 => (0x48, true),
        0xff53 => (0x4D, true),
        0xff54 => (0x50, true),
        0xff55 => (0x49, true),
        0xff56 => (0x51, true),
        0xff57 => (0x4F, true),
        0xff63 => (0x52, true),
        0xffe1 => (0x2A, false),
        0xffe2 => (0x36, false),
        0xffe3 => (0x1D, false),
        0xffe4 => (0x1D, true),
        0xffe5 => (0x3A, false),
        0xffe9 => (0x38, false),
        0xffea => (0x38, true),
        0xffeb => (0x5B, true),
        0xffec => (0x5C, true),
        0xff67 => (0x5D, true),
        0xff7f => (0x45, false),
        0xff14 => (0x46, false),
        0xff8d => (0x1C, true),
        _ => {
            if (0xffbe..=0xffc9).contains(&keysym) {
                let idx = keysym - 0xffbe;
                return Some((0x3B + idx as u16, false));
            }
            return None;
        }
    };
    Some(mapped)
}

/// 可打印 ASCII → PC scancode（shifted 字符落到同一物理键）。
fn ascii_scancode(ch: u8) -> Option<u16> {
    let c = ch.to_ascii_uppercase();
    let sc = match c {
        b'Q' => 0x10,
        b'W' => 0x11,
        b'E' => 0x12,
        b'R' => 0x13,
        b'T' => 0x14,
        b'Y' => 0x15,
        b'U' => 0x16,
        b'I' => 0x17,
        b'O' => 0x18,
        b'P' => 0x19,
        b'A' => 0x1E,
        b'S' => 0x1F,
        b'D' => 0x20,
        b'F' => 0x21,
        b'G' => 0x22,
        b'H' => 0x23,
        b'J' => 0x24,
        b'K' => 0x25,
        b'L' => 0x26,
        b'Z' => 0x2C,
        b'X' => 0x2D,
        b'C' => 0x2E,
        b'V' => 0x2F,
        b'B' => 0x30,
        b'N' => 0x31,
        b'M' => 0x32,
        b'1' | b'!' => 0x02,
        b'2' | b'@' => 0x03,
        b'3' | b'#' => 0x04,
        b'4' | b'$' => 0x05,
        b'5' | b'%' => 0x06,
        b'6' | b'^' => 0x07,
        b'7' | b'&' => 0x08,
        b'8' | b'*' => 0x09,
        b'9' | b'(' => 0x0A,
        b'0' | b')' => 0x0B,
        b'-' | b'_' => 0x0C,
        b'=' | b'+' => 0x0D,
        b'[' | b'{' => 0x1A,
        b']' | b'}' => 0x1B,
        b';' | b':' => 0x27,
        0x27 | 0x22 => 0x28,
        0x60 | 0x7E => 0x29,
        0x5C | 0x7C => 0x2B,
        b',' | b'<' => 0x33,
        b'.' | b'>' => 0x34,
        b'/' | b'?' => 0x35,
        b' ' => 0x39,
        0x09 => 0x0F,
        0x0A | 0x0D => 0x1C,

        _ => return None,
    };
    Some(sc)
}

/// 把窗口缓冲（BGRA）写成 PPM(P6)，方便验收时拉回来看。
fn write_ppm(path: &str, view: &[u8], w: u16, h: u16) -> Result<(), String> {
    let mut out = Vec::with_capacity(usize::from(w) * usize::from(h) * 3 + 32);
    out.extend_from_slice(b"P6\n");
    out.extend_from_slice(format!("{w} {h}\n255\n").as_bytes());
    for px in view.chunks_exact(4) {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
    }
    let mut f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    f.write_all(&out).map_err(|e| format!("写 {path} 失败: {e}"))
}

// ---------------------------------------------------------------- main

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("lrc-client-linux: {e}");
            print!("{USAGE}");
            std::process::exit(2);
        }
    };
    log::info!(
        "启动：connect={:?} signal={:?} room={} p2p={:?} stats={}",
        cfg.connect,
        cfg.signal,
        cfg.room,
        cfg.p2p,
        cfg.stats
    );
    let shared = Arc::new(Shared::new(cfg.title.clone()));

    let net_cfg = Config {
        password: cfg.password.clone(),
        connect: cfg.connect.clone(),
        signal: cfg.signal.clone(),
        room: cfg.room,
        p2p: cfg.p2p,
        title: cfg.title.clone(),
        stats: cfg.stats,
        snapshot: None,
        width: cfg.width,
        height: cfg.height,
    };
    let net_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name(String::from("lrc-client-net"))
        .spawn(move || supervise(net_cfg, net_shared));
    if let Err(e) = spawned {
        eprintln!("lrc-client-linux: 网络线程启动失败: {e}");
        std::process::exit(1);
    }

    if let Err(e) = run_ui(cfg, shared.clone()) {
        log::error!("界面退出：{e}");
        shared.quit.store(true, Ordering::Relaxed);
        std::process::exit(1);
    }
    shared.quit.store(true, Ordering::Relaxed);
    log::info!("窗口已关闭，退出");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_order_prefers_previous_path() {
        assert_eq!(attempt_order(false, PathKind::Udp), vec![PathKind::Tcp]);
        assert_eq!(
            attempt_order(true, PathKind::Udp),
            vec![PathKind::Udp, PathKind::Tcp]
        );
        assert_eq!(
            attempt_order(true, PathKind::Tcp),
            vec![PathKind::Tcp, PathKind::Udp]
        );
    }

    #[test]
    fn keymap_maps_common_keys() {
        assert_eq!(ascii_scancode(b'q'), Some(0x10));
        assert_eq!(ascii_scancode(b'A'), Some(0x1E));
        assert_eq!(ascii_scancode(b'1'), Some(0x02));
        assert_eq!(ascii_scancode(b' '), Some(0x39));
        assert_eq!(keysym_to_scancode(u32::from(b'z')), Some((0x2C, false)));
        assert_eq!(keysym_to_scancode(0xff51), Some((0x4B, true)));
        assert_eq!(keysym_to_scancode(0xffbe), Some((0x3B, false)));
        assert_eq!(keysym_to_scancode(0xffe3), Some((0x1D, false)));
        assert!(keysym_to_scancode(0x1234_5678).is_none());
    }

    #[test]
    fn block_rect_clips_edges() {
        let grid = Grid {
            block_size: 64,
            blocks_x: 30,
            blocks_y: 17,
        };
        let r = block_rect(&grid, 0, 1920, 1080).expect("rect");
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 64, 64));
        let idx = 16 * 30 + 29;
        let r = block_rect(&grid, idx, 1920, 1080).expect("rect");
        assert_eq!((r.x, r.y, r.w, r.h), (1856, 1024, 64, 56));
        assert!(block_rect(&grid, 17 * 30, 1920, 1080).is_none());
    }
}









