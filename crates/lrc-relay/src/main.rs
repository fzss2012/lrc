//! lrc-relay 服务端：房间登记 + 地址交换 + UDP 中继。
//!
//! 用法：
//! ```text
//! lrc-relay --udp 0.0.0.0:7801 [--room-ttl 60] [--max-rooms 64] [--stats]
//! ```
//!
//! 协议见 lib.rs 顶部表格。要点：
//! - 单线程 `recv_from` 循环即可（自用场景最多 1–2 个房间）
//! - 房间状态：`room -> { host_addr, host_tag, client_addr, client_tag, last_seen, relayed_bytes }`
//! - 收到 `RELAY_OUT{dst_tag}` → 查 tag 找到对端地址 → 回发 `RELAY_IN{src_tag, payload}`
//! - `--stats`：每 5 秒打一行 `rooms=… direct=… relayed_bytes=… relayed_pkts=…`
//!
//! 由 windows-gfx 实现。

fn main() {
    println!("lrc-relay: 待实现（见 lib.rs 顶部协议表）");
}
