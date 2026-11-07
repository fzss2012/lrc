//! 帧缓冲（紧排 BGRA）+ 块哈希。
//!
//! 哈希实现与 lrc-capture::hash_block 保持一致，这样 bench 的"脏块判定"和真实
//! 被控端是同一套语义（逐行 xxh3 再混合）。

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use lrc_proto::grid::Rect;
use xxhash_rust::xxh3::xxh3_64;

/// 极简 xorshift64*，只用来生成确定性合成素材。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    pub fn next_range(&mut self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            self.next_u32() % n
        }
    }
}

/// 一行的字节数（BGRA，4 字节/像素）。
pub fn row_bytes(w: u32) -> Result<usize> {
    usize::try_from(w)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| anyhow!("宽度 {w} 换算行字节数溢出"))
}

/// 紧排的 BGRA 帧。
#[derive(Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn new(w: u32, h: u32) -> Result<Self> {
        let stride = row_bytes(w)?;
        let h_us = usize::try_from(h).map_err(|_| anyhow!("高度 {h} 溢出"))?;
        let len = stride
            .checked_mul(h_us)
            .ok_or_else(|| anyhow!("{w}x{h} 帧缓冲长度溢出"))?;
        Ok(Self {
            w,
            h,
            stride,
            data: vec![0u8; len],
        })
    }

    /// 从原始 .bgra 文件加载（紧排 BGRA，长度必须正好 w*h*4）。
    pub fn from_file(path: &Path, w: u32, h: u32) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("读帧文件 {} 失败", path.display()))?;
        let stride = row_bytes(w)?;
        let h_us = usize::try_from(h).map_err(|_| anyhow!("高度 {h} 溢出"))?;
        let need = stride
            .checked_mul(h_us)
            .ok_or_else(|| anyhow!("{w}x{h} 帧缓冲长度溢出"))?;
        if data.len() != need {
            bail!(
                "帧文件 {} 有 {} 字节，但 {w}x{h} BGRA 需要 {need} 字节（差 {}）；请确认 --width/--height 与抓帧时一致",
                path.display(),
                data.len(),
                i64::try_from(data.len()).unwrap_or(i64::MAX) - i64::try_from(need).unwrap_or(0)
            );
        }
        Ok(Self {
            w,
            h,
            stride,
            data,
        })
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let start = (y as usize).saturating_mul(self.stride);
        let end = (start + self.stride).min(self.data.len());
        &self.data[start..end]
    }

    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let start = (y as usize).saturating_mul(self.stride);
        let end = (start + self.stride).min(self.data.len());
        &mut self.data[start..end]
    }

    /// 块左上角像素在 data 里的字节偏移。
    pub fn rect_offset(&self, r: Rect) -> Option<usize> {
        if r.is_empty() || r.x < 0 || r.y < 0 {
            return None;
        }
        let x = usize::try_from(r.x).ok()?;
        let y = usize::try_from(r.y).ok()?;
        y.checked_mul(self.stride)?.checked_add(x.checked_mul(4)?)
    }

    /// 把 src 的 r 矩形整块拷进 self（调用方保证 r 在两者范围内）。
    pub fn blit_from(&mut self, src: &Frame, r: Rect) {
        let row = (r.w as usize).saturating_mul(4);
        for y in r.y..r.bottom() {
            let dst_start = (y as usize).saturating_mul(self.stride) + (r.x as usize).saturating_mul(4);
            let src_start = dst_start;
            if dst_start + row > self.data.len() || src_start + row > src.data.len() {
                break;
            }
            self.data[dst_start..dst_start + row]
                .copy_from_slice(&src.data[src_start..src_start + row]);
        }
    }

    /// 用单色填充矩形（自动裁剪到帧内）。
    pub fn fill_rect(&mut self, r: Rect, bgra: [u8; 4]) {
        let clipped = r.intersect(&Rect::new(0, 0, self.w, self.h));
        if clipped.is_empty() {
            return;
        }
        let row = (clipped.w as usize).saturating_mul(4);
        for y in clipped.y..clipped.bottom() {
            let start = (y as usize).saturating_mul(self.stride)
                + (clipped.x as usize).saturating_mul(4);
            if start + row > self.data.len() {
                break;
            }
            let line = &mut self.data[start..start + row];
            for px in line.chunks_exact_mut(4) {
                px.copy_from_slice(&bgra);
            }
        }
    }

    /// 块哈希（与 lrc-capture 同款）。
    pub fn hash_rect(&self, r: Rect) -> u64 {
        match self.rect_offset(r) {
            Some(off) => hash_block(&self.data[off..], self.stride, r.w, r.h),
            None => 0,
        }
    }

    /// 把紧排（行宽 = w*4）的小位图整块贴到 (x, y)（不透明覆盖）。
    pub fn blit_tight(&mut self, bitmap: &[u8], bw: u32, bh: u32, x: i32, y: i32) {
        let row = (bw as usize).saturating_mul(4);
        for by in 0..bh {
            let dy = y + by as i32;
            if dy < 0 || dy >= self.h as i32 {
                continue;
            }
            let src_start = (by as usize).saturating_mul(row);
            if src_start + row > bitmap.len() {
                break;
            }
            let dst_start = (dy as usize).saturating_mul(self.stride) + (x.max(0) as usize) * 4;
            if dst_start + row > self.data.len() {
                break;
            }
            self.data[dst_start..dst_start + row]
                .copy_from_slice(&bitmap[src_start..src_start + row]);
        }
    }
}

/// 块哈希：逐行 xxh3 再混合（与 lrc-capture::hash_block 完全一致）。
pub fn hash_block(src: &[u8], stride: usize, w: u32, h: u32) -> u64 {
    if w == 0 || h == 0 {
        return 0;
    }
    let row_bytes = (w as usize).saturating_mul(4);
    if stride < row_bytes || src.is_empty() {
        return 0;
    }

    // 起始种子带上行数，避免"不同高但内容相同"的块撞车。
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15 ^ u64::from(h).wrapping_mul(0x0000_0100_0000_01B3);
    for y in 0..h as usize {
        let Some(start) = y.checked_mul(stride) else {
            return 0;
        };
        let Some(end) = start.checked_add(row_bytes) else {
            return 0;
        };
        let Some(row) = src.get(start..end) else {
            return 0;
        };
        acc = acc.rotate_left(7) ^ xxh3_64(row);
    }

    let mut x = acc;
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^= x >> 33;
    x
}

/// 帧文件缺失时的兜底：合成一张"办公桌面"（壁纸渐变 + 3 个窗口 + 细线文字）。
pub fn synth_desktop(w: u32, h: u32) -> Result<Frame> {
    let mut frame = Frame::new(w, h)?;
    let wf = f64::from(w.max(1));
    let hf = f64::from(h.max(1));

    // 壁纸：对角渐变
    for y in 0..h {
        let row = frame.row_mut(y);
        for x in 0..w {
            let fx = f64::from(x) / wf;
            let fy = f64::from(y) / hf;
            let i = (x as usize) * 4;
            row[i] = (36.0 + 28.0 * fx) as u8;
            row[i + 1] = (44.0 + 24.0 * fy) as u8;
            row[i + 2] = (58.0 + 30.0 * (fx + fy) * 0.5) as u8;
            row[i + 3] = 0xFF;
        }
    }

    let mut rng = Rng::new(0x1CE_5EED);
    let windows = [
        (120i32, 90i32, 900u32, 620u32, [0xF3, 0xF3, 0xF3, 0xFF], [0x30, 0x30, 0x30, 0xFF]),
        (1010, 260, 760, 460, [0x2D, 0x2D, 0x2D, 0xFF], [0xD8, 0xD8, 0xD8, 0xFF]),
        (420, 720, 640, 320, [0xFF, 0xFF, 0xFF, 0xFF], [0x28, 0x28, 0x28, 0xFF]),
    ];
    for (x0, y0, ww, hh, body, ink) in windows {
        frame.fill_rect(Rect::new(x0, y0, ww, hh), body);
        frame.fill_rect(Rect::new(x0, y0, ww, 28), [0x3C, 0x6E, 0xA5, 0xFF]);
        // 标题
        for x in (x0 + 10)..(x0 + 120) {
            frame.fill_rect(Rect::new(x, y0 + 12, 1, 1), [0xFF, 0xFF, 0xFF, 0xFF]);
        }
        // 正文：一行行细线"文字"
        let mut y = y0 + 44;
        while y + 1 < y0 + hh as i32 - 8 {
            let mut x = x0 + 14 + rng.next_range(30) as i32;
            while x + 3 < x0 + ww as i32 - 10 {
                let run = 1 + rng.next_range(6) as i32;
                frame.fill_rect(Rect::new(x, y, run as u32, 1), ink);
                x += run + 1 + rng.next_range(3) as i32;
            }
            y += 5;
        }
    }
    Ok(frame)
}
