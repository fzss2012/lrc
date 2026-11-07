//! 接收线程：TCP 连接 + HMAC 握手 + 收消息 → 逐块解码成脏块。
//!
//! 线程分工：
//! - 接收线程（本模块）只读 socket，解块后把像素塞进共享队列；
//! - 发送线程持有写半边，主线程/接收线程通过 mpsc 投递已编码 payload；
//! - 呈现只在窗口线程做（D3D11 设备非线程安全）。

use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use lrc_codec as codec;
use lrc_proto::{
    auth_reason, ClientHello, Message, Pong, ServerHello, VideoFrame, PROTO_VERSION,
};
use lrc_transport::{Conn, ConnReader, ConnWriter};
use sha2::Sha256;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::cli::Config;
use crate::state::{BlockUpdate, CursorShapeOwned, Shared};

type HmacSha256 = Hmac<Sha256>;

/// 启动接收线程。连接失败会自动重试，直到窗口关闭。
pub fn spawn(
    cfg: &Config,
    shared: Arc<Shared>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
) -> io::Result<()> {
    let addr = cfg.connect.clone();
    let password = cfg.password.clone();
    std::thread::Builder::new()
        .name(String::from("lrc-client-net"))
        .spawn(move || {
            if let Err(e) = receiver_main(&addr, &password, &shared, tx, rx) {
                if !shared.is_quit() {
                    shared.fail(format!("{e:#}"));
                }
            }
        })?;
    Ok(())
}

fn receiver_main(
    addr: &str,
    password: &str,
    shared: &Arc<Shared>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut conn = connect_retry(addr, shared)?;

    // ---- 握手：ClientHello → ServerHello → AuthRequest → AuthResult ----
    let client_id = random_id()?;
    let nonce_c = random_id()?;
    let hello = Message::ClientHello(ClientHello {
        version: PROTO_VERSION,
        client_id,
        nonce_c,
    });
    conn.send(&hello.encode())?;

    let mut buf = Vec::with_capacity(4096);
    let server: ServerHello = match recv_msg(&mut conn, &mut buf)? {
        Some(Message::ServerHello(hello)) => hello,
        Some(other) => {
            return Err(anyhow::anyhow!(
                "握手失败：期望 ServerHello，收到消息类型 {:#x}",
                other.msg_type()
            ))
        }
        None => return Err(anyhow::anyhow!("握手失败：对端在 ServerHello 之前关闭连接")),
    };
    if server.version != PROTO_VERSION {
        return Err(anyhow::anyhow!(
            "协议版本不匹配：对端 v{}，本地 v{}",
            server.version,
            PROTO_VERSION
        ));
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
    let mac = hmac_sha256(password.as_bytes(), &mac_input)?;
    conn.send(&Message::AuthRequest(lrc_proto::AuthRequest { mac }).encode())?;

    match recv_msg(&mut conn, &mut buf)? {
        Some(Message::AuthResult(result)) => {
            if !result.ok {
                return Err(anyhow::anyhow!(
                    "认证失败：{}",
                    auth_reason_text(result.reason)
                ));
            }
        }
        Some(other) => {
            return Err(anyhow::anyhow!(
                "握手失败：期望 AuthResult，收到消息类型 {:#x}",
                other.msg_type()
            ))
        }
        None => return Err(anyhow::anyhow!("握手失败：对端在 AuthResult 之前关闭连接")),
    }
    log::info!("认证通过，开始接收视频");

    // ---- 拆成收发两半：写半边交给发送线程，读半边留在本线程 ----
    let (writer, mut reader) = conn.split()?;
    spawn_writer(writer, rx, shared.clone());
    shared.set_connected(true);
    // 首帧要整屏；被控端如果实现了 RequestFull 会立刻发全量块。
    send(&tx, Message::RequestFull.encode());

    let result = video_loop(&mut reader, shared, &tx);
    shared.set_connected(false);
    result
}

/// 连接失败时每秒重试一次（窗口还没关就会一直试）。
fn connect_retry(addr: &str, shared: &Arc<Shared>) -> anyhow::Result<Conn> {
    let mut attempt = 0u32;
    // 失败日志最多每 10 s 一条，免得一直连不上时刷屏
    let mut next_log = std::time::Instant::now();
    loop {
        if shared.is_quit() {
            return Err(anyhow::anyhow!("窗口已关闭"));
        }
        match Conn::connect(addr) {
            Ok(conn) => {
                log::info!("已连接 {addr}");
                return Ok(conn);
            }
            Err(e) => {
                attempt = attempt.saturating_add(1);
                let now = std::time::Instant::now();
                if now >= next_log {
                    log::warn!("连接 {addr} 失败（第 {attempt} 次）: {e}；1s 后重试");
                    next_log = now + Duration::from_secs(10);
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

fn video_loop(
    reader: &mut ConnReader,
    shared: &Arc<Shared>,
    tx: &Sender<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    loop {
        if shared.is_quit() {
            return Ok(());
        }
        let n = match reader.recv(&mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => {
                if shared.is_quit() {
                    return Ok(());
                }
                return Err(anyhow::anyhow!("对端关闭了连接"));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::anyhow!("收包失败: {e}")),
        };
        shared.add_recv_bytes(n as u64);

        match Message::decode(&buf) {
            Ok(Message::VideoFrame(frame)) => handle_frame(&frame, shared),
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
fn handle_frame(frame: &VideoFrame<'_>, shared: &Shared) {
    let Some(grid) = frame.grid() else {
        log::warn!("帧几何非法: block_size/blocks 为 0");
        return;
    };
    if frame.is_full() {
        shared.mark_first_full_frame();
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
