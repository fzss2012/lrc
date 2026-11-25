//! X11 显示器/屏幕尺寸与采集区域探测。

use std::fmt;

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::Window;

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

/// 采集区域：root 坐标下的矩形（像素）。
///
/// 用来只推一个显示器（例如 1920x1080），而不是整个 X root
/// （双屏被 X 合并后 root 可能是 3840x1080）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// 宽高必须非 0，坐标不能为负（X11 root 坐标不会为负）。
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 || x < 0 || y < 0 {
            return None;
        }
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// 解析 X 风格的几何串：1920x1080、1920x1080+0+0、1920x1080+1920+0。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim();
        let (dims, pos) = match s.split_once('+') {
            Some((d, p)) => (d, Some(p)),
            None => (s, None),
        };
        let (w, h) = dims
            .split_once('x')
            .ok_or_else(|| format!("--region 需要 WxH[+X+Y]，收到 {raw}"))?;
        let width = w
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("--region 宽不是整数：{w}"))?;
        let height = h
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("--region 高不是整数：{h}"))?;
        let (x, y) = match pos {
            None => (0, 0),
            Some(p) => {
                let (xs, ys) = p
                    .split_once('+')
                    .ok_or_else(|| format!("--region 需要 +X+Y，收到 {raw}"))?;
                let x = xs
                    .trim()
                    .parse::<i32>()
                    .map_err(|_| format!("--region X 不是整数：{xs}"))?;
                let y = ys
                    .trim()
                    .parse::<i32>()
                    .map_err(|_| format!("--region Y 不是整数：{ys}"))?;
                (x, y)
            }
        };
        Self::new(x, y, width, height)
            .ok_or_else(|| format!("--region 参数非法（宽高需 > 0，坐标需 >= 0）：{raw}"))
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }
}

/// 一个已连接的 RandR 输出（物理显示器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn region(&self) -> Region {
        Region {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

impl fmt::Display for MonitorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{}+{}+{}{}",
            self.name,
            self.width,
            self.height,
            self.x,
            self.y,
            if self.primary { " (primary)" } else { "" }
        )
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

/// 连到默认 DISPLAY（环境变量 DISPLAY）并返回屏幕尺寸。
///
/// Wayland 下 DISPLAY 通常也指向 Xwayland，能拿到画面但**抓不到原生 Wayland 窗口**，
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

/// 枚举当前**已连接**的显示器（RandR）。顺序与 xrandr 一致，主屏标记 primary。
///
/// 只依赖 randr 扩展；扩展不可用或没有已连接输出时返回空表（调用方自己回退整屏）。
pub fn monitors() -> Result<Vec<MonitorInfo>, DisplayError> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| DisplayError::Connect(e.to_string()))?;
    let root: Window = conn
        .setup()
        .roots
        .get(screen_num)
        .map(|s| s.root)
        .ok_or_else(|| DisplayError::Query(format!("screen {screen_num} 不存在")))?;

    conn.randr_query_version(1, 6)
        .map_err(|e| DisplayError::Query(format!("randr_query_version: {e}")))?
        .reply()
        .map_err(|e| DisplayError::Query(format!("randr_query_version reply: {e}")))?;

    let primary = conn
        .randr_get_output_primary(root)
        .map_err(|e| DisplayError::Query(format!("randr_get_output_primary: {e}")))?
        .reply()
        .map(|r| r.output)
        .unwrap_or(0);

    let res = conn
        .randr_get_screen_resources_current(root)
        .map_err(|e| DisplayError::Query(format!("randr_get_screen_resources_current: {e}")))?
        .reply()
        .map_err(|e| DisplayError::Query(format!("randr resources reply: {e}")))?;

    let mut out: Vec<MonitorInfo> = Vec::new();
    for output in &res.outputs {
        let info = match conn
            .randr_get_output_info(*output, res.config_timestamp)
            .map_err(|e| DisplayError::Query(format!("randr_get_output_info: {e}")))?
            .reply()
        {
            Ok(i) => i,
            Err(_) => continue,
        };
        if info.connection != x11rb::protocol::randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        let crtc = match conn
            .randr_get_crtc_info(info.crtc, res.config_timestamp)
            .map_err(|e| DisplayError::Query(format!("randr_get_crtc_info: {e}")))?
            .reply()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        if crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        out.push(MonitorInfo {
            name: String::from_utf8_lossy(&info.name).to_string(),
            x: i32::from(crtc.x),
            y: i32::from(crtc.y),
            width: u32::from(crtc.width),
            height: u32::from(crtc.height),
            primary: *output == primary,
        });
    }
    Ok(out)
}

/// 按名字挑一个显示器；name 为空时优先 primary，其次第一个已连接输出。
pub fn pick_monitor(monitors: &[MonitorInfo], name: Option<&str>) -> Option<MonitorInfo> {
    if let Some(want) = name {
        let want = want.trim();
        if !want.is_empty() {
            return monitors
                .iter()
                .find(|m| m.name == want)
                .or_else(|| monitors.iter().find(|m| m.name.starts_with(want)))
                .cloned();
        }
    }
    monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
        .cloned()
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

    #[test]
    fn region_parse_forms() {
        assert_eq!(
            Region::parse("1920x1080"),
            Ok(Region { x: 0, y: 0, width: 1920, height: 1080 })
        );
        assert_eq!(
            Region::parse("1920x1080+0+0"),
            Ok(Region { x: 0, y: 0, width: 1920, height: 1080 })
        );
        assert_eq!(
            Region::parse("1920x1080+1920+0"),
            Ok(Region { x: 1920, y: 0, width: 1920, height: 1080 })
        );
        assert_eq!(
            Region::parse(" 1280x720+10+20 "),
            Ok(Region { x: 10, y: 20, width: 1280, height: 720 })
        );
    }

    #[test]
    fn region_parse_rejects_bad() {
        assert!(Region::parse("1920").is_err());
        assert!(Region::parse("0x1080").is_err());
        assert!(Region::parse("1920x0").is_err());
        assert!(Region::parse("-100x100").is_err());
        assert!(Region::parse("1920x1080+").is_err());
        assert!(Region::parse("axb").is_err());
    }

    #[test]
    fn region_display_roundtrips() {
        let r = Region::parse("1920x1080+1920+0").expect("region");
        assert_eq!(r.to_string(), "1920x1080+1920+0");
    }

    #[test]
    fn monitor_region_and_pick() {
        let a = MonitorInfo {
            name: "HDMI-0".to_string(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            primary: false,
        };
        let b = MonitorInfo {
            name: "DP-1".to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let list = vec![a.clone(), b.clone()];
        assert_eq!(
            a.region(),
            Region { x: 1920, y: 0, width: 1920, height: 1080 }
        );
        // 默认挑 primary
        assert_eq!(pick_monitor(&list, None), Some(b.clone()));
        // 按名字精确 / 前缀
        assert_eq!(pick_monitor(&list, Some("HDMI-0")), Some(a.clone()));
        assert_eq!(pick_monitor(&list, Some("HDMI")), Some(a.clone()));
        // 空名字 = 默认
        assert_eq!(pick_monitor(&list, Some("  ")), Some(b));
        // 找不到
        assert_eq!(pick_monitor(&list, Some("DP-9")), None);
        assert_eq!(pick_monitor(&[], None), None);
    }
}
