//! 接收线程：TCP 连接 + HMAC 握手 + 收消息 → 逐块解码成脏块。
//!
//! 线程分工：
//! - 接收线程（本模块）只读 socket，解块后把像素塞进共享队列；
//! - 发送线程持有写半边，主线程/接收线程通过 mpsc 投递已编码 payload；
//! - 呈现只在窗口线程做（D3D11 设备非线程安全）。

use std::io;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use lrc_codec as codec;
use lrc_proto::{
    auth_reason, codec_tag, ClientHello, DisplayInfo, Message, Pong, VideoAu, VideoFrame,
    PROTO_VERSION,
};
use crate::audio::AudioSink;
use lrc_transport::{Conn, ConnReader, ConnWriter};
use lrc_video::{DecoderConfig, FfmpegDecoder, VideoDecoder};
use sha2::Sha256;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::cli::Config;
use crate::state::{
    backoff_delay, host_clock_ms, BlockUpdate, ConnStatus, CursorShapeOwned, Shared,
    VideoFrameUpdate,
};

type HmacSha256 = Hmac<Sha256>;

/// 会话结束原因（供重连逻辑判断）。
enum SessionEnd {
    /// 窗口关闭，接收线程直接退出
    Quit,
    /// 连接断了（可以重连）
    Disconnected(String),
}

/// 握手失败：认证/版本问题不重试，IO/时序问题可以重连。
enum HandshakeError {
    /// 认证失败、协议版本不匹配等：再试也没用
    Terminal(String),
    /// 网络类问题：退避后重连
    Transient(String),
}

/// 启动接收线程：连接 → 握手 → 收流；断线自动退避重连，直到窗口关闭。
pub fn spawn(cfg: &Config, shared: Arc<Shared>) -> io::Result<()> {
    let addr = cfg.connect.clone();
    let password = cfg.password.clone();
    let audio_on = cfg.audio;
    let audio_dump = cfg.audio_dump.clone();
    std::thread::Builder::new()
        .name(String::from("lrc-client-net"))
        .spawn(move || supervise(&addr, &password, audio_on, audio_dump, &shared))?;
    Ok(())
}

/// 连接监管循环：断线不退出，指数退避重连；认证失败才终止（窗口保留）。
fn supervise(
    addr: &str,
    password: &str,
    audio_on: bool,
    audio_dump: Option<String>,
    shared: &Arc<Shared>,
) {
    let mut attempt: u32 = 0;
    let mut ever_connected = false;
    // 音频线程跟进程同寿命（跨重连保留设备）；--audio off 时是 None
    let mut audio = AudioSink::start(audio_on, shared.clone(), audio_dump);
    loop {
        if shared.is_quit() {
            return;
        }

        // ---- 1) 建连 ----
        let conn = match Conn::connect(addr) {
            Ok(conn) => conn,
            Err(e) => {
                attempt = attempt.saturating_add(1);
                shared.set_status(ConnStatus::Reconnecting { attempt });
                let delay = backoff_delay(attempt);
                log::warn!("连接 {addr} 失败（第 {attempt} 次）: {e}；{}s 后重试", delay.as_secs());
                if !sleep_backoff(shared, attempt) {
                    return;
                }
                continue;
            }
        };

        // ---- 2) 握手 ----
        match handshake(conn, password, shared) {
            Ok((writer, mut reader)) => {
                if ever_connected {
                    shared.bump_reconnects();
                }
                ever_connected = true;
                attempt = 0;
                let (tx, rx) = channel::<Vec<u8>>();
                spawn_writer(writer, rx, shared.clone());
                // 新会话：清掉上一会话的脏块/视频帧/解码器状态（解码器是 VideoRx 的局部变量，随会话结束 drop）
                shared.clear_session_state();
                shared.set_out_tx(Some(tx.clone()));
                shared.set_connected(true);
                shared.set_status(ConnStatus::Connected);
                log::info!("已连接 {addr}，开始接收");
                // 首帧要整屏；被控端收到会回一整帧
                send(&tx, Message::RequestFull.encode());

                if let Some(sink) = audio.as_mut() {
                    sink.reset();
                }
                let end = session_loop(&mut reader, shared, &tx, &mut audio);
                shared.set_out_tx(None);
                shared.set_connected(false);
                shared.clear_session_state();
                drop(tx);

                match end {
                    SessionEnd::Quit => return,
                    SessionEnd::Disconnected(reason) => {
                        if shared.is_quit() {
                            return;
                        }
                        attempt = attempt.saturating_add(1);
                        shared.set_status(ConnStatus::Reconnecting { attempt });
                        let delay = backoff_delay(attempt);
                        log::warn!(
                            "连接断开（{reason}）；第 {attempt} 次重连，{}s 后重试",
                            delay.as_secs()
                        );
                        if !sleep_backoff(shared, attempt) {
                            return;
                        }
                    }
                }
            }
            Err(HandshakeError::Terminal(message)) => {
                log::error!("{message}（不再重连）");
                shared.set_out_tx(None);
                shared.set_connected(false);
                shared.clear_session_state();
                shared.set_status(ConnStatus::Terminal { message });
                return;
            }
            Err(HandshakeError::Transient(e)) => {
                attempt = attempt.saturating_add(1);
                shared.set_status(ConnStatus::Reconnecting { attempt });
                let delay = backoff_delay(attempt);
                log::warn!("握手失败（第 {attempt} 次）: {e}；{}s 后重试", delay.as_secs());
                if !sleep_backoff(shared, attempt) {
                    return;
                }
            }
        }
    }
}

/// 退避等待；窗口关闭时立刻返回 false。
fn sleep_backoff(shared: &Shared, attempt: u32) -> bool {
    let mut left = backoff_delay(attempt);
    while !left.is_zero() {
        if shared.is_quit() {
            return false;
        }
        let step = left.min(Duration::from_millis(100));
        std::thread::sleep(step);
        left -= step;
    }
    !shared.is_quit()
}

/// 握手：ClientHello → ServerHello → AuthRequest → AuthResult；成功后拆成读写两半。
fn handshake(
    mut conn: Conn,
    password: &str,
    shared: &Shared,
) -> Result<(ConnWriter, ConnReader), HandshakeError> {
    let client_id = random_id().map_err(|e| HandshakeError::Transient(format!("{e:#}")))?;
    let nonce_c = random_id().map_err(|e| HandshakeError::Transient(format!("{e:#}")))?;
    let hello = Message::ClientHello(ClientHello {
        version: PROTO_VERSION,
        client_id,
        nonce_c,
    });
    conn.send(&hello.encode())
        .map_err(|e| HandshakeError::Transient(format!("发 ClientHello 失败: {e}")))?;

    let mut buf = Vec::with_capacity(4096);
    let server = match recv_msg(&mut conn, &mut buf) {
        Ok(Some(Message::ServerHello(hello))) => hello,
        Ok(Some(other)) => {
            return Err(HandshakeError::Transient(format!(
                "期望 ServerHello，收到消息类型 {:#x}",
                other.msg_type()
            )))
        }
        Ok(None) => {
            return Err(HandshakeError::Transient(String::from(
                "对端在 ServerHello 之前关闭连接",
            )))
        }
        Err(e) => return Err(HandshakeError::Transient(format!("读 ServerHello 失败: {e}"))),
    };
    if server.version != PROTO_VERSION {
        return Err(HandshakeError::Terminal(format!(
            "协议版本不匹配：对端 v{}，本地 v{}",
            server.version, PROTO_VERSION
        )));
    }
    shared.set_remote_size(server.width, server.height);
    log::info!(
        "ServerHello: {}x{} rotation={} 协议 v{}",
        server.width,
        server.height,
        server.rotation,
        server.version
    );

    // mac = HMAC-SHA256(key = password, msg = nonce_c || nonce_s)
    let mut mac_input = [0u8; 32];
    mac_input[..16].copy_from_slice(&nonce_c);
    mac_input[16..].copy_from_slice(&server.nonce_s);
    let mac = hmac_sha256(password.as_bytes(), &mac_input)
        .map_err(|e| HandshakeError::Transient(format!("{e:#}")))?;
    conn.send(&Message::AuthRequest(lrc_proto::AuthRequest { mac }).encode())
        .map_err(|e| HandshakeError::Transient(format!("发 AuthRequest 失败: {e}")))?;

    match recv_msg(&mut conn, &mut buf) {
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
        Err(e) => return Err(HandshakeError::Transient(format!("读 AuthResult 失败: {e}"))),
    }
    log::info!("认证通过");

    conn.split()
        .map_err(|e| HandshakeError::Transient(format!("split 失败: {e}")))
}

/// 会话循环：收消息 → 解码。连接断了返回 Disconnected（由上层重连），窗口关了返回 Quit。
fn session_loop(
    reader: &mut ConnReader,
    shared: &Arc<Shared>,
    tx: &Sender<Vec<u8>>,
    audio: &mut Option<AudioSink>,
) -> SessionEnd {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    // 解码器跟着会话走：断线时随 VideoRx drop 掉，重连后重新建（不串台）
    let mut video = VideoRx::new();
    loop {
        if shared.is_quit() {
            return SessionEnd::Quit;
        }
        let n = match reader.recv(&mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => {
                if shared.is_quit() {
                    return SessionEnd::Quit;
                }
                return SessionEnd::Disconnected(String::from("对端关闭了连接"));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if shared.is_quit() {
                    return SessionEnd::Quit;
                }
                return SessionEnd::Disconnected(format!("收包失败: {e}"));
            }
        };
        shared.add_recv_bytes(n as u64);

        match Message::decode(&buf) {
            Ok(Message::VideoFrame(frame)) => {
                if shared.is_video_mode() {
                    log::info!("从视频模式切回块模式");
                }
                shared.set_mode_block();
                if frame.is_full() {
                    // 整帧：窗口线程要用它重建 clean，清掉视频帧/光标残影
                    shared.mark_block_full_reset();
                }
                handle_frame(&frame, shared);
            }
            Ok(Message::VideoAu(au)) => video.handle_au(&au, shared, tx),
            Ok(Message::AudioFrame(af)) => {
                if let Some(sink) = audio.as_mut() {
                    sink.push_frame(&af);
                }
            }
            Ok(Message::DisplayList(list)) => {
                let displays: Vec<DisplayInfo> = list.iter().copied().collect();
                let summary: Vec<String> = displays
                    .iter()
                    .map(|d| {
                        format!(
                            "#{} {}x{}{}",
                            d.index,
                            d.width,
                            d.height,
                            if d.primary { "(primary)" } else { "" }
                        )
                    })
                    .collect();
                log::info!("可用显示输出 {} 个：{}", displays.len(), summary.join("，"));
                shared.set_displays(displays);
            }
            Ok(Message::SelectDisplay(sel)) => {
                log::debug!("忽略对端发来的 SelectDisplay #{}", sel.index);
            }
            Ok(Message::Ping(ping)) => {
                send(tx, Message::Pong(Pong { ts_ms: ping.ts_ms }).encode());
            }
            Ok(Message::Pong(pong)) => {
                if let Some(micros) = shared.finish_ping(pong.ts_ms) {
                    log::trace!("RTT {:.3} ms", micros as f64 / 1000.0);
                }
            }
            Ok(Message::CursorPos(pos)) => {
                // 坐标是**被控端帧像素**（与块坐标同一坐标系），y 轴向下
                shared.set_cursor_pos(i32::from(pos.x), i32::from(pos.y), pos.visible);
                log::trace!("CursorPos {pos:?}");
            }
            Ok(Message::CursorShape(shape)) => {
                // 线上字段叫 rgba，实际是 BGRA（alpha 在第 4 字节）
                let owned = if shape.width == 0 || shape.height == 0 || shape.rgba.is_empty() {
                    None
                } else {
                    Some(CursorShapeOwned {
                        width: u32::from(shape.width),
                        height: u32::from(shape.height),
                        hotspot_x: i32::from(shape.hotspot_x),
                        hotspot_y: i32::from(shape.hotspot_y),
                        bgra: shape.rgba.to_vec(),
                    })
                };
                match &owned {
                    Some(s) => log::info!(
                        "光标位图 {}x{} 热点=({},{})",
                        s.width,
                        s.height,
                        s.hotspot_x,
                        s.hotspot_y
                    ),
                    None => log::info!("光标位图被清空（隐藏）"),
                }
                shared.set_cursor_shape(owned);
            }
            Ok(Message::ServerHello(hello)) => {
                if shared.set_remote_size(hello.width, hello.height) {
                    log::info!("被控端尺寸变化: {}x{}", hello.width, hello.height);
                    send(tx, Message::RequestFull.encode());
                }
            }
            Ok(Message::AuthResult(result)) => {
                log::debug!("重复 AuthResult ok={} reason={}", result.ok, result.reason);
            }
            Ok(other) => log::debug!("忽略消息类型 {:#x}", other.msg_type()),
            Err(e) => log::warn!("消息解码失败，丢弃: {e}"),
        }
    }
}

/// 逐块解码一帧，把成功解出的块塞进共享队列。
/// 视频通道接收侧状态（只被接收线程使用）。
struct VideoRx {
    /// (codec_tag, 解码器)
    decoder: Option<(u8, FfmpegDecoder)>,
    /// 是否已经解出过关键帧：没拿到 IDR 之前收到的 P 帧全丢
    seen_key: bool,
    /// 连续解码失败次数：连坏几次就把解码器扔掉，等下一帧 IDR 重建
    consecutive_errors: u32,
    /// 上次发 RequestFull 的时刻（1 秒最多一次，避免刷屏）
    last_full_request: Option<Instant>,
}

impl VideoRx {
    fn new() -> Self {
        Self {
            decoder: None,
            seen_key: false,
            consecutive_errors: 0,
            last_full_request: None,
        }
    }

    /// 限流版 RequestFull（host 收到会立刻出 IDR）。
    fn request_full(&mut self, tx: &Sender<Vec<u8>>, why: &str) {
        let now = Instant::now();
        if let Some(last) = self.last_full_request {
            if now.duration_since(last) < Duration::from_secs(1) {
                return;
            }
        }
        self.last_full_request = Some(now);
        log::info!("请求整帧（{why}）");
        send(tx, Message::RequestFull.encode());
    }

    /// 处理一个视频访问单元：解码 → 存最新帧；缺 IDR/解码失败 → 请求整帧。
    fn handle_au(&mut self, au: &VideoAu<'_>, shared: &Arc<Shared>, tx: &Sender<Vec<u8>>) {
        let Some(name) = codec_name(au.codec) else {
            log::warn!("未知视频码流 codec={}", au.codec);
            shared.bump_decode_error();
            self.request_full(tx, "未知视频 codec");
            return;
        };
        // 解码器要精确的宽高来定 frame_bytes；ServerHello 之后才有
        let Some((width, height)) = shared.remote_size() else {
            self.request_full(tx, "收到视频帧但还不知道被控端尺寸");
            return;
        };

        // 解码器按需建立；codec 变了且来了 IDR → 重建
        let need_decoder = match &self.decoder {
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
                Ok(decoder) => {
                    log::info!("视频解码器已建立：{} {}x{}", cfg.codec, width, height);
                    self.decoder = Some((au.codec, decoder));
                    self.seen_key = false;
                    self.consecutive_errors = 0;
                }
                Err(e) => {
                    log::error!("视频解码器创建失败: {e}");
                    shared.bump_decode_error();
                    self.request_full(tx, "视频解码器不可用");
                    return;
                }
            }
        }

        // 还没见过关键帧：P 帧解不出来，直接丢并要 IDR
        if !self.seen_key && !au.is_key() {
            log::debug!("视频 AU 非关键帧且还没见过 IDR（seq={}），丢弃", au.seq);
            self.request_full(tx, "缺少关键帧");
            return;
        }

        let Some((_, decoder)) = self.decoder.as_mut() else {
            return;
        };
        let started = Instant::now();
        let result = decoder.decode(au.data);
        let decode_ms = started.elapsed().as_secs_f64() * 1000.0;

        match result {
            Ok(Some(bgra)) => {
                let need = (width as usize)
                    .saturating_mul(height as usize)
                    .saturating_mul(4);
                if bgra.len() < need {
                    log::warn!("解码帧字节数不足: {} < {}", bgra.len(), need);
                    shared.bump_decode_error();
                    self.request_full(tx, "解码帧尺寸不符");
                    return;
                }
                if au.is_key() {
                    self.seen_key = true;
                }
                self.consecutive_errors = 0;
                // 管线延迟：ts_ms 用 GetTickCount64（系统开机毫秒）作基准，
                // 同机时 host/client 共享同一系统时钟 → 是真延迟；
                // 跨机时两端开机时间不同，这个数只作参考。
                let lat_ms = u64::from(host_clock_ms().wrapping_sub(au.ts_ms));
                shared.add_latency_ms(lat_ms);
                shared.video_frames.fetch_add(1, Ordering::Relaxed);
                shared.last_seq.store(au.seq, Ordering::Relaxed);
                shared.set_mode_video();
                // 只保留最新一帧：解码跟不上时旧的直接覆盖丢掉
                shared.put_video_frame(VideoFrameUpdate {
                    width,
                    height,
                    stride: width as usize * 4,
                    bgra,
                    ts_ms: au.ts_ms,
                    seq: au.seq,
                });
                log::trace!(
                    "视频帧 seq={} key={} 解码 {decode_ms:.1}ms lat={lat_ms}ms",
                    au.seq,
                    au.is_key()
                );
            }
            Ok(None) => log::debug!("视频 AU 没有产出画面（seq={}）", au.seq),
            Err(e) => {
                log::warn!(
                    "视频解码失败 seq={} key={}（{decode_ms:.1}ms）: {e}",
                    au.seq,
                    au.is_key()
                );
                shared.bump_decode_error();
                self.seen_key = false;
                self.consecutive_errors = self.consecutive_errors.saturating_add(1);
                if self.consecutive_errors >= 3 {
                    log::warn!(
                        "连续 {} 次解码失败，丢弃解码器等新的 IDR",
                        self.consecutive_errors
                    );
                    if let Some((_, mut decoder)) = self.decoder.take() {
                        decoder.shutdown();
                    }
                    self.consecutive_errors = 0;
                }
                self.request_full(tx, "视频解码失败");
            }
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

fn handle_frame(frame: &VideoFrame<'_>, shared: &Shared) {
    let Some(grid) = frame.grid() else {
        log::warn!("帧几何非法: block_size/blocks 为 0");
        return;
    };
    if frame.is_full() {
        shared.mark_first_full_frame();
        // 服务端降分辨率时 ServerHello 的宽高不再等于实际帧：以 FULL 帧网格为准
        if let Some(grid) = frame.grid() {
            if shared.reconcile_canvas_from_grid(grid) {
                log::info!(
                    "按 FULL 帧网格校正画布（grid={:?}）→ {:?}",
                    grid,
                    shared.remote_size()
                );
            }
        }
    }
    let (screen_w, screen_h) = match shared.remote_size() {
        Some(size) => size,
        None => {
            // 兜底：ServerHello 没给尺寸时按网格估一个（可能比真实尺寸大不到一个块）
            let w = u32::from(grid.blocks_x) * u32::from(grid.block_size);
            let h = u32::from(grid.blocks_y) * u32::from(grid.block_size);
            if w == 0 || h == 0 {
                log::warn!("帧尺寸未知，丢弃");
                return;
            }
            log::warn!("ServerHello 未提供尺寸，按网格估算 {w}x{h}");
            shared.set_remote_size(w, h);
            (w, h)
        }
    };

    let mut blocks: Vec<BlockUpdate> = Vec::with_capacity(frame.entries.len());
    let mut decoded = 0u64;
    for entry in &frame.entries {
        let Some(rect) = grid.block_rect(entry.idx, screen_w, screen_h) else {
            log::warn!("块下标越界 idx={} grid={:?}", entry.idx, grid);
            continue;
        };
        let Some(stride) = codec::row_bytes(rect.w) else {
            continue;
        };
        let Some(need) = stride.checked_mul(rect.h as usize) else {
            continue;
        };
        let mut data = vec![0u8; need];
        match codec::decode_block(entry.codec, entry.data, rect.w, rect.h, &mut data, stride) {
            Ok(()) => {
                blocks.push(BlockUpdate {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    data,
                });
                decoded = decoded.saturating_add(1);
            }
            Err(e) => log::warn!(
                "块解码失败 idx={} codec={} {}x{}: {e}",
                entry.idx,
                entry.codec,
                rect.w,
                rect.h
            ),
        }
    }

    shared
        .decoded_blocks
        .fetch_add(decoded, std::sync::atomic::Ordering::Relaxed);
    shared
        .frame_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared
        .last_seq
        .store(frame.seq, std::sync::atomic::Ordering::Relaxed);
    log::trace!(
        "帧 seq={} flags={:#x} 解出 {}/{} 块",
        frame.seq,
        frame.flags,
        decoded,
        frame.entries.len()
    );
    shared.push_blocks(blocks);
}

/// 发送线程：从 channel 取已编码 payload 写 socket，窗口关闭后自退。
fn spawn_writer(mut writer: ConnWriter, rx: Receiver<Vec<u8>>, shared: Arc<Shared>) {
    let spawned = std::thread::Builder::new()
        .name(String::from("lrc-client-send"))
        .spawn(move || {
            let _ = writer.set_write_timeout(Some(Duration::from_secs(5)));
            loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(payload) => {
                        if let Err(e) = writer.send(&payload) {
                            log::warn!("发送失败，结束发送线程: {e}");
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if shared.is_quit() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            // 关掉两个方向，让阻塞中的接收线程退出。
            writer.shutdown();
        });
    if let Err(e) = spawned {
        log::error!("发送线程启动失败: {e}");
    }
}

fn send(tx: &Sender<Vec<u8>>, payload: Vec<u8>) {
    // 发送线程退出（连接已断）时这里会失败，属于正常情况。
    let _ = tx.send(payload);
}

/// 阻塞读一条消息；`Ok(None)` 表示对端正常关闭。
fn recv_msg<'a>(conn: &mut Conn, buf: &'a mut Vec<u8>) -> anyhow::Result<Option<Message<'a>>> {
    match conn.recv(buf)? {
        Some(_) => Ok(Some(Message::decode(buf)?)),
        None => Ok(None),
    }
}

fn random_id() -> anyhow::Result<[u8; 16]> {
    let mut id = [0u8; 16];
    let status = unsafe { BCryptGenRandom(None, &mut id, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status
        .ok()
        .map_err(|e| anyhow::anyhow!("BCryptGenRandom 失败: {e}"))?;
    Ok(id)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("HMAC-SHA256 初始化失败: {e}"))?;
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag[..]);
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_test_case_2() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?").expect("hmac");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn auth_reason_text_covers_known_codes() {
        assert_eq!(auth_reason_text(auth_reason::BAD_PASSWORD), "密码错误");
        assert_eq!(auth_reason_text(auth_reason::RATE_LIMITED), "尝试过于频繁，被限流");
        assert_eq!(auth_reason_text(99), "未知原因");
    }

    #[test]
    fn random_id_is_not_constant() {
        let a = random_id().expect("random");
        let b = random_id().expect("random");
        assert_ne!(a, b);
    }
}
