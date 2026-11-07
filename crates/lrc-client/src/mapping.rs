//! 主控端坐标/输入映射（纯函数，不碰 Win32，方便单测）。
//!
//! 画面按"保持纵横比 + letterbox"显示：客户区里只有中间一块是画面，四周黑边不参与映射。

use lrc_proto::InputEvent;

/// Win32 `WHEEL_DELTA`：滚轮一"格"对应的高字值。
pub const WHEEL_DELTA: i16 = 120;

/// letterbox 缩放参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    /// 被控端 1 像素 → 客户区多少像素
    pub scale: f64,
    /// 画面左上角在客户区中的偏移
    pub ox: f64,
    /// 画面左上角在客户区中的偏移
    pub oy: f64,
}

/// 计算等比缩放参数；任一尺寸为 0 时返回 `None`。
pub fn fit(client: (u32, u32), remote: (u32, u32)) -> Option<Fit> {
    if client.0 == 0 || client.1 == 0 || remote.0 == 0 || remote.1 == 0 {
        return None;
    }
    let sx = f64::from(client.0) / f64::from(remote.0);
    let sy = f64::from(client.1) / f64::from(remote.1);
    let scale = sx.min(sy);
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let shown_w = f64::from(remote.0) * scale;
    let shown_h = f64::from(remote.1) * scale;
    Some(Fit {
        scale,
        ox: (f64::from(client.0) - shown_w) * 0.5,
        oy: (f64::from(client.1) - shown_h) * 0.5,
    })
}

/// letterbox 目标矩形 `(x, y, w, h)`（客户区坐标，用于 GDI 回退绘制）。
pub fn letterbox_rect(client: (u32, u32), remote: (u32, u32)) -> Option<(i32, i32, i32, i32)> {
    let fit = fit(client, remote)?;
    let w = ((f64::from(remote.0) * fit.scale).round() as i32).max(1);
    let h = ((f64::from(remote.1) * fit.scale).round() as i32).max(1);
    let x = ((f64::from(client.0) - f64::from(w)) * 0.5).round() as i32;
    let y = ((f64::from(client.1) - f64::from(h)) * 0.5).round() as i32;
    Some((x, y, w, h))
}

/// 客户区坐标 → 被控端屏幕像素坐标。
///
/// 黑边里的点被 clamp 到画面边缘（而不是映射到屏幕外），保证被控端光标不会飞出去。
pub fn client_to_remote(cx: i32, cy: i32, client: (u32, u32), remote: (u32, u32)) -> (i32, i32) {
    if remote.0 == 0 || remote.1 == 0 {
        return (0, 0);
    }
    let Some(fit) = fit(client, remote) else {
        return (0, 0);
    };
    let rx = (f64::from(cx) - fit.ox) / fit.scale;
    let ry = (f64::from(cy) - fit.oy) / fit.scale;
    (clamp_pixel(rx, remote.0), clamp_pixel(ry, remote.1))
}

fn clamp_pixel(v: f64, len: u32) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let max = i32::try_from(len.saturating_sub(1)).unwrap_or(i32::MAX);
    let v = v.floor();
    if v <= 0.0 {
        0
    } else if v >= f64::from(max) {
        max
    } else {
        v as i32
    }
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
        // 1920x1080 → 1280x720 正好整除：scale = 2/3
        assert_eq!(
            client_to_remote(640, 360, (1280, 720), (1920, 1080)),
            (960, 540)
        );
        assert_eq!(
            client_to_remote(1279, 719, (1280, 720), (1920, 1080)),
            (1918, 1078)
        );
    }

    #[test]
    fn letterbox_bars_clamp_to_edges() {
        // 1000x1000 窗口放 16:9 画面：上下各约 219px 黑边
        let (x, y, w, h) = letterbox_rect((1000, 1000), (1920, 1080)).expect("rect");
        assert_eq!(x, 0);
        assert_eq!(w, 1000);
        assert!(h == 562 || h == 563, "h={h}");
        assert!(y == 218 || y == 219, "y={y}");

        // 黑边里的点被夹到画面边缘
        assert_eq!(client_to_remote(500, 0, (1000, 1000), (1920, 1080)).1, 0);
        assert_eq!(
            client_to_remote(500, 999, (1000, 1000), (1920, 1080)).1,
            1079
        );
        // 画面正中 → 被控端正中
        assert_eq!(
            client_to_remote(500, 500, (1000, 1000), (1920, 1080)),
            (960, 540)
        );
    }

    #[test]
    fn degenerate_sizes_are_safe() {
        assert_eq!(client_to_remote(5, 5, (0, 0), (1920, 1080)), (0, 0));
        assert_eq!(client_to_remote(5, 5, (1280, 720), (0, 0)), (0, 0));
        assert!(letterbox_rect((0, 720), (1920, 1080)).is_none());
    }

    #[test]
    fn lparam_xy_sign_handles_negative() {
        let packed = ((100i32) << 16) | 50;
        assert_eq!(lparam_xy(i64::from(packed) as isize), (50, 100));
        // 负数坐标（鼠标拖到窗口左上外面时会出现）
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
        // 重复次数 1 | scancode 0x1E << 16 | extended bit24
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
        // 无 extended 位
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
