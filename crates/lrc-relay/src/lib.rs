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
//! **`dst_tag` 语义（M4 约定，已与 wire-codec 对齐）**：
//! - `dst_tag != 0` → 按 tag 精确路由到该 tag 对应的地址；
//! - **`dst_tag == 0` → 由 relay 按房间路由**，即转发给"发送方所在房间里的另一端"。
//!
//! 之所以需要 0：协议表里 PEER_WANTS / CONNECT_OK 都不携带**对方**的 tag
//! （CONNECT_OK.tag 是 client 自己的 tag），所以 relay 模式下两个方向都拿不到对端 tag，
//! 统一用 0 让服务端按房间找对端。`UdpConn` 写 RELAY_OUT 信封时把 `PeerMode::Relay.dst_tag` 原样写入即可。
//!
//! AddrPort = u8 版本(4/6) + u16 端口 + 16 字节地址（IPv4 只用前 4 字节）。
//!
//! 打洞：relay 把双方公网地址告诉对方后，两端**同时**向对方地址发探测包（1 字节 `0xFF`），
//! 谁先收到对方的探测包就算直连成功；`punch()` 内部重试若干次，超时则返回 `PunchResult::Failed`，
//! 调用方改用 relay 模式。
//!
//! 由 windows-gfx 实现。

pub mod server;

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 房间号（单观看者场景下 host 与 client 用同一个 room）。
pub type Room = u32;

/// 默认信令/中继端口。
pub const DEFAULT_RELAY_PORT: u16 = 7801;

// ---- 消息类型 ----
pub const T_REGISTER: u8 = 0x01;
pub const T_REGISTER_OK: u8 = 0x81;
pub const T_CONNECT: u8 = 0x02;
pub const T_CONNECT_OK: u8 = 0x82;
pub const T_PEER_WANTS: u8 = 0x83;
pub const T_ROOM_EMPTY: u8 = 0x84;
pub const T_KEEPALIVE: u8 = 0x03;
pub const T_RELAY_OUT: u8 = 0x10;
pub const T_RELAY_IN: u8 = 0x90;

/// 打洞探测包内容（1 字节）。
pub const PUNCH_BYTE: u8 = 0xFF;

pub const ROLE_HOST: u8 = 1;
pub const ROLE_CLIENT: u8 = 2;

/// REGISTER 重试次数与单次超时。
const REGISTER_TRIES: u32 = 3;
const REGISTER_TIMEOUT: Duration = Duration::from_millis(3000);
/// KEEPALIVE 间隔。
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1000);
/// 打洞探测包间隔。
const PUNCH_INTERVAL: Duration = Duration::from_millis(50);
/// 单次 recv 的最长阻塞，用来在循环里做别的事（发保活/探测）。
const RECV_SLICE: Duration = Duration::from_millis(20);

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

// ---------------------------------------------------------------------------
// 线格式编解码（服务端与客户端共用，保证两边不会各写一套）
// ---------------------------------------------------------------------------

/// 写一个 AddrPort：u8 版本 + u16 端口 + 16 字节地址。
pub fn put_addr(buf: &mut Vec<u8>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(v4) => {
            buf.push(4);
            buf.extend_from_slice(&addr.port().to_le_bytes());
            buf.extend_from_slice(&v4.octets());
            buf.extend_from_slice(&[0u8; 12]);
        }
        IpAddr::V6(v6) => {
            buf.push(6);
            buf.extend_from_slice(&addr.port().to_le_bytes());
            buf.extend_from_slice(&v6.octets());
        }
    }
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let s = buf.get(*pos..*pos + 4)?;
    *pos += 4;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Option<u16> {
    let s = buf.get(*pos..*pos + 2)?;
    *pos += 2;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u8(buf: &[u8], pos: &mut usize) -> Option<u8> {
    let b = buf.get(*pos).copied()?;
    *pos += 1;
    Some(b)
}

/// 读一个 AddrPort。
pub fn read_addr(buf: &[u8], pos: &mut usize) -> Option<SocketAddr> {
    let ver = read_u8(buf, pos)?;
    let port = read_u16(buf, pos)?;
    let raw = buf.get(*pos..*pos + 16)?;
    *pos += 16;
    match ver {
        4 => {
            let ip = Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        6 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(raw);
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), port))
        }
        _ => None,
    }
}

/// 解析出来的 relay 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayMsg {
    Register { room: Room, role: u8 },
    RegisterOk { room: Room, tag: u32 },
    Connect { room: Room },
    ConnectOk { room: Room, tag: u32, host: SocketAddr },
    PeerWants { room: Room, client: SocketAddr },
    RoomEmpty { room: Room },
    Keepalive { room: Room, tag: u32 },
    RelayOut { dst_tag: u32, payload: Vec<u8> },
    RelayIn { src_tag: u32, payload: Vec<u8> },
}

impl RelayMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(48);
        match self {
            RelayMsg::Register { room, role } => {
                b.push(T_REGISTER);
                b.extend_from_slice(&room.to_le_bytes());
                b.push(*role);
            }
            RelayMsg::RegisterOk { room, tag } => {
                b.push(T_REGISTER_OK);
                b.extend_from_slice(&room.to_le_bytes());
                b.extend_from_slice(&tag.to_le_bytes());
            }
            RelayMsg::Connect { room } => {
                b.push(T_CONNECT);
                b.extend_from_slice(&room.to_le_bytes());
            }
            RelayMsg::ConnectOk { room, tag, host } => {
                b.push(T_CONNECT_OK);
                b.extend_from_slice(&room.to_le_bytes());
                b.extend_from_slice(&tag.to_le_bytes());
                put_addr(&mut b, host);
            }
            RelayMsg::PeerWants { room, client } => {
                b.push(T_PEER_WANTS);
                b.extend_from_slice(&room.to_le_bytes());
                put_addr(&mut b, client);
            }
            RelayMsg::RoomEmpty { room } => {
                b.push(T_ROOM_EMPTY);
                b.extend_from_slice(&room.to_le_bytes());
            }
            RelayMsg::Keepalive { room, tag } => {
                b.push(T_KEEPALIVE);
                b.extend_from_slice(&room.to_le_bytes());
                b.extend_from_slice(&tag.to_le_bytes());
            }
            RelayMsg::RelayOut { dst_tag, payload } => {
                b.push(T_RELAY_OUT);
                b.extend_from_slice(&dst_tag.to_le_bytes());
                b.extend_from_slice(payload);
            }
            RelayMsg::RelayIn { src_tag, payload } => {
                b.push(T_RELAY_IN);
                b.extend_from_slice(&src_tag.to_le_bytes());
                b.extend_from_slice(payload);
            }
        }
        b
    }

    pub fn decode(buf: &[u8]) -> RelayResult<Self> {
        let mut pos = 0usize;
        let ty = read_u8(buf, &mut pos)
            .ok_or_else(|| RelayError::Protocol("空包".to_string()))?;
        let msg = match ty {
            T_REGISTER => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let role = read_u8(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::Register { room, role }
            }
            T_REGISTER_OK => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let tag = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::RegisterOk { room, tag }
            }
            T_CONNECT => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::Connect { room }
            }
            T_CONNECT_OK => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let tag = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let host = read_addr(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::ConnectOk { room, tag, host }
            }
            T_PEER_WANTS => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let client = read_addr(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::PeerWants { room, client }
            }
            T_ROOM_EMPTY => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::RoomEmpty { room }
            }
            T_KEEPALIVE => {
                let room = read_u32(buf, &mut pos).ok_or_else(bad)?;
                let tag = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::Keepalive { room, tag }
            }
            T_RELAY_OUT => {
                let dst_tag = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::RelayOut {
                    dst_tag,
                    payload: buf.get(pos..).unwrap_or(&[]).to_vec(),
                }
            }
            T_RELAY_IN => {
                let src_tag = read_u32(buf, &mut pos).ok_or_else(bad)?;
                RelayMsg::RelayIn {
                    src_tag,
                    payload: buf.get(pos..).unwrap_or(&[]).to_vec(),
                }
            }
            other => return Err(RelayError::Protocol(format!("未知类型 0x{other:02X}"))),
        };
        Ok(msg)
    }
}

fn bad() -> RelayError {
    RelayError::Protocol("包太短".to_string())
}

/// 打洞结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchResult {
    /// 直连成功，peer 地址可用
    Direct(SocketAddr),
    /// 没打通，应该改用 relay
    Failed,
}

fn bind_socket(bind: Option<SocketAddr>) -> RelayResult<UdpSocket> {
    let addr = bind.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let sock = UdpSocket::bind(addr).map_err(|e| RelayError::Io(format!("bind {addr}: {e}")))?;
    sock.set_read_timeout(Some(RECV_SLICE))
        .map_err(|e| RelayError::Io(format!("set_read_timeout: {e}")))?;
    Ok(sock)
}

/// 这个错误是不是"只是这一条 datagram 的 ICMP 报错"，可以直接忽略继续收。
///
/// Windows 上 UDP socket 在**上一次 send 的目标端口不可达**时，下一次 recv 会返回
/// `WSAECONNRESET`(10054) / `WSAECONNREFUSED`(10061)。这在 relay 建连/重连期间非常常见
/// （比如 udp-proxy 重启、对端刚掉线），**绝对不能当致命错误**——否则 host/client 会直接退出，
/// 表现为"弱网恢复后永远连不回来"。
pub fn is_transient_recv_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable
    )
}

fn recv_msg(sock: &UdpSocket, buf: &mut [u8]) -> RelayResult<Option<(RelayMsg, SocketAddr)>> {
    match sock.recv_from(buf) {
        Ok((n, from)) => {
            if n == 1 && buf[0] == PUNCH_BYTE {
                // 打洞探测包：原样当作"来自这个地址的包"，由 punch() 判定
                return Ok(Some((
                    RelayMsg::RelayIn {
                        src_tag: 0,
                        payload: vec![PUNCH_BYTE],
                    },
                    from,
                )));
            }
            match RelayMsg::decode(&buf[..n]) {
                Ok(m) => Ok(Some((m, from))),
                Err(_) => Ok(None), // 不认识的包直接忽略，别让服务端/对端把连接弄崩
            }
        }
        // 超时 / ICMP 触发的 10054 等都只代表"这一拍没有可用包"，继续循环即可
        Err(e) if is_transient_recv_error(&e) => Ok(None),
        Err(e) => Err(RelayError::Io(format!("recv_from: {e}"))),
    }
}

/// host 侧会话：在 relay 登记房间，等客户端来连。
pub struct HostSession {
    sock: UdpSocket,
    relay: SocketAddr,
    room: Room,
    tag: u32,
}

impl HostSession {
    /// 绑定本地 UDP（`bind` 为 None 则 0.0.0.0:0），并向 `relay` 登记 `room`。
    pub fn register(
        relay: SocketAddr,
        room: Room,
        bind: Option<SocketAddr>,
    ) -> RelayResult<Self> {
        let sock = bind_socket(bind)?;
        let pkt = RelayMsg::Register {
            room,
            role: ROLE_HOST,
        }
        .encode();
        let mut buf = vec![0u8; 2048];
        for _ in 0..REGISTER_TRIES {
            sock.send_to(&pkt, relay)
                .map_err(|e| RelayError::Io(format!("send REGISTER: {e}")))?;
            let deadline = Instant::now() + REGISTER_TIMEOUT;
            while Instant::now() < deadline {
                if let Some((msg, _from)) = recv_msg(&sock, &mut buf)? {
                    if let RelayMsg::RegisterOk { room: r, tag } = msg {
                        if r == room {
                            return Ok(Self {
                                sock,
                                relay,
                                room,
                                tag,
                            });
                        }
                    }
                }
            }
        }
        Err(RelayError::Io(format!(
            "REGISTER room={room} 超时（relay {relay} 没响应）"
        )))
    }

    pub fn tag(&self) -> u32 {
        self.tag
    }

    pub fn room(&self) -> Room {
        self.room
    }

    pub fn local_addr(&self) -> RelayResult<SocketAddr> {
        self.sock
            .local_addr()
            .map_err(|e| RelayError::Io(format!("local_addr: {e}")))
    }

    pub fn relay_addr(&self) -> SocketAddr {
        self.relay
    }

    fn send_keepalive(&self) {
        let pkt = RelayMsg::Keepalive {
            room: self.room,
            tag: self.tag,
        }
        .encode();
        let _ = self.sock.send_to(&pkt, self.relay);
    }

    /// 阻塞等一个客户端连进来（期间持续发 KEEPALIVE 保持房间存活）。
    pub fn wait_peer(&mut self, timeout: Duration) -> RelayResult<Option<SocketAddr>> {
        self.wait_peer_until(timeout, &AtomicBool::new(false))
    }

    /// 同 wait_peer，但可以被 stop 提前打断（返回 Ok(None)）。
    ///
    /// 用途：host 同时开 P2P 和 TCP 兜底时，P2P 等待在独立线程里跑；
    /// TCP 先连进来就把 stop 置位，让 P2P 线程立刻收手，不要攥着房间干等。
    pub fn wait_peer_until(
        &mut self,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> RelayResult<Option<SocketAddr>> {
        let deadline = Instant::now() + timeout;
        let mut next_ka = Instant::now();
        let mut buf = vec![0u8; 2048];
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if now >= next_ka {
                self.send_keepalive();
                next_ka = now + KEEPALIVE_INTERVAL;
            }
            match recv_msg(&self.sock, &mut buf)? {
                Some((RelayMsg::PeerWants { room, client }, _from)) if room == self.room => {
                    return Ok(Some(client));
                }
                Some(_) => {}
                None => {}
            }
        }
    }

    /// 与已知 peer 打洞。
    pub fn punch(&mut self, peer: SocketAddr, timeout: Duration) -> RelayResult<PunchResult> {
        punch_on(&self.sock, peer, timeout)
    }

    /// 拿到底层 socket（交给 `lrc_udp::UdpConn::new`）。
    pub fn take_socket(self) -> UdpSocket {
        self.sock
    }
}

/// client 侧会话：找房间、拿 host 地址。
pub struct ClientSession {
    sock: UdpSocket,
    relay: SocketAddr,
    room: Room,
    tag: u32,
    host: SocketAddr,
}

impl ClientSession {
    pub fn connect(relay: SocketAddr, room: Room, bind: Option<SocketAddr>) -> RelayResult<Self> {
        let sock = bind_socket(bind)?;
        let pkt = RelayMsg::Connect { room }.encode();
        let mut buf = vec![0u8; 2048];
        for _ in 0..REGISTER_TRIES {
            sock.send_to(&pkt, relay)
                .map_err(|e| RelayError::Io(format!("send CONNECT: {e}")))?;
            let deadline = Instant::now() + REGISTER_TIMEOUT;
            while Instant::now() < deadline {
                if let Some((msg, _from)) = recv_msg(&sock, &mut buf)? {
                    match msg {
                        RelayMsg::ConnectOk {
                            room: r,
                            tag,
                            host,
                        } if r == room => {
                            return Ok(Self {
                                sock,
                                relay,
                                room,
                                tag,
                                host,
                            });
                        }
                        RelayMsg::RoomEmpty { room: r } if r == room => {
                            return Err(RelayError::RoomEmpty);
                        }
                        _ => {}
                    }
                }
            }
        }
        Err(RelayError::Io(format!(
            "CONNECT room={room} 超时（relay {relay} 没响应）"
        )))
    }

    pub fn tag(&self) -> u32 {
        self.tag
    }

    pub fn room(&self) -> Room {
        self.room
    }

    /// relay 告知的 host 公网地址
    pub fn host_addr(&self) -> SocketAddr {
        self.host
    }

    pub fn relay_addr(&self) -> SocketAddr {
        self.relay
    }

    pub fn local_addr(&self) -> RelayResult<SocketAddr> {
        self.sock
            .local_addr()
            .map_err(|e| RelayError::Io(format!("local_addr: {e}")))
    }

    pub fn punch(&mut self, timeout: Duration) -> RelayResult<PunchResult> {
        punch_on(&self.sock, self.host, timeout)
    }

    pub fn take_socket(self) -> UdpSocket {
        self.sock
    }
}

/// 双方同时向 peer 发 0xFF 探测包；收到来自该地址的任意包就判定直连成功。
fn punch_on(sock: &UdpSocket, peer: SocketAddr, timeout: Duration) -> RelayResult<PunchResult> {
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 2048];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(PunchResult::Failed);
        }
        if now >= next_send {
            // 探测包发不出去（比如网络不可达）不算致命错误，继续试
            let _ = sock.send_to(&[PUNCH_BYTE], peer);
            next_send = now + PUNCH_INTERVAL;
        }
        match sock.recv_from(&mut buf) {
            Ok((_n, from)) if from == peer => return Ok(PunchResult::Direct(peer)),
            // relay 回的包 / 其它地址的包：忽略，继续等对端
            Ok(_) => {}
            Err(e) if is_transient_recv_error(&e) => {}
            Err(e) => return Err(RelayError::Io(format!("punch recv: {e}"))),
        }
    }
}

/// relay 模式下对端的 `dst_tag`。
///
/// 协议表里 PEER_WANTS / CONNECT_OK 都不带对方 tag，所以两个方向都**不知道**对端 tag；
/// 这里统一用 0 表示"同房间里的另一端"，由 relay 按房间路由（见 server.rs）。
pub const RELAY_DST_TAG_ROOM: u32 = 0;

/// 便捷封装：一次调用完成"登记/连接 + 打洞"，失败自动降级到 relay。
///
/// 返回 (socket, peer 地址, mode 描述, 是否走了 relay)。
pub fn establish(
    is_host: bool,
    relay: SocketAddr,
    room: Room,
    bind: Option<SocketAddr>,
    peer_wait: Duration,
    punch_timeout: Duration,
) -> RelayResult<Established> {
    establish_cancellable(
        is_host,
        relay,
        room,
        bind,
        peer_wait,
        punch_timeout,
        &AtomicBool::new(false),
    )
}

/// 同 [establish]，但 host 侧等 peer 时可被 stop 打断。
pub fn establish_cancellable(
    is_host: bool,
    relay: SocketAddr,
    room: Room,
    bind: Option<SocketAddr>,
    peer_wait: Duration,
    punch_timeout: Duration,
    stop: &AtomicBool,
) -> RelayResult<Established> {
    if is_host {
        let mut hs = HostSession::register(relay, room, bind)?;
        let tag = hs.tag();
        let Some(peer) = hs.wait_peer_until(peer_wait, stop)? else {
            // 没人来：仍然给一个 relay 模式（host 会继续被 KEEPALIVE 保活）
            return Ok(Established {
                socket: hs.take_socket(),
                mode: lrc_udp::PeerMode::Relay {
                    relay,
                    src_tag: tag,
                    dst_tag: RELAY_DST_TAG_ROOM,
                },
                direct_peer: None,
            });
        };
        match hs.punch(peer, punch_timeout)? {
            PunchResult::Direct(p) => Ok(Established {
                socket: hs.take_socket(),
                mode: lrc_udp::PeerMode::Direct,
                direct_peer: Some(p),
            }),
            PunchResult::Failed => Ok(Established {
                socket: hs.take_socket(),
                mode: lrc_udp::PeerMode::Relay {
                    relay,
                    src_tag: tag,
                    dst_tag: RELAY_DST_TAG_ROOM,
                },
                direct_peer: None,
            }),
        }
    } else {
        let mut cs = ClientSession::connect(relay, room, bind)?;
        let tag = cs.tag();
        match cs.punch(punch_timeout)? {
            PunchResult::Direct(p) => Ok(Established {
                socket: cs.take_socket(),
                mode: lrc_udp::PeerMode::Direct,
                direct_peer: Some(p),
            }),
            PunchResult::Failed => Ok(Established {
                socket: cs.take_socket(),
                mode: lrc_udp::PeerMode::Relay {
                    relay,
                    src_tag: tag,
                    dst_tag: RELAY_DST_TAG_ROOM,
                },
                direct_peer: None,
            }),
        }
    }
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
                lrc_udp::PeerMode::Direct => SocketAddr::from(([0, 0, 0, 0], 0)),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_roundtrip_v4_v6() {
        for a in [
            SocketAddr::from(([127, 0, 0, 1], 7801)),
            SocketAddr::from(([192, 168, 1, 42], 65535)),
            SocketAddr::from(([0u16; 8], 1234)),
        ] {
            let mut b = Vec::new();
            put_addr(&mut b, &a);
            assert_eq!(b.len(), 1 + 2 + 16);
            let mut pos = 0;
            assert_eq!(read_addr(&b, &mut pos), Some(a));
        }
    }

    #[test]
    fn all_messages_roundtrip() {
        let host = SocketAddr::from(([1, 2, 3, 4], 5555));
        let client = SocketAddr::from(([5, 6, 7, 8], 6666));
        let msgs = vec![
            RelayMsg::Register {
                room: 7,
                role: ROLE_HOST,
            },
            RelayMsg::RegisterOk { room: 7, tag: 42 },
            RelayMsg::Connect { room: 7 },
            RelayMsg::ConnectOk {
                room: 7,
                tag: 43,
                host,
            },
            RelayMsg::PeerWants { room: 7, client },
            RelayMsg::RoomEmpty { room: 7 },
            RelayMsg::Keepalive { room: 7, tag: 42 },
            RelayMsg::RelayOut {
                dst_tag: 43,
                payload: vec![1, 2, 3, 4, 5],
            },
            RelayMsg::RelayIn {
                src_tag: 43,
                payload: vec![9, 8, 7],
            },
        ];
        for m in msgs {
            let enc = m.encode();
            let dec = RelayMsg::decode(&enc).expect("decode");
            assert_eq!(dec, m);
        }
    }

    #[test]
    fn relay_payload_survives_binary() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let m = RelayMsg::RelayIn {
            src_tag: 1,
            payload: payload.clone(),
        };
        match RelayMsg::decode(&m.encode()).expect("decode") {
            RelayMsg::RelayIn { payload: p, .. } => assert_eq!(p, payload),
            other => panic!("类型不对: {other:?}"),
        }
    }

    /// `--p2p relay` 在 host/client 侧是用 punch_timeout = 0 表达的：
    /// 这种情况下 punch 必须**立刻返回 Failed 且一个探测包都不发**，也不能改动 socket 状态
    /// （否则会对端收到莫名奇妙的 0xFF，或留下待读数据干扰后续 lrc-udp）。
    #[test]
    fn zero_timeout_punch_sends_nothing() {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
        peer.set_read_timeout(Some(Duration::from_millis(150)))
            .expect("rt");
        let peer_addr = peer.local_addr().expect("addr");
        let me = UdpSocket::bind("127.0.0.1:0").expect("bind me");

        let t0 = Instant::now();
        let r = punch_on(&me, peer_addr, Duration::ZERO).expect("punch");
        let elapsed = t0.elapsed();
        assert_eq!(r, PunchResult::Failed);
        assert!(
            elapsed < Duration::from_millis(50),
            "punch_timeout=0 必须立刻返回，实际 {elapsed:?}"
        );

        // 对端不该收到任何探测包
        let mut buf = [0u8; 64];
        assert!(
            peer.recv_from(&mut buf).is_err(),
            "punch_timeout=0 不应发出任何探测包"
        );
    }

    #[test]
    fn short_and_unknown_packets_are_rejected() {
        assert!(RelayMsg::decode(&[]).is_err());
        assert!(RelayMsg::decode(&[T_CONNECT]).is_err()); // 缺 room
        assert!(RelayMsg::decode(&[0x7F, 1, 2, 3]).is_err()); // 未知类型
    }
}
