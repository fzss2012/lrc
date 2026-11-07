//! DXGI 采集自测：枚举输出 / 帧率 / 采集耗时 / dirty rect 统计 / 静止时 None 计数。
//!
//! 用法：
//!   cargo run --release -p lrc-capture --bin capture-probe -- [--display N] [--seconds 3]
//!        [--timeout 16] [--out D:/lrc/captures/frame0.bgra] [--no-save]
//!
//! 退出码：0 正常，1 初始化/采集失败（会打印确切 HRESULT）。

use std::time::{Duration, Instant};

use lrc_capture::{hash_block, list_outputs, CaptureError, Capturer, Frame};
use lrc_proto::Grid;

const DEFAULT_OUT: &str = "D:/lrc/captures/frame0.bgra";
const BLOCK_SIZE: u16 = 64;

struct Args {
    display: usize,
    seconds: f64,
    timeout_ms: u32,
    out: String,
    save: bool,
    dump: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            display: 0,
            seconds: 3.0,
            timeout_ms: 16,
            out: DEFAULT_OUT.to_string(),
            save: true,
            dump: 0,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--display" | "-d" => {
                a.display = it
                    .next()
                    .ok_or("--display 需要参数")?
                    .parse()
                    .map_err(|_| "--display 需要整数")?;
            }
            "--seconds" | "-s" => {
                a.seconds = it
                    .next()
                    .ok_or("--seconds 需要参数")?
                    .parse()
                    .map_err(|_| "--seconds 需要数字")?;
            }
            "--timeout" | "-t" => {
                a.timeout_ms = it
                    .next()
                    .ok_or("--timeout 需要参数")?
                    .parse()
                    .map_err(|_| "--timeout 需要整数毫秒")?;
            }
            "--out" | "-o" => {
                a.out = it.next().ok_or("--out 需要参数")?;
            }
            "--no-save" => a.save = false,
            "--dump" => {
                a.dump = it
                    .next()
                    .ok_or("--dump 需要参数")?
                    .parse()
                    .map_err(|_| "--dump 需要整数（打印前 N 帧的 DXGI 细节）")?;
            }
            "--help" | "-h" => {
                println!(
                    "capture-probe [--display N] [--seconds N] [--timeout MS] [--out PATH] [--no-save] [--dump N]"
                );
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
    // 与生产端一致：物理像素
    dpi_aware();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[capture-probe] 参数错误: {e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&args) {
        eprintln!("[capture-probe] 失败: {e}");
        std::process::exit(1);
    }
}

fn dpi_aware() {
    // 已经是 DPI aware 或老系统上会失败，都不影响采集
    lrc_capture::set_dpi_aware();
}

/// 适配器 + 每个适配器的输出（判断走的是独显还是软件/远程适配器）。
fn print_adapters() {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_REMOTE, DXGI_ADAPTER_FLAG_SOFTWARE,
        DXGI_ERROR_NOT_FOUND,
    };

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(e) => {
            println!("[capture-probe] CreateDXGIFactory1 失败: 0x{:08X}", e.code().0 as u32);
            return;
        }
    };
    let mut ai = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => {
                println!("  adapter{ai} 枚举失败: 0x{:08X}", e.code().0 as u32);
                break;
            }
        };
        let desc = match unsafe { adapter.GetDesc1() } {
            Ok(d) => d,
            Err(e) => {
                println!("  adapter{ai} GetDesc1 失败: 0x{:08X}", e.code().0 as u32);
                ai += 1;
                continue;
            }
        };
        let n = desc
            .Description
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..n]);
        let flags = desc.Flags;
        let mut tags = Vec::new();
        if flags & (DXGI_ADAPTER_FLAG_REMOTE.0 as u32) != 0 {
            tags.push("REMOTE");
        }
        if flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
            tags.push("SOFTWARE");
        }
        println!(
            "  adapter{ai} vendor=0x{:04X} device=0x{:04X} vram={}MB flags=0x{flags:X}{} : {name}",
            desc.VendorId,
            desc.DeviceId,
            desc.DedicatedVideoMemory / (1024 * 1024),
            if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(","))
            }
        );

        let mut oi = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => break,
            };
            if let Ok(od) = unsafe { output.GetDesc() } {
                let c = od.DesktopCoordinates;
                println!(
                    "    L adapter{ai}.output{oi} {}x{} origin=({},{}) rotation={} attached={} {:?}",
                    c.right - c.left,
                    c.bottom - c.top,
                    c.left,
                    c.top,
                    od.Rotation.0,
                    od.AttachedToDesktop.as_bool(),
                    {
                        let n = od.DeviceName.iter().position(|c| *c == 0).unwrap_or(od.DeviceName.len());
                        String::from_utf16_lossy(&od.DeviceName[..n])
                    }
                );
            }
            oi += 1;
        }
        ai += 1;
    }
}

fn print_outputs() -> Result<usize, Box<dyn std::error::Error>> {
    let outs = list_outputs()?;
    println!("[capture-probe] 显示输出 {} 个:", outs.len());
    for (i, o) in outs.iter().enumerate() {
        println!(
            "  output{i} {}x{} origin=({},{}) rotation={} portrait={}",
            o.width,
            o.height,
            o.origin_x,
            o.origin_y,
            o.rotation,
            o.is_portrait()
        );
    }
    Ok(outs.len())
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    print_adapters();
    let n = print_outputs()?;
    if n == 0 {
        return Err("没有可用输出".into());
    }
    if args.display >= n {
        return Err(format!("--display {} 越界（0..{}）", args.display, n).into());
    }

    println!(
        "[capture-probe] 打开 output{} timeout={}ms 采集 {:.1}s ...",
        args.display, args.timeout_ms, args.seconds
    );

    let mut cap = match Capturer::new(args.display, args.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[capture-probe] Capturer::new 失败: {e}");
            if let CaptureError::Init(m) = &e {
                eprintln!("[capture-probe] 详细: {m}");
            }
            return Err(e.into());
        }
    };
    let info = cap.info();
    println!(
        "[capture-probe] 采集画面 {}x{} origin=({},{}) rotation={}",
        info.width, info.height, info.origin_x, info.origin_y, info.rotation
    );

    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds);
    let mut grabs: u64 = 0;
    let mut frames: u64 = 0;
    let mut nones: u64 = 0;
    let mut full_frames: u64 = 0;
    let mut max_none_run: u64 = 0;
    let mut none_run: u64 = 0;
    let mut first_grab_us: u64 = 0;
    let mut grab_sum_us: u64 = 0;
    let mut grab_max_us: u64 = 0;
    let mut all_sum_us: u64 = 0;
    let mut none_sum_us: u64 = 0;
    let mut none_max_us: u64 = 0;
    let mut no_rect_total: u64 = 0;
    let mut dirty_total: u64 = 0;
    let mut dirty_max: usize = 0;
    let mut dirty_area_total: u64 = 0;
    let mut move_total: u64 = 0;
    let mut phase = [0u64; 5]; // meta / copy / map / blit / unmap
    let mut readback_full_n: u64 = 0;
    let mut readback_px_sum: u64 = 0;
    let mut saved = false;
    let mut last_bucket = u64::MAX;
    let t_start = Instant::now();

    while Instant::now() < deadline {
        let t0 = Instant::now();
        let res = cap.grab();
        let elapsed = t0.elapsed().as_micros() as u64;
        grabs += 1;
        all_sum_us += elapsed;

        match res {
            Ok(Some(frame)) => {
                frames += 1;
                none_run = 0;
                grab_sum_us += elapsed;
                grab_max_us = grab_max_us.max(elapsed);
                if first_grab_us == 0 {
                    first_grab_us = elapsed;
                    report_first_frame(&frame, args);
                    if args.save {
                        saved = save_frame(&frame, &args.out)?;
                    }
                }
                let dbg = cap.last_debug();
                no_rect_total = dbg.no_rect_frames;
                if dbg.readback_full {
                    readback_full_n += 1;
                }
                readback_px_sum += dbg.readback_px;
                phase[0] += dbg.meta_us;
                phase[1] += dbg.copy_us;
                phase[2] += dbg.map_us;
                phase[3] += dbg.blit_us;
                phase[4] += dbg.unmap_us;
                if frames <= args.dump {
                    let rects = frame
                        .dirty
                        .iter()
                        .take(2)
                        .map(|r| format!("({},{},{}x{})", r.x, r.y, r.w, r.h))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("      dirty: {rects}");
                    println!(
                        "  #{} acc={} meta_b={} dirty={} moves={} full={} readback={}px{} pitch={} sysmem={} | meta={}us copy={}us map={}us blit={}us unmap={}us",
                        frame_count_line(frame.ts_ms),
                        dbg.accumulated_frames,
                        dbg.total_metadata_bytes,
                        frame.dirty.len(),
                        frame.moves.len(),
                        frame.full,
                        dbg.readback_px,
                        if dbg.readback_full { "(整帧)" } else { "(脏区)" },
                        dbg.row_pitch,
                        dbg.desktop_in_system_memory,
                        dbg.meta_us,
                        dbg.copy_us,
                        dbg.map_us,
                        dbg.blit_us,
                        dbg.unmap_us
                    );
                }
                if frame.full {
                    full_frames += 1;
                }
                dirty_total += frame.dirty.len() as u64;
                dirty_max = dirty_max.max(frame.dirty.len());
                dirty_area_total += frame.dirty.iter().map(|r| r.area()).sum::<u64>();
                move_total += frame.moves.len() as u64;
            }
            Ok(None) => {
                nones += 1;
                none_run += 1;
                max_none_run = max_none_run.max(none_run);
                none_sum_us += elapsed;
                none_max_us = none_max_us.max(elapsed);
                no_rect_total = cap.last_debug().no_rect_frames;
            }
            Err(CaptureError::Lost) => {
                eprintln!("[capture-probe] DXGI_ERROR_ACCESS_LOST（需要重建 Capturer）");
                return Err("access lost".into());
            }
            Err(e) => {
                eprintln!("[capture-probe] grab 失败: {e}");
                return Err(e.into());
            }
        }

        let bucket = t_start.elapsed().as_millis() as u64 / 1000;
        if bucket != last_bucket {
            last_bucket = bucket;
            println!(
                "  [t={bucket}s] grabs={grabs} frames={frames} none={nones} cur_none_run={none_run}"
            );
        }
    }

    let wall = t_start.elapsed().as_secs_f64();
    let screen_area = u64::from(info.width) * u64::from(info.height);

    println!();
    println!("==== capture-probe 汇总 ({:.2}s wall) ====", wall);
    println!("grab() 调用        : {grabs}");
    println!(
        "产出帧             : {frames}  ({:.1} 帧/秒)",
        frames as f64 / wall.max(1e-9)
    );
    println!(
        "返回 None          : {nones}  ({:.1}%)",
        nones as f64 * 100.0 / grabs.max(1) as f64
    );
    println!("最长连续 None      : {max_none_run}  (静止时连续 {max_none_run} 次无新帧)");
    if frames > 0 {
        println!(
            "采集耗时(有帧)     : avg {:.0}us  max {}us  (首帧 {}us)",
            grab_sum_us as f64 / frames as f64,
            grab_max_us,
            first_grab_us
        );
        println!(
            "dirty rect         : 总计 {dirty_total}  avg {:.1}/帧  max {}  | 累计面积占屏 {:.1}%",
            dirty_total as f64 / frames as f64,
            dirty_max,
            if screen_area > 0 {
                dirty_area_total as f64 * 100.0 / (screen_area as f64 * frames as f64)
            } else {
                0.0
            }
        );
        println!(
            "整帧 vs 局部       : full={} 局部={}",
            full_frames,
            frames - full_frames
        );
        println!("move rect          : {move_total}");
    }
    println!(
        "grab() 平均耗时    : {:.0}us (含等待 None 的调用)",
        all_sum_us as f64 / grabs.max(1) as f64
    );
    println!(
        "  ├ 有帧 n={frames:<6} : avg {:.0}us  max {}us",
        if frames > 0 { grab_sum_us as f64 / frames as f64 } else { 0.0 },
        grab_max_us
    );
    println!(
        "  └ idle(None) n={nones:<5}: avg {:.0}us  max {}us   (--timeout {}ms)",
        if nones > 0 { none_sum_us as f64 / nones as f64 } else { 0.0 },
        none_max_us,
        args.timeout_ms
    );
    println!("no_rect_frames     : {no_rect_total}  (有元数据但解析不出矩形的帧数，应为 0)");
    if frames > 0 {
        let n = frames as f64;
        println!(
            "回读               : 整帧 {} 次 / 脏区 {} 次 | 平均回读 {:.1} 万像素/帧 ({:.1}% 屏幕)",
            readback_full_n,
            frames - readback_full_n,
            readback_px_sum as f64 / n / 10000.0,
            if screen_area > 0 {
                readback_px_sum as f64 * 100.0 / (screen_area as f64 * n)
            } else {
                0.0
            }
        );
        println!(
            "阶段耗时/帧        : meta {:.0}us  copy {:.0}us  map {:.0}us  blit {:.0}us  unmap {:.0}us",
            phase[0] as f64 / n,
            phase[1] as f64 / n,
            phase[2] as f64 / n,
            phase[3] as f64 / n,
            phase[4] as f64 / n
        );
    }
    if saved {
        println!("首帧已保存         : {}", args.out);
    }
    Ok(())
}

fn frame_count_line(ts_ms: u64) -> u64 {
    ts_ms % 100_000
}

/// 首帧：块哈希基准 + 内容摘要。
fn report_first_frame(frame: &Frame, _args: &Args) {
    let grid = Grid::new(BLOCK_SIZE, frame.width, frame.height);
    println!(
        "[capture-probe] 首帧 {}x{} stride={} full={} dirty={} moves={}",
        frame.width,
        frame.height,
        frame.stride,
        frame.full,
        frame.dirty.len(),
        frame.moves.len()
    );
    let Some(grid) = grid else {
        println!("[capture-probe] 网格无效，跳过哈希基准");
        return;
    };
    let count = grid.count();
    let mut hashes = vec![0u64; count as usize];
    let mut best = u64::MAX;
    for _ in 0..3 {
        let t0 = Instant::now();
        for idx in 0..count {
            let Some(r) = grid.block_rect(idx, frame.width, frame.height) else {
                continue;
            };
            let off = r.y as usize * frame.stride + r.x as usize * 4;
            let h = hash_block(&frame.bgra[off..], frame.stride, r.w, r.h);
            if let Some(slot) = hashes.get_mut(idx as usize) {
                *slot = h;
            }
        }
        best = best.min(t0.elapsed().as_micros() as u64);
    }
    println!(
        "[capture-probe] hash_block 全屏 {count} 块（{}x{}）: {}us",
        frame.width, frame.height, best
    );
    // 脏区场景：只哈希少量块（host 侧会按脏区限制哈希范围）
    for n in [1usize, 8, 50] {
        let n = n.min(count as usize);
        let mut best_n = u64::MAX;
        for _ in 0..5 {
            let t0 = Instant::now();
            for idx in 0..n as u32 {
                let Some(r) = grid.block_rect(idx, frame.width, frame.height) else {
                    continue;
                };
                let off = r.y as usize * frame.stride + r.x as usize * 4;
                std::hint::black_box(hash_block(&frame.bgra[off..], frame.stride, r.w, r.h));
            }
            best_n = best_n.min(t0.elapsed().as_micros() as u64);
        }
        println!(
            "[capture-probe] hash_block 仅 {n} 块: {}us ({:.1}us/块)",
            best_n,
            best_n as f64 / n.max(1) as f64
        );
    }
    let nonzero = hashes.iter().filter(|h| **h != 0).count();
    println!("[capture-probe] 非零块哈希 {nonzero}/{count}");
}

fn save_frame(frame: &Frame, path: &str) -> Result<bool, Box<dyn std::error::Error>> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // 紧凑行（stride == width*4），照着 width/height 读即可
    std::fs::write(path, &frame.bgra)?;
    Ok(true)
}
