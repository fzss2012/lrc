//! 假 host：只用来验证客户端的视频通道（不需要真采集/真桌面）。
//!
//! 用法：
//!   cargo run --release -p lrc-client --example fake-video-host -- \
//!       --port 7840 --scenario full --markers D:\temp\markers.txt [--codec hevc_nvenc|h264_nvenc]
//!
//! scenario：
//! - full：视频帧 → 光标出现/移动/隐藏 → 切回块模式（整屏 RAW），每一步写一行 marker 方便外部截图核对；
//! - recover：先只发 P 帧（无 IDR）逼客户端 RequestFull，再重建编码器发 IDR 验证恢复。
//!
//! 只用于测试：不校验密码、不采集、不注入输入。

use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lrc_proto::{
    auth_reason, codec_tag, frame_flags, AuthResult, BlockEntry, Grid, Message, Pong, ServerHello,
    VideoAu, VideoFrame, PROTO_VERSION,
};
use lrc_transport::{Conn, ConnReader, ConnWriter};
use lrc_video::{EncoderConfig, FfmpegEncoder, VideoEncoder};
use windows::Win32::System::SystemInformation::GetTickCount64;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const STRIDE: usize = WIDTH as usize * 4;
const BLOCK: u16 = 64;

struct Args {
    port: u16,
    scenario: String,
    markers: Option<String>,
    codec: String,
    /// 门控文件：存在后才开始发场景（外部用来先摆好窗口/准备好截图）
    start: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: 7840,
        scenario: String::from("full"),
        markers: None,
        codec: String::from("hevc_nvenc"),
        start: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = match it.next() {
            Some(v) => v,
            None => return Err(format!("{flag} 缺少取值")),
        };
        match flag.as_str() {
            "--port" => args.port = value.parse().map_err(|e| format!("--port: {e}"))?,
            "--scenario" => args.scenario = value,
            "--markers" => args.markers = Some(value),
            "--codec" => args.codec = value,
            "--start" => args.start = Some(value),
            "--help" | "-h" => {
                println!("fake-video-host --port N --scenario full|recover --markers PATH --codec hevc_nvenc|h264_nvenc");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    Ok(args)
}

fn host_clock_ms() -> u32 {
    let uptime = unsafe { GetTickCount64() };
    (uptime & 0xFFFF_FFFF) as u32
}

fn mark(path: &Option<String>, text: &str) {
    println!("{text}");
    let _ = std::io::stdout().flush();
    if let Some(p) = path {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{text}");
            let _ = f.flush();
        }
    }
}

fn fill_rect(bgra: &mut [u8], x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    for row in y..y.saturating_add(h).min(HEIGHT) {
        for col in x..x.saturating_add(w).min(WIDTH) {
            let off = (row as usize * WIDTH as usize + col as usize) * 4;
            if let Some(px) = bgra.get_mut(off..off + 4) {
                px.copy_from_slice(&color);
            }
        }
    }
}

/// 静态测试图：深灰背景 + 四个已知色块（截图按坐标采样即可验证解码正确）。
fn make_pattern() -> Vec<u8> {
    let mut bgra = vec![0u8; STRIDE * HEIGHT as usize];
    for px in bgra.chunks_exact_mut(4) {
        px.copy_from_slice(&[30, 30, 30, 255]);
    }
    fill_rect(&mut bgra, 200, 200, 128, 128, [0, 0, 255, 255]); // 红
    fill_rect(&mut bgra, 800, 200, 128, 128, [0, 255, 0, 255]); // 绿
    fill_rect(&mut bgra, 1500, 700, 128, 128, [255, 0, 0, 255]); // 蓝
    fill_rect(&mut bgra, 400, 800, 128, 128, [255, 255, 255, 255]); // 白
    bgra
}

fn solid_cursor(size: u32, color: [u8; 4]) -> Vec<u8> {
    let mut bgra = vec![0u8; (size * size * 4) as usize];
    for px in bgra.chunks_exact_mut(4) {
        px.copy_from_slice(&color);
    }
    bgra
}

fn send_cursor_shape(tx: &Sender<Vec<u8>>, size: u32, color: [u8; 4]) {
    let bgra = solid_cursor(size, color);
    let shape = Message::CursorShape(lrc_proto::CursorShape {
        width: size as u16,
        height: size as u16,
        hotspot_x: 0,
        hotspot_y: 0,
        rgba: &bgra,
    });
    let _ = tx.send(shape.encode());
}

fn send_cursor_pos(tx: &Sender<Vec<u8>>, x: i16, y: i16, visible: bool) {
    let msg = Message::CursorPos(lrc_proto::CursorPos { x, y, visible });
    let _ = tx.send(msg.encode());
}

fn make_encoder(codec: &str) -> Result<FfmpegEncoder, String> {
    let cfg = EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        gop: 60,
        fps: 30,
        codec: String::from(codec),
        ffmpeg_path: None,
    };
    FfmpegEncoder::new(cfg).map_err(|e| format!("创建编码器失败: {e}"))
}

fn codec_tag_of(codec: &str) -> u8 {
    let c = codec.to_ascii_lowercase();
    if c.contains("hevc") || c.contains("265") {
        codec_tag::HEVC
    } else {
        codec_tag::H264
    }
}

fn codec_name_of(tag: u8) -> &'static str {
    if tag == codec_tag::HEVC {
        "hevc_nvenc"
    } else {
        "h264_nvenc"
    }
}

struct AuSender {
    tx: Sender<Vec<u8>>,
    codec: u8,
    seq: u32,
}

impl AuSender {
    fn send_au(&mut self, ts_ms: u32, key: bool, data: &[u8]) {
        let msg = Message::VideoAu(VideoAu {
            ts_ms,
            seq: self.seq,
            codec: self.codec,
            flags: if key { 1 } else { 0 },
            data,
        });
        let _ = self.tx.send(msg.encode());
        self.seq = self.seq.wrapping_add(1);
    }

    /// 整屏 RAW 块（深蓝），用来验证"视频 → 块"模式切换。
    fn send_full_blocks(&mut self) {
        let Some(grid) = Grid::new(BLOCK, WIDTH, HEIGHT) else {
            return;
        };
        let mut blobs: Vec<(u32, Vec<u8>)> = Vec::new();
        for idx in 0..grid.count() {
            let Some(rect) = grid.block_rect(idx, WIDTH, HEIGHT) else {
                continue;
            };
            let mut data = vec![0u8; (rect.w * rect.h * 4) as usize];
            for px in data.chunks_exact_mut(4) {
                px.copy_from_slice(&[120, 0, 0, 255]); // 深蓝
            }
            blobs.push((idx, data));
        }
        let entries: Vec<BlockEntry<'_>> = blobs
            .iter()
            .map(|(idx, data)| BlockEntry {
                idx: *idx,
                codec: codec_tag::RAW,
                data: data.as_slice(),
            })
            .collect();
        let frame = Message::VideoFrame(VideoFrame {
            seq: self.seq,
            flags: frame_flags::FULL | frame_flags::KEY,
            base_seq: 0,
            block_size: BLOCK,
            blocks_x: grid.blocks_x,
            blocks_y: grid.blocks_y,
            entries,
        });
        let _ = self.tx.send(frame.encode());
        self.seq = self.seq.wrapping_add(1);
    }
}

struct Flags {
    request_full: AtomicU64,
}

fn spawn_writer(mut writer: ConnWriter, rx: Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Ok(payload) = rx.recv() {
            if writer.send(&payload).is_err() {
                break;
            }
        }
        writer.shutdown();
    });
}

fn spawn_reader(mut reader: ConnReader, tx: Sender<Vec<u8>>, flags: Arc<Flags>) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        loop {
            match reader.recv(&mut buf) {
                Ok(Some(_)) => match Message::decode(&buf) {
                    Ok(Message::Ping(ping)) => {
                        let _ = tx.send(Message::Pong(Pong { ts_ms: ping.ts_ms }).encode());
                    }
                    Ok(Message::RequestFull) => {
                        flags.request_full.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(_) | Err(_) => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

fn handshake(conn: &mut Conn) -> Result<(), String> {
    let mut buf = Vec::new();
    // ClientHello
    match conn.recv(&mut buf) {
        Ok(Some(_)) => match Message::decode(&buf) {
            Ok(Message::ClientHello(hello)) => {
                println!("ClientHello v{} nonce_c={:02x?}", hello.version, hello.nonce_c);
            }
            _ => return Err(String::from("期望 ClientHello")),
        },
        _ => return Err(String::from("读 ClientHello 失败")),
    }
    let mut nonce_s = [0u8; 16];
    let seed = u64::from(host_clock_ms());
    for (i, byte) in nonce_s.iter_mut().enumerate() {
        *byte = (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> ((i % 8) * 8)) as u8;
    }
    let hello = Message::ServerHello(ServerHello {
        version: PROTO_VERSION,
        width: WIDTH,
        height: HEIGHT,
        rotation: 0,
        nonce_s,
    });
    conn.send(&hello.encode())
        .map_err(|e| format!("发 ServerHello 失败: {e}"))?;
    // 读掉 AuthRequest（测试用 host 不校验密码）
    let _ = conn.recv(&mut buf);
    let result = Message::AuthResult(AuthResult {
        ok: true,
        reason: auth_reason::OK,
    });
    conn.send(&result.encode())
        .map_err(|e| format!("发 AuthResult 失败: {e}"))?;
    Ok(())
}

fn pump(
    out: &mut AuSender,
    encoder: &mut FfmpegEncoder,
    pattern: &[u8],
    frames: u32,
) -> Result<(), String> {
    for _ in 0..frames {
        let ts = host_clock_ms();
        let aus = encoder
            .encode(pattern, STRIDE)
            .map_err(|e| format!("编码失败: {e}"))?;
        let key = encoder.last_au_was_keyframe();
        for au in aus {
            out.send_au(ts, key, &au);
        }
        // 模拟 30 fps：编码 ~10ms + 睡 20ms
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn scenario_full(
    out: &mut AuSender,
    encoder: &mut FfmpegEncoder,
    pattern: &[u8],
    markers: &Option<String>,
) -> Result<(), String> {
    // 注意：marker 写在每个阶段**开始**处（外部脚本看到 marker 后再截图，
    // 这时场景还停在该阶段）。
    pump(out, encoder, pattern, 6)?;
    mark(markers, "MARK video_on");
    pump(out, encoder, pattern, 10)?;

    // 光标出现（32x32 纯红，热点 0,0）
    send_cursor_shape(&out.tx, 32, [0, 0, 255, 255]);
    send_cursor_pos(&out.tx, 448, 384, true);
    mark(markers, "MARK cursor_a");
    pump(out, encoder, pattern, 12)?;

    // 光标移到别处（旧位置应被视频帧的干净底图擦掉）
    send_cursor_pos(&out.tx, 896, 640, true);
    mark(markers, "MARK cursor_b");
    pump(out, encoder, pattern, 12)?;

    // 隐藏
    send_cursor_pos(&out.tx, 896, 640, false);
    mark(markers, "MARK cursor_hidden");
    pump(out, encoder, pattern, 12)?;

    // 光标重新出现，然后切回块模式（整屏 RAW）
    send_cursor_pos(&out.tx, 448, 384, true);
    pump(out, encoder, pattern, 2)?;
    out.send_full_blocks();
    mark(markers, "MARK blocks_on");
    std::thread::sleep(Duration::from_millis(1200));
    mark(markers, "MARK done");
    Ok(())
}

fn scenario_recover(
    out: &mut AuSender,
    encoder: &mut FfmpegEncoder,
    pattern: &[u8],
    markers: &Option<String>,
    flags: &Flags,
) -> Result<(), String> {
    // 先编 5 帧，但只发 P 帧（扣下第一个 IDR）：客户端必须丢弃并 RequestFull
    let mut aus_all: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        let aus = encoder
            .encode(pattern, STRIDE)
            .map_err(|e| format!("编码失败: {e}"))?;
        aus_all.extend(aus);
    }
    let base = flags.request_full.load(Ordering::SeqCst);
    for au in aus_all.iter().skip(1) {
        out.send_au(host_clock_ms(), false, au);
    }
    mark(markers, "MARK nokey_sent");

    // 等客户端来要整帧（最多 3s）
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if flags.request_full.load(Ordering::SeqCst) > base {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let got = flags.request_full.load(Ordering::SeqCst).saturating_sub(base);
    mark(markers, &format!("MARK got_request_full={got}"));

    // 重建编码器（保证从新的 IDR 开始），发 IDR + 后续帧
    let mut fresh = make_encoder(codec_name_of(out.codec))?;
    for _ in 0..40 {
        let ts = host_clock_ms();
        let aus = fresh
            .encode(pattern, STRIDE)
            .map_err(|e| format!("编码失败: {e}"))?;
        let key = fresh.last_au_was_keyframe();
        for au in aus {
            out.send_au(ts, key, &au);
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    mark(markers, "MARK recovered");

    // 再补一段"坏码流"：验证解码失败 → 记错误 + RequestFull（客户端不能崩）
    let garbage = vec![0xAAu8; 64];
    out.send_au(host_clock_ms(), false, &garbage);
    out.send_au(host_clock_ms(), false, &garbage);
    mark(markers, "MARK corrupt_sent");
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fake-video-host: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .map_err(|e| format!("绑定端口 {} 失败: {e}", args.port))?;
    println!(
        "fake-video-host 监听 127.0.0.1:{} scenario={}",
        args.port, args.scenario
    );
    let mut conn = Conn::accept(&listener).map_err(|e| format!("accept 失败: {e}"))?;
    println!("客户端已连接");
    handshake(&mut conn)?;
    println!("握手完成");

    let (writer, reader) = conn.split().map_err(|e| format!("split 失败: {e}"))?;
    let (tx, rx) = channel::<Vec<u8>>();
    spawn_writer(writer, rx);
    let flags = Arc::new(Flags {
        request_full: AtomicU64::new(0),
    });
    spawn_reader(reader, tx.clone(), flags.clone());

    // 等客户端启动时的那个 RequestFull 过去，避免干扰 recover 场景判断
    std::thread::sleep(Duration::from_millis(500));

    // 可选门控：等外部文件出现再开始（先让测试脚本摆好窗口/截图参数）
    if let Some(start) = &args.start {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !std::path::Path::new(start).exists() {
            if Instant::now() > deadline {
                return Err(String::from("等待 --start 门控文件超时"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("START_GATE_OPEN");
    }

    let pattern = make_pattern();
    let mut encoder = make_encoder(&args.codec)?;
    let mut out = AuSender {
        tx,
        codec: codec_tag_of(&args.codec),
        seq: 1,
    };
    let result = match args.scenario.as_str() {
        "full" => scenario_full(&mut out, &mut encoder, &pattern, &args.markers),
        "recover" => scenario_recover(&mut out, &mut encoder, &pattern, &args.markers, &flags),
        other => Err(format!("未知 scenario {other}")),
    };
    drop(out);
    std::thread::sleep(Duration::from_millis(500));
    result
}
