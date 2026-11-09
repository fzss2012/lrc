//! Linux 被控端：X11 采集 → 块哈希差分 → 编码 → 传输；输入回放。
//!
//! 用法（与 Windows 版一致）：
//! ```text
//! lrc-host-linux --password 1234 [--display :1] [--listen 0.0.0.0:7800]
//!                [--signal <vps>:7801 --room 1 --p2p auto|relay|off]
//! ```
//!
//! host-app 实现。可以直接参考 `crates/lrc-host/src/session.rs` 的结构，
//! 但**不要**照搬 Windows 专有部分（DXGI/窗口/音频），Linux 音频本期不做。

fn main() {
    println!("lrc-host-linux: 待实现");
}
