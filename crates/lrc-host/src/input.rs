//! 输入注入：把客户端回传的事件用 SendInput 回放到本机。
//!
//! 坐标语义（与 Lead 钉死的契约一致）：
//! - InputEvent 里的 x/y 是**被采集那一路输出的本地像素坐标**（左上角 0,0）；
//! - 先加输出在虚拟桌面里的原点得到虚拟桌面坐标；
//! - 再按 MOUSEEVENTF_ABSOLUTE 的约定归一化到 0..=65535（**整个虚拟桌面**范围）。

use lrc_proto::input::mouse_button;
use lrc_proto::InputEvent;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEINPUT,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CXSCREEN, SM_CYVIRTUALSCREEN, SM_CYSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 一格滚轮的 WHEEL_DELTA（windows crate 没导出，Win32 头文件里的常量）。
pub const WHEEL_DELTA: i32 = 120;
/// XBUTTON1 / XBUTTON2（MOUSEINPUT.mouseData 的取值）。
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;
/// MOUSEEVENTF_ABSOLUTE 的满量程。
const ABS_MAX: i64 = 65_535;

#[derive(Debug)]
pub enum InputError {
    /// SendInput 没有把事件全部插进输入队列（常见原因：目标窗口提权，UIPI 拦截）。
    Rejected {
        want: u32,
        got: u32,
        last_error: u32,
    },
    /// 事件内容不支持（未知鼠标键、非法码点等）。
    Unsupported(&'static str),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::Rejected {
                want,
                got,
                last_error,
            } => write!(
                f,
                "SendInput 只插入 {got}/{want} 个事件（GetLastError={last_error}）"
            ),
            InputError::Unsupported(m) => write!(f, "不支持的输入事件：{m}"),
        }
    }
}

impl std::error::Error for InputError {}

/// 虚拟桌面（多显示器时是各屏的并集，坐标可为负）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl VirtualDesktop {
    /// 至少有 2×2，保证归一化的分母不会为 0。
    fn sane(self) -> Self {
        Self {
            x: self.x,
            y: self.y,
            w: self.w.max(2),
            h: self.h.max(2),
        }
    }
}

/// 读虚拟桌面范围（物理像素）。读不到时退回主屏尺寸。
pub fn virtual_desktop() -> VirtualDesktop {
    // SAFETY: GetSystemMetrics 只读系统指标。
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w > 1 && h > 1 {
        return VirtualDesktop { x, y, w, h }.sane();
    }
    // 虚拟桌面指标拿不到（罕见）：退回主屏，原点按 0,0 处理
    let (fw, fh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    VirtualDesktop {
        x: 0,
        y: 0,
        w: fw,
        h: fh,
    }
    .sane()
}

/// 输出本地坐标 → 虚拟桌面绝对坐标（0..=65535）。
pub fn local_to_absolute(
    local_x: i32,
    local_y: i32,
    origin: (i32, i32),
    vd: VirtualDesktop,
) -> (i32, i32) {
    let vd = vd.sane();
    let desk_x = i64::from(origin.0).saturating_add(i64::from(local_x));
    let desk_y = i64::from(origin.1).saturating_add(i64::from(local_y));
    let denom_x = i64::from(vd.w) - 1;
    let denom_y = i64::from(vd.h) - 1;
    let nx = (desk_x - i64::from(vd.x)).saturating_mul(ABS_MAX) / denom_x;
    let ny = (desk_y - i64::from(vd.y)).saturating_mul(ABS_MAX) / denom_y;
    (
        nx.clamp(0, ABS_MAX) as i32,
        ny.clamp(0, ABS_MAX) as i32,
    )
}

/// 回放一个输入事件。origin 是被采集输出的虚拟桌面原点（ScreenInfo.origin_x/y）。
pub fn apply(ev: InputEvent, origin: (i32, i32)) -> Result<(), InputError> {
    let vd = virtual_desktop();
    match ev {
        InputEvent::MouseMove { x, y } => {
            let (ax, ay) = local_to_absolute(x, y, origin, vd);
            send(&[mouse_move(ax, ay)])
        }
        InputEvent::MouseButton {
            button,
            down,
            x,
            y,
        } => {
            let (ax, ay) = local_to_absolute(x, y, origin, vd);
            let (flags, data) =
                button_flags(button, down).ok_or(InputError::Unsupported("未知鼠标键"))?;
            // 先把光标挪到位再按：某些窗口只看按下那一刻的位置。
            let mut buf = [INPUT::default(); 2];
            buf[0] = mouse_move(ax, ay);
            buf[1] = mouse_input(0, 0, flags, data);
            send(&buf)
        }
        InputEvent::Wheel { dx, dy, x, y } => {
            let (ax, ay) = local_to_absolute(x, y, origin, vd);
            let mut buf = [INPUT::default(); 3];
            buf[0] = mouse_move(ax, ay);
            let mut n = 1usize;
            if dy != 0 {
                buf[n] = mouse_input(0, 0, MOUSEEVENTF_WHEEL, wheel_data(dy));
                n += 1;
            }
            if dx != 0 {
                buf[n] = mouse_input(0, 0, MOUSEEVENTF_HWHEEL, wheel_data(dx));
                n += 1;
            }
            if n == 1 {
                return Ok(());
            }
            send(&buf[..n])
        }
        InputEvent::Key {
            scancode,
            down,
            extended,
        } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            // 用扫描码而不是虚拟键码：与键盘布局无关（DESIGN §5）。
            send(&[key_input(VIRTUAL_KEY(0), scancode, flags)])
        }
        InputEvent::KeyUnicode { codepoint, down } => apply_unicode(codepoint, down),
    }
}

/// 输入法兜底：按 UTF-16 码元注入（非 BMP 字符会拆成两个代理项事件）。
fn apply_unicode(codepoint: u32, down: bool) -> Result<(), InputError> {
    let ch = char::from_u32(codepoint).ok_or(InputError::Unsupported("非法 Unicode 码点"))?;
    let mut units = [0u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    if encoded.is_empty() {
        return Ok(());
    }
    let mut flags = KEYEVENTF_UNICODE;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let mut buf = [INPUT::default(); 2];
    for (i, unit) in encoded.iter().enumerate() {
        buf[i] = key_input(VIRTUAL_KEY(0), *unit, flags);
    }
    send(&buf[..encoded.len()])
}

fn wheel_data(delta: i16) -> u32 {
    (i32::from(delta).saturating_mul(WHEEL_DELTA)) as u32
}

fn button_flags(button: u8, down: bool) -> Option<(MOUSE_EVENT_FLAGS, u32)> {
    let pair = match (button, down) {
        (mouse_button::LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (mouse_button::LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
        (mouse_button::RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (mouse_button::RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (mouse_button::MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (mouse_button::MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (mouse_button::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (mouse_button::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (mouse_button::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (mouse_button::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        _ => return None,
    };
    Some(pair)
}

fn mouse_move(abs_x: i32, abs_y: i32) -> INPUT {
    mouse_input(
        abs_x,
        abs_y,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        0,
    )
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let want = inputs.len() as u32;
    let got = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if got != want {
        let last_error = unsafe { GetLastError() }.0;
        return Err(InputError::Rejected {
            want,
            got,
            last_error,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vd_dual() -> VirtualDesktop {
        // 本机是双 1920x1080 并排：虚拟桌面 3840x1080
        VirtualDesktop {
            x: 0,
            y: 0,
            w: 3840,
            h: 1080,
        }
    }

    #[test]
    fn single_screen_corners() {
        let vd = VirtualDesktop {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(local_to_absolute(0, 0, (0, 0), vd), (0, 0));
        assert_eq!(local_to_absolute(1919, 1079, (0, 0), vd), (65535, 65535));
    }

    #[test]
    fn second_screen_uses_origin() {
        let vd = vd_dual();
        // 第二块屏（origin=1920,0）的右上角
        let (ax, ay) = local_to_absolute(1919, 1079, (1920, 0), vd);
        assert_eq!(ax, 65535);
        assert_eq!(ay, 65535);
        // 第二块屏左上角 → 虚拟桌面中点附近
        let (mx, my) = local_to_absolute(0, 0, (1920, 0), vd);
        assert!((32000..=33000).contains(&mx), "mx={mx}");
        assert_eq!(my, 0);
        // 第一块屏左上角
        assert_eq!(local_to_absolute(0, 0, (0, 0), vd), (0, 0));
    }

    #[test]
    fn negative_origin_left_monitor() {
        let vd = VirtualDesktop {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        };
        assert_eq!(local_to_absolute(0, 0, (-1920, 0), vd), (0, 0));
        assert_eq!(local_to_absolute(1919, 1079, (0, 0), vd), (65535, 65535));
    }

    #[test]
    fn clamps_out_of_range() {
        let vd = vd_dual();
        assert_eq!(local_to_absolute(-5000, 0, (0, 0), vd).0, 0);
        assert_eq!(local_to_absolute(9999, 0, (0, 0), vd).0, 65535);
        assert_eq!(local_to_absolute(0, -5000, (0, 0), vd).1, 0);
        assert_eq!(local_to_absolute(0, 9999, (0, 0), vd).1, 65535);
    }

    #[test]
    fn degenerate_desktop_does_not_divide_by_zero() {
        let vd = VirtualDesktop {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let (ax, ay) = local_to_absolute(10, 10, (0, 0), vd);
        assert!(ax > 0 && ay > 0);
    }

    #[test]
    fn button_flags_are_complete() {
        for button in [
            mouse_button::LEFT,
            mouse_button::RIGHT,
            mouse_button::MIDDLE,
            mouse_button::X1,
            mouse_button::X2,
        ] {
            assert!(button_flags(button, true).is_some());
            assert!(button_flags(button, false).is_some());
        }
        assert!(button_flags(9, true).is_none());
        assert_eq!(button_flags(mouse_button::X2, false).map(|p| p.1), Some(XBUTTON2));
    }

    #[test]
    fn wheel_delta_scaling() {
        assert_eq!(wheel_data(1), 120);
        assert_eq!(wheel_data(-1), (-120i32) as u32);
        assert_eq!(wheel_data(0), 0);
    }
}
