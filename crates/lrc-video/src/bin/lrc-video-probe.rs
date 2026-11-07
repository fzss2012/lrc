//! NVENC 视频通道可行性验证（M2 spike）。
//!
//! 跑完整链路：合成帧 → ffmpeg/NVENC 编码 → 码流分帧 → ffmpeg 解码 → 与源帧比 PSNR。
//!
//!   lrc-video-probe --frames 300 --bitrate 4000000 --codec hevc_nvenc
//!                   [--scenario static|typing|scroll|video|all] [--ffmpeg <path>]
//!                   [--width 1920] [--height 1080] [--fps 30] [--save]
//!
//! 底图优先用 D:/lrc/captures/frame0.bgra（真实桌面），没有就合成一张。

use std::error::Error;
use std::time::Instant;

use lrc_video::ffmpeg::{decode_command_line, encode_command_line, resolve_ffmpeg};
use lrc_video::{DecoderConfig, EncoderConfig, FfmpegDecoder, FfmpegEncoder, VideoDecoder, VideoEncoder};

const BASE_PATH: &str = "D:/lrc/captures/frame0.bgra";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Static,
    Typing,
    Scroll,
    Video,
}

impl Scenario {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "static" => Some(Self::Static),
            "typing" => Some(Self::Typing),
            "scroll" => Some(Self::Scroll),
            "video" => Some(Self::Video),
            _ => None,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Typing => "typing",
            Self::Scroll => "scroll",
            Self::Video => "video",
        }
    }
}

struct Args {
    frames: usize,
    bitrate: u32,
    codec: String,
    scenarios: Vec<Scenario>,
    ffmpeg: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    save: bool,
    dump_au: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            frames: 120,
            bitrate: 4_000_000,
            codec: "hevc_nvenc".into(),
            scenarios: vec![Scenario::Video],
            ffmpeg: None,
            width: 1920,
            height: 1080,
            fps: 30,
            save: false,
            dump_au: 0,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{arg} 需要参数"));
        match arg.as_str() {
            "--frames" | "-n" => a.frames = next()?.parse().map_err(|_| "--frames 需要整数")?,
            "--bitrate" | "-b" => {
                a.bitrate = next()?.parse().map_err(|_| "--bitrate 需要整数(bps)")?
            }
            "--codec" | "-c" => {
                let c = next()?;
                a.codec = match c.as_str() {
                    "hevc" | "h265" => "hevc_nvenc".into(),
                    "h264" | "avc" => "h264_nvenc".into(),
                    _ => c,
                };
            }
            "--scenario" | "-s" => {
                let s = next()?;
                if s == "all" {
                    a.scenarios = vec![
                        Scenario::Static,
                        Scenario::Typing,
                        Scenario::Scroll,
                        Scenario::Video,
                    ];
                } else {
                    a.scenarios = vec![Scenario::parse(&s).ok_or("未知 scenario")?];
                }
            }
            "--ffmpeg" => a.ffmpeg = Some(next()?),
            "--width" => a.width = next()?.parse().map_err(|_| "--width 需要整数")?,
            "--height" => a.height = next()?.parse().map_err(|_| "--height 需要整数")?,
            "--fps" => a.fps = next()?.parse().map_err(|_| "--fps 需要整数")?,
            "--save" => a.save = true,
            "--dump-au" => a.dump_au = next()?.parse().map_err(|_| "--dump-au 需要整数")?,
            "--help" | "-h" => {
                println!("lrc-video-probe --frames N --bitrate B --codec hevc_nvenc|h264_nvenc [--scenario static|typing|scroll|video|all] [--width W --height H] [--fps N] [--ffmpeg path] [--save]");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    Ok(a)
}

/// 合成/加载底图 + 按场景生成帧。
struct Frames {
    w: u32,
    h: u32,
    base: Vec<u8>,
    out: Vec<u8>,
    col: Vec<i32>,
    row: Vec<i32>,
}

impl Frames {
    fn new(w: u32, h: u32) -> Self {
        let mut f = Self {
            w,
            h,
            base: Vec::new(),
            out: vec![0u8; w as usize * h as usize * 4],
            col: Vec::with_capacity(w as usize),
            row: Vec::with_capacity(h as usize),
        };
        f.base = f.load_base();
        f
    }

    fn load_base(&self) -> Vec<u8> {
        let want = self.w as usize * self.h as usize * 4;
        if let Ok(data) = std::fs::read(BASE_PATH) {
            if data.len() == want {
                return data;
            }
        }
        // 合成一张"桌面感"的底图：渐变 + 几条窗口边 + 文字行
        let mut v = vec![0u8; want];
        let w = self.w as usize;
        for y in 0..self.h as usize {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let b = (40 + (x * 90 / w)) as u8;
                let g = (45 + (y * 100 / self.h as usize)) as u8;
                let r = (55 + ((x + y) % 60)) as u8;
                v[i] = b;
                v[i + 1] = g;
                v[i + 2] = r;
                v[i + 3] = 255;
            }
        }
        // 一块"窗口"
        for y in 100..600usize {
            for x in 200..1400usize {
                let i = (y * w + x) * 4;
                v[i] = 240;
                v[i + 1] = 240;
                v[i + 2] = 245;
            }
        }
        // "文字"行
        for line in 0..12usize {
            let y0 = 140 + line * 30;
            for x in 0..900usize {
                if (x + line * 7) % 5 == 0 {
                    continue;
                }
                for dy in 0..12usize {
                    let i = ((y0 + dy) * w + 220 + x) * 4;
                    if i + 3 < v.len() {
                        v[i] = 30;
                        v[i + 1] = 30;
                        v[i + 2] = 30;
                    }
                }
            }
        }
        v
    }

    /// 真实场景更像"桌面上有个视频窗口在播"：底色是静态桌面，只有一块区域全帧变化。
    fn fill_video(&mut self, t: usize) {
        let w = self.w as usize;
        let h = self.h as usize;
        // 先把静态桌面铺上
        self.out.copy_from_slice(&self.base);
        let tf = t as i32;
        // 列/行 LUT：避免 200 万次 sin
        self.col.clear();
        for x in 0..w {
            let v = ((x as i32 * 13 + tf * 7) / 61 % 128) - 64;
            self.col.push(v);
        }
        self.row.clear();
        for y in 0..h {
            let v = ((y as i32 * 17 - tf * 5) / 71 % 128) - 64;
            self.row.push(v);
        }
        // 视频窗口区域（约占屏幕 40%），全帧变化
        let (vx0, vy0) = (300usize, 120usize);
        let (vx1, vy1) = ((vx0 + 1280).min(w), (vy0 + 720).min(h));
        let out = &mut self.out;
        for y in vy0..vy1 {
            let ry = self.row[y];
            for x in vx0..vx1 {
                let v = ry * self.col[x] / 64;
                let i = (y * w + x) * 4;
                out[i] = (100 + v).clamp(0, 255) as u8;
                out[i + 1] = (110 + ry + v / 2).clamp(0, 255) as u8;
                out[i + 2] = (140 + v / 3).clamp(0, 255) as u8;
            }
        }
        // 窗口里两条移动色块
        for k in 0..2usize {
            let bx = vx0 + ((t * (17 + k * 11) + k * 300) % (vx1 - vx0).saturating_sub(260).max(1));
            let by = vy0 + ((t * (9 + k * 5) + k * 170) % (vy1 - vy0).saturating_sub(180).max(1));
            let col = [
                (60 + k * 40) as u8,
                (200 - k * 30) as u8,
                (120 + k * 20) as u8,
            ];
            for y in by..(by + 180).min(vy1) {
                for x in bx..(bx + 260).min(vx1) {
                    let i = (y * w + x) * 4;
                    out[i] = col[0];
                    out[i + 1] = col[1];
                    out[i + 2] = col[2];
                }
            }
        }
    }

    fn next(&mut self, i: usize, scenario: Scenario) -> &[u8] {
        let w = self.w as usize;
        let h = self.h as usize;
        match scenario {
            Scenario::Static => &self.base,
            Scenario::Typing => {
                self.out.copy_from_slice(&self.base);
                // 一个 6x14 的"插入符"在几行文字上跳
                let col_x = 220 + (i * 7) % 700;
                let line_y = 140 + ((i / 7) % 12) * 30;
                for dy in 0..14usize.min(h) {
                    for dx in 0..6usize {
                        let x = col_x + dx;
                        let y = line_y + dy;
                        if x < w && y < h {
                            let idx = (y * w + x) * 4;
                            self.out[idx] = 20;
                            self.out[idx + 1] = 20;
                            self.out[idx + 2] = 230;
                        }
                    }
                }
                &self.out
            }
            Scenario::Scroll => {
                let shift = (i * 4) % h.max(1);
                for y in 0..h {
                    let src = ((y + shift) % h) * w * 4;
                    let dst = y * w * 4;
                    self.out[dst..dst + w * 4].copy_from_slice(&self.base[src..src + w * 4]);
                }
                &self.out
            }
            Scenario::Video => {
                self.fill_video(i);
                &self.out
            }
        }
    }
}

fn psnr_db(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    let mut se: u64 = 0;
    let mut cnt: u64 = 0;
    // 只比 RGB，跳过 alpha（NVENC 不支持 BGRA，alpha 必然丢）
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        if cnt * 4 >= n as u64 * 4 {
            break;
        }
        for c in 0..3 {
            let d = i64::from(pa[c]) - i64::from(pb[c]);
            se = se.saturating_add((d * d) as u64);
        }
        cnt += 3;
    }
    if cnt == 0 {
        return 0.0;
    }
    let mse = se as f64 / cnt as f64;
    if mse <= 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn pct(v: &mut [u64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    let idx = ((v.len() as f64 - 1.0) * p / 100.0).round() as usize;
    v[idx.min(v.len() - 1)] as f64
}

fn avg(v: &[u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<u64>() as f64 / v.len() as f64
}

struct Row {
    scenario: &'static str,
    frames: usize,
    #[allow(dead_code)]
    bytes: u64,
    kbps: f64,
    enc_avg: f64,
    enc_p95: f64,
    dec_avg: f64,
    dec_p95: f64,
    e2e_p50: f64,
    e2e_p95: f64,
    psnr: f64,
    dropped: usize,
    keyframes: usize,
}

fn main() {
    env_logger::init();
    // --probe：只做可用性探测，然后退出
    if std::env::args().any(|a| a == "--probe") {
        let ffmpeg = std::env::args().skip_while(|a| a != "--ffmpeg").nth(1);
        for codec in ["hevc_nvenc", "h264_nvenc", "hevc", "h264"] {
            let t0 = Instant::now();
            match lrc_video::probe_with_ffmpeg(codec, ffmpeg.as_deref()) {
                Ok(info) => println!(
                    "probe {codec:<12} available={:<5} {:>6}us  ffmpeg={}  detail={}",
                    info.available,
                    t0.elapsed().as_micros(),
                    info.ffmpeg,
                    if info.available {
                        "OK".to_string()
                    } else {
                        info.detail.replace('\n', " | ")
                    }
                ),
                Err(e) => println!("probe {codec:<12} ERROR: {e}"),
            }
        }
        return;
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[lrc-video-probe] 参数错误: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("[lrc-video-probe] 失败: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), Box<dyn Error>> {
    let ffmpeg = resolve_ffmpeg(args.ffmpeg.as_deref());
    println!("[lrc-video-probe] ffmpeg = {}", ffmpeg.display());
    println!(
        "[lrc-video-probe] {}x{} @{}fps  codec={}  bitrate={} bps  frames/场景={}",
        args.width, args.height, args.fps, args.codec, args.bitrate, args.frames
    );

    let enc_cfg = EncoderConfig {
        width: args.width,
        height: args.height,
        bitrate_bps: args.bitrate,
        gop: args.fps * 2,
        fps: args.fps,
        codec: args.codec.clone(),
        ffmpeg_path: Some(ffmpeg.to_string_lossy().into_owned()),
    };
    let dec_cfg = DecoderConfig {
        width: args.width,
        height: args.height,
        codec: if args.codec.contains("hevc") { "hevc".into() } else { "h264".into() },
        ffmpeg_path: Some(ffmpeg.to_string_lossy().into_owned()),
    };
    println!();
    println!("[编码命令] {}", encode_command_line(&enc_cfg, &ffmpeg).join(" "));
    println!("[解码命令] {}", decode_command_line(&dec_cfg.codec, &ffmpeg).join(" "));
    println!();

    let mut enc = FfmpegEncoder::new(enc_cfg)?;
    let mut dec = FfmpegDecoder::new(&dec_cfg)?;

    let mut frames = Frames::new(args.width, args.height);
    let mut rows: Vec<Row> = Vec::new();
    let mut saved = false;

    for scenario in &args.scenarios {
        let mut enc_us: Vec<u64> = Vec::new();
        let mut dec_us: Vec<u64> = Vec::new();
        let mut e2e_us: Vec<u64> = Vec::new();
        let mut bytes_total: u64 = 0;
        let mut dropped = 0usize;
        let mut keyframes = 0usize;
        let mut psnr_sum = 0.0f64;
        let mut psnr_n = 0usize;

        for i in 0..args.frames {
            let src = frames.next(i, *scenario).to_vec();
            let t0 = Instant::now();
            let aus = enc.encode(&src, args.width as usize * 4)?;
            let t1 = Instant::now();
            if enc.last_au_was_keyframe() {
                keyframes += 1;
            }
            let mut decoded = None;
            for (k, au) in aus.iter().enumerate() {
                if i < args.dump_au {
                    let _ = std::fs::create_dir_all("D:/lrc/target/aus");
                    let _ = std::fs::write(format!("D:/lrc/target/aus/{i:04}_{k}.hevc"), au);
                }
                bytes_total = bytes_total.saturating_add(au.len() as u64);
                match dec.decode(au) {
                    Ok(Some(f)) => decoded = Some(f),
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("[lrc-video-probe] 解码失败（第 {i} 帧）: {e}");
                        dropped += 1;
                    }
                }
            }
            let t2 = Instant::now();
            enc_us.push((t1 - t0).as_micros() as u64);
            dec_us.push((t2 - t1).as_micros() as u64);
            e2e_us.push((t2 - t0).as_micros() as u64);
            match decoded {
                Some(f) => {
                    let p = psnr_db(&src, &f);
                    psnr_sum += p;
                    psnr_n += 1;
                    if args.save && !saved {
                        let _ = std::fs::write("D:/lrc/captures/video-decoded.bgra", &f);
                        saved = true;
                    }
                }
                None => dropped += 1,
            }
        }

        let dur_s = args.frames as f64 / args.fps.max(1) as f64;
        let row = Row {
            scenario: scenario.name(),
            frames: args.frames,
            bytes: bytes_total,
            kbps: bytes_total as f64 * 8.0 / dur_s / 1000.0,
            enc_avg: avg(&enc_us) / 1000.0,
            enc_p95: pct(&mut enc_us, 95.0) / 1000.0,
            dec_avg: avg(&dec_us) / 1000.0,
            dec_p95: pct(&mut dec_us, 95.0) / 1000.0,
            e2e_p50: pct(&mut e2e_us, 50.0) / 1000.0,
            e2e_p95: pct(&mut e2e_us, 95.0) / 1000.0,
            psnr: if psnr_n > 0 { psnr_sum / psnr_n as f64 } else { 0.0 },
            dropped,
            keyframes,
        };
        println!(
            "[{}] kbps={:.0} enc={:.1}/{:.1}ms dec={:.1}/{:.1}ms e2e p50={:.1} p95={:.1}ms psnr={:.1}dB dropped={} keyframes={}",
            row.scenario, row.kbps, row.enc_avg, row.enc_p95, row.dec_avg, row.dec_p95,
            row.e2e_p50, row.e2e_p95, row.psnr, row.dropped, row.keyframes
        );
        rows.push(row);
    }

    enc.shutdown();
    dec.shutdown();

    println!();
    println!(
        "| {:<7} | {:>6} | {:>12} | {:>10} | {:>8} | {:>14} | {:>14} | {:>13} | {:>7} | {:>7} |",
        "scene", "frames", "codec", "bitrate", "kbps", "enc ms avg/p95", "dec ms avg/p95", "e2e ms p50/p95", "psnr", "dropped"
    );
    for r in &rows {
        println!(
            "| {:<7} | {:>6} | {:>12} | {:>10} | {:>8.0} | {:>14} | {:>14} | {:>13} | {:>7.1} | {:>7} |",
            r.scenario,
            r.frames,
            args.codec,
            format!("{}k", args.bitrate / 1000),
            r.kbps,
            format!("{:.1}/{:.1}", r.enc_avg, r.enc_p95),
            format!("{:.1}/{:.1}", r.dec_avg, r.dec_p95),
            format!("{:.1}/{:.1}", r.e2e_p50, r.e2e_p95),
            r.psnr,
            r.dropped
        );
    }
    Ok(())
}
