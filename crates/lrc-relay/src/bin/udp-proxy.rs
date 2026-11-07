//! UDP 有损链路模拟器：夹在两个 UDP 端点之间，按概率丢包 / 加延迟 / 乱序。
//!
//! 用途：验收 `lrc-udp` 的重传与有序交付（本机一条命令就能造弱网）。
//!
//! 用法：
//! ```text
//! udp-proxy --listen 127.0.0.1:7900 --target 127.0.0.1:7801 --loss 5 --delay-ms 20 --reorder 2
//! ```
//! 语义：本工具在 listen 上收包（来自一端），转发给 target（另一端）；
//! target 回包再转发回最近一次发来数据的客户端。**只适合单会话调试**。
//!
//! 由 windows-gfx 实现。

fn main() {
    println!("udp-proxy: 待实现");
}
