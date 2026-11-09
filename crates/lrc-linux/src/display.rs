//! X11 显示器/屏幕尺寸探测。

use std::fmt;

use x11rb::connection::Connection;

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

/// 连到默认 DISPLAY（环境变量 `DISPLAY`）并返回屏幕尺寸。
///
/// Wayland 下 `DISPLAY` 通常也指向 Xwayland，能拿到画面但**抓不到原生 Wayland 窗口**，
/// 本期只保证 X11 会话。
pub fn query() -> Result<DisplayInfo, DisplayError> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| DisplayError::Connect(e.to_string()))?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| DisplayError::Query(format!("screen {screen_num} 不存在")))?;
    let width = screen.width_in_pixels as u32;
    let height = screen.height_in_pixels as u32;
    if width == 0 || height == 0 {
        return Err(DisplayError::Query(format!("屏幕尺寸非法: {width}x{height}")));
    }
    Ok(DisplayInfo {
        width,
        height,
        screen: screen_num as u32,
    })
}

/// 读环境变量里的 DISPLAY（probe / 日志用；没设置返回空串）。
pub fn display_env() -> String {
    std::env::var("DISPLAY").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_info_formats() {
        let d = DisplayInfo {
            width: 1920,
            height: 1080,
            screen: 1,
        };
        assert_eq!(d.to_string(), "1920x1080 (screen 1)");
    }
}
