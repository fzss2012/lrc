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
    /// 用 InputEvent 让被控端挪鼠标，回看是否收到 CursorPos/CursorShape（光标全链路自测）
    wiggle: bool,
    /// 第 N 秒发一次 RequestFull（验证静止桌面的整帧兜底）
    request_full_at: Option<f64>,
    /// 把视频 AU 裸流写到这里，方便事后用 ffmpeg 验证可解码
    dump_video: Option<String>,
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
            wiggle: false,
            request_full_at: None,
            dump_video: None,
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
            "--wiggle" => opts.wiggle = true,
            "--request-full-at" => opts.request_full_at = Some(value()?.parse::<f64>()?),
            "--dump-video" => opts.dump_video = Some(value()?),
            "--move" => {
                let raw = value()?;
                let (x, y) = raw
                    .split_once(',')
                    .ok_or_else(|| format!("{raw} 不是 x,y"))?;
                opts.move_to = Some((x.trim().parse::<i32>()?, y.trim().parse::<i32>()?));
            }
            "-h" | "--help" => {
                println!(
                    "probe-client --addr <IP:PORT> --password <PASS> [--seconds N] [--frames N] [--bad-password] [--move X,Y] [--wiggle] [--request-full-at SECS]"
                );
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

    let started = Instant::now();
    let deadline = started + Duration::from_secs(opts.seconds);
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
    let mut dump: Option<std::fs::File> = match &opts.dump_video {
        Some(path) => Some(std::fs::File::create(path)?),
        None => None,
    };

    // 光标全链路自测状态
    let wiggle_at = [1.0f64, 1.35, 1.7, 2.05];
    let wiggle_pos = [(120, 120), (500, 320), (1100, 620), (300, 800)];
    let mut wiggle_step = 0usize;
    let mut cursor_pos_msgs = 0usize;
    let mut cursor_shape_msgs = 0usize;
    let mut shape_geometry_ok = true;
    let mut last_cursor_pos: Option<(i16, i16, bool)> = None;
    let mut cursor_at_settle: Option<usize> = None;
    let mut request_full_sent_at: Option<f64> = None;
    let mut full_frames_after_request = 0usize;
    // 视频通道
    let mut video_aus = 0u64;
    let mut video_bytes = 0u64;
    let mut video_keyframes = 0u64;
    let mut video_codec = 0u8;
    let mut video_ts_first: Option<u32> = None;
    let mut video_ts_last = 0u32;
    // 采集→客户端收到的延迟（GetTickCount64 低 32 位，同机可用）
    let mut lat_min = u32::MAX;
    let mut lat_max = 0u32;
    let mut lat_sum = 0u64;
    let mut lat_n = 0u64;
    let mut sec_video_aus = 0u64;
    let mut sec_video_bytes = 0u64;

    while Instant::now() < deadline && frames < opts.frames {
        if last_ping.elapsed() >= Duration::from_secs(1) {
            writer.send(&Message::Ping(Ping { ts_ms: 424242 }).encode())?;
            last_ping = Instant::now();
        }
        let elapsed = started.elapsed().as_secs_f64();
        // 用被控端的输入通道挪鼠标：正常的 CursorPos 应该原路回传
        if opts.wiggle && wiggle_step < wiggle_at.len() && elapsed >= wiggle_at[wiggle_step] {
            let (x, y) = wiggle_pos[wiggle_step];
            writer.send(&Message::Input(InputEvent::MouseMove { x, y }).encode())?;
            println!("WIGGLE step={wiggle_step} move=({x},{y}) t={elapsed:.2}");
            wiggle_step += 1;
        }
        // 全部挪完 + 0.4 秒后记下计数，之后静止 1.2 秒看还有没有多余的光标消息
        if opts.wiggle
            && wiggle_step >= wiggle_at.len()
            && cursor_at_settle.is_none()
            && elapsed >= wiggle_at[wiggle_at.len() - 1] + 0.4
        {
            cursor_at_settle = Some(cursor_pos_msgs);
        }
        if let Some(t) = opts.request_full_at {
            if request_full_sent_at.is_none() && elapsed >= t {
                writer.send(&Message::RequestFull.encode())?;
                request_full_sent_at = Some(elapsed);
                println!("REQUEST_FULL sent at t={elapsed:.2}");
            }
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
                            if request_full_sent_at.is_some() {
                                full_frames_after_request += 1;
                            }
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
                    Message::VideoAu(au) => {
                        video_aus += 1;
                        sec_video_aus += 1;
                        video_bytes += n as u64;
                        sec_video_bytes += n as u64;
                        video_codec = au.codec;
                        if au.is_key() {
                            video_keyframes += 1;
                        }
                        if video_ts_first.is_none() {
                            video_ts_first = Some(au.ts_ms);
                            println!(
                                "VIDEO_AU first ts_ms={} seq={} codec={} key={} bytes={}",
                                au.ts_ms,
                                au.seq,
                                au.codec,
                                au.is_key(),
                                au.data.len()
                            );
                        }
                        if au.ts_ms != 0 {
                            video_ts_last = au.ts_ms;
                            // SAFETY: 只读系统计时器
                            let now = unsafe {
                                windows::Win32::System::SystemInformation::GetTickCount64()
                            } as u32;
                            let lat = now.wrapping_sub(au.ts_ms);
                            lat_min = lat_min.min(lat);
                            lat_max = lat_max.max(lat);
                            lat_sum += u64::from(lat);
                            lat_n += 1;
                        }
                        if let Some(f) = dump.as_mut() {
                            use std::io::Write as _;
                            f.write_all(au.data)?;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::CursorPos(pos) => {
                        cursor_pos_msgs += 1;
                        last_cursor_pos = Some((pos.x, pos.y, pos.visible));
                        if cursor_pos_msgs <= 3 {
                            println!("CursorPos {} {} visible={}", pos.x, pos.y, pos.visible);
                        }
                    }
                    Message::CursorShape(shape) => {
                        cursor_shape_msgs += 1;
                        let expect = 4 * shape.width as usize * shape.height as usize;
                        if shape.rgba.len() != expect {
                            shape_geometry_ok = false;
                            println!(
                                "CursorShape 长度不对: {}x{} len={} 期望 {}",
                                shape.width,
                                shape.height,
                                shape.rgba.len(),
                                expect
                            );
                        }
                        println!(
                            "CursorShape {}x{} hotspot=({}, {}) bytes={}",
                            shape.width,
                            shape.height,
                            shape.hotspot_x,
                            shape.hotspot_y,
                            shape.rgba.len()
                        );
                    }
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
                "recv fps={:.1} bytes/s={} blocks={} frames_total={} video_fps={:.1} video_bytes/s={}",
                sec_frames as f64 / secs,
                (sec_bytes as f64 / secs) as u64,
                blocks,
                frames,
                sec_video_aus as f64 / secs,
                (sec_video_bytes as f64 / secs) as u64
            );
            last_report = Instant::now();
            sec_frames = 0;
            sec_bytes = 0;
            sec_video_aus = 0;
            sec_video_bytes = 0;
        }
    }

    let nonzero = fb.iter().filter(|b| **b != 0).count();
    println!(
        "SUMMARY frames={frames} full_frames={full_frames} blocks={blocks} bytes={bytes} decode_err={decode_err} bad_message={bad_message} recv_total={recv_total} nonzero_bytes={nonzero} fb_bytes={}",
        fb.len()
    );
    println!(
        "CURSOR pos_msgs={cursor_pos_msgs} shape_msgs={cursor_shape_msgs} last_pos={last_cursor_pos:?} shape_geometry_ok={shape_geometry_ok}"
    );
    println!(
        "VIDEO aus={video_aus} bytes={video_bytes} keyframes={video_keyframes} codec={video_codec} ts_first={video_ts_first:?} ts_last={video_ts_last} ts_nonzero={}",
        video_ts_last != 0
    );
    if lat_n > 0 {
        println!(
            "VIDEO_LAT ms min={} avg={:.1} max={} n={lat_n}（抓帧→客户端收到 AU，同机时钟）",
            lat_min,
            lat_sum as f64 / lat_n as f64,
            lat_max
        );
    }
    if let Some(at_settle) = cursor_at_settle {
        println!(
            "CURSOR idle_window pos_msgs_after_settle={} (期望 0)",
            cursor_pos_msgs.saturating_sub(at_settle)
        );
    }
    if let Some(t) = request_full_sent_at {
        println!(
            "REQUEST_FULL sent_at={t:.2} full_frames_after_request={full_frames_after_request}"
        );
    }
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
