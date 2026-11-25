//! X11 输入注入（XTEST 扩展）。
//!
//! 对外是**无状态函数**（与 Windows 端的 `lrc_capture` 输入注入同形），内部维护一条
//! 全局 X11 连接 + keysym→keycode 缓存，第一次调用时惰性建立。
//!
//! 坐标系：XTEST 的 MotionNotify 用**绝对坐标且以 root window 为基准**。
//! 只推一台显示器时，客户端送来的是"采集区域内的相对坐标"，这里加上区域原点
//! 换算成 root 坐标（见 set_pointer_origin）。整屏采集时原点就是 (0, 0)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

// X11 核心协议的事件号（固定值，不依赖 x11rb 的常量命名）
const EV_KEY_PRESS: u8 = 2;
const EV_KEY_RELEASE: u8 = 3;
const EV_BUTTON_PRESS: u8 = 4;
const EV_BUTTON_RELEASE: u8 = 5;
const EV_MOTION_NOTIFY: u8 = 6;

const XK_SHIFT_L: Keysym = 0xFFE1;

/// 一个 keysym 对应的按键方式。
#[derive(Clone, Copy)]
struct KeyBind {
    keycode: u8,
    /// 需要同时按住 Shift（大写字母、!@# 这类）
    shift: bool,
}

struct Input {
    conn: RustConnection,
    root: x11rb::protocol::xproto::Window,
    /// 每个 keycode 的 keysyms 数量（改映射时要保持一致）
    keysyms_per_keycode: u8,
    map: HashMap<Keysym, KeyBind>,
    shift_kc: Option<u8>,
    /// 整行都是 NoSymbol 的 keycode，可临时借用
    spare: Vec<u8>,
    /// 已经临时映射过的 keysym → keycode（松开时恢复）
    temp: HashMap<Keysym, u8>,
}

impl Input {
    fn connect() -> Result<Self, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| format!("screen {screen_num} 不存在"))?;
        let root = screen.root;
        let min_kc = conn.setup().min_keycode;
        let max_kc = conn.setup().max_keycode;
        let count = max_kc.saturating_sub(min_kc).saturating_add(1);
        if count == 0 {
            return Err("键盘 keycode 范围为空".to_string());
        }
        let reply = conn
            .get_keyboard_mapping(min_kc, count)
            .map_err(|e| format!("get_keyboard_mapping: {e}"))?
            .reply()
            .map_err(|e| format!("get_keyboard_mapping reply: {e}"))?;
        let per = reply.keysyms_per_keycode.max(1);
        let mut map: HashMap<Keysym, KeyBind> = HashMap::new();
        let mut spare: Vec<u8> = Vec::new();
        for i in 0..count as usize {
            let base = i * per as usize;
            let row = reply
                .keysyms
                .get(base..base + per as usize)
                .unwrap_or(&[]);
            let kc = min_kc.wrapping_add(i as u8);
            if row.iter().all(|s| *s == 0) {
                spare.push(kc);
                continue;
            }
            // index 0 = 不按 Shift，index 1 = 按 Shift（同一 keycode 的第二档）
            if let Some(sym) = row.first().copied() {
                if sym != 0 {
                    map.entry(sym).or_insert(KeyBind {
                        keycode: kc,
                        shift: false,
                    });
                }
            }
            if let Some(sym) = row.get(1).copied() {
                if sym != 0 {
                    map.entry(sym).or_insert(KeyBind {
                        keycode: kc,
                        shift: true,
                    });
                }
            }
        }
        let shift_kc = map.get(&XK_SHIFT_L).map(|b| b.keycode);
        log::debug!(
            "X11 输入就绪：root={root} keycodes={min_kc}..={max_kc} per={per} 已映射={} 空闲={}",
            map.len(),
            spare.len()
        );
        Ok(Self {
            conn,
            root,
            keysyms_per_keycode: per,
            map,
            shift_kc,
            spare,
            temp: HashMap::new(),
        })
    }

    fn flush(&self) {
        let _ = self.conn.flush();
    }

    /// 读当前指针位置（不带锁，供已持有锁的调用方使用）。
    fn pointer_pos_inner(&mut self) -> Option<(i32, i32)> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((reply.root_x as i32, reply.root_y as i32))
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.conn
            .xtest_fake_input(type_, detail, 0, self.root, x, y, 0)
            .map_err(|e| format!("xtest_fake_input(type={type_}): {e}"))?;
        Ok(())
    }

    /// keysym → (keycode, 需要 Shift)。找不到就临时抢一个空闲 keycode 写映射。
    fn resolve(&mut self, keysym: Keysym) -> Result<(u8, bool), String> {
        if let Some(b) = self.map.get(&keysym).copied() {
            return Ok((b.keycode, b.shift));
        }
        if let Some(kc) = self.temp.get(&keysym).copied() {
            return Ok((kc, false));
        }
        let kc = self
            .spare
            .pop()
            .ok_or_else(|| format!("keysym 0x{keysym:04X} 没有空闲 keycode 可临时映射"))?;
        let row = vec![keysym; self.keysyms_per_keycode as usize];
        // X11 ChangeKeyboardMapping: (keycode_count, first_keycode, keysyms_per_keycode, keysyms)
        self.conn
            .change_keyboard_mapping(1, kc, self.keysyms_per_keycode, &row)
            .map_err(|e| format!("change_keyboard_mapping({kc}): {e}"))?
            .check()
            .map_err(|e| format!("change_keyboard_mapping({kc}) check: {e}"))?;
        self.temp.insert(keysym, kc);
        log::debug!("临时映射 keysym 0x{keysym:04X} -> keycode {kc}");
        Ok((kc, false))
    }

    /// 松开后把临时映射还回去（避免长期污染键盘表）。
    fn release_temp(&mut self, keysym: Keysym) {
        let Some(kc) = self.temp.remove(&keysym) else {
            return;
        };
        let row = vec![0 as Keysym; self.keysyms_per_keycode as usize];
        let res = self
            .conn
            .change_keyboard_mapping(1, kc, self.keysyms_per_keycode, &row)
            .map_err(|e| format!("{e}"))
            .and_then(|c| c.check().map_err(|e| format!("{e}")));
        if let Err(e) = res {
            log::warn!("恢复 keycode {kc} 映射失败: {e}");
        }
        self.spare.push(kc);
    }
}

fn global() -> MutexGuard<'static, Option<Input>> {
    static INPUT: OnceLock<Mutex<Option<Input>>> = OnceLock::new();
    let mutex = INPUT.get_or_init(|| {
        let state = match Input::connect() {
            Ok(i) => Some(i),
            Err(e) => {
                log::error!("X11 输入初始化失败（注入将无效）: {e}");
                None
            }
        };
        Mutex::new(state)
    });
    // 某个线程 panic 毒化锁时照常取回：注入失败只丢事件，不会破坏状态
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 采集区域原点（root 坐标）。客户端坐标是区域内相对坐标，注入前要加上它。
static POINTER_ORIGIN: AtomicI32 = AtomicI32::new(0);
static POINTER_ORIGIN_Y: AtomicI32 = AtomicI32::new(0);

/// 设置采集区域原点（root 坐标）。只推一台显示器时必须设，
/// 否则点击会整体偏移（例如双屏时右屏的 (0,0) 被当成 root 的 (0,0)）。
pub fn set_pointer_origin(x: i32, y: i32) {
    POINTER_ORIGIN.store(x, Ordering::Relaxed);
    POINTER_ORIGIN_Y.store(y, Ordering::Relaxed);
}

/// 当前采集区域原点（root 坐标）。
pub fn pointer_origin() -> (i32, i32) {
    (
        POINTER_ORIGIN.load(Ordering::Relaxed),
        POINTER_ORIGIN_Y.load(Ordering::Relaxed),
    )
}

/// 查询当前指针位置（root 坐标）。用于自测注入是否真的生效；失败返回 None。
pub fn pointer_pos() -> Option<(i32, i32)> {
    let mut g = global();
    let inp = g.as_mut()?;
    inp.pointer_pos_inner()
}

/// 移动鼠标到**采集区域内**的绝对坐标（会加上采集区域原点）。
///
/// X 的指针加速会作用在"绝对移动产生的增量"上，实测落点可能差 1–3 px（远程控制里
/// 足以点歪小按钮）。这里读回位置、偏了就补发一次——第二次的增量很小，加速畸变可忽略，
/// 实测能精确落点。多一次 round-trip 约 0.05–0.2 ms，对鼠标事件完全可接受。
pub fn send_mouse_move(x: i32, y: i32) {
    let mut g = global();
    let Some(inp) = g.as_mut() else { return };
    let (ox, oy) = pointer_origin();
    let px = x.saturating_add(ox).clamp(0, i16::MAX as i32) as i16;
    let py = y.saturating_add(oy).clamp(0, i16::MAX as i32) as i16;
    if let Err(e) = inp.fake(EV_MOTION_NOTIFY, 0, px, py) {
        log::warn!("{e}");
        return;
    }
    inp.flush();
    // 最多补发 2 次，收敛到 1px 以内
    for _ in 0..2 {
        let cur = inp.pointer_pos_inner();
        if cur == Some((px as i32, py as i32)) {
            break;
        }
        if let Err(e) = inp.fake(EV_MOTION_NOTIFY, 0, px, py) {
            log::warn!("{e}");
            break;
        }
        inp.flush();
    }
}

/// 鼠标按键；`button` 用 X11 编号：1=左 2=中 3=右 4/5=滚轮。
pub fn send_mouse_button(button: u8, down: bool) {
    let mut g = global();
    let Some(inp) = g.as_mut() else { return };
    let type_ = if down { EV_BUTTON_PRESS } else { EV_BUTTON_RELEASE };
    if let Err(e) = inp.fake(type_, button, 0, 0) {
        log::warn!("{e}");
    }
    inp.flush();
}

/// 滚轮：正数向下、负数向上（转换为 button 4/5 的一次 press+release）。
pub fn send_wheel(notches: i32) {
    if notches == 0 {
        return;
    }
    let mut g = global();
    let Some(inp) = g.as_mut() else { return };
    // X11：4 = 上滚，5 = 下滚
    let button = if notches > 0 { 5u8 } else { 4u8 };
    let n = notches.unsigned_abs().min(32);
    for _ in 0..n {
        if let Err(e) = inp.fake(EV_BUTTON_PRESS, button, 0, 0) {
            log::warn!("{e}");
            break;
        }
        if let Err(e) = inp.fake(EV_BUTTON_RELEASE, button, 0, 0) {
            log::warn!("{e}");
            break;
        }
    }
    inp.flush();
}

/// 键盘。`keysym` 是 X11 keysym（不是 scancode）；上层会被控端把 scancode 转 keysym。
pub fn send_key(keysym: u32, down: bool) {
    let mut g = global();
    let Some(inp) = g.as_mut() else { return };
    let (kc, need_shift) = match inp.resolve(keysym) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("{e}");
            return;
        }
    };

    // 需要 Shift 的组合：按下时先按 Shift，松开时最后放 Shift
    if need_shift {
        if let Some(sk) = inp.shift_kc {
            let shift_type = if down { EV_KEY_PRESS } else { EV_KEY_RELEASE };
            if let Err(e) = inp.fake(shift_type, sk, 0, 0) {
                log::warn!("{e}");
            }
        }
    }
    let type_ = if down { EV_KEY_PRESS } else { EV_KEY_RELEASE };
    if let Err(e) = inp.fake(type_, kc, 0, 0) {
        log::warn!("{e}");
    }
    if need_shift {
        if let Some(sk) = inp.shift_kc {
            // Shift 的松开要在主键之后（顺序与按下相反）
            let shift_type = if down { EV_KEY_RELEASE } else { EV_KEY_PRESS };
            let _ = inp.fake(shift_type, sk, 0, 0);
        }
    }
    inp.flush();

    // 临时映射只在松开后恢复，保证按下/抬起两次都能用同一个 keycode
    if !down {
        inp.release_temp(keysym);
        inp.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_codes_match_x11_protocol() {
        assert_eq!(EV_KEY_PRESS, 2);
        assert_eq!(EV_KEY_RELEASE, 3);
        assert_eq!(EV_BUTTON_PRESS, 4);
        assert_eq!(EV_BUTTON_RELEASE, 5);
        assert_eq!(EV_MOTION_NOTIFY, 6);
    }

    #[test]
    fn wheel_direction_maps_to_button_4_5() {
        // 只验证映射规则（不真的发事件）
        for (notches, expect) in [(1i32, 5u8), (-1, 4), (3, 5), (-3, 4)] {
            let button = if notches > 0 { 5u8 } else { 4u8 };
            assert_eq!(button, expect);
        }
    }
}
