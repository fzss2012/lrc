//! 光标采集：位置 / 可见性 / 形状位图（GDI）。
//!
//! **坐标系**：位置是**虚拟桌面物理像素**（和 @@ScreenInfo.origin_x/origin_y@@ 同一坐标系）。
//! host 侧要减去 origin，才是"采集画面本地坐标"。
//! ⚠ 进程必须先 @@set_dpi_aware()@@（PER_MONITOR_AWARE_V2），否则 Windows 会做 DPI 虚拟化，
//! GetCursorInfo 返回的是缩放过的逻辑坐标，和采集画面像素对不上。
//!
//! **形状**：GetCursorInfo → CopyIcon → GetIconInfo → GetDIBits 转 32bpp BGRA；
//! 单色光标（@@hbmColor@@ 为空）按 AND/XOR 掩码合成。GetIconInfo 建出来的两个位图
//! 和 CopyIcon 出来的图标都在本模块里释放，不泄漏 GDI 对象。
//!
//! **变化检测**：句柄 + 位置 + 可见性做键，句柄没变就不重抓位图；位图内容用 xxh3 兜底
//! （句柄值会被系统回收复用，所以周期性复核一次内容，代价是一次 32x32 的 GetDIBits）。

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BI_RGB, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, RGBQUAD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HCURSOR, HICON,
    ICONINFO,
};

/// 光标位图尺寸上限（防驱动给出离谱尺寸把内存打爆）。
const MAX_SHAPE_DIM: i32 = 512;
/// 句柄没变时，最多这么久复核一次位图内容（防句柄回收复用）。
const REVERIFY_MS: u64 = 1000;

/// 光标位图（BGRA，长度 = 4*w*h，第 4 字节是 alpha）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bgra: Vec<u8>,
}

impl CursorShape {
    /// 内容哈希（尺寸/热点/像素一起算），用于判断"形状真的变了"。
    fn content_hash(&self) -> u64 {
        use xxhash_rust::xxh3::Xxh3;
        let mut h = Xxh3::new();
        h.update(&self.width.to_le_bytes());
        h.update(&self.height.to_le_bytes());
        h.update(&self.hotspot_x.to_le_bytes());
        h.update(&self.hotspot_y.to_le_bytes());
        h.update(&self.bgra);
        h.digest()
    }
}

/// 某一时刻的完整光标状态。
#[derive(Debug, Clone)]
pub struct CursorState {
    /// 虚拟桌面物理像素坐标
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// 形状（@@Arc@@ 复用，调用方拿到的就是同一份，不要原地改）
    pub shape: Option<Arc<CursorShape>>,
}

/// 采集统计（probe / 日志用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorWatcherStats {
    /// poll() 调用次数
    pub polls: u64,
    /// poll() 返回 Some 的次数（= 真的有变化）
    pub changes: u64,
    /// 真正抓位图的次数（句柄变化或周期性复核）
    pub shape_captures: u64,
    /// 最近一次抓位图耗时（微秒）
    pub last_shape_capture_us: u64,
}

/// 光标监视器：每次 @@poll()@@ 只返回**有变化**的状态。
pub struct CursorWatcher {
    initialized: bool,
    last_x: i32,
    last_y: i32,
    last_visible: bool,
    last_icon: isize,
    shape: Option<Arc<CursorShape>>,
    shape_hash: u64,
    last_capture_ms: u64,
    stats: CursorWatcherStats,
}

impl Default for CursorWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorWatcher {
    pub fn new() -> Self {
        Self {
            initialized: false,
            last_x: 0,
            last_y: 0,
            last_visible: false,
            last_icon: 0,
            shape: None,
            shape_hash: 0,
            last_capture_ms: 0,
            stats: CursorWatcherStats::default(),
        }
    }

    pub fn stats(&self) -> CursorWatcherStats {
        self.stats
    }

    /// 拉一次当前光标；**没有变化时返回 None**（避免每帧都发消息）。
    pub fn poll(&mut self) -> Option<CursorState> {
        self.stats.polls = self.stats.polls.saturating_add(1);

        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..CURSORINFO::default()
        };
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            // 拿不到（会话切换/桌面未就绪）：不上报变化
            return None;
        }
        let visible = (ci.flags.0 & CURSOR_SHOWING.0) != 0;
        let x = ci.ptScreenPos.x;
        let y = ci.ptScreenPos.y;
        let icon = ci.hCursor.0 as isize;

        let now = crate::now_ms();
        let due_verify =
            self.shape.is_some() && now.saturating_sub(self.last_capture_ms) >= REVERIFY_MS;
        let need_capture =
            icon != 0 && (icon != self.last_icon || self.shape.is_none() || due_verify);

        let mut shape_changed = false;
        if need_capture {
            let t0 = Instant::now();
            let captured = capture_shape(ci.hCursor);
            let us = t0.elapsed().as_micros() as u64;
            self.stats.last_shape_capture_us = us;
            self.stats.shape_captures = self.stats.shape_captures.saturating_add(1);
            self.last_icon = icon;
            self.last_capture_ms = now;
            match captured {
                Ok(shape) => {
                    let hash = shape.content_hash();
                    // 内容没变（同一光标被系统换了句柄值）→ 复用旧 Arc，不算变化
                    if self.shape.is_none() || hash != self.shape_hash {
                        self.shape_hash = hash;
                        self.shape = Some(Arc::new(shape));
                        shape_changed = true;
                    }
                }
                Err(e) => {
                    log::debug!("cursor: 抓位图失败（保留上一份形状）: {e}");
                }
            }
        }

        let changed = !self.initialized
            || shape_changed
            || x != self.last_x
            || y != self.last_y
            || visible != self.last_visible;

        self.initialized = true;
        self.last_x = x;
        self.last_y = y;
        self.last_visible = visible;

        if !changed {
            return None;
        }
        self.stats.changes = self.stats.changes.saturating_add(1);
        Some(CursorState {
            x,
            y,
            visible,
            shape: self.shape.clone(),
        })
    }
}

/// 拷一份图标再读，避免调用方随时销毁原句柄。
fn capture_shape(hcursor: HCURSOR) -> Result<CursorShape, String> {
    // HCURSOR 和 HICON 在 Win32 里是同一类对象，只是类型标签不同
    let hicon: HICON = unsafe { CopyIcon(HICON(hcursor.0)) }.map_err(|e| format!("CopyIcon: {e}"))?;
    let out = unsafe { capture_from_icon(hicon) };
    let _ = unsafe { DestroyIcon(hicon) };
    out
}

unsafe fn capture_from_icon(hicon: HICON) -> Result<CursorShape, String> {
    let mut ii = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut ii) }.map_err(|e| format!("GetIconInfo: {e}"))?;

    // 无论走哪条路径，这两个位图都是 GetIconInfo 替我们建的，必须 DeleteObject
    let out = if ii.hbmColor.0.is_null() {
        unsafe { capture_mono(&ii) }
    } else {
        unsafe { capture_color(&ii) }
    };
    if !ii.hbmMask.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(ii.hbmMask.0)) };
    }
    if !ii.hbmColor.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(ii.hbmColor.0)) };
    }
    out
}

fn shape_dims(hbm: HBITMAP, what: &str) -> Result<(u32, u32), String> {
    let mut bm = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        )
    };
    if got == 0 {
        return Err(format!("GetObjectW({what}) 失败"));
    }
    if bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmWidth > MAX_SHAPE_DIM || bm.bmHeight > MAX_SHAPE_DIM * 2
    {
        return Err(format!("{what} 尺寸异常 {}x{}", bm.bmWidth, bm.bmHeight));
    }
    Ok((bm.bmWidth as u32, bm.bmHeight as u32))
}

/// 读一张 GDI 位图成 BGRA（biHeight 用负值 = 自上而下，省一次翻转）。
unsafe fn get_dib_bgra(hbm: HBITMAP, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return Err("CreateCompatibleDC 失败".into());
    }
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let lines = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        )
    };
    let _ = unsafe { DeleteDC(dc) };
    if lines == 0 {
        return Err("GetDIBits(32bpp) 返回 0 行".into());
    }
    Ok(buf)
}

/// 读 1bpp 掩码位图 → 每个像素 1 字节（0/1），自上而下。
unsafe fn get_dib_mask(hbm: HBITMAP, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return Err("CreateCompatibleDC 失败".into());
    }
    // GetDIBits 对 1bpp 要求带 2 项调色板，BITMAPINFO 只自带 1 项，自己按 4 字节对齐扩一块
    let hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let pal_size = 2 * std::mem::size_of::<RGBQUAD>();
    let words = (hdr_size + pal_size).div_ceil(4) + 1;
    let mut storage: Vec<u32> = vec![0u32; words];
    let base = storage.as_mut_ptr() as *mut u8;
    unsafe {
        let hdr = base as *mut BITMAPINFOHEADER;
        *hdr = BITMAPINFOHEADER {
            biSize: hdr_size as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            biClrUsed: 2,
            ..BITMAPINFOHEADER::default()
        };
        let pal = base.add(hdr_size) as *mut RGBQUAD;
        *pal = RGBQUAD {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbReserved: 0,
        };
        *pal.add(1) = RGBQUAD {
            rgbBlue: 255,
            rgbGreen: 255,
            rgbRed: 255,
            rgbReserved: 0,
        };
    }

    let stride = ((w as usize + 31) / 32) * 4;
    let mut bits = vec![0u8; stride * h as usize];
    let lines = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            h,
            Some(bits.as_mut_ptr() as *mut c_void),
            base as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        )
    };
    let _ = unsafe { DeleteDC(dc) };
    if lines == 0 {
        return Err("GetDIBits(1bpp) 返回 0 行".into());
    }

    let mut out = vec![0u8; w as usize * h as usize];
    for y in 0..h as usize {
        let row = y * stride;
        for x in 0..w as usize {
            let byte = bits.get(row + x / 8).copied().unwrap_or(0);
            out[y * w as usize + x] = (byte >> (7 - (x % 8))) & 1;
        }
    }
    Ok(out)
}

/// 彩色光标：32bpp BGRA；如果图标没有 alpha 通道，用掩码补。
unsafe fn capture_color(ii: &ICONINFO) -> Result<CursorShape, String> {
    let (w, h) = shape_dims(ii.hbmColor, "hbmColor")?;
    let mut bgra = unsafe { get_dib_bgra(ii.hbmColor, w, h)? };

    // 老式图标（无 alpha）需要靠 AND 掩码决定透明
    let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
    if !has_alpha && !ii.hbmMask.0.is_null() {
        if let Ok(mask) = unsafe { get_dib_mask(ii.hbmMask, w, h) } {
            for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
                let transparent = mask.get(i).copied().unwrap_or(1) == 1;
                px[3] = if transparent { 0 } else { 255 };
            }
        }
    }

    Ok(CursorShape {
        width: w,
        height: h,
        hotspot_x: ii.xHotspot as i32,
        hotspot_y: ii.yHotspot as i32,
        bgra,
    })
}

/// 单色光标的单像素合成（BGRA）。规则见 [`capture_mono`] 的说明。
fn mono_pixel(and_bit: u8, xor_bit: u8) -> [u8; 4] {
    match (and_bit & 1, xor_bit & 1) {
        (1, 0) => [0, 0, 0, 0],          // 透明
        (0, 1) => [255, 255, 255, 255],  // 白
        _ => [0, 0, 0, 255],             // 黑（含 AND=1,XOR=1 的近似）
    }
}

/// 单色光标：hbmMask 高 = 2*光标高，上半 AND、下半 XOR。
///
/// 合成规则（(屏幕 & AND) ^ XOR 的静态近似）：
/// AND=1 XOR=0 → 透明；AND=0 XOR=0 → 黑；AND=0 XOR=1 → 白。
/// AND=1 XOR=1 本意是"反色"，静态位图无法表达，这里**近似成黑色**
/// （办公白底场景下与真实观感一致；深色背景下可能偏暗，M2 可改成按桌面像素真反色）。
unsafe fn capture_mono(ii: &ICONINFO) -> Result<CursorShape, String> {
    let (w, mh) = shape_dims(ii.hbmMask, "hbmMask")?;
    let h = if mh >= 2 { mh / 2 } else { mh };

    let mask = unsafe { get_dib_mask(ii.hbmMask, w, mh)? };
    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let and_bit = mask.get(y * w as usize + x).copied().unwrap_or(1);
            let xor_bit = mask
                .get((h as usize + y) * w as usize + x)
                .copied()
                .unwrap_or(0);
            let i = (y * w as usize + x) * 4;
            bgra[i..i + 4].copy_from_slice(&mono_pixel(and_bit, xor_bit));
        }
    }

    Ok(CursorShape {
        width: w,
        height: h,
        hotspot_x: ii.xHotspot as i32,
        hotspot_y: ii.yHotspot as i32,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_and_xor_rules() {
        // AND=1 XOR=0 → 透明
        assert_eq!(mono_pixel(1, 0)[3], 0);
        // AND=0 XOR=1 → 白
        assert_eq!(mono_pixel(0, 1), [255, 255, 255, 255]);
        // AND=0 XOR=0 → 黑
        assert_eq!(mono_pixel(0, 0), [0, 0, 0, 255]);
        // AND=1 XOR=1（反色）→ 目前近似成黑，但必须不透明
        assert_eq!(mono_pixel(1, 1)[3], 255);
    }

    #[test]
    fn shape_hash_detects_pixels_and_hotspot() {
        let a = CursorShape {
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra: vec![1, 2, 3, 255, 0, 0, 0, 0, 7, 7, 7, 128, 9, 9, 9, 255],
        };
        let mut b = a.clone();
        assert_eq!(a.content_hash(), b.content_hash());
        b.hotspot_x = 1;
        assert_ne!(a.content_hash(), b.content_hash());
        let mut c = a.clone();
        c.bgra[0] = 42;
        assert_ne!(a.content_hash(), c.content_hash());
    }
}
