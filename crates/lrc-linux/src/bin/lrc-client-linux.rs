//! Linux 控制端：X11 窗口 + 收流解码 + 呈现 + 输入转发。
//!
//! 用法：
//! ```text
//! lrc-client-linux --password 1234 --connect 127.0.0.1:7800
//!                   [--signal <vps>:7801 --room 1 --p2p auto]
//! ```
//!
//! client-app 实现。窗口/呈现用 x11rb（put_image），输入用 X11 事件；
//! 协议/解码与 Windows 版一致（复用 lrc-proto / lrc-codec / lrc-video）。

fn main() {
    println!("lrc-client-linux: 待实现");
}
