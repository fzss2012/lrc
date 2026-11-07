//! 假 host：验证客户端音频通道 + 显示切换（不需要真采集）。
//!
//! 用法：
//!   cargo run --release -p lrc-client --example fake-audio-host -- \
//!       --port 7870 --markers D:\temp\audio-markers.txt --start D:\temp\go.flag [--capture]
//!
//! 做的事：
//! 1. 握手后发 \`DisplayList\`（2 路显示输出）；
//! 2. 用 lrc-audio 的 OpusEncoder 现场编 48kHz/2ch 的双音，按 20ms 节奏发 \`AudioFrame\`；
//! 3. 中间插一个 seq 跳变（验证客户端丢包处理）、再发一串突发包（验证有界队列丢旧）；
//! 4. 收到客户端的 \`SelectDisplay\` 就写 marker；
//! 5. \`--capture\` 时用回环采集录客户端放出来的声音，算 RMS 写 marker（证明确实出声）。

use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lrc_audio::{pcm_stats, AudioFormat, LoopbackCapturer, OpusEncoder};
use lrc_proto::{
    auth_reason, AudioFrame, AuthResult, DisplayInfo, Message, Pong, SelectDisplay, ServerHello,
    PROTO_VERSION,
};
use lrc_transport::{Conn, ConnReader, ConnWriter};

struct Args {
    port: u16,
    markers: Option<String>,
    start: Option<String>,
    capture: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: 7870,
        markers: None,
        start: None,
        capture: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--capture" => args.capture = true,
            "--help" | "-h" => {
                println!("fake-audio-host --port N [--markers PATH] [--start PATH] [--capture]");
                std::process::exit(0);
            }
            other => {
                let value = it.next().ok_or_else(|| format!("{other} 缺少取值"))?;
                match other {
                    "--port" => args.port = value.parse().map_err(|e| format!("--port: {e}"))?,
                    "--markers" => args.markers = Some(value),
                    "--start" => args.start = Some(value),
                    _ => return Err(format!("未知参数 {other}")),
                }
            }
        }
    }
    Ok(args)
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

fn spawn_reader(mut reader: ConnReader, tx: Sender<Vec<u8>>, markers: Option<String>, shared_rx: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        loop {
            match reader.recv(&mut buf) {
                Ok(Some(_)) => match Message::decode(&buf) {
                    Ok(Message::Ping(ping)) => {
                        let _ = tx.send(Message::Pong(Pong { ts_ms: ping.ts_ms }).encode());
                    }
                    Ok(Message::SelectDisplay(SelectDisplay { index })) => {
                        mark(&markers, &format!("MARK select_display={index}"));
                    }
                    Ok(Message::RequestFull) => mark(&markers, "MARK request_full"),
                    Ok(_) | Err(_) => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }
        shared_rx.store(true, Ordering::SeqCst);
    });
}

fn handshake(conn: &mut Conn) -> Result<(), String> {
    let mut buf = Vec::new();
    match conn.recv(&mut buf) {
        Ok(Some(_)) => {
            if let Ok(Message::ClientHello(hello)) = Message::decode(&buf) {
                println!("ClientHello v{}", hello.version);
            }
        }
        _ => return Err(String::from("读 ClientHello 失败")),
    }
    let mut nonce_s = [0u8; 16];
    for (i, b) in nonce_s.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17).wrapping_add(3);
    }
    let hello = Message::ServerHello(ServerHello {
        version: PROTO_VERSION,
        width: 1920,
        height: 1080,
        rotation: 0,
        nonce_s,
    });
    conn.send(&hello.encode())
        .map_err(|e| format!("发 ServerHello 失败: {e}"))?;
    let _ = conn.recv(&mut buf);
    let result = Message::AuthResult(AuthResult {
        ok: true,
        reason: auth_reason::OK,
    });
    conn.send(&result.encode())
        .map_err(|e| format!("发 AuthResult 失败: {e}"))?;
    Ok(())
}

/// 双音测试信号（48kHz/2ch/16bit）。
fn synth_tone(format: AudioFormat, seconds: f32) -> Vec<u8> {
    let frames = (format.sample_rate as f32 * seconds) as usize;
    let mut out = Vec::with_capacity(frames * format.channels as usize * 2);
    let mut phase = 0.0f32;
    for i in 0..frames {
        let t = i as f32 / format.sample_rate as f32;
        for c in 0..format.channels {
            let f = if c == 0 { 440.0 } else { 660.0 };
            let v = 0.25 * (2.0 * std::f32::consts::PI * f * t).sin();
            let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        phase += 1.0;
        let _ = phase;
    }
    out
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fake-audio-host: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .map_err(|e| format!("绑定端口 {} 失败: {e}", args.port))?;
    println!("fake-audio-host 监听 127.0.0.1:{}", args.port);
    let mut conn = Conn::accept(&listener).map_err(|e| format!("accept 失败: {e}"))?;
    handshake(&mut conn)?;
    println!("握手完成");

    let (writer, reader) = conn.split().map_err(|e| format!("split 失败: {e}"))?;
    let (tx, rx) = channel::<Vec<u8>>();
    spawn_writer(writer, rx);
    let disconnected = Arc::new(AtomicBool::new(false));
    spawn_reader(reader, tx.clone(), args.markers.clone(), disconnected.clone());

    // 可选门控：外部脚本先摆好窗口再放行
    if let Some(start) = &args.start {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !std::path::Path::new(start).exists() {
            if Instant::now() > deadline {
                return Err(String::from("等待 --start 超时"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("START_GATE_OPEN");
    }

    // 1) 显示输出列表
    let displays = Message::DisplayList(vec![
        DisplayInfo {
            index: 0,
            width: 1920,
            height: 1080,
            origin_x: 0,
            origin_y: 0,
            primary: true,
        },
        DisplayInfo {
            index: 1,
            width: 1280,
            height: 720,
            origin_x: 1920,
            origin_y: 0,
            primary: false,
        },
    ]);
    let _ = tx.send(displays.encode());
    mark(&args.markers, "MARK display_list_sent");

    // 2) 回环采集线程（可选）：录客户端播放出来的声音。
    //    注意：WASAPI 对象不是 Send，必须在采集线程内部创建。
    let capture = if args.capture {
        let markers = args.markers.clone();
        Some(std::thread::spawn(move || {
            let mut cap = match LoopbackCapturer::new(AudioFormat::default()) {
                Ok(cap) => cap,
                Err(e) => {
                    mark(&markers, &format!("MARK loopback_unavailable={e}"));
                    return Vec::new();
                }
            };
            let mut captured: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 3840];
            let deadline = Instant::now() + Duration::from_secs(6);
            while Instant::now() < deadline {
                match cap.read(&mut buf) {
                    Ok(n) if n > 0 => captured.extend_from_slice(&buf[..n]),
                    _ => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            captured
        }))
    } else {
        None
    };

    // 3) Opus 编码 + 按 20ms 节奏发
    let format = AudioFormat::default();
    let tone = synth_tone(format, 8.0);
    let block = format.bytes_per_20ms();
    let mut encoder = OpusEncoder::new(format, 32_000, None)
        .map_err(|e| format!("创建 Opus 编码器失败: {e}"))?;
    let mut seq: u32 = 1;
    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut out = Vec::new();
    for chunk in tone.chunks(block) {
        out.clear();
        encoder
            .encode(chunk, &mut out)
            .map_err(|e| format!("编码失败: {e}"))?;
        if !out.is_empty() {
            aus.push(out.clone());
        }
    }
    println!("编码得到 {} 个 Opus 包", aus.len());
    mark(&args.markers, "MARK audio_start");
    let ts0 = Instant::now();

    let mut send_packet = |tx: &Sender<Vec<u8>>, seq: &mut u32, data: &[u8]| {
        let frame = Message::AudioFrame(AudioFrame {
            ts_ms: 0,
            seq: *seq,
            flags: 1,
            sample_rate: format.sample_rate as u16,
            channels: format.channels as u8,
            data,
        });
        let _ = tx.send(frame.encode());
        *seq = seq.wrapping_add(1);
    };

    // 前 100 包：正常 20ms 节奏（2 秒）
    let normal = aus.len().min(100);
    for (i, au) in aus.iter().take(normal).enumerate() {
        let target = ts0 + Duration::from_millis(20 * (i as u64 + 1));
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        send_packet(&tx, &mut seq, au);
    }

    // 4) seq 跳变：故意丢一个（客户端应丢当前包并重新对齐）
    if aus.len() > normal {
        seq = seq.wrapping_add(1); // 制造一个空洞
        send_packet(&tx, &mut seq, &aus[normal]);
        mark(&args.markers, "MARK gap_sent");
        std::thread::sleep(Duration::from_millis(40));
    }

    // 5) 突发：一次灌 40 包（验证客户端有界队列丢最旧）
    let burst_start = normal.wrapping_add(1) as usize;
    let burst: Vec<Vec<u8>> = aus
        .iter()
        .skip(burst_start)
        .take(40)
        .cloned()
        .collect();
    for au in &burst {
        send_packet(&tx, &mut seq, au);
    }
    mark(
        &args.markers,
        &format!("MARK burst_sent count={}", burst.len()),
    );

    // 6) 再按正常节奏发 1 秒，确认通道没被打挂
    let after = burst_start + burst.len();
    let loop_start = Instant::now();
    for (i, au) in aus.iter().skip(after).take(50).enumerate() {
        let target = loop_start + Duration::from_millis(20 * (i as u64 + 1));
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        send_packet(&tx, &mut seq, au);
    }
    mark(&args.markers, &format!("MARK audio_done sent={seq}"));

    // 7) 收尾：等外部按快捷键 / 采集结束
    std::thread::sleep(Duration::from_millis(1500));
    if let Some(handle) = capture {
        match handle.join() {
            Ok(captured) => {
                let (rms, peak, nonzero) = pcm_stats(&captured);
                mark(
                    &args.markers,
                    &format!(
                        "MARK loopback_rms={rms:.4} peak={peak:.4} nz={nonzero:.2} bytes={}",
                        captured.len()
                    ),
                );
            }
            Err(_) => mark(&args.markers, "MARK loopback_join_failed"),
        }
    }
    mark(&args.markers, "MARK done");
    std::thread::sleep(Duration::from_millis(500));
    let _ = disconnected.load(Ordering::SeqCst);
    Ok(())
}
