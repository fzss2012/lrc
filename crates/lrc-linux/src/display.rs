//! X11 显示器/屏幕尺寸探测。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    /// X11 的 display 名，例如 ":1"
    pub screen: u32,
}

impl fmt::Display for DisplayInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} (screen {})", self.width, self.height, self.screen)
    }
}

#[derive(Debug)]
pub enum DisplayError {
    Connect(String),
    Query(String),
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisplayError::Connect(m) => write!(f, "X11 连接失败: {m}"),
            DisplayError::Query(m) => write!(f, "X11 查询失败: {m}"),
        }
    }
}

impl std::error::Error for DisplayError {}

/// 连到默认 DISPLAY 并返回屏幕尺寸。
pub fn query() -> Result<DisplayInfo, DisplayError> {
    todo!("windows-gfx 实现：x11rb 连接 + setup 里取 width_in_pixels/height_in_pixels")
}
