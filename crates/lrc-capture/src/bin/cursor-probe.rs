//! 光标采集自测：位置/可见性/形状位图 + 变化检测。
//!
//! 用法：cargo run --release -p lrc-capture --bin cursor-probe -- [--seconds 5] [--interval-ms 16] [--move] [--dump 3]
//!
//! --move：用 SetCursorPos 做一次脚本化移动（会真的动一下鼠标），
//!         用来验证"动了 → Some、不动 → None"。

use std::sync::Arc;
use std::time::{Duration, Instant};

use lrc_capture::{CursorShape, CursorWatcher};

struct Args {
    seconds: f64,
    interval_ms: u64,
    do_move: bool,
    dump: u64,
    art: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seconds: 5.0,
            interval_ms: 16,
            do_move: false,
            dump: 3,
            art: false,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" | "-s" => {
                a.seconds = it
                    .next()
                    .ok_or("--seconds 需要参数")?
                    .parse()
                    .map_err(|_| "--seconds 需要数字")?;
            }
            "--interval-ms" | "-i" => {
                a.interval_ms = it
                    .next()
                    .ok_or("--interval-ms 需要参数")?
                    .parse()
                    .map_err(|_| "--interval-ms 需要整数")?;
            }
            "--dump" => {
                a.dump = it
                    .next()
                    .ok_or("--dump 需要参数")?
                    .parse()
                    .map_err(|_| "--dump 需要整数")?;
            }
            "--move" => a.do_move = true,
            "--art" => a.art = true,
            "--help" | "-h" => {
                println!("cursor-probe [--seconds N] [--interval-ms N] [--move] [--dump N] [--art]");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    if a.seconds <= 0.0 {
        return Err("--seconds 必须 > 0".into());
    }
    Ok(a)
}

fn main() {
    // 必须先设：否则 GetCursorInfo 返回 DPI 虚拟化后的逻辑坐标，和采集画面像素对不上
    lrc_capture::set_dpi_aware();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[cursor-probe] 参数错误: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("[cursor-probe] 失败: {e}");
        std::process::exit(1);
    }
}

/// 当前进程的 GDI 对象数（验证 GetIconInfo 的位图没泄漏）。
fn gdi_objects() -> u32 {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS};
    unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) }
}

fn shape_digest(s: &CursorShape) -> String {
    let opaque = s.bgra.chunks_exact(4).filter(|p| p[3] != 0).count();
    format!(
        "{}x{} hotspot=({},{}) 不透明像素={}",
        s.width, s.height, s.hotspot_x, s.hotspot_y, opaque
    )
}

/// 把光标位图的 alpha 画成 ASCII，肉眼确认"形状真的是个箭头/I 字"。
fn print_art(s: &CursorShape) {
    if s.width > 64 || s.height > 64 {
        return;
    }
    println!(
        "  [art] {}x{} hotspot=({},{})",
        s.width, s.height, s.hotspot_x, s.hotspot_y
    );
    for y in 0..s.height as usize {
        let mut line = String::with_capacity(s.width as usize);
        for x in 0..s.width as usize {
            let a = s.bgra.get((y * s.width as usize + x) * 4 + 3).copied().unwrap_or(0);
            line.push(if a >= 192 {
                '#'
            } else if a >= 32 {
                '+'
            } else {
                ' '
            });
        }
        println!("  |{line}|");
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut watcher = CursorWatcher::new();
    let gdi_before = gdi_objects();
    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds);
    let step = Duration::from_millis(args.interval_ms.max(1));

    let mut polls: u64 = 0;
    let mut somes: u64 = 0;
    let mut nones: u64 = 0;
    let mut none_run: u64 = 0;
    let mut max_none_run: u64 = 0;
    let mut dump_left = args.dump;
    let mut last_shape: Option<Arc<CursorShape>> = None;
    let mut distinct_pos: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let mut printed = 0u64;

    // 可选：脚本化移动鼠标（验证"动了才有事件"）
    let mut move_plan: Vec<(i32, i32)> = Vec::new();
    if args.do_move {
        let start = Instant::now();
        // 先取当前位置做基点
        let mut ci = windows::Win32::UI::WindowsAndMessaging::CURSORINFO {
            cbSize: std::mem::size_of::<windows::Win32::UI::WindowsAndMessaging::CURSORINFO>()
                as u32,
            ..Default::default()
        };
        let base = if unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetCursorInfo(&mut ci)
        }
        .is_ok()
        {
            (ci.ptScreenPos.x, ci.ptScreenPos.y)
        } else {
            (200, 200)
        };
        // 200ms 后挪 3 个位置，每个停 400ms，然后再回到基点停 1.5s
        for (i, (dx, dy)) in [(0, 0), (120, 60), (240, 120), (0, 0)].iter().enumerate() {
            let _ = i;
            move_plan.push((base.0 + dx, base.1 + dy));
        }
        let _ = start;
    }

    let t_start = Instant::now();
    let mut move_idx = 0usize;
    let mut next_move_at = Duration::from_millis(300);

    while Instant::now() < deadline {
        // 移动脚本：每 400ms 一步
        if args.do_move && move_idx < move_plan.len() && t_start.elapsed() >= next_move_at {
            let (x, y) = move_plan[move_idx];
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y)
            };
            move_idx += 1;
            next_move_at += Duration::from_millis(400);
        }

        polls += 1;
        let t0 = Instant::now();
        let state = watcher.poll();
        let us = t0.elapsed().as_micros() as u64;

        match state {
            Some(s) => {
                somes += 1;
                none_run = 0;
                distinct_pos.insert((s.x, s.y));
                let shape_changed = s
                    .shape
                    .as_ref()
                    .map(|sh| {
                        last_shape
                            .as_ref()
                            .map(|prev| !std::sync::Arc::ptr_eq(prev, sh))
                            .unwrap_or(true)
                    })
                    .unwrap_or(false);
                let shape_txt = match &s.shape {
                    Some(sh) => format!(
                        "shape[{}] {}",
                        if shape_changed { "换" } else { "复用" },
                        shape_digest(sh)
                    ),
                    None => "shape=None".to_string(),
                };
                if args.art && shape_changed {
                    if let Some(sh) = &s.shape {
                        print_art(sh);
                    }
                }
                if let Some(sh) = &s.shape {
                    last_shape = Some(sh.clone());
                }
                if dump_left > 0 {
                    dump_left -= 1;
                    println!(
                        "  poll#{polls} Some: pos=({},{}) visible={} {shape_txt} ({}us)",
                        s.x, s.y, s.visible, us
                    );
                } else if printed < 6 && s.shape.is_some() {
                    printed += 1;
                }
            }
            None => {
                nones += 1;
                none_run += 1;
                max_none_run = max_none_run.max(none_run);
            }
        }

        std::thread::sleep(step);
    }

    let st = watcher.stats();
    let elapsed = t_start.elapsed().as_secs_f64();
    println!();
    println!("==== cursor-probe 汇总 ({elapsed:.2}s, 轮询间隔 {}ms) ====", args.interval_ms);
    println!(
        "poll()             : {polls}  ({} 次/秒)",
        (polls as f64 / elapsed.max(1e-9)) as u64
    );
    println!(
        "返回 Some（有变化） : {somes}  ({:.1}%)",
        somes as f64 * 100.0 / polls.max(1) as f64
    );
    println!(
        "返回 None（无变化） : {nones}  ({:.1}%)  最长连续 None = {max_none_run}",
        nones as f64 * 100.0 / polls.max(1) as f64
    );
    println!("不同光标位置        : {}", distinct_pos.len());
    println!(
        "抓位图次数          : {}  最近一次 {}us",
        st.shape_captures, st.last_shape_capture_us
    );
    println!("watcher 内部 changes: {}", st.changes);
    let gdi_after = gdi_objects();
    println!(
        "GDI 对象泄漏检查    : 开始 {gdi_before} → 结束 {gdi_after}  (抓了 {} 次位图 / 每次都建 2 个 bitmap + 1 个 icon)",
        st.shape_captures
    );
    if let Some(sh) = &last_shape {
        println!("最后形状            : {}", shape_digest(sh));
    } else {
        println!("最后形状            : 还没抓到（可能光标不可见/无形状）");
    }
    Ok(())
}
