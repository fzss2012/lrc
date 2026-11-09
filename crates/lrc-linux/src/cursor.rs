//! X11 光标采集（XFixes `GetCursorImage`）。
//!
//! 为什么需要单独抓光标：X11 的 `GetImage` **不含鼠标光标**（光标是合成层的东西，
//! 和 Windows DXGI 一样），客户端要自己画，所以被控端必须把"光标位置 + 光标位图"发给它。
//!
//! 语义与 Windows 端 `lrc_capture::cursor::CursorWatcher` 保持一致：
//! - `poll()` 只在**位置 / 可见性 / 形状**任一变化时返回 `Some`；
//! - `shape` 字段只在**形状变化**时是 `Some`（客户端缓存上一张位图即可），其余时候是 `None`；
//! - 坐标是 **root 绝对坐标**（= 整屏 BGRA 像素坐标），上层直接塞进 `CursorPos`，不用换算。
//!
//! 注意：XFixes 返回的光标位图是 **预乘 alpha 的 ARGB**，这里转成**非预乘 BGRA**
//! （与 Windows 端 GetDIBits 出来的直通 alpha 口径一致），否则客户端按普通 alpha 混合时
//! 光标边缘会偏暗。

use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

/// 光标位图（BGRA，非预乘 alpha，行紧凑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// 长度 = 4 * width * height
    pub bgra: Vec<u8>,
}

/// 一次光标状态变化。
#[derive(Debug, Clone)]
pub struct CursorState {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// 只有形状变化时才是 Some（其余时候复用上一张）
    pub shape: Option<Arc<CursorShape>>,
}

/// 与 `CursorState` 同义（对外文档里的叫法）。
pub type CursorPoll = CursorState;

#[derive(Debug, Clone, Copy, Default)]
pub struct CursorWatcherStats {
    /// poll 次数
    pub polls: u64,
    /// 返回 Some 的次数（真正上报的变化）
    pub changes: u64,
    /// 抓位图次数（形状变化）
    pub shape_captures: u64,
    /// 最近一次抓位图耗时（微秒）
    pub last_shape_capture_us: u64,
    /// XFixes 扩展是否可用
    pub xfixes: bool,
}

#[derive(Debug)]
pub enum CursorError {
    Connect(String),
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorError::Connect(m) => write!(f, "光标采集初始化失败: {m}"),
        }
    }
}

impl std::error::Error for CursorError {}

/// FNV-1a 64：给光标位图算指纹，判断"形状有没有变"。
fn hash_pixels(px: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in px {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

/// 预乘 ARGB → 非预乘 BGRA 并写入 `out`（out 长度至少 4*w*h）。
fn argb_to_bgra_premultiplied_to_straight(px: &[u32], out: &mut [u8]) {
    for (i, p) in px.iter().enumerate() {
        let a = (p >> 24) & 0xFF;
        let r = (p >> 16) & 0xFF;
        let g = (p >> 8) & 0xFF;
        let b = p & 0xFF;
        // 非预乘：c' = c * 255 / a（a=0 时整像素透明，直接清零）
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else if a == 255 {
            (r, g, b)
        } else {
            (
                (r * 255 / a).min(255),
                (g * 255 / a).min(255),
                (b * 255 / a).min(255),
            )
        };
        let o = i * 4;
        if o + 3 < out.len() {
            out[o] = b as u8;
            out[o + 1] = g as u8;
            out[o + 2] = r as u8;
            out[o + 3] = a as u8;
        }
    }
}

/// 光标轮询器（复用同一条 X11 连接）。
pub struct CursorWatcher {
    conn: RustConnection,
    root: Window,
    xfixes: bool,
    last_pos: (i32, i32),
    last_visible: bool,
    last_hash: u64,
    last_shape: Option<Arc<CursorShape>>,
    stats: CursorWatcherStats,
    inited: bool,
}

impl CursorWatcher {
    /// `root` 传 `None` 时用默认 screen 的 root。
    pub fn new(root: Option<Window>) -> Result<Self, CursorError> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| CursorError::Connect(e.to_string()))?;
        let root = match root {
            Some(r) => r,
            None => conn
                .setup()
                .roots
                .get(screen_num)
                .map(|s| s.root)
                .ok_or_else(|| CursorError::Connect(format!("screen {screen_num} 不存在")))?,
        };
        let xfixes = match conn.xfixes_query_version(5, 0) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => {
                    log::debug!(
                        "XFixes {}.{} 可用",
                        reply.major_version,
                        reply.minor_version
                    );
                    true
                }
                Err(e) => {
                    log::warn!("XFixes 不可用（光标将不上报）: {e}");
                    false
                }
            },
            Err(e) => {
                log::warn!("XFixes 请求失败（光标将不上报）: {e}");
                false
            }
        };
        Ok(Self {
            conn,
            root,
            xfixes,
            last_pos: (i32::MIN, i32::MIN),
            last_visible: true,
            last_hash: 0,
            last_shape: None,
            stats: CursorWatcherStats {
                xfixes,
                ..Default::default()
            },
            inited: false,
        })
    }

    /// 本 watcher 用的 root window（上层若要自己做坐标换算可用）。
    pub fn root(&self) -> Window {
        self.root
    }

    pub fn stats(&self) -> CursorWatcherStats {
        self.stats
    }

    /// 当前缓存的位图（没变过就是上一张）。
    pub fn cached_shape(&self) -> Option<Arc<CursorShape>> {
        self.last_shape.clone()
    }

    /// 轮询一次；**没有变化返回 None**（调用方按 30–60 Hz 轮询即可）。
    pub fn poll(&mut self) -> Option<CursorState> {
        if !self.xfixes {
            return None;
        }
        self.stats.polls += 1;
        let reply = match self.conn.xfixes_get_cursor_image() {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("GetCursorImage 失败: {e}");
                    return None;
                }
            },
            Err(e) => {
                log::warn!("GetCursorImage 请求失败: {e}");
                return None;
            }
        };

        let x = reply.x as i32;
        let y = reply.y as i32;
        let w = reply.width as u32;
        let h = reply.height as u32;
        let px = &reply.cursor_image;
        if px.len() < (w as usize) * (h as usize) {
            log::warn!(
                "GetCursorImage 数据不足: {} < {}x{}",
                px.len(),
                w,
                h
            );
            return None;
        }

        // 全透明的位图 = 光标不可见（X11 没有显式的 hidden 标志，这是最接近的口径）
        let visible = w > 0 && h > 0 && px.iter().any(|p| (p >> 24) & 0xFF != 0);
        let hash = hash_pixels(px);

        let mut changed = false;
        let mut shape_out: Option<Arc<CursorShape>> = None;

        if !self.inited || hash != self.last_hash {
            let t0 = std::time::Instant::now();
            let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
            argb_to_bgra_premultiplied_to_straight(px, &mut bgra);
            let shape = Arc::new(CursorShape {
                width: w,
                height: h,
                hotspot_x: reply.xhot as i32,
                hotspot_y: reply.yhot as i32,
                bgra,
            });
            self.stats.last_shape_capture_us = t0.elapsed().as_micros() as u64;
            self.stats.shape_captures += 1;
            self.last_hash = hash;
            self.last_shape = Some(shape.clone());
            shape_out = Some(shape);
            changed = true;
        }

        if !self.inited || (x, y) != self.last_pos || visible != self.last_visible {
            changed = true;
        }

        self.inited = true;
        self.last_pos = (x, y);
        self.last_visible = visible;

        if !changed {
            return None;
        }
        self.stats.changes += 1;
        Some(CursorState {
            x,
            y,
            visible,
            shape: shape_out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_argb_converts_to_straight_bgra() {
        // 不透明红：ARGB = FF FF 00 00 → BGRA = 00 00 FF FF
        // 半透明白（预乘 128）：ARGB = 80 80 80 80 → 非预乘 ≈ 255 → BGRA = FF FF FF 80
        // 全透明：A=0 → 全 0
        let px = [0xFFFF_0000u32, 0x8080_8080, 0x0012_3456];
        let mut out = vec![0u8; 12];
        argb_to_bgra_premultiplied_to_straight(&px, &mut out);
        assert_eq!(&out[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        assert_eq!(&out[4..8], &[0xFF, 0xFF, 0xFF, 0x80]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn hash_detects_change() {
        let a = [1u32, 2, 3, 4];
        let b = [1u32, 2, 3, 5];
        assert_eq!(hash_pixels(&a), hash_pixels(&a));
        assert_ne!(hash_pixels(&a), hash_pixels(&b));
    }
}
