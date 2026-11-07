//! 信令 + 中继（P2P 打洞的 rendezvous 与失败兜底）。
//!
//! 一个进程两种角色：
//! - **服务端**（`lrc-relay` 二进制）：跑在公网 VPS 上，负责房间登记、交换对端地址、以及打洞失败时转发 UDP 包
//! - **客户端库**（本 lib）：被 `lrc-host`/`lrc-client` 调用，完成"登记/寻找对端/打洞/降级到 relay"
//!
//! # 协议（UDP，小端，首字节为类型）
//!
//! | type | 方向 | 名称 | 字段 |
//! |---|---|---|---|
//! | 0x01 | peer→relay | REGISTER | room:u32, role:u8 (1=host,2=client) |
//! | 0x81 | relay→peer | REGISTER_OK | room:u32, tag:u32 |
//! | 0x02 | peer→relay | CONNECT | room:u32 |
//! | 0x82 | relay→peer | CONNECT_OK | room:u32, tag:u32, host_addr:AddrPort |
//! | 0x83 | relay→host | PEER_WANTS | room:u32, client_addr:AddrPort |
//! | 0x84 | relay→peer | ROOM_EMPTY | room:u32（host 还没登记 / 已过期） |
//! | 0x03 | peer→relay | KEEPALIVE | room:u32, tag:u32 |
//! | 0x10 | peer→relay | RELAY_OUT | dst_tag:u32, payload |
//! | 0x90 | relay→peer | RELAY_IN | src_tag:u32, payload |
//!
//! AddrPort = u8 版本(4/6) + u16 端口 + 16 字节地址（IPv4 只用前 4 字节）。
//!
//! 打洞：relay 把双方公网地址告诉对方后，两端**同时**向对方地址发探测包（1 字节 `0xFF`），
//! 谁先收到对方的探测包就算直连成功；`punch()` 内部重试若干次，超时则返回 `PunchResult::Failed`，
//! 调用方改用 relay 模式。
//!
//! 由 windows-gfx 实现。

use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

/// 房间号（单观看者场景下 host 与 client 用同一个 room）。
pub type Room = u32;

#[derive(Debug)]
pub enum RelayError {
    Io(String),
    /// 服务端明确回复房间不存在
    RoomEmpty,
    /// 协议格式不对
    Protocol(String),
    /// 打洞失败（调用方应改用 relay）
    PunchFailed,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Io(m) => write!(f, "relay: IO {m}"),
            RelayError::RoomEmpty => write!(f, "relay: 房间没有在线 host"),
            RelayError::Protocol(m) => write!(f, "relay: 协议错误 {m}"),
            RelayError::PunchFailed => write!(f, "relay: 打洞失败"),
        }
    }
}

impl std::error::Error for RelayError {}

pub type RelayResult<T> = Result<T, RelayError>;

/// 打洞结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchResult {
    /// 直连成功，peer 地址可用
    Direct(SocketAddr),
    /// 没打通，应该改用 relay
    Failed,
}

/// host 侧会话：在 relay 登记房间，等客户端来连。
pub struct HostSession {
    _priv: (),
}

impl HostSession {
    /// 绑定本地 UDP（`bind` 为 None 则 0.0.0.0:0），并向 `relay` 登记 `room`。
    pub fn register(
        _relay: SocketAddr,
        _room: Room,
        _bind: Option<SocketAddr>,
    ) -> RelayResult<Self> {
        todo!("windows-gfx 实现")
    }

    pub fn tag(&self) -> u32 {
        todo!("windows-gfx 实现")
    }

    pub fn local_addr(&self) -> RelayResult<SocketAddr> {
        todo!("windows-gfx 实现")
    }

    pub fn relay_addr(&self) -> SocketAddr {
        todo!("windows-gfx 实现")
    }

    /// 阻塞等一个客户端连进来（期间持续发 KEEPALIVE 保持房间存活）。
    pub fn wait_peer(&mut self, _timeout: Duration) -> RelayResult<Option<SocketAddr>> {
        todo!("windows-gfx 实现")
    }

    /// 与已知 peer 打洞。
    pub fn punch(&mut self, _peer: SocketAddr, _timeout: Duration) -> RelayResult<PunchResult> {
        todo!("windows-gfx 实现")
    }

    /// 拿到底层 socket（交给 `lrc_udp::UdpConn::new`）。
    pub fn take_socket(self) -> UdpSocket {
        todo!("windows-gfx 实现")
    }
}

/// client 侧会话：找房间、拿 host 地址。
pub struct ClientSession {
    _priv: (),
}

impl ClientSession {
    pub fn connect(_relay: SocketAddr, _room: Room, _bind: Option<SocketAddr>) -> RelayResult<Self> {
        todo!("windows-gfx 实现")
    }

    pub fn tag(&self) -> u32 {
        todo!("windows-gfx 实现")
    }

    /// relay 告知的 host 公网地址
    pub fn host_addr(&self) -> SocketAddr {
        todo!("windows-gfx 实现")
    }

    pub fn relay_addr(&self) -> SocketAddr {
        todo!("windows-gfx 实现")
    }

    pub fn punch(&mut self, _timeout: Duration) -> RelayResult<PunchResult> {
        todo!("windows-gfx 实现")
    }

    pub fn take_socket(self) -> UdpSocket {
        todo!("windows-gfx 实现")
    }
}

/// 便捷封装：一次调用完成"登记/连接 + 打洞"，失败自动降级到 relay。
///
/// 返回 (socket, peer 地址, mode 描述, 是否走了 relay)。
pub fn establish(
    _is_host: bool,
    _relay: SocketAddr,
    _room: Room,
    _bind: Option<SocketAddr>,
    _peer_wait: Duration,
    _punch_timeout: Duration,
) -> RelayResult<Established> {
    todo!("windows-gfx 实现：把上面的流程串起来（host 先 wait_peer，两侧都 punch，失败走 relay）")
}

/// `establish` 的结果：拿到 socket 直接交给 `lrc_udp::UdpConn::new(socket, peer, mode)`。
pub struct Established {
    pub socket: UdpSocket,
    pub mode: lrc_udp::PeerMode,
    /// 直连成功时是对方公网地址；走 relay 时为 None
    pub direct_peer: Option<SocketAddr>,
}

impl Established {
    /// 传给 `UdpConn::new` 的 peer 参数（relay 模式下被忽略）。
    pub fn conn_peer(&self) -> SocketAddr {
        self.direct_peer
            .unwrap_or_else(|| match self.mode {
                lrc_udp::PeerMode::Relay { relay, .. } => relay,
                lrc_udp::PeerMode::Direct => "0.0.0.0:0"
                    .parse()
                    .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0))),
            })
    }
}
