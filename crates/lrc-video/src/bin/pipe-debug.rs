//! 诊断：把 6 个 AU 一次性喂给解码器，看活管道上能不能出帧。

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

const FFMPEG: &str = r"C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::path::Path::new("D:/lrc/target/aus");
    let mut aus: Vec<Vec<u8>> = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().map(|x| x == "hevc").unwrap_or(false) {
            aus.push(std::fs::read(&p)?);
        }
    }
    aus.sort_by_key(|v| v.len());
    println!("读到 {} 个 AU", aus.len());

    for (label, args) in [
        ("raw hevc", vec!["-f", "hevc", "-i", "pipe:0"]),
        ("mpegts", vec!["-f", "mpegts", "-i", "pipe:0"]),
    ] {
        let mut full: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        full.extend(args.iter().map(|s| s.to_string()));
        full.extend(["-f".into(), "rawvideo".into(), "-pix_fmt".into(), "bgra".into(), "pipe:1".into()]);
        let mut child = Command::new(FFMPEG).args(&full)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = vec![0u8; 262144];
            while let Ok(k) = stdout.read(&mut buf) {
                if k == 0 { break }
                if tx.send(buf[..k].to_vec()).is_err() { break }
            }
        });
        let t0 = Instant::now();
        for au in &aus { stdin.write_all(au)?; }
        stdin.flush()?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut got = 0usize;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(d) => got += d.len(),
                Err(_) => { if got > 0 { break } }
            }
        }
        println!("{label:<10} -> {got} 字节 = {:.2} 帧", got as f64 / 8294400.0);
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(())
}
