//! 可靠有序的 UDP 消息传输（KCP 血统的极简实现），供 P2P / relay 两条路径共用。
//!
//! 目标：**在 UDP 上提供"消息边界 + 可靠 + 有序"**，语义与 `lrc-transport` 的 TCP 版一致，
//! 使 host/client 只需要换一个连接对象，上层逻辑（lrc-proto 的帧）完全不用改。
//!
//! # 线格式（本 crate 内部，不外泄给 lrc-proto）
//!
//! ```
//! 数据分片 (type 0x01):
//!   [0x01][conn_id u32][seq u32][frag u8][fcnt u8][len u16][payload len]
//! ACK (type 0x02):
//!   [0x02][conn_id u32][ack u32][ack_bits u32][recv_wnd u16]
//! 保活/探测 (type 0x03):
//!   [0x03][conn_id u32][nonce u32]
//! 保活回应 (type 0x04):
//!   [0x04][conn_id u32][nonce u32]
//! ```
//! - 一条消息按 `MTU_PAYLOAD` 分片，`seq` 是**消息序号**（同一消息的所有分片 seq 相同，frag 递增）
//! - 接收端按 seq 顺序交付；乱序/缺失时**等待**（用 `ack_bits` 表达 32 个窗口内的收包情况）
//! - 发送端维护发送队列 + 未确认集合，`RTO = max(100ms, 2*RTT)`，超时重传
//! - 收发都在 `UdpConn` 内部线程安全地跑，上层可以 send/recv 分别在不同线程

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

/// 单条消息上限（与 TCP 传输层的 MAX_PAYLOAD 对齐）。
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// 单个分片承载的负载字节数（留出头部，总包 < 1400）。
pub const MTU_PAYLOAD: usize = 1200;
/// 一条消息最多分片数。
pub const MAX_FRAGS: usize = 64;

/// 下行路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerMode {
    /// 打洞成功，点对点直发
    Direct,
    /// 走 relay 转发（每包外面包一层 relay 信封）
    Relay {
        relay: SocketAddr,
        /// 本端在 relay 上登记的标签
        src_tag: u32,
        /// 对端在 relay 上登记的标签
        dst_tag: u32,
    },
}

impl PeerMode {
    pub fn kind(&self) -> &'static str {
        match self {
            PeerMode::Direct => "direct_udp",
            PeerMode::Relay { .. } => "relay_udp",
        }
    }
}

/// 运行期统计（供 --stats 与自适应控制器使用）。
#[derive(Clone, Copy, Debug, Default)]
pub struct UdpStats {
    pub rtt_ms: f32,
    pub loss_permille: u16,
    /// 发出的分片数（含重传）
    pub sent_frags: u64,
    /// 收到的分片数
    pub recv_frags: u64,
    /// 重传次数
    pub retransmits: u64,
    /// 未确认消息数
    pub inflight: u32,
    /// 收到但乱序等待中的消息数
    pub pending: u32,
}

/// 一条可靠 UDP 连接。
///
/// 线程安全：所有方法都取 `&self`，内部用 `Mutex` 保护状态；
/// 推荐上层写 `Arc<UdpConn>`，收线程调 `recv_timeout`、发线程调 `send`。
///
/// 注意：`recv_timeout` 内部**不能长时间持有锁**（否则会挡住 send），
/// 实现上应该"锁内把 socket 抽干 + 取消息，锁外 sleep"，单次持锁 < 1 ms。
pub struct UdpConn {
    _priv: (),
}

impl UdpConn {
    /// 用一个**已经完成打洞/已确定路径**的 socket 建连接。
    ///
    /// `peer`：Direct 时是对端地址；Relay 时忽略（用 `mode.relay`）。
    /// 注意：内部会把 socket 设为非阻塞。
    pub fn new(_socket: UdpSocket, _peer: SocketAddr, _mode: PeerMode) -> io::Result<Self> {
        todo!("wire-codec 实现：分片/重组/ACK/RTO 重传/有序交付")
    }

    /// 发送一条完整消息（内部自动分片入队；返回时**不保证**已经出网）。
    ///
    /// 取 `&self`（内部 Mutex）→ 上层可以 `Arc<UdpConn>` 让收/发两个线程共享同一个连接。
    pub fn send(&self, _payload: &[u8]) -> io::Result<()> {
        todo!("wire-codec 实现")
    }

    /// 阻塞最多 `timeout` 等一条完整消息。
    ///
    /// - `Ok(Some(n))` 拿到一条（`out` 已被清空并填好）
    /// - `Ok(None)` 超时，没有完整消息
    /// - `Err` 致命 IO 错误
    pub fn recv_timeout(
        &self,
        _out: &mut Vec<u8>,
        _timeout: Duration,
    ) -> io::Result<Option<usize>> {
        todo!("wire-codec 实现")
    }

    /// 驱动内部状态机（重传/保活/收包）。上层每 10–50 ms 调一次，
    /// 或者用 recv_timeout 内部自动驱动（实现里二选一，但要保证**不发数据也不会掉线**）。
    pub fn tick(&self) -> io::Result<()> {
        todo!("wire-codec 实现")
    }

    pub fn stats(&self) -> UdpStats {
        todo!("wire-codec 实现")
    }

    pub fn mode(&self) -> PeerMode {
        todo!("wire-codec 实现")
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        todo!("wire-codec 实现")
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        todo!("wire-codec 实现")
    }
}
