//! 自测用假客户端（不是产品代码，只用来验证 lrc-host 的握手/推流/解码链路）。
//!
//! 用法：
//!   cargo run -p lrc-host --release --example probe-client -- \
//!       --addr 127.0.0.1:7800 --password 1234 --seconds 6 [--frames 300] [--bad-password]
//!
//! 它会：ClientHello → 收 ServerHello → HMAC 鉴权 → 逐块 decode_block 校验 → 回 ack/ping，
//! 最后打印一行 SUMMARY（frames / blocks / bytes / decode_err / 非零像素数）。

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use lrc_codec as codec;
use lrc_proto::{
    auth_reason, frame_flags, AuthRequest, ClientHello, FrameAck, InputEvent, Message, Ping,
    PROTO_VERSION,
};
use sha2::Sha256;

#[derive(Debug)]
struct Opts {
    addr: String,
    password: String,
    seconds: u64,
    frames: usize,
    bad_password: bool,
    /// 鉴权后把鼠标移到"采集画面本地坐标"并退出（验证输入注入链路）
    move_to: Option<(i32, i32)>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:7800".to_string(),
            password: "1234".to_string(),
            seconds: 6,
            frames: 100_000,
            bad_password: false,
            move_to: None,
        }
    }
}

fn parse_args() -> Result<Opts, Box<dyn std::error::Error>> {
    let mut opts = Opts::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0usize;
    while i < argv.len() {
        let key = argv[i].as_str();
        let mut value = || -> Result<String, Box<dyn std::error::Error>> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("{} 缺少取值", key).into())
        };
        match key {
            "--addr" => opts.addr = value()?,
            "--password" => opts.password = value()?,
            "--seconds" => opts.seconds = value()?.parse::<u64>()?,
            "--frames" => opts.frames = value()?.parse::<usize>()?,
            "--bad-password" => opts.bad_password = true,
            "--move" => {
                let raw = value()?;
                let (x, y) = raw
                    .split_once(',')
                    .ok_or_else(|| format!("{raw} 不是 x,y"))?;
                opts.move_to = Some((x.trim().parse::<i32>()?, y.trim().parse::<i32>()?));
            }
            "-h" | "--help" => {
                println!("probe-client --addr <IP:PORT> --password <PASS> [--seconds N] [--frames N] [--bad-password]");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}").into()),
        }
        i += 1;
    }
    Ok(opts)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| format!("hmac: {e}"))?;
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag);
    Ok(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_args()?;

    let mut conn = lrc_transport::Conn::connect(&opts.addr)?;
    let nonce_c = [0x5au8; 16];
    conn.send(
        &Message::ClientHello(ClientHello {
            version: PROTO_VERSION,
            client_id: [7u8; 16],
            nonce_c,
        })
        .encode(),
    )?;

    let mut handshake_buf = Vec::with_capacity(256);
    let srv = match recv_one(&mut conn, &mut handshake_buf)? {
        Message::ServerHello(s) => s,
        other => {
            return Err(format!("期待 ServerHello，收到 type=0x{:02x}", other.msg_type()).into())
        }
    };
    println!(
        "ServerHello: {}x{} rotation={} version={}",
        srv.width, srv.height, srv.rotation, srv.version
    );

    let mut mac_msg = [0u8; 32];
    mac_msg[..16].copy_from_slice(&nonce_c);
    mac_msg[16..].copy_from_slice(&srv.nonce_s);
    let mut password = opts.password.clone();
    if opts.bad_password {
        password.push('x');
    }
    let mac = hmac_sha256(password.as_bytes(), &mac_msg)?;
    conn.send(&Message::AuthRequest(AuthRequest { mac }).encode())?;

    let mut auth_buf = Vec::with_capacity(64);
    let result = match recv_one(&mut conn, &mut auth_buf)? {
        Message::AuthResult(r) => r,
        other => {
            return Err(format!("期待 AuthResult，收到 type=0x{:02x}", other.msg_type()).into())
        }
    };
    println!("AuthResult ok={} reason={}", result.ok, result.reason);
    if !result.ok {
        if result.reason == auth_reason::BAD_PASSWORD {
            println!("AUTH_REJECTED_BAD_PASSWORD");
        } else {
            println!("AUTH_REJECTED reason={}", result.reason);
        }
        return Ok(());
    }

    let (mut writer, mut reader) = conn.split()?;
    reader.set_read_timeout(Some(Duration::from_millis(1000)))?;

    // 只测输入注入：把鼠标挪到"采集输出本地坐标"，然后退出
    if let Some((x, y)) = opts.move_to {
        for _ in 0..3 {
            writer.send(&Message::Input(InputEvent::MouseMove { x, y }).encode())?;
            std::thread::sleep(Duration::from_millis(80));
        }
        println!("INPUT_SENT move_local=({x},{y})");
        std::thread::sleep(Duration::from_millis(200));
        writer.shutdown();
        return Ok(());
    }

    let screen_w = srv.width;
    let screen_h = srv.height;
    let stride = (screen_w as usize) * 4;
    let mut fb = vec![0u8; stride * screen_h as usize];

    let deadline = Instant::now() + Duration::from_secs(opts.seconds);
    let mut frames = 0usize;
    let mut full_frames = 0usize;
    let mut blocks = 0usize;
    let mut bytes = 0u64;
    let mut decode_err = 0usize;
    let mut bad_message = 0usize;
    let mut recv_total = 0u64;
    let mut last_ping = Instant::now();
    let mut last_report = Instant::now();
    let mut sec_frames = 0usize;
    let mut sec_bytes = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);

    while Instant::now() < deadline && frames < opts.frames {
        if last_ping.elapsed() >= Duration::from_secs(1) {
            writer.send(&Message::Ping(Ping { ts_ms: 424242 }).encode())?;
            last_ping = Instant::now();
        }
        match reader.recv(&mut buf) {
            Ok(Some(n)) => {
                recv_total += n as u64;
                let decoded = Message::decode(&buf[..n]);
                let msg = match decoded {
                    Ok(m) => m,
                    Err(e) => {
                        bad_message += 1;
                        println!("无法解析消息：{e}");
                        continue;
                    }
                };
                match msg {
                    Message::VideoFrame(vf) => {
                        frames += 1;
                        sec_frames += 1;
                        bytes += n as u64;
                        sec_bytes += n as u64;
                        if vf.flags & frame_flags::FULL != 0 {
                            full_frames += 1;
                        }
                        match vf.grid() {
                            Some(grid) => {
                                for entry in &vf.entries {
                                    blocks += 1;
                                    match grid.block_rect(entry.idx, screen_w, screen_h) {
                                        Some(rect) => {
                                            let offset =
                                                rect.y as usize * stride + rect.x as usize * 4;
                                            match codec::decode_block(
                                                entry.codec,
                                                entry.data,
                                                rect.w,
                                                rect.h,
                                                &mut fb[offset..],
                                                stride,
                                            ) {
                                                Ok(()) => {}
                                                Err(_) => decode_err += 1,
                                            }
                                        }
                                        None => decode_err += 1,
                                    }
                                }
                            }
                            None => decode_err += 1,
                        }
                        writer.send(
                            &Message::VideoAck(FrameAck {
                                seq: vf.seq,
                                recv_bytes: recv_total,
                            })
                            .encode(),
                        )?;
                    }
                    Message::Pong(_) => {}
                    Message::CursorPos(_) => {}
                    other => println!("收到其它消息 type=0x{:02x}", other.msg_type()),
                }
            }
            Ok(None) => {
                println!("服务端关闭了连接");
                break;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(e) => return Err(format!("读取失败：{e}").into()),
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let secs = last_report.elapsed().as_secs_f64();
            println!(
                "recv fps={:.1} bytes/s={} blocks={} frames_total={}",
                sec_frames as f64 / secs,
                (sec_bytes as f64 / secs) as u64,
                blocks,
                frames
            );
            last_report = Instant::now();
            sec_frames = 0;
            sec_bytes = 0;
        }
    }

    let nonzero = fb.iter().filter(|b| **b != 0).count();
    println!(
        "SUMMARY frames={frames} full_frames={full_frames} blocks={blocks} bytes={bytes} decode_err={decode_err} bad_message={bad_message} recv_total={recv_total} nonzero_bytes={nonzero} fb_bytes={}",
        fb.len()
    );
    writer.shutdown();
    Ok(())
}

fn recv_one(
    conn: &mut lrc_transport::Conn,
    out: &mut Vec<u8>,
) -> Result<Message<'static>, Box<dyn std::error::Error>> {
    // 握手阶段只需要小小的单条消息：把 payload 拷出来转成 'static，省得处理借用
    match conn.recv(out)? {
        Some(_) => {}
        None => return Err("对端在握手阶段关闭".into()),
    }
    let owned = out.clone();
    let msg = Message::decode(&owned)?;
    let boxed: Box<Message<'static>> = match msg {
        Message::ServerHello(s) => Box::new(Message::ServerHello(s)),
        Message::AuthResult(r) => Box::new(Message::AuthResult(r)),
        other => return Err(format!("握手阶段收到 type=0x{:02x}", other.msg_type()).into()),
    };
    Ok(*boxed)
}
