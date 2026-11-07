//! 五种使用场景的确定性帧生成器。
//!
//! 每个场景都维护"上一帧"（cur），每次 tick 只做该场景该有的变化，于是
//! "脏块数量"就是真实办公/滚动/满帧/视频场景下的脏块数量。

use anyhow::Result;
use lrc_proto::grid::Rect;

use crate::frame::{Rng, Frame};

/// 场景种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 静止：一帧都不变（验证零发送）
    Static,
    /// 打字：600x24 文本行每次改 1/3 字符，行位置逐帧下移
    Typing,
    /// 滚动：全宽 x 200 的水平带每帧上移 8 px，底部补新内容
    Scroll,
    /// 满帧：整屏每帧都变（最坏情况，全帧重编码）
    Full,
    /// 视频：整屏平滑渐变 + 轻噪声（有损编码的压力测试）
    Video,
}

impl Kind {
    pub fn all() -> [Kind; 5] {
        [Kind::Static, Kind::Typing, Kind::Scroll, Kind::Full, Kind::Video]
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Static => "static",
            Kind::Typing => "typing",
            Kind::Scroll => "scroll",
            Kind::Full => "full",
            Kind::Video => "video",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "static" => Some(Kind::Static),
            "typing" => Some(Kind::Typing),
            "scroll" => Some(Kind::Scroll),
            "full" => Some(Kind::Full),
            "video" => Some(Kind::Video),
            _ => None,
        }
    }
}

/// typing 场景的文本行几何。
const LINE_W: u32 = 600;
const LINE_H: u32 = 24;
const CELL_W: u32 = 10;
const CELLS: usize = (LINE_W / CELL_W) as usize;
/// 染色：文本行的底/字色。
const LINE_BG: [u8; 4] = [0x1E, 0x1E, 0x1E, 0xFF];
const LINE_INK: [u8; 4] = [0xD4, 0xD4, 0xD4, 0xFF];
const LINE_INK_ALT: [u8; 4] = [0x56, 0x9C, 0xD6, 0xFF];

/// 场景私有的可变状态。
enum State {
    Idle,
    Typing {
        /// 文本行位图（LINE_W x LINE_H，BGRA 紧排）
        line: Vec<u8>,
        prev_x: i32,
        prev_y: i32,
    },
    Scroll {
        band_y: u32,
        band_h: u32,
        step: u32,
    },
    Video {
        /// 平滑渐变用的正弦表（-127..=127）
        sin: Vec<i32>,
    },
}

/// 场景状态机。
pub struct Scenario {
    kind: Kind,
    /// 原始帧（擦除/合成用）
    base: Frame,
    /// 当前帧（= 上一帧）
    cur: Frame,
    idx: u32,
    state: State,
}

impl Scenario {
    pub fn new(kind: Kind, base: Frame) -> Result<Self> {
        let cur = base.clone();
        let state = match kind {
            Kind::Typing => {
                let mut line = vec![0u8; (LINE_W * LINE_H * 4) as usize];
                let mut rng = Rng::new(0x7E71_0001);
                for cell in 0..CELLS {
                    render_cell(&mut line, cell, (rng.next_u32() & 0xFF) as u8);
                }
                State::Typing {
                    line,
                    prev_x: -1,
                    prev_y: -1,
                }
            }
            Kind::Scroll => {
                let band_h = 200u32.min(base.h);
                let band_y = base.h.saturating_sub(band_h) / 2;
                State::Scroll {
                    band_y,
                    band_h,
                    step: 8,
                }
            }
            Kind::Video => {
                let mut sin = Vec::with_capacity(256);
                for i in 0..256 {
                    let v = (f64::from(i) * std::f64::consts::TAU / 256.0).sin();
                    sin.push((v * 127.0) as i32);
                }
                State::Video { sin }
            }
            _ => State::Idle,
        };
        Ok(Self {
            kind,
            base,
            cur,
            idx: 0,
            state,
        })
    }

    pub fn frame(&self) -> &Frame {
        &self.cur
    }

    /// 生成下一帧（返回后 frame() 即是新帧）。
    pub fn tick(&mut self) {
        match self.kind {
            Kind::Static => {}
            Kind::Typing => self.tick_typing(),
            Kind::Scroll => self.tick_scroll(),
            Kind::Full => self.tick_full(),
            Kind::Video => self.tick_video(),
        }
        self.idx = self.idx.wrapping_add(1);
    }

    // ---------------------------------------------------------- 打字

    fn tick_typing(&mut self) {
        let (prev_x, prev_y) = match &self.state {
            State::Typing { prev_x, prev_y, .. } => (*prev_x, *prev_y),
            _ => return,
        };

        // 1) 擦掉上一帧的文本行（还原成原始桌面）
        if prev_y >= 0 {
            let r = Rect::new(prev_x, prev_y, LINE_W, LINE_H)
                .intersect(&Rect::new(0, 0, self.base.w, self.base.h));
            if !r.is_empty() {
                self.cur.blit_from(&self.base, r);
            }
        }

        // 2) 改掉 1/3 的"字符"（确定性）
        let mut rng = Rng::new(
            0x7479_7069_6E67u64 ^ u64::from(self.idx).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        if let State::Typing { line, .. } = &mut self.state {
            for cell in 0..CELLS {
                if (cell + self.idx as usize) % 3 == 0 {
                    let p = (rng.next_u32() & 0xFF) as u8;
                    render_cell(line, cell, p);
                }
            }
        }

        // 3) 画到新位置：逐帧下移 7 px，到底回绕
        let span = self.base.h.saturating_sub(LINE_H).max(1);
        let y = 10 + ((self.idx * 7) % span.max(2)) as i32;
        let x = 180i32.min(self.base.w.saturating_sub(LINE_W) as i32).max(0);
        if let State::Typing { line, .. } = &self.state {
            self.cur.blit_tight(line, LINE_W, LINE_H, x, y);
        }
        if let State::Typing {
            prev_x, prev_y, ..
        } = &mut self.state
        {
            *prev_x = x;
            *prev_y = y;
        }
    }

    // ---------------------------------------------------------- 滚动

    fn tick_scroll(&mut self) {
        let (band_y, band_h, step) = match &self.state {
            State::Scroll {
                band_y,
                band_h,
                step,
            } => (*band_y, *band_h, *step),
            _ => return,
        };
        let stride = self.cur.stride;
        let start = band_y as usize * stride;
        let end = ((band_y + band_h) as usize * stride).min(self.cur.data.len());
        let shift = step as usize * stride;
        if start + shift < end {
            let len = end - start - shift;
            // 内容整体上移 step 行
            self.cur.data.copy_within(start + shift..start + shift + len, start);
        }

        // 底部补 step 行"滚出来的新内容"：直接取真实帧里另一段的内容，
        // 保证条带里的画面始终是真实桌面纹理（不是自己造的白底黑字）。
        let bias = (self.base.h as usize).max(1);
        let src_base = (self.idx as usize * step as usize) % bias;
        let first_new = (band_y + band_h).saturating_sub(step);
        for k in 0..step as usize {
            let y = first_new as usize + k;
            if y >= self.cur.h as usize {
                break;
            }
            let src_y = (src_base + k) % bias;
            let src_row = self.base.row(src_y as u32).to_vec();
            let dst = self.cur.row_mut(y as u32);
            let n = dst.len().min(src_row.len());
            dst[..n].copy_from_slice(&src_row[..n]);
        }
    }

    // ---------------------------------------------------------- 满帧

    fn tick_full(&mut self) {
        let (w, h) = (self.base.w, self.base.h);
        if w == 0 || h == 0 {
            return;
        }
        let dx = ((self.idx as u64 * 3) % u64::from(w)) as usize;
        let dy = ((self.idx as u64 * 2) % u64::from(h)) as usize;
        let row = self.base.stride;
        for y in 0..h as usize {
            let src_y = (y + dy) % h as usize;
            let src = self.base.row(src_y as u32);
            let dst = self.cur.row_mut(y as u32);
            let split = dx * 4;
            if split >= row {
                dst.copy_from_slice(src);
                continue;
            }
            dst[..row - split].copy_from_slice(&src[split..]);
            dst[row - split..].copy_from_slice(&src[..split]);
        }
    }

    // ---------------------------------------------------------- 视频

    fn tick_video(&mut self) {
        let idx = self.idx as u32;
        let (w, h) = (self.cur.w, self.cur.h);
        let sin = match &self.state {
            State::Video { sin } => sin,
            _ => return,
        };

        for y in 0..h {
            let row = self.cur.row_mut(y);
            let ly = (((y / 2).wrapping_add(idx.wrapping_mul(2))) & 0xFF) as usize;
            let b = sin[ly];
            for x in 0..w {
                let lx = (((x / 2).wrapping_add(idx.wrapping_mul(3))) & 0xFF) as usize;
                let a = sin[lx];
                let i = (x as usize) * 4;
                // 每像素 3 个独立噪声分量（0..7 步进），既轻又足以让无损失效
                let n1 = pixel_noise(x, y, idx, 0x9E37_79B9) as i32;
                let n2 = pixel_noise(x, y, idx, 0x85EB_CA6B) as i32;
                let n3 = pixel_noise(x, y, idx, 0xC2B2_AE35) as i32;
                row[i] = clamp_u8(118 + a / 2 + b / 3 + n1);
                row[i + 1] = clamp_u8(122 + b / 2 + a / 3 + n2);
                row[i + 2] = clamp_u8(128 + (a + b) / 4 + n3);
                row[i + 3] = 0xFF;
            }
        }
    }
}

/// 视频场景每像素噪声幅度（-N..=N-1）。太大就不是"视频"而是纯噪声，
/// 太小又会让无损压赢——调到"JPEG 该赢、且字节数量级像真实画面"的档位。
const VIDEO_NOISE: i16 = 3;

/// 视频场景的轻噪声：按像素位置 + 帧号确定性生成。
fn pixel_noise(x: u32, y: u32, idx: u32, salt: u32) -> i16 {
    let mut v = x
        .wrapping_mul(0x27D4_EB2D)
        .wrapping_add(y.wrapping_mul(0x1656_67B1))
        .wrapping_add(idx.wrapping_mul(0x1B87_3593))
        ^ salt;
    v ^= v >> 15;
    v = v.wrapping_mul(0x2545_F491);
    v ^= v >> 13;
    let span = (VIDEO_NOISE as u32) * 2;
    ((v % span) as i16) - VIDEO_NOISE
}

fn clamp_u8(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// 在文本行位图里画一个"字符"（10x24 的小格子）：几条横笔 + 可选一条竖笔。
fn render_cell(line: &mut [u8], cell: usize, pattern: u8) {
    let cw = CELL_W as usize;
    let ch = LINE_H as usize;
    let x0 = cell * cw;
    let cols = line.len() / 4;

    // 清底
    for y in 0..ch {
        for x in x0..(x0 + cw).min(cols) {
            let i = (y * (LINE_W as usize) + x) * 4;
            line[i..i + 4].copy_from_slice(&LINE_BG);
        }
    }

    // 几笔"笔画"：1~3 条横向 + 0~1 条纵向
    let ink = if pattern & 0x80 != 0 {
        LINE_INK_ALT
    } else {
        LINE_INK
    };
    let bars = 1 + ((pattern >> 4) & 0x03) as usize;
    for k in 0..bars {
        let gy = 5 + k * 5 + ((pattern >> 2) & 0x01) as usize;
        if gy >= ch {
            continue;
        }
        let len = 3 + (pattern as usize + k * 3) % 6;
        let gx = x0 + 1 + (pattern as usize + k) % 3;
        for dx in 0..len {
            let x = gx + dx;
            if x >= x0 + cw || x >= cols {
                break;
            }
            let i = (gy * (LINE_W as usize) + x) * 4;
            line[i..i + 4].copy_from_slice(&ink);
        }
    }
    if pattern & 0x01 != 0 {
        let gx = x0 + 2 + ((pattern >> 1) & 0x03) as usize;
        if gx < x0 + cw && gx < cols {
            for gy in 4..(ch - 5) {
                let i = (gy * (LINE_W as usize) + gx) * 4;
                line[i..i + 4].copy_from_slice(&ink);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::synth_desktop;
    use lrc_proto::grid::Grid;

    fn changed_blocks(a: &Frame, b: &Frame, grid: &Grid) -> usize {
        let mut n = 0;
        for idx in 0..grid.count() {
            let Some(r) = grid.block_rect(idx, a.w, a.h) else {
                continue;
            };
            if a.hash_rect(r) != b.hash_rect(r) {
                n += 1;
            }
        }
        n
    }

    fn setup(kind: Kind) -> (Scenario, Grid) {
        setup_sized(kind, 320, 192)
    }

    fn setup_sized(kind: Kind, w: u32, h: u32) -> (Scenario, Grid) {
        let base = synth_desktop(w, h).unwrap();
        let grid = Grid::new(64, w, h).unwrap();
        (Scenario::new(kind, base).unwrap(), grid)
    }

    #[test]
    fn static_never_changes() {
        let (mut sc, grid) = setup(Kind::Static);
        sc.tick();
        let first = sc.frame().clone();
        for _ in 0..5 {
            sc.tick();
            assert_eq!(sc.frame().data, first.data, "static 场景不该有任何像素变化");
            assert_eq!(changed_blocks(&first, sc.frame(), &grid), 0);
        }
    }

    #[test]
    fn full_changes_every_block() {
        let (mut sc, grid) = setup(Kind::Full);
        sc.tick();
        let first = sc.frame().clone();
        for step in 1..4 {
            sc.tick();
            assert_ne!(sc.frame().data, first.data, "full 场景每帧都应整屏变化");
            assert_eq!(
                changed_blocks(&first, sc.frame(), &grid),
                grid.count() as usize,
                "full 第 {step} 帧应该所有块都脏"
            );
        }
    }

    #[test]
    fn typing_changes_a_small_region() {
        let (mut sc, grid) = setup(Kind::Typing);
        sc.tick();
        let first = sc.frame().clone();
        let mut seen_some = false;
        for _ in 0..4 {
            sc.tick();
            let n = changed_blocks(&first, sc.frame(), &grid);
            assert!(n > 0, "打字场景应该只有少量块变化，实际 0");
            assert!(
                n < grid.count() as usize / 2,
                "打字场景不该刷掉半屏，实际 {n}/{}",
                grid.count()
            );
            seen_some = true;
        }
        assert!(seen_some);
    }

    #[test]
    fn scroll_changes_the_band_only() {
        // 帧要比 200px 的滚动条带高，才能验证"只有条带内的块变脏"
        let (mut sc, grid) = setup_sized(Kind::Scroll, 320, 512);
        sc.tick();
        let first = sc.frame().clone();
        let mut last = 0;
        for _ in 0..4 {
            sc.tick();
            last = changed_blocks(&first, sc.frame(), &grid);
            assert!(last > 0, "滚动场景应该有条带变化");
            assert!(
                last < (grid.count() as usize * 3) / 4,
                "滚动场景不该整屏都脏，实际 {last}/{}",
                grid.count()
            );
        }
        assert!(last > 0);
    }

    #[test]
    fn video_changes_every_block() {
        let (mut sc, grid) = setup(Kind::Video);
        sc.tick();
        let first = sc.frame().clone();
        for _ in 0..3 {
            sc.tick();
            assert_eq!(
                changed_blocks(&first, sc.frame(), &grid),
                grid.count() as usize,
                "视频场景每帧都应整屏变化"
            );
        }
    }

    #[test]
    fn scenario_is_deterministic() {
        let base = synth_desktop(320, 192).unwrap();
        let mut a = Scenario::new(Kind::Typing, base.clone()).unwrap();
        let mut b = Scenario::new(Kind::Typing, base).unwrap();
        for _ in 0..5 {
            a.tick();
            b.tick();
            assert_eq!(a.frame().data, b.frame().data, "同种子场景必须可复现");
        }
    }

    #[test]
    fn kind_parse_roundtrip() {
        for k in Kind::all() {
            assert_eq!(Kind::parse(k.name()), Some(k));
        }
        assert_eq!(Kind::parse("nope"), None);
    }
}
