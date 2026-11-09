//! X11 环境体检 + 采集/注入自测。
//!
//! ```text
//! x11-probe [--save out.bgra] [--frames 5] [--move] [--click] [--dump]
//! ```
//!
//! 用法（yanfa2 上）：
//! ```text
//! DISPLAY=:1 XAUTHORITY=/run/user/1000/gdm/Xauthority ./x11-probe --frames 30 --save /tmp/frame.bgra --dump
//! ```

use std::process::ExitCode;
use std::time::Instant;

use lrc_linux::capture::X11Capturer;
use lrc_linux::display;
use lrc_linux::input;

struct Args {
    frames: u32,
    save: Option<String>,
    do_move: bool,
    do_click: bool,
    dump: bool,
    warmup: u32,
    cursor: bool,
    sweep: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            frames: 5,
            save: None,
            do_move: false,
            do_click: false,
            dump: false,
            warmup: 1,
            cursor: false,
            sweep: false,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" | "-n" => {
                a.frames = it
                    .next()
                    .ok_or("--frames 需要参数")?
                    .parse()
                    .map_err(|_| "--frames 需要整数")?;
            }
            "--save" | "-s" => a.save = Some(it.next().ok_or("--save 需要路径")?),
            "--warmup" => {
                a.warmup = it
                    .next()
                    .ok_or("--warmup 需要参数")?
                    .parse()
                    .map_err(|_| "--warmup 需要整数")?;
            }
            "--move" => a.do_move = true,
            "--click" => a.do_click = true,
            "--dump" => a.dump = true,
            "--cursor" => a.cursor = true,
            "--sweep" => a.sweep = true,
            "-h" | "--help" => {
                println!("x11-probe [--frames N] [--save out.bgra] [--move] [--click] [--dump] [--cursor] [--warmup N]");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    if a.frames == 0 {
        return Err("--frames 必须 > 0".to_string());
    }
    Ok(a)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("x11-probe: {e}");
            return ExitCode::from(2);
        }
    };

    let disp = display::display_env();
    println!("DISPLAY={}", if disp.is_empty() { "(未设置)" } else { &disp });

    let info = match display::query() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("x11-probe: {e}");
            eprintln!("提示：X11 会话需要 DISPLAY（本机是 :1）和 XAUTHORITY（/run/user/1000/gdm/Xauthority）");
            return ExitCode::from(1);
        }
    };
    println!("屏幕: {info}");

    let mut cap = match X11Capturer::new(info) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("x11-probe: {e}");
            return ExitCode::from(1);
        }
    };

    println!("采集路径: {}", cap.path_name());

    // 预热（第一次 GetImage 会有一次额外的服务器往返）
    for _ in 0..args.warmup {
        if let Err(e) = cap.capture() {
            eprintln!("x11-probe: 预热采集失败: {e}");
            return ExitCode::from(1);
        }
    }

    let mut durations: Vec<u128> = Vec::with_capacity(args.frames as usize);
    let mut last = None;
    for _ in 0..args.frames {
        let t0 = Instant::now();
        match cap.capture() {
            Ok(f) => {
                durations.push(t0.elapsed().as_micros());
                last = Some(f);
            }
            Err(e) => {
                eprintln!("x11-probe: 采集失败: {e}");
                return ExitCode::from(1);
            }
        }
    }

    let Some(frame) = last else {
        eprintln!("x11-probe: 没有采集到任何帧");
        return ExitCode::from(1);
    };

    let n = durations.len() as f64;
    let sum: u128 = durations.iter().sum();
    let avg_ms = sum as f64 / n / 1000.0;
    let max_ms = durations.iter().copied().max().unwrap_or(0) as f64 / 1000.0;
    let min_ms = durations.iter().copied().min().unwrap_or(0) as f64 / 1000.0;
    println!(
        "采集 {} 帧: avg {:.2} ms  min {:.2} ms  max {:.2} ms  ({:.0} fps 上限)",
        args.frames,
        avg_ms,
        min_ms,
        max_ms,
        1000.0 / avg_ms.max(0.001)
    );
    println!("帧: {frame:?}");

    // alpha 抽样：应全 255
    let total_px = frame.bgra.len() / 4;
    let step = (total_px / 4096).max(1);
    let mut bad_alpha = 0usize;
    let mut i = 0usize;
    while i < total_px {
        if frame.bgra[i * 4 + 3] != 255 {
            bad_alpha += 1;
        }
        i += step;
    }
    println!(
        "alpha 抽样 {}/{} 正确{}",
        total_px.div_ceil(step) - bad_alpha,
        total_px.div_ceil(step),
        if bad_alpha == 0 { "（全 255 ✅）" } else { "（有非 255！）" }
    );

    if args.dump {
        let head: Vec<String> = frame.bgra.iter().take(32).map(|b| format!("{b:02X}")).collect();
        println!("前 32 字节: {}", head.join(" "));
        // 取几个像素看 BGRA 顺序
        let pts = [
            (0usize, 0usize),
            (frame.width as usize / 2, frame.height as usize / 2),
            (frame.width as usize - 1, frame.height as usize - 1),
        ];
        for (px, py) in pts {
            let off = (py * frame.width as usize + px) * 4;
            if off + 3 < frame.bgra.len() {
                println!(
                    "像素({px},{py}) = B:{} G:{} R:{} A:{}",
                    frame.bgra[off],
                    frame.bgra[off + 1],
                    frame.bgra[off + 2],
                    frame.bgra[off + 3]
                );
            }
        }
    }

    if let Some(path) = args.save.as_deref() {
        match std::fs::write(path, &frame.bgra) {
            Ok(()) => println!("已保存 {path}（{} 字节，{}x{} BGRA）", frame.bgra.len(), frame.width, frame.height),
            Err(e) => {
                eprintln!("x11-probe: 保存 {path} 失败: {e}");
                return ExitCode::from(1);
            }
        }
    }

    if args.do_move || args.do_click {
        let cx = (frame.width / 2) as i32;
        let cy = (frame.height / 2) as i32;
        let before = input::pointer_pos();
        println!("注入前指针: {before:?}");

        for (tx, ty) in [(cx, cy), (cx + 120, cy + 80), (cx, cy)] {
            input::send_mouse_move(tx, ty);
            std::thread::sleep(std::time::Duration::from_millis(120));
            let now = input::pointer_pos();
            let ok = now == Some((tx, ty));
            println!("  移动 -> ({tx},{ty}) 实测 {now:?} {}", if ok { "✅" } else { "❌" });
        }

        if args.do_click {
            println!("注入测试：左键单击 ({cx},{cy})");
            input::send_mouse_button(1, true);
            std::thread::sleep(std::time::Duration::from_millis(40));
            input::send_mouse_button(1, false);
            // 滚轮也顺手验一下（不会报错即为通路正常）
            input::send_wheel(1);
            input::send_wheel(-1);
        }
        let after = input::pointer_pos();
        println!("注入后指针: {after:?}");
    }

    if args.cursor {
        run_cursor_test(frame.width, frame.height);
    }
    if args.sweep {
        run_shape_sweep(frame.width, frame.height);
    }

    println!("x11-probe: OK");
    ExitCode::SUCCESS
}

/// 光标形状变化自测：在屏幕上扫一遍网格，打印每次形状变化的位置。
///
/// X11 没有"强制设置光标"的公开接口，所以只能靠移动到不同窗口（文本框 I-beam、
/// 桌面/面板箭头、边框调整光标…）来触发形状变化。
fn run_shape_sweep(width: u32, height: u32) {
    use lrc_linux::cursor::CursorWatcher;

    println!("--- 光标形状扫描（移动网格，看形状何时变化）---");
    let mut w = match CursorWatcher::new(None) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("x11-probe: {e}");
            return;
        }
    };
    let mut cur: Option<std::sync::Arc<lrc_linux::cursor::CursorShape>> = None;
    let mut changes = 0;
    let mut last_sig: Option<(u32, u32, i32, i32)> = None;

    let xs = [60i32, 200, 480, 800, 1200, 1600, 1860, 2100, 2500, 3000, 3400, 3800];
    let ys = [30i32, 120, 400, 700, 1000];
    for &y in &ys {
        for &x in &xs {
            let tx = x.min(width as i32 - 1);
            let ty = y.min(height as i32 - 1);
            lrc_linux::input::send_mouse_move(tx, ty);
            std::thread::sleep(std::time::Duration::from_millis(25));
            for _ in 0..4 {
                if let Some(st) = w.poll() {
                    if let Some(sh) = st.shape {
                        let sig = (sh.width, sh.height, sh.hotspot_x, sh.hotspot_y);
                        if Some(sig) != last_sig {
                            changes += 1;
                            println!(
                                "  形状变化 #{} @ ({tx},{ty}): {}x{} hotspot=({},{})",
                                changes, sig.0, sig.1, sig.2, sig.3
                            );
                            last_sig = Some(sig);
                        }
                        cur = Some(sh);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        }
    }
    let st = w.stats();
    match cur {
        Some(sh) => println!(
            "扫描结束: 形状变化 {changes} 次，最后一次 {}x{}；shape_captures={} polls={} changes={}",
            sh.width, sh.height, st.shape_captures, st.polls, st.changes
        ),
        None => println!("扫描结束: 没拿到任何形状"),
    }
}

/// 光标自测：位置变化 / 形状变化 / 不动时不返回。
fn run_cursor_test(width: u32, height: u32) {
    use lrc_linux::cursor::CursorWatcher;

    println!("--- 光标（XFixes GetCursorImage）---");
    let mut w = match CursorWatcher::new(None) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("x11-probe: {e}");
            return;
        }
    };

    // 1) 第一次轮询必然带形状
    let mut quiet = 0;
    let mut first = None;
    for _ in 0..20 {
        match w.poll() {
            Some(st) => {
                if first.is_none() {
                    first = Some(st.clone());
                }
                quiet = 0;
            }
            None => {
                quiet += 1;
                if quiet >= 3 && first.is_some() {
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    match first.as_ref().and_then(|s| s.shape.as_ref()) {
        Some(sh) => println!(
            "首次轮询: pos=({},{}) visible={} 形状 {}x{} hotspot=({},{}) 位图 {} 字节 抓图 {:.0} us",
            first.as_ref().map(|s| s.x).unwrap_or(0),
            first.as_ref().map(|s| s.y).unwrap_or(0),
            first.as_ref().map(|s| s.visible).unwrap_or(false),
            sh.width,
            sh.height,
            sh.hotspot_x,
            sh.hotspot_y,
            sh.bgra.len(),
            w.stats().last_shape_capture_us as f64
        ),
        None => println!("首次轮询没有拿到形状（XFixes 可能不可用）"),
    }

    // 2) 不动：连续 30 次轮询应该一次都不返回（顺便量一次 poll 的往返耗时）
    let mut still_hits = 0;
    let t0 = std::time::Instant::now();
    for _ in 0..30 {
        if w.poll().is_some() {
            still_hits += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    let total_us = t0.elapsed().as_micros() as f64;
    println!("不动 30 次轮询 -> 上报 {still_hits} 次（期望 0）{}", if still_hits == 0 { " ✅" } else { " ❌" });
    println!(
        "poll 成本: 30 次轮询总 {:.1} ms（含 30×16ms 睡眠）→ 单次往返约 {:.0} us",
        total_us / 1000.0,
        (total_us - 30.0 * 16000.0).max(0.0) / 30.0
    );

    // 3) 移动：每次移动后应该恰好上报（带新位置）
    let targets = [
        (width as i32 / 4, height as i32 / 2),
        (width as i32 / 2, height as i32 / 3),
        (width as i32 * 3 / 4, height as i32 * 2 / 3),
    ];
    let mut move_hits = 0;
    for (tx, ty) in targets {
        lrc_linux::input::send_mouse_move(tx, ty);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut got = None;
        for _ in 0..5 {
            if let Some(st) = w.poll() {
                got = Some(st);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        match got {
            Some(st) => {
                move_hits += 1;
                let ok = st.x == tx && st.y == ty;
                println!(
                    "  移动到 ({tx},{ty}) -> 上报 pos=({},{}) 形状变化={} {}",
                    st.x,
                    st.y,
                    st.shape.is_some(),
                    if ok { "✅" } else { "❌ 位置不符" }
                );
            }
            None => println!("  移动到 ({tx},{ty}) -> **没有上报** ❌"),
        }
    }

    let st = w.stats();
    println!(
        "光标统计: polls={} changes={} shape_captures={} last_shape_us={} xfixes={}",
        st.polls, st.changes, st.shape_captures, st.last_shape_capture_us, st.xfixes
    );
    println!(
        "小结: 移动 {}/{} 次上报，不动 {} 次误报",
        move_hits,
        targets.len(),
        still_hits
    );
}
