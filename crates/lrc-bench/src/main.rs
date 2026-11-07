//! 离线真内容基准：用真实桌面帧（captures/*.bgra）跑完整的"分块→哈希差分→编码"管线，
//! 输出不同使用场景下的每帧字节数与折算码率。
//!
//! 用途：
//! 1. 在 M1 端到端跑通之前，先回答"办公场景到底要多少带宽"（对应调研报告 §5.2）
//! 2. 之后每次改编码策略，用它做回归对比
//!
//! 线上字节口径（见 docs/DESIGN.md §2.2）：
//! `bytes = 4(传输长度前缀) + 17(VideoFrame 头) + Σ(9 + 块载荷)`，
//! 脏块为 0 时不产生任何消息（0 字节）——这就是"零变化零发送"。
//!
//! 五种场景（详见 scenarios.rs，全部确定性、可复现）：
//! - static：一帧不变 → 必须 0 字节
//! - typing：600x24 文本行改 1/3 字符 + 逐帧下移 7px
//! - scroll：全宽 x 200 的条带每帧上移 8px，底部补真实帧内容
//! - full：整屏对角位移（整帧重编码的最坏情况）
//! - video：整屏平滑渐变 + 轻噪声（有损编码压力测试）
//!
//! 表格口径：
//! - 每场景先跑 1 帧建立哈希基线（相当于关键帧，不计入统计），再测 `--frames` 帧。
//! - `enc_ms_avg` 只算真实管线的「哈希差分 + encode_block」，不含解码校验和
//!   策略对比用的旁路编码。
//! - 「全无损 / 全 JPEG」两行是 bench 内复刻 codec 容器的旁路实现，运行时会与
//!   lrc-codec 的真实输出逐字节交叉校验（不一致会打印告警）。

mod frame;
mod scenarios;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use lrc_codec::{codec_tag, decode_block, encode_block, Options};
use lrc_proto::grid::Grid;

use frame::Frame;
use scenarios::{Kind, Scenario};

/// 传输层长度前缀。
const TRANSPORT_PREFIX: usize = 4;
/// VideoFrame 头：seq:u32 + flags:u8 + base_seq:u32 + block_size:u16 + blocks_x:u16 + blocks_y:u16 + count:u16。
const FRAME_HEADER: usize = 17;
/// 每个块的条目头：idx:u32 + codec:u8 + len:u32。
const ENTRY_HEADER: usize = 9;

fn main() {
    if let Err(e) = run() {
        eprintln!("lrc-bench: {e:#}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- 参数

struct Args {
    frame: PathBuf,
    width: u32,
    height: u32,
    block: u16,
    fps: u32,
    frames: u32,
    quality: u8,
    /// 无损"够好"阈值：lossless_len <= raw * ratio 就直接用无损、不再试 JPEG
    ratio_limit: f32,
    verify: bool,
    kinds: Vec<Kind>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            frame: PathBuf::from("captures/frame0.bgra"),
            width: 1920,
            height: 1080,
            block: 64,
            fps: 30,
            frames: 120,
            quality: 85,
            ratio_limit: Options::default().lossless_ratio_limit,
            verify: true,
            kinds: Kind::all().to_vec(),
        }
    }
}

fn print_usage() {
    println!(
        "lrc-bench：离线真内容带宽基准\n\
         \n\
         用法: lrc-bench [选项]\n\
         \n\
         --frame <path>     原始 BGRA 帧文件（默认 captures/frame0.bgra；找不到就用合成桌面兜底）\n\
         --width <px>       帧宽（默认 1920）\n\
         --height <px>      帧高（默认 1080）\n\
         --block <px>       块边长（默认 64）\n\
         --fps <n>          折算码率用的帧率（默认 30）\n\
         --frames <n>       每个场景测量多少帧（默认 120）\n\
         --quality <1..100> JPEG 质量（默认 85）\n\
         --ratio-limit <0..1> 无损「够好」阈值（默认 0.55；调小会更倾向 JPEG）\n\
         --scenario <list>  只跑指定场景，逗号分隔：static,typing,scroll,full,video\n\
         --no-verify        跳过逐块解码校验（跑得更快）\n\
         -h, --help         显示本帮助"
    );
}

impl Args {
    /// 返回 None 表示用户只要帮助。
    fn parse(argv: &[String]) -> Result<Option<Self>> {
        let mut args = Args::default();
        let mut i = 0usize;
        while i < argv.len() {
            let key = argv[i].as_str();
            match key {
                "--frame" => args.frame = PathBuf::from(take_value(argv, &mut i, key)?),
                "--width" => args.width = parse_num(argv, &mut i, key)?,
                "--height" => args.height = parse_num(argv, &mut i, key)?,
                "--block" => args.block = parse_num(argv, &mut i, key)?,
                "--fps" => args.fps = parse_num(argv, &mut i, key)?,
                "--frames" => args.frames = parse_num(argv, &mut i, key)?,
                "--quality" => args.quality = parse_num(argv, &mut i, key)?,
                "--ratio-limit" => args.ratio_limit = parse_num(argv, &mut i, key)?,
                "--scenario" => {
                    let raw = take_value(argv, &mut i, key)?;
                    let mut kinds = Vec::new();
                    for part in raw.split(',') {
                        let part = part.trim();
                        if part.is_empty() {
                            continue;
                        }
                        let kind = Kind::parse(part)
                            .ok_or_else(|| anyhow!("未知场景 {part}（可选 static,typing,scroll,full,video）"))?;
                        if !kinds.contains(&kind) {
                            kinds.push(kind);
                        }
                    }
                    if kinds.is_empty() {
                        bail!("--scenario 至少要给一个场景名");
                    }
                    args.kinds = kinds;
                }
                "--no-verify" => args.verify = false,
                "--help" | "-h" => {
                    print_usage();
                    return Ok(None);
                }
                other => bail!("未知参数 {other}（--help 看用法）"),
            }
            i += 1;
        }

        if args.width == 0 || args.height == 0 {
            bail!("--width/--height 必须大于 0");
        }
        if args.block == 0 {
            bail!("--block 必须大于 0");
        }
        if args.fps == 0 {
            bail!("--fps 必须大于 0");
        }
        if args.frames == 0 {
            bail!("--frames 必须大于 0");
        }
        if !(1..=100).contains(&args.quality) {
            bail!("--quality 必须在 1..=100（现在是 {}）", args.quality);
        }
        if !args.ratio_limit.is_finite() || !(0.0..=1.0).contains(&args.ratio_limit) {
            bail!(
                "--ratio-limit 必须在 0.0..=1.0（现在是 {}）",
                args.ratio_limit
            );
        }
        Ok(Some(args))
    }
}

fn take_value(argv: &[String], i: &mut usize, key: &str) -> Result<String> {
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("参数 {key} 缺少取值"))
}

fn parse_num<T: std::str::FromStr>(argv: &[String], i: &mut usize, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = take_value(argv, i, key)?;
    raw.parse::<T>()
        .map_err(|e| anyhow!("参数 {key} 取值 {raw} 不合法: {e}"))
}

// ---------------------------------------------------------------- 主流程

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(args) = Args::parse(&argv)? else {
        return Ok(());
    };
    // 日志只走 stderr，stdout 全是表格。
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .try_init();

    let (base, source, is_real) = load_base(&args)?;
    let grid = Grid::new(args.block, base.w, base.h)
        .ok_or_else(|| anyhow!("块网格参数不合法：block={} {}x{}", args.block, base.w, base.h))?;
    let opts = Options {
        jpeg_quality: args.quality,
        lossless_ratio_limit: args.ratio_limit,
        ..Options::default()
    };

    println!("=== lrc-bench：离线真内容带宽基准 ===");
    println!("帧源   : {source}");
    if !is_real {
        println!("         ⚠ 用的是合成桌面帧，数字只反映管线行为，不代表真实桌面内容");
    }
    println!(
        "网格   : {}x{} → {}x{} = {} 块",
        args.block,
        args.block,
        grid.blocks_x,
        grid.blocks_y,
        grid.count()
    );
    println!(
        "参数   : frames={} fps={} quality={} zstd_level={} ratio_limit={:.2} 校验={}",
        args.frames,
        args.fps,
        args.quality,
        opts.zstd_level,
        opts.lossless_ratio_limit,
        if args.verify { "开" } else { "关" }
    );
    println!(
        "口径   : bytes = {TRANSPORT_PREFIX}(传输长度前缀) + {FRAME_HEADER}(VideoFrame 头) + Σ({ENTRY_HEADER} + 块载荷)；脏块=0 时 0 字节"
    );

    let mut reports: Vec<ScenarioReport> = Vec::with_capacity(args.kinds.len());
    for kind in &args.kinds {
        let report = run_scenario(*kind, &base, &args, &grid, &opts)
            .with_context(|| format!("场景 {} 跑挂了", kind.name()))?;
        println!(
            "  · 跑完 {}：{} 帧，脏块均值 {:.1}，混合策略均值 {:.1} B/帧",
            kind.name(),
            report.frames,
            report.avg_changed(),
            report.avg_bytes()
        );
        reports.push(report);
    }

    print_bandwidth_table(&reports, &args);
    print_strategy_table(&reports);
    print_footnotes(&reports, &opts);
    Ok(())
}

fn load_base(args: &Args) -> Result<(Frame, String, bool)> {
    match resolve_frame_path(&args.frame) {
        Some(path) => {
            let f = Frame::from_file(&path, args.width, args.height)
                .with_context(|| format!("加载帧 {} 失败", path.display()))?;
            if frame_is_degenerate(&f) {
                // 文件在、长度也对，但内容全是 0：多半是抓帧失败留下的占位文件。
                eprintln!(
                    "警告：帧文件 {} 长度正确（{} 字节）但内容几乎全是 0，视为占位帧，改用合成桌面帧兜底",
                    path.display(),
                    f.data.len()
                );
                let synth = frame::synth_desktop(args.width, args.height)?;
                return Ok((
                    synth,
                    format!(
                        "合成桌面帧（{} 内容全 0，是占位帧；换 --frame 指向真实抓帧可得到真实数字）",
                        path.display()
                    ),
                    false,
                ));
            }
            let source = format!(
                "{}（{}x{}，{} 字节，真实桌面帧）",
                path.display(),
                f.w,
                f.h,
                f.data.len()
            );
            Ok((f, source, true))
        }
        None => {
            eprintln!(
                "警告：找不到帧文件 {}，改用合成桌面帧兜底；这些数字只用于验证管线，不代表真实桌面",
                args.frame.display()
            );
            let f = frame::synth_desktop(args.width, args.height)?;
            Ok((
                f,
                format!("合成桌面帧（未找到 {}，兜底生成）", args.frame.display()),
                false,
            ))
        }
    }
}

/// 帧内容是否"没有可用信息"：抽样看非零字节比例，低于 1% 就当占位帧（全 0 的抓帧产物）。
fn frame_is_degenerate(f: &Frame) -> bool {
    if f.data.is_empty() {
        return true;
    }
    let step = (f.data.len() / 65536).max(1);
    let mut total = 0usize;
    let mut nonzero = 0usize;
    let mut i = 0usize;
    while i < f.data.len() {
        total += 1;
        if f.data[i] != 0 {
            nonzero += 1;
        }
        i += step;
    }
    total > 0 && nonzero * 100 < total
}

/// 帧文件解析：给的是相对路径就先按 cwd 找，再按仓库根找（cwd 不一定是仓库根）。
fn resolve_frame_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.is_absolute() {
        return None;
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidate = repo_root.join(path);
    if candidate.is_file() {
        return Some(candidate);
    }
    let fallback = repo_root.join("captures/frame0.bgra");
    if fallback.is_file() {
        return Some(fallback);
    }
    None
}

// ---------------------------------------------------------------- 单场景

struct ScenarioReport {
    kind: Kind,
    frames: usize,
    changed: Vec<usize>,
    bytes: Vec<usize>,
    ll_bytes: Vec<usize>,
    jpg_bytes: Vec<usize>,
    enc_us: Vec<u64>,
    tags: [usize; 3],
    verify_blocks: usize,
    verify_fail: usize,
    cross_fail: usize,
}

impl ScenarioReport {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            frames: 0,
            changed: Vec::new(),
            bytes: Vec::new(),
            ll_bytes: Vec::new(),
            jpg_bytes: Vec::new(),
            enc_us: Vec::new(),
            tags: [0; 3],
            verify_blocks: 0,
            verify_fail: 0,
            cross_fail: 0,
        }
    }

    fn avg_changed(&self) -> f64 {
        mean(&self.changed)
    }

    fn avg_bytes(&self) -> f64 {
        mean(&self.bytes)
    }

    fn avg_enc_ms(&self) -> f64 {
        if self.enc_us.is_empty() {
            0.0
        } else {
            self.enc_us.iter().sum::<u64>() as f64 / self.enc_us.len() as f64 / 1000.0
        }
    }

    fn kbps(&self, fps: u32) -> f64 {
        self.avg_bytes() * f64::from(fps) * 8.0 / 1000.0
    }

    fn p95(&self) -> usize {
        let mut v = self.bytes.clone();
        v.sort_unstable();
        percentile(&v, 0.95)
    }

    fn max(&self) -> usize {
        self.bytes.iter().copied().max().unwrap_or(0)
    }

}

fn mean(v: &[usize]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<usize>() as f64 / v.len() as f64
    }
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as f64;
    let idx = (n * p).ceil() as usize;
    let idx = idx.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// 单帧测量结果。
struct FrameStats {
    changed: usize,
    bytes: usize,
    ll_bytes: usize,
    jpg_bytes: usize,
    enc_us: u64,
    tags: [usize; 3],
    verify_blocks: usize,
    verify_fail: usize,
    cross_fail: usize,
}

/// 复用的临时缓冲，避免每块都分配。
#[derive(Default)]
struct Work {
    tile: Vec<u8>,
    out: Vec<u8>,
    dst: Vec<u8>,
    ll: Vec<u8>,
    jpg: Vec<u8>,
    predicted: Vec<u8>,
}

fn wire_bytes(blocks: usize, payload: usize) -> usize {
    if blocks == 0 {
        0
    } else {
        TRANSPORT_PREFIX + FRAME_HEADER + blocks * ENTRY_HEADER + payload
    }
}

fn run_scenario(
    kind: Kind,
    base: &Frame,
    args: &Args,
    grid: &Grid,
    opts: &Options,
) -> Result<ScenarioReport> {
    let mut scenario = Scenario::new(kind, base.clone())?;
    let mut force = ForceEncoders::new(opts.zstd_level, opts.jpeg_quality)?;
    let mut work = Work::default();
    let mut report = ScenarioReport::new(kind);

    // 帧 0 只用来建立哈希基线：相当于关键帧，不计入统计，
    // 这样 static 场景从第一帧起就真的是"零变化零发送"。
    scenario.tick();
    let mut prev = vec![0u64; grid.count() as usize];
    for idx in 0..grid.count() {
        if let Some(rect) = grid.block_rect(idx, base.w, base.h) {
            prev[idx as usize] = scenario.frame().hash_rect(rect);
        }
    }

    for _ in 0..args.frames {
        scenario.tick();
        let st = measure_frame(
            scenario.frame(),
            grid,
            base.w,
            base.h,
            opts,
            &mut prev,
            &mut work,
            &mut force,
            args.verify,
        )?;
        report.frames += 1;
        report.changed.push(st.changed);
        report.bytes.push(st.bytes);
        report.ll_bytes.push(st.ll_bytes);
        report.jpg_bytes.push(st.jpg_bytes);
        report.enc_us.push(st.enc_us);
        for (dst, src) in report.tags.iter_mut().zip(st.tags.iter()) {
            *dst += *src;
        }
        report.verify_blocks += st.verify_blocks;
        report.verify_fail += st.verify_fail;
        report.cross_fail += st.cross_fail;
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn measure_frame(
    frame: &Frame,
    grid: &Grid,
    w: u32,
    h: u32,
    opts: &Options,
    prev: &mut [u64],
    work: &mut Work,
    force: &mut ForceEncoders,
    verify: bool,
) -> Result<FrameStats> {
    let mut enc_ns = 0u128;
    let mut stats = FrameStats {
        changed: 0,
        bytes: 0,
        ll_bytes: 0,
        jpg_bytes: 0,
        enc_us: 0,
        tags: [0; 3],
        verify_blocks: 0,
        verify_fail: 0,
        cross_fail: 0,
    };
    let mut payload_mixed = 0usize;
    let mut payload_ll = 0usize;
    let mut payload_jpg = 0usize;

    for idx in 0..grid.count() {
        let Some(rect) = grid.block_rect(idx, w, h) else {
            continue;
        };
        let t_block = Instant::now();
        let hash = frame.hash_rect(rect);
        if prev[idx as usize] == hash {
            enc_ns += t_block.elapsed().as_nanos(); // 未变块只花哈希时间
            continue; // 未变：一个字节都不编码
        }
        prev[idx as usize] = hash;
        stats.changed += 1;

        // 紧排块（强制策略和解码校验都基于它）
        let row = rect.w as usize * 4;
        work.tile.clear();
        for y in 0..rect.h as usize {
            let src_y = rect.y as usize + y;
            let start = src_y * frame.stride + rect.x as usize * 4;
            if start + row > frame.data.len() {
                break;
            }
            work.tile.extend_from_slice(&frame.data[start..start + row]);
        }
        if work.tile.len() != row * rect.h as usize {
            bail!("块 {idx} 越界（{}x{} @ {}x{}）", rect.w, rect.h, rect.x, rect.y);
        }

        // 1) 混合策略（真实 lrc-codec）
        let off = frame
            .rect_offset(rect)
            .ok_or_else(|| anyhow!("块 {idx} 偏移越界"))?;
        let tag = encode_block(
            &frame.data[off..],
            frame.stride,
            rect.w,
            rect.h,
            opts,
            &mut work.out,
        )
        .with_context(|| format!("块 {idx} 编码失败"))?;
        payload_mixed += work.out.len();
        enc_ns += t_block.elapsed().as_nanos(); // 真实管线只有"哈希 + 编码"（不含下面的测压旁路）
        match tag {
            codec_tag::LOSSLESS => stats.tags[0] += 1,
            codec_tag::JPEG => stats.tags[1] += 1,
            codec_tag::RAW => stats.tags[2] += 1,
            _ => {}
        }

        // 2) 强制策略（bench 内复刻容器格式）
        force.lossless_into(
            &work.tile,
            rect.w as u16,
            rect.h as u16,
            &mut work.predicted,
            &mut work.ll,
        )?;
        payload_ll += work.ll.len();
        force.jpeg_into(&work.tile, rect.w as u16, rect.h as u16, &mut work.jpg)?;
        payload_jpg += work.jpg.len();

        // 3) 交叉校验：复刻格式必须和真实 codec 的输出逐字节一致
        let same = match tag {
            codec_tag::LOSSLESS => work.ll == work.out,
            codec_tag::JPEG => work.jpg == work.out,
            _ => true,
        };
        if !same {
            stats.cross_fail += 1;
        }

        // 4) 解码校验：保证 bench 测的是"真能解出来"的数据（不进主计时）
        if verify {
            work.dst.clear();
            work.dst.resize(work.tile.len(), 0);
            let decoded = decode_block(tag, &work.out, rect.w, rect.h, &mut work.dst, row);
            match decoded {
                Ok(()) => {
                    let ok = match tag {
                        codec_tag::LOSSLESS | codec_tag::RAW => work.dst == work.tile,
                        _ => true, // JPEG 有损：能解出来即可
                    };
                    if !ok {
                        stats.verify_fail += 1;
                    }
                }
                Err(e) => {
                    log::warn!("块 {idx} 解码失败（tag={tag}）: {e}");
                    stats.verify_fail += 1;
                }
            }
            stats.verify_blocks += 1;
        }
    }

    stats.enc_us = (enc_ns / 1000) as u64;
    stats.bytes = wire_bytes(stats.changed, payload_mixed);
    stats.ll_bytes = wire_bytes(stats.changed, payload_ll);
    stats.jpg_bytes = wire_bytes(stats.changed, payload_jpg);
    Ok(stats)
}

// ---------------------------------------------------------------- 强制策略编码器

/// 复刻 lrc-codec 的两种块容器格式，用来算"全无损 / 全 JPEG"策略的字节数。
///
/// 这是纯测量用的旁路实现（不改 lrc-codec、不引新非工作区依赖）：
/// 每个块的输出都会和真实 `encode_block` 的结果做逐字节交叉校验，
/// 一旦 codec 改了格式，bench 会在输出里报"交叉校验不一致"。
struct ForceEncoders {
    quality: u8,
    compressor: zstd::bulk::Compressor<'static>,
}

impl ForceEncoders {
    fn new(level: i32, quality: u8) -> Result<Self> {
        let compressor = zstd::bulk::Compressor::new(level)
            .map_err(|e| anyhow!("zstd 压缩器初始化失败（level={level}）: {e}"))?;
        Ok(Self {
            quality: quality.clamp(1, 100),
            compressor,
        })
    }

    /// u8 mode + u16 w + u16 h + zstd(payload)，左预测/原样两种模式取小。
    fn lossless_into(
        &mut self,
        tile: &[u8],
        w: u16,
        h: u16,
        predicted: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let row = usize::from(w) * 4;
        predicted.clear();
        predicted.reserve(tile.len());
        for y in 0..usize::from(h) {
            let line = &tile[y * row..(y + 1) * row];
            let first = line.len().min(4);
            predicted.extend_from_slice(&line[..first]);
            for i in first..line.len() {
                predicted.push(line[i].wrapping_sub(line[i - 1]));
            }
        }
        let z_pred = self
            .compressor
            .compress(predicted)
            .map_err(|e| anyhow!("zstd(左预测) 压缩失败: {e}"))?;
        let z_raw = self
            .compressor
            .compress(tile)
            .map_err(|e| anyhow!("zstd(原样) 压缩失败: {e}"))?;
        let (mode, payload) = if z_pred.len() <= z_raw.len() {
            (0u8, z_pred)
        } else {
            (1u8, z_raw)
        };
        out.clear();
        out.push(mode);
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(())
    }

    /// u8 quality + u16 w + u16 h + 基线 JPEG（4:4:4）。
    fn jpeg_into(&mut self, tile: &[u8], w: u16, h: u16, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.push(self.quality);
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        let mut encoder = jpeg_encoder::Encoder::new(out, self.quality);
        encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        encoder
            .encode(tile, w, h, jpeg_encoder::ColorType::Bgra)
            .map_err(|e| anyhow!("jpeg 编码失败: {e}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------- 输出

fn print_bandwidth_table(reports: &[ScenarioReport], args: &Args) {
    println!();
    println!("=== 场景带宽（混合策略：无损优先，超阈值才 JPEG）===");
    println!(
        "{:<10} {:>14} {:>10} {:>9} {:>9} {:>13} {:>10}  {}",
        "scenario",
        "changed_blocks",
        "bytes_avg",
        "bytes_p95",
        "bytes_max",
        &format!("kbps@{}fps", args.fps),
        "enc_ms_avg",
        "tag[L/J/R]"
    );
    for r in reports {
        println!(
            "{:<10} {:>14.1} {:>10.1} {:>9} {:>9} {:>13.2} {:>10.3}  L{}/J{}/R{}",
            r.kind.name(),
            r.avg_changed(),
            r.avg_bytes(),
            r.p95(),
            r.max(),
            r.kbps(args.fps),
            r.avg_enc_ms(),
            r.tags[0],
            r.tags[1],
            r.tags[2]
        );
    }
}

fn print_strategy_table(reports: &[ScenarioReport]) {
    println!();
    println!("=== 编码策略对比（同一批脏块，每帧线上字节均值）===");
    let mut header = format!("{:<18}", "strategy");
    for r in reports {
        header.push_str(&format!(" {:>10}", r.kind.name()));
    }
    header.push_str(&format!(" {:>12}", "total"));
    println!("{header}");

    let row = |name: &str, pick: &dyn Fn(&ScenarioReport) -> f64| -> String {
        let mut line = format!("{:<18}", name);
        let mut total = 0.0;
        for r in reports {
            let v = pick(r);
            total += v;
            line.push_str(&format!(" {:>10.1}", v));
        }
        line.push_str(&format!(" {:>12.1}", total));
        line
    };

    println!("{}", row("all-lossless", &|r| mean(&r.ll_bytes)));
    println!("{}", row("all-jpeg-q85", &|r| mean(&r.jpg_bytes)));
    println!("{}", row("mixed (policy)", &|r| mean(&r.bytes)));

    // 混合策略相对另外两种策略的倍数
    let ratio_row = |name: &str, base: &dyn Fn(&ScenarioReport) -> f64| -> String {
        let mut line = format!("{:<18}", name);
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for r in reports {
            let mixed = mean(&r.bytes);
            let other = base(r);
            num += other;
            den += mixed;
            if other > 0.0 {
                line.push_str(&format!(" {:>10.2}x", other / mixed));
            } else {
                line.push_str(&format!(" {:>10}", "-"));
            }
        }
        if den > 0.0 && num > 0.0 {
            line.push_str(&format!(" {:>12.2}x", num / den));
        } else {
            line.push_str(&format!(" {:>12}", "-"));
        }
        line
    };
    println!("{}", ratio_row("mixed / all-jpeg", &|r| mean(&r.jpg_bytes)));
    println!("{}", ratio_row("mixed / all-loss", &|r| mean(&r.ll_bytes)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rng;

    #[test]
    fn wire_bytes_counts_headers() {
        assert_eq!(wire_bytes(0, 0), 0, "没有脏块就不该发消息");
        assert_eq!(wire_bytes(0, 12345), 0, "没有脏块时载荷也不该计入");
        assert_eq!(wire_bytes(1, 100), 4 + 17 + 9 + 100);
        assert_eq!(wire_bytes(3, 0), 4 + 17 + 27);
    }

    #[test]
    fn percentile_picks_upper_tail() {
        let v = [0usize, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&v, 0.5), 50);
        assert_eq!(percentile(&v, 0.95), 100);
        assert_eq!(percentile(&[], 0.95), 0);
    }

    /// 强制策略的容器格式必须和 lrc-codec 的真实输出逐字节一致，
    /// 否则策略对比表就是错的。
    #[test]
    fn forced_encoders_match_codec_output() {
        let opts = Options::default();
        let mut force = ForceEncoders::new(opts.zstd_level, opts.jpeg_quality).unwrap();
        let mut work = Work::default();
        let mut rng = Rng::new(0x1234_5678);
        let (w, h) = (64u32, 64u32);
        let row = (w * 4) as usize;

        for case in 0..5 {
            let mut tile = vec![0u8; row * h as usize];
            match case {
                0 => {
                    // 纯色：走无损
                    for px in tile.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0x1E, 0x1E, 0x1E, 0xFF]);
                    }
                }
                1 => {
                    // 细线文字：走无损
                    for y in 0..h as usize {
                        for x in 0..w as usize {
                            let i = (y * row) + x * 4;
                            let on = y % 6 < 2 && (x + y) % 5 < 3;
                            tile[i..i + 4].copy_from_slice(if on {
                                &[0xD4, 0xD4, 0xD4, 0xFF]
                            } else {
                                &[0x1E, 0x1E, 0x1E, 0xFF]
                            });
                        }
                    }
                }
                _ => {
                    // 噪声：走 JPEG
                    for b in tile.iter_mut() {
                        *b = (rng.next_u32() & 0xFF) as u8;
                    }
                    for px in tile.chunks_exact_mut(4) {
                        px[3] = 0xFF;
                    }
                }
            }

            let tag = encode_block(&tile, row, w, h, &opts, &mut work.out).unwrap();
            force
                .lossless_into(&tile, w as u16, h as u16, &mut work.predicted, &mut work.ll)
                .unwrap();
            force.jpeg_into(&tile, w as u16, h as u16, &mut work.jpg).unwrap();

            match tag {
                codec_tag::LOSSLESS => {
                    assert_eq!(work.ll, work.out, "case {case}: 无损复刻必须与 lrc-codec 一致")
                }
                codec_tag::JPEG => {
                    assert_eq!(work.jpg, work.out, "case {case}: JPEG 复刻必须与 lrc-codec 一致")
                }
                _ => {}
            }
        }

        // 噪声块必须真的走到 JPEG，否则上面的 JPEG 分支没被覆盖
        let mut noise = vec![0u8; row * h as usize];
        for b in noise.iter_mut() {
            *b = (rng.next_u32() & 0xFF) as u8;
        }
        let tag = encode_block(&noise, row, w, h, &opts, &mut work.out).unwrap();
        assert_eq!(tag, codec_tag::JPEG, "随机噪声块应该走 JPEG");
        force
            .jpeg_into(&noise, w as u16, h as u16, &mut work.jpg)
            .unwrap();
        assert_eq!(work.jpg, work.out);
    }

    #[test]
    fn degenerate_frame_detects_placeholder() {
        let zero = Frame::new(64, 64).unwrap();
        assert!(frame_is_degenerate(&zero), "全 0 帧应判为占位帧");
        let mut real = Frame::new(64, 64).unwrap();
        for px in real.data.chunks_exact_mut(4) {
            px.copy_from_slice(&[0x40, 0x40, 0x40, 0xFF]);
        }
        assert!(!frame_is_degenerate(&real), "正常帧不该被判成占位");
    }
}

fn print_footnotes(reports: &[ScenarioReport], opts: &Options) {
    println!();
    println!("说明：");
    println!("- bytes = {TRANSPORT_PREFIX}(传输长度前缀) + {FRAME_HEADER}(VideoFrame 头) + Σ({ENTRY_HEADER} + 块载荷)；脏块为 0 时一个字节都不发（零变化零发送）。");
    println!("- enc_ms_avg 只统计「哈希差分 + 编码」的时间，解码校验不计入。");

    let verify_blocks: usize = reports.iter().map(|r| r.verify_blocks).sum();
    let verify_fail: usize = reports.iter().map(|r| r.verify_fail).sum();
    if verify_blocks > 0 {
        println!(
            "- 解码校验：{verify_blocks} 个块全部解回，LOSSLESS/RAW 逐字节一致；失败 {verify_fail} 个。"
        );
    } else {
        println!("- 解码校验：已按 --no-verify 关闭。");
    }
    let cross_fail: usize = reports.iter().map(|r| r.cross_fail).sum();
    if cross_fail == 0 {
        println!("- 强制策略的容器字节已与 lrc-codec 真实输出逐字节交叉校验（不一致 0 块），可以直接和混合策略比较。");
    } else {
        println!("- ⚠ 强制策略复刻与 lrc-codec 输出有 {cross_fail} 块不一致：codec 格式可能改过，策略对比表需重新核对。");
    }

    // 混合策略在个别内容上可能比全 JPEG 更差（无损阈值过早接受），这里点名。
    let worse: Vec<String> = reports
        .iter()
        .filter(|r| {
            let mixed = mean(&r.bytes);
            let jpeg = mean(&r.jpg_bytes);
            mixed > 0.0 && jpeg > 0.0 && mixed > jpeg * 1.05
        })
        .map(|r| format!("{}（混合 {:.0} B vs 全JPEG {:.0} B）", r.kind.name(), mean(&r.bytes), mean(&r.jpg_bytes)))
        .collect();
    if !worse.is_empty() {
        println!(
            "- ⚠ 混合策略在这些场景反而比全 JPEG 大：{}。说明 lossless_ratio_limit={:.2} 在平滑图像块上过早接受了无损，值得调小阈值或对图像块强制试一次 JPEG。",
            worse.join("、"),
            opts.lossless_ratio_limit
        );
    }

    let tag_total: [usize; 3] = [
        reports.iter().map(|r| r.tags[0]).sum(),
        reports.iter().map(|r| r.tags[1]).sum(),
        reports.iter().map(|r| r.tags[2]).sum(),
    ];
    let all: usize = tag_total.iter().sum();
    if all > 0 {
        println!(
            "- 全程编码块 {all} 个：LOSSLESS {:.1}%，JPEG {:.1}%，RAW {:.1}%。",
            tag_total[0] as f64 * 100.0 / all as f64,
            tag_total[1] as f64 * 100.0 / all as f64,
            tag_total[2] as f64 * 100.0 / all as f64
        );
    }
}
