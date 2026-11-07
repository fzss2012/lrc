//! 主控端坐标/输入映射（纯函数，不碰 Win32，方便单测）。
//!
//! 画面口径见 `docs/DESIGN.md` §4.5：Presenter 用 `DXGI_SCALING_STRETCH` 把被控端画面
//! **铺满**主控端客户区（不 letterbox），所以鼠标坐标按"等比拉伸"映射；
//! 另外收到首帧后主控端会把客户区调成被控端宽高比，常规使用下几乎不存在拉伸失真。

use lrc_proto::InputEvent;

/// Win32 `WHEEL_DELTA`：滚轮一"格"对应的高字值。
pub const WHEEL_DELTA: i16 = 120;

/// 客户区与被控端宽高比的相对偏差超过这个阈值就调整客户区尺寸（2%）。
pub const ASPECT_TOLERANCE: f64 = 0.02;

/// 客户区坐标 → 被控端像素坐标（拉伸映射 + clamp 到屏幕内）。
///
/// `remote_x = client_x * remote_w / client_w`（`remote_y` 同理）。
pub fn client_to_remote(cx: i32, cy: i32, client: (u32, u32), remote: (u32, u32)) -> (i32, i32) {
    if client.0 == 0 || client.1 == 0 || remote.0 == 0 || remote.1 == 0 {
        return (0, 0);
    }
    let rx = i64::from(cx) * i64::from(remote.0) / i64::from(client.0);
    let ry = i64::from(cy) * i64::from(remote.1) / i64::from(client.1);
    (clamp_pixel(rx, remote.0), clamp_pixel(ry, remote.1))
}

fn clamp_pixel(v: i64, len: u32) -> i32 {
    let max = i64::from(len.saturating_sub(1));
    let clamped = if max <= 0 { 0 } else { v.clamp(0, max) };
    i32::try_from(clamped).unwrap_or(i32::MAX)
}

/// 客户区与被控端的宽高比相对偏差（0.0 = 完全一致，1.0 = 差 100%）。
pub fn aspect_deviation(client: (u32, u32), remote: (u32, u32)) -> f64 {
    if client.0 == 0 || client.1 == 0 || remote.0 == 0 || remote.1 == 0 {
        return 0.0;
    }
    let client_aspect = f64::from(client.0) / f64::from(client.1);
    let remote_aspect = f64::from(remote.0) / f64::from(remote.1);
    ((client_aspect - remote_aspect) / remote_aspect).abs()
}

/// 保持宽度、按被控端宽高比算出的客户区高度（四舍五入到像素）。
///
/// `max_height` > 0 时作为上限（通常是显示器工作区高度减去窗口边框）。
pub fn client_height_for_remote(client_w: u32, remote: (u32, u32), max_height: u32) -> u32 {
    if client_w == 0 || remote.0 == 0 || remote.1 == 0 {
        return 0;
    }
    let numerator = u64::from(client_w) * u64::from(remote.1) + u64::from(remote.0) / 2;
    let mut height = (numerator / u64::from(remote.0)).max(1);
    if max_height > 0 {
        height = height.min(u64::from(max_height));
    }
    u32::try_from(height).unwrap_or(u32::MAX)
}

/// 鼠标消息 lParam → (x, y)：低字 x、高字 y，均为 signed 16-bit。
pub fn lparam_xy(lparam: isize) -> (i32, i32) {
    let raw = lparam as u32;
    let x = (raw & 0xFFFF) as u16 as i16;
    let y = ((raw >> 16) & 0xFFFF) as u16 as i16;
    (i32::from(x), i32::from(y))
}

/// 鼠标消息 wParam 的高字（signed 16-bit），滚轮 delta 就在这儿。
pub fn hiword_signed(wparam: usize) -> i16 {
    let raw = wparam as u32;
    ((raw >> 16) & 0xFFFF) as u16 as i16
}

/// 滚轮 delta → 格数（120 = 1 格）。
pub fn wheel_notches(delta: i16) -> i16 {
    delta / WHEEL_DELTA
}

/// 键盘消息 → `InputEvent::Key`。
///
/// scancode 取 lParam bit16-23，extended 取 bit24 —— 被控端用
/// `KEYEVENTF_SCANCODE` 回放，与键盘布局无关。
pub fn key_from_lparam(lparam: isize, down: bool) -> InputEvent {
    let raw = lparam as usize;
    InputEvent::Key {
        scancode: ((raw >> 16) & 0xFF) as u16,
        down,
        extended: (raw >> 24) & 0x1 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_is_identity() {
        assert_eq!(client_to_remote(0, 0, (1920, 1080), (1920, 1080)), (0, 0));
        assert_eq!(
            client_to_remote(100, 200, (1920, 1080), (1920, 1080)),
            (100, 200)
        );
        assert_eq!(
            client_to_remote(1919, 1079, (1920, 1080), (1920, 1080)),
            (1919, 1079)
        );
    }

    #[test]
    fn scales_1080p_into_720p_window() {
        // 1920x1080 → 1280x720 正好 2/3
        assert_eq!(
            client_to_remote(640, 360, (1280, 720), (1920, 1080)),
            (960, 540)
        );
        assert_eq!(
            client_to_remote(1279, 719, (1280, 720), (1920, 1080)),
            (1918, 1078)
        );
        assert_eq!(client_to_remote(0, 0, (1280, 720), (1920, 1080)), (0, 0));
    }

    #[test]
    fn stretches_when_aspect_mismatch() {
        // 口径：铺满客户区（letterbox 会把这些点夹到边缘，拉伸不会）
        // 客户区 1000x1000 显示 1920x1080
        assert_eq!(
            client_to_remote(500, 100, (1000, 1000), (1920, 1080)),
            (960, 108)
        );
        assert_eq!(
            client_to_remote(500, 900, (1000, 1000), (1920, 1080)),
            (960, 972)
        );
        assert_eq!(
            client_to_remote(500, 500, (1000, 1000), (1920, 1080)),
            (960, 540)
        );
        // 整数除法向下取整：999*1080/1000 = 1078（不是 1079）
        assert_eq!(
            client_to_remote(999, 999, (1000, 1000), (1920, 1080)),
            (1918, 1078)
        );
    }

    #[test]
    fn out_of_client_is_clamped() {
        // 拖出窗口（捕获状态下会出现负坐标/超界坐标）
        assert_eq!(
            client_to_remote(-50, -50, (1280, 720), (1920, 1080)),
            (0, 0)
        );
        assert_eq!(
            client_to_remote(5000, 5000, (1280, 720), (1920, 1080)),
            (1919, 1079)
        );
        assert_eq!(
            client_to_remote(1400, 900, (1280, 720), (1920, 1080)),
            (1919, 1079)
        );
    }

    #[test]
    fn degenerate_sizes_are_safe() {
        assert_eq!(client_to_remote(5, 5, (0, 0), (1920, 1080)), (0, 0));
        assert_eq!(client_to_remote(5, 5, (1280, 720), (0, 0)), (0, 0));
        assert_eq!(client_height_for_remote(0, (1920, 1080), 1000), 0);
        assert_eq!(client_height_for_remote(1280, (0, 1080), 1000), 0);
        assert_eq!(aspect_deviation((0, 720), (1920, 1080)), 0.0);
    }

    #[test]
    fn aspect_deviation_detects_mismatch() {
        assert!(aspect_deviation((1280, 720), (1920, 1080)) < 1e-9);
        // 1920x1200 的被控端配 1280x720 的客户区：16:9 vs 16:10
        let deviation = aspect_deviation((1280, 720), (1920, 1200));
        assert!((deviation - 1.0 / 9.0).abs() < 1e-6, "deviation={deviation}");
        assert!(deviation > ASPECT_TOLERANCE);
        // 接近 16:9 的窗口不该触发调整
        assert!(aspect_deviation((1280, 724), (1920, 1080)) < ASPECT_TOLERANCE);
    }

    #[test]
    fn target_height_keeps_width() {
        assert_eq!(client_height_for_remote(1280, (1920, 1200), 2000), 800);
        assert_eq!(client_height_for_remote(1280, (1920, 1080), 2000), 720);
        // 工作区不够高时按上限截断
        assert_eq!(client_height_for_remote(1280, (1920, 1200), 760), 760);
        // 0 表示不限制
        assert_eq!(client_height_for_remote(1920, (1920, 1200), 0), 1200);
    }

    #[test]
    fn lparam_xy_sign_handles_negative() {
        let packed = ((100i32) << 16) | 50;
        assert_eq!(lparam_xy(i64::from(packed) as isize), (50, 100));
        assert_eq!(lparam_xy(-1), (-1, -1));
        let packed = ((-5i32) << 16) | ((-7i32) & 0xFFFF);
        assert_eq!(lparam_xy(i64::from(packed) as isize), (-7, -5));
    }

    #[test]
    fn wheel_delta_to_notches() {
        assert_eq!(wheel_notches(hiword_signed(120usize << 16)), 1);
        assert_eq!(wheel_notches(hiword_signed((-120i16 as u16 as usize) << 16)), -1);
        assert_eq!(wheel_notches(hiword_signed((240usize) << 16)), 2);
        assert_eq!(hiword_signed(0), 0);
    }

    #[test]
    fn scancode_and_extended_from_lparam() {
        let lparam = (0x1_1Eu32 << 16) as isize | 1;
        match key_from_lparam(lparam, true) {
            InputEvent::Key {
                scancode,
                down,
                extended,
            } => {
                assert_eq!(scancode, 0x1E);
                assert!(down);
                assert!(extended);
            }
            other => panic!("unexpected {other:?}"),
        }
        let lparam = (0x39u32 << 16) as isize | 1;
        match key_from_lparam(lparam, false) {
            InputEvent::Key {
                scancode,
                down,
                extended,
            } => {
                assert_eq!(scancode, 0x39);
                assert!(!down);
                assert!(!extended);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
