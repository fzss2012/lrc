//! X11 输入注入（XTEST 扩展）。

/// 移动鼠标到屏幕绝对坐标。
pub fn send_mouse_move(_x: i32, _y: i32) {
    todo!("windows-gfx 实现：xtest::fake_input(MotionNotify, x, y, root)")
}

/// 鼠标按键；`button` 用 X11 编号：1=左 2=中 3=右 4/5=滚轮。
pub fn send_mouse_button(_button: u8, _down: bool) {
    todo!("windows-gfx 实现")
}

/// 滚轮：正数向下、负数向上（转换为 button 4/5 的一次 press+release）。
pub fn send_wheel(_notches: i32) {
    todo!("windows-gfx 实现")
}

/// 键盘。`keysym` 是 X11 keysym（不是 scancode）；上层会被控端把 scancode 转 keysym。
pub fn send_key(_keysym: u32, _down: bool) {
    todo!("windows-gfx 实现：keycode = 查 keymap；必要时动态换 keyboard mapping")
}
