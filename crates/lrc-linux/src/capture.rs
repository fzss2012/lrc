//! X11 屏幕采集（整帧 BGRA）。
//!
//! 契约：`capture()` 返回**整屏 BGRA**（紧凑行，stride = width*4）。
//! 增量由上层（host）用 64×64 块哈希自己算 —— 与 Windows 端保持一致的分工。

use crate::display::DisplayInfo;
use std::fmt;

/// 一帧整屏画面，BGRA8，行紧凑。
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bgra: Vec<u8>,
    /// 采集完成时刻（毫秒，单调时钟）
    pub ts_ms: u64,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame {}x{} stride={} bytes={}", self.width, self.height, self.stride, self.bgra.len())
    }
}

#[derive(Debug)]
pub enum CaptureError {
    Connect(String),
    Grab(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Connect(m) => write!(f, "采集连接失败: {m}"),
            CaptureError::Grab(m) => write!(f, "采集失败: {m}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// X11 采集器（复用同一条连接，避免每帧重连）。
pub struct X11Capturer {
    _priv: (),
}

impl X11Capturer {
    pub fn new(_display: DisplayInfo) -> Result<Self, CaptureError> {
        todo!("windows-gfx 实现：连接 X11 + 记录 root window + 尺寸")
    }

    /// 抓一整帧。X11 是"请求-应答"，没有脏区通知，所以每次都是整屏；
    /// 上层靠块哈希过滤没变的块（1080p 全屏哈希约 1ms，可接受）。
    pub fn capture(&mut self) -> Result<Frame, CaptureError> {
        todo!("windows-gfx 实现：get_image(ZPixmap) → 转 BGRA（X11 是 BGRX，注意 alpha 补 255）")
    }
}
