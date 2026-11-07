//! relay 服务端：房间登记 + 地址交换 + UDP 中继转发。
//!
//! 单线程 `recv_from` 循环就够（自用场景 1–2 个房间）。
//! 关键点：
//! - 每个 peer 分配一个 `tag`，`tag -> addr`、`addr -> room` 双向索引；
//! - `RELAY_OUT{dst_tag}`：dst_tag != 0 走 tag 精确路由；**dst_tag == 0 表示"同房间另一端"**，
//!   按发送方的房间找对端（协议表里 PEER_WANTS/CONNECT_OK 都不带对方 tag，所以两个方向都用 0）；
//! - 房间 `last_seen` 超过 `room_ttl` 就清掉，client 再来会收到 `ROOM_EMPTY`。

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{RelayMsg, Room, T_CONNECT, T_KEEPALIVE, T_REGISTER, T_RELAY_OUT};

/// 服务端配置。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 监听地址
    pub bind: SocketAddr,
    /// 房间空闲多久算过期
    pub room_ttl: Duration,
    /// 最多同时存在多少个房间
    pub max_rooms: usize,
    /// --stats 打印间隔（None = 关闭）
    pub stats_interval: Option<Duration>,
    /// 每个包打一行日志
    pub verbose: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], crate::DEFAULT_RELAY_PORT)),
            room_ttl: Duration::from_secs(60),
            max_rooms: 64,
            stats_interval: Some(Duration::from_secs(5)),
            verbose: false,
        }
    }
}

#[derive(Debug)]
struct RoomState {
    host: Option<SocketAddr>,
    host_tag: u32,
    client: Option<SocketAddr>,
    client_tag: u32,
    last_seen: Instant,
}

impl RoomState {
    fn new() -> Self {
        Self {
            host: None,
            host_tag: 0,
            client: None,
            client_tag: 0,
            last_seen: Instant::now(),
        }
    }
}

/// relay 服务端。
pub struct Server {
    sock: UdpSocket,
    cfg: ServerConfig,
    rooms: HashMap<Room, RoomState>,
    /// tag -> 地址
    tag_addr: HashMap<u32, SocketAddr>,
    /// 地址 -> (房间, tag)
    addr_info: HashMap<SocketAddr, (Room, u32)>,
    next_tag: u32,
    /// 中转统计
    pub relayed_bytes: u64,
    pub relayed_pkts: u64,
    last_stats: Instant,
}

impl Server {
    /// 绑定端口（`cfg.bind` 的端口可以传 0，测试用）。
    pub fn bind(cfg: ServerConfig) -> io::Result<Self> {
        let sock = UdpSocket::bind(cfg.bind)?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(Self {
            sock,
            cfg,
            rooms: HashMap::new(),
            tag_addr: HashMap::new(),
            addr_info: HashMap::new(),
            next_tag: 1,
            relayed_bytes: 0,
            relayed_pkts: 0,
            last_stats: Instant::now(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    fn alloc_tag(&mut self) -> u32 {
        let t = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        t
    }

    fn send_msg(&self, msg: &RelayMsg, to: SocketAddr) {
        let buf = msg.encode();
        if let Err(e) = self.sock.send_to(&buf, to) {
            log::warn!("relay: 发给 {to} 失败: {e}");
        }
    }

    /// 找"同房间里除去 sender 的另一端"。
    fn room_other(&self, room: Room, sender: SocketAddr) -> Option<(SocketAddr, u32)> {
        let st = self.rooms.get(&room)?;
        if Some(sender) == st.host {
            st.client.map(|a| (a, st.client_tag))
        } else if Some(sender) == st.client {
            st.host.map(|a| (a, st.host_tag))
        } else {
            None
        }
    }

    fn handle(&mut self, buf: &[u8], from: SocketAddr) {
        if buf.len() == 1 && buf[0] == crate::PUNCH_BYTE {
            // 打洞探测包：relay 不参与，直接忽略
            return;
        }
        let ty = buf.first().copied().unwrap_or(0);
        // 刷新 last_seen（任何来自已登记地址的包都算活跃）
        if let Some((room, _tag)) = self.addr_info.get(&from).copied() {
            if let Some(st) = self.rooms.get_mut(&room) {
                st.last_seen = Instant::now();
            }
        }
        match ty {
            T_REGISTER => {
                let room = match read_room_and_role(buf) {
                    Some((r, _role)) => r,
                    None => return,
                };
                if !self.rooms.contains_key(&room) && self.rooms.len() >= self.cfg.max_rooms {
                    log::warn!("relay: 房间数已满({})，拒绝 room={room}", self.cfg.max_rooms);
                    return;
                }
                let tag = self.alloc_tag();
                let st = self.rooms.entry(room).or_insert_with(RoomState::new);
                // 同一个 host 重新登记：把旧 tag 清掉
                if st.host_tag != 0 {
                    self.tag_addr.remove(&st.host_tag);
                }
                st.host = Some(from);
                st.host_tag = tag;
                st.last_seen = Instant::now();
                self.tag_addr.insert(tag, from);
                self.addr_info.insert(from, (room, tag));
                self.send_msg(&RelayMsg::RegisterOk { room, tag }, from);
                if self.cfg.verbose {
                    log::info!("relay: REGISTER room={room} host={from} tag={tag}");
                }
            }
            T_CONNECT => {
                let room = match read_room_u32(buf) {
                    Some(r) => r,
                    None => return,
                };
                let host = self
                    .rooms
                    .get(&room)
                    .and_then(|st| st.host)
                    .filter(|_| {
                        self.rooms
                            .get(&room)
                            .map(|st| st.last_seen.elapsed() <= self.cfg.room_ttl)
                            .unwrap_or(false)
                    });
                match host {
                    Some(host) => {
                        let tag = self.alloc_tag();
                        if let Some(st) = self.rooms.get_mut(&room) {
                            st.client = Some(from);
                            st.client_tag = tag;
                            st.last_seen = Instant::now();
                        }
                        self.tag_addr.insert(tag, from);
                        self.addr_info.insert(from, (room, tag));
                        self.send_msg(
                            &RelayMsg::ConnectOk {
                                room,
                                tag,
                                host,
                            },
                            from,
                        );
                        self.send_msg(
                            &RelayMsg::PeerWants {
                                room,
                                client: from,
                            },
                            host,
                        );
                        if self.cfg.verbose {
                            log::info!("relay: CONNECT room={room} client={from} tag={tag} host={host}");
                        }
                    }
                    None => {
                        self.send_msg(&RelayMsg::RoomEmpty { room }, from);
                        if self.cfg.verbose {
                            log::info!("relay: CONNECT room={room} 无 host -> ROOM_EMPTY");
                        }
                    }
                }
            }
            T_KEEPALIVE => {
                if let Some(room) = read_room_u32(buf) {
                    if let Some(st) = self.rooms.get_mut(&room) {
                        st.last_seen = Instant::now();
                    }
                }
            }
            T_RELAY_OUT => {
                let dst_tag = match read_dst_tag(buf) {
                    Some(t) => t,
                    None => return,
                };
                let target = if dst_tag != 0 {
                    self.tag_addr.get(&dst_tag).copied()
                } else {
                    // 0 = 同房间的另一端
                    self.addr_info
                        .get(&from)
                        .copied()
                        .and_then(|(room, _)| self.room_other(room, from))
                        .map(|(a, _)| a)
                };
                let src_tag = self
                    .addr_info
                    .get(&from)
                    .map(|(_, t)| *t)
                    .unwrap_or(0);
                let payload = buf.get(5..).unwrap_or(&[]);
                match target {
                    Some(to) => {
                        let out = RelayMsg::RelayIn {
                            src_tag,
                            payload: payload.to_vec(),
                        };
                        let enc = out.encode();
                        match self.sock.send_to(&enc, to) {
                            Ok(_) => {
                                self.relayed_bytes = self.relayed_bytes.saturating_add(payload.len() as u64);
                                self.relayed_pkts = self.relayed_pkts.saturating_add(1);
                            }
                            Err(e) => log::warn!("relay: 转发给 {to} 失败: {e}"),
                        }
                    }
                    None => {
                        log::debug!("relay: RELAY_OUT 找不到对端（from={from} dst_tag={dst_tag}）");
                    }
                }
            }
            _ => {}
        }
    }

    /// 清理过期房间 + 打印统计。
    fn tick(&mut self) {
        let now = Instant::now();
        let ttl = self.cfg.room_ttl;
        let expired: Vec<Room> = self
            .rooms
            .iter()
            .filter(|(_, st)| now.duration_since(st.last_seen) > ttl)
            .map(|(r, _)| *r)
            .collect();
        for room in expired {
            if let Some(st) = self.rooms.remove(&room) {
                for tag in [st.host_tag, st.client_tag] {
                    if tag != 0 {
                        self.tag_addr.remove(&tag);
                    }
                }
                for a in [st.host, st.client].into_iter().flatten() {
                    self.addr_info.remove(&a);
                }
                log::info!("relay: 房间 {room} 过期清除");
            }
        }
        if let Some(iv) = self.cfg.stats_interval {
            if self.last_stats.elapsed() >= iv {
                self.last_stats = now;
                self.print_stats();
            }
        }
    }

    pub fn print_stats(&self) {
        let reachable = self
            .rooms
            .values()
            .filter(|st| st.host.is_some() && st.last_seen.elapsed() <= self.cfg.room_ttl)
            .count();
        println!(
            "rooms={} reachable={} relayed_bytes={} relayed_pkts={}",
            self.rooms.len(),
            reachable,
            self.relayed_bytes,
            self.relayed_pkts
        );
    }

    /// 主循环；`stop` 置 true 后退出（测试用，正常运行时永不置位）。
    pub fn run(&mut self, stop: &AtomicBool) -> io::Result<()> {
        let mut buf = vec![0u8; 4096];
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => self.handle(&buf[..n], from),
                // 超时 → 继续循环；Windows 上 UDP 收到 ICMP 端口不可达会在下一次 recv 返回
                // WSAECONNRESET(10054)。relay 上这种情况很常见（客户端重启/断线），
                // **绝对不能因此退出**，否则整个信令/中继服务直接挂掉。
                Err(e) if crate::is_transient_recv_error(&e) => {}
                Err(e) => return Err(e),
            }
            self.tick();
        }
    }
}

fn read_room_u32(buf: &[u8]) -> Option<Room> {
    let s = buf.get(1..5)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_room_and_role(buf: &[u8]) -> Option<(Room, u8)> {
    let room = read_room_u32(buf)?;
    let role = *buf.get(5)?;
    Some((room, role))
}

fn read_dst_tag(buf: &[u8]) -> Option<u32> {
    let s = buf.get(1..5)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientSession, HostSession, PunchResult, RelayError, T_CONNECT_OK, T_REGISTER_OK,
    };
    use std::sync::Arc;
    use std::thread;

    /// 收包直到拿到 RELAY_IN（中间可能有 PEER_WANTS/探测包等干扰）。
    fn recv_relay_in(sock: &UdpSocket, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Ok(RelayMsg::RelayIn { payload, .. }) = RelayMsg::decode(&buf[..n]) {
                        return Some(payload);
                    }
                }
                Err(_) => {}
            }
        }
        None
    }

    fn start_server(ttl: Duration) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let cfg = ServerConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            room_ttl: ttl,
            max_rooms: 8,
            stats_interval: None,
            verbose: false,
        };
        let mut srv = Server::bind(cfg).expect("bind");
        let addr = srv.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let h = thread::spawn(move || {
            let _ = srv.run(&st);
        });
        (addr, stop, h)
    }

    #[test]
    fn register_and_connect_exchange_addrs() {
        let (addr, stop, h) = start_server(Duration::from_secs(30));
        let host = HostSession::register(addr, 1, None).expect("register");
        let host_local = host.local_addr().expect("host addr");
        assert!(host.tag() > 0);

        let client = ClientSession::connect(addr, 1, None).expect("connect");
        assert!(client.tag() > 0);
        // relay 把 host 的**观测地址**告诉 client（host 绑的是 0.0.0.0，所以只比端口）
        assert_eq!(client.host_addr().port(), host_local.port());
        assert!(client.host_addr().ip().is_loopback());
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
    }

    #[test]
    fn connect_to_missing_room_is_room_empty() {
        let (addr, stop, h) = start_server(Duration::from_secs(30));
        let r = ClientSession::connect(addr, 999, None);
        assert!(matches!(r, Err(RelayError::RoomEmpty)));
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
    }

    #[test]
    fn room_expires_and_then_is_empty() {
        let (addr, stop, h) = start_server(Duration::from_millis(400));
        let host = HostSession::register(addr, 5, None).expect("register");
        // 不发 keepalive，等 TTL 过去
        thread::sleep(Duration::from_millis(900));
        let r = ClientSession::connect(addr, 5, None);
        assert!(
            matches!(r, Err(RelayError::RoomEmpty)),
            "房间过期后必须 ROOM_EMPTY（拿到 {:?}）",
            r.err()
        );
        drop(host);
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
    }

    #[test]
    fn punch_succeeds_on_loopback() {
        let (addr, stop, h) = start_server(Duration::from_secs(30));
        let mut host = HostSession::register(addr, 2, None).expect("register");
        let mut client = ClientSession::connect(addr, 2, None).expect("connect");

        // 双线程同时 punch（host 先 wait_peer 拿地址）
        let t = thread::spawn(move || {
            let peer = host.wait_peer(Duration::from_secs(3)).expect("wait_peer");
            let peer = peer.expect("应该有 client");
            let t0 = Instant::now();
            let r = host.punch(peer, Duration::from_millis(1500)).expect("punch");
            (r, t0.elapsed())
        });
        let t0 = Instant::now();
        let cr = client.punch(Duration::from_millis(1500)).expect("punch");
        let client_elapsed = t0.elapsed();
        let (hr, host_elapsed) = t.join().expect("join");
        assert!(matches!(cr, PunchResult::Direct(_)), "client 打洞应成功");
        assert!(matches!(hr, PunchResult::Direct(_)), "host 打洞应成功");
        println!(
            "punch 成功：client {:.1}ms, host {:.1}ms",
            client_elapsed.as_secs_f64() * 1000.0,
            host_elapsed.as_secs_f64() * 1000.0
        );
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
    }

    #[test]
    fn relay_forwards_data_both_ways() {
        let (addr, stop, h) = start_server(Duration::from_secs(30));
        let host = HostSession::register(addr, 3, None).expect("register");
        let host_tag = host.tag();
        let client = ClientSession::connect(addr, 3, None).expect("connect");
        let client_tag = client.tag();

        // 用裸包验证 relay 转发：client -> host
        let cs = client.take_socket();
        let hs = host.take_socket();
        cs.set_read_timeout(Some(Duration::from_millis(500))).expect("rt");
        hs.set_read_timeout(Some(Duration::from_millis(500))).expect("rt");

        let out = RelayMsg::RelayOut {
            dst_tag: 0, // 0 = 同房间另一端（host）
            payload: b"hello-host".to_vec(),
        };
        cs.send_to(&out.encode(), addr).expect("send");
        let payload = recv_relay_in(&hs, Duration::from_secs(2)).expect("host 应收到转发包");
        assert_eq!(payload, b"hello-host");

        // host -> client（host 不知道 client tag，同样用 0）
        let out = RelayMsg::RelayOut {
            dst_tag: 0,
            payload: b"hello-client".to_vec(),
        };
        hs.send_to(&out.encode(), addr).expect("send");
        let payload = recv_relay_in(&cs, Duration::from_secs(2)).expect("client 应收到转发包");
        assert_eq!(payload, b"hello-client");
        // client 的 tag 拿到过（CONNECT_OK 里）
        assert!(client_tag > 0 && host_tag > 0 && client_tag != host_tag);
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
    }

    #[test]
    fn raw_protocol_constants_are_stable() {
        // 线格式钉死，改常量等于改协议
        assert_eq!(T_REGISTER, 0x01);
        assert_eq!(T_REGISTER_OK, 0x81);
        assert_eq!(T_CONNECT_OK, 0x82);
    }
}
