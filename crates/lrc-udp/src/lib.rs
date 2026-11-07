//! 可靠有序的 UDP 消息传输（KCP 血统的极简实现），供 P2P / relay 两条路径共用。
//!
//! 目标：**在 UDP 上提供"消息边界 + 可靠 + 有序"**，语义与 lrc-transport 的 TCP 版一致，
//! 使 host/client 只需要换一个连接对象，上层逻辑（lrc-proto 的帧）完全不用改。
//!
//! # 线格式（本 crate 内部，不外泄给 lrc-proto；relay 只做转发不解析）
//!
//! ```text
//! 数据分片 (type 0x01):
//!   [0x01][conn_id u32][seq u32][frag u16][fcnt u16][len u16][payload len]
//! ACK (type 0x02):
//!   [0x02][conn_id u32][ack u32][ack_bits u32][recv_wnd u16]
//!   [nack_seq u32][nack_cnt u8][frag u16 × nack_cnt]
//! 保活/探测 (type 0x03):
//!   [0x03][conn_id u32][nonce u32]
//! 保活回应 (type 0x04):
//!   [0x04][conn_id u32][nonce u32]
//! relay 出站信封:  [0x10][dst_tag u32][内层包]   （先发到 relay，由 relay 转发）
//! relay 入站信封:  [0x90][src_tag u32][内层包]   （收到时剥掉这 5 字节再当普通包处理）
//! ```
//!
//! - 一条消息按 `MTU_PAYLOAD` 分片，`seq` 是**消息序号**（同一消息的所有分片 seq 相同，frag 递增）；
//! - 接收端按 seq 顺序交付；乱序/缺失时**等待**，并用 ACK 里的 `ack_bits` 告诉发送端
//!   窗口内哪些消息已经收全；
//! - 发送端维护发送队列 + 未确认集合，`RTO = max(100ms, 2*RTT)`，指数退避上限 1s，**只重传缺失分片**；
//! - 收发都在 `UdpConn` 内部线程安全地跑，上层可以 send/recv 分别在不同线程。
//!
//! ## 与最初注释的两处差异（实现时按"必须支持 1 MB 分片消息"做的调整）
//!
//! 1. `frag` / `fcnt` 从 u8 拓宽成 **u16**：`MAX_FRAGS = 8192` 才能覆盖 8 MiB 的
//!    `MAX_MESSAGE`（原注释里的 64 片 × 1200B 只够 75 KB，和"1 MB 分片消息"的自测要求矛盾）。
//! 2. ACK 尾部多了一段**NACK 清单**（`nack_seq` + 缺失 frag 下标）：原格式只有消息级的
//!    `ack/ack_bits`，表达不了"某条消息缺了第几片"，而需求要求**只重传缺失分片**。
//!    这是向后兼容的追加字段（老解析器读前 20 字节即可）。
//!
//! # 语义
//!
//! - `send` 只负责把消息分片入队并尽力发出，返回时**不保证出网**；重传由 `tick` /
//!   `recv_timeout` 内部驱动，所以**上层只要持续调 recv_timeout 就够**（不需要额外线程）。
//! - 空闲 1s 发一次 PING，对端回 PONG；连续 `DEAD_AFTER`（默认 6s，可用 `LRC_UDP_DEAD_MS` 覆盖）没收到对方任何包
//!   → `recv_timeout` 返回 `io::ErrorKind::TimedOut`（P2P 下 NAT 映射会过期，这条是断线信号）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 单条消息上限（与 TCP 传输层的 MAX_PAYLOAD 对齐）。
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// 单个分片承载的负载字节数（留出头部，总包 < 1400）。
pub const MTU_PAYLOAD: usize = 1200;
/// 一条消息最多分片数（需覆盖 MAX_MESSAGE：8192 * 1200 > 8 MiB）。
pub const MAX_FRAGS: usize = 8192;

/// 数据分片。
const TYPE_DATA: u8 = 0x01;
/// 确认 + NACK。
const TYPE_ACK: u8 = 0x02;
/// 保活探测。
const TYPE_PING: u8 = 0x03;
/// 保活回应。
const TYPE_PONG: u8 = 0x04;
/// relay 出站信封。
const RELAY_OUT: u8 = 0x10;
/// relay 入站信封。
const RELAY_IN: u8 = 0x90;
/// relay 信封头部长度：类型 1 字节 + tag 4 字节。
const RELAY_HDR: usize = 5;

/// 数据分片头长度。
const DATA_HDR: usize = 1 + 4 + 4 + 2 + 2 + 2;
/// ACK 固定头长度（不含 NACK 清单）：type(1)+conn(4)+ack(4)+bits(4)+wnd(2)+nack_seq(4)+cnt(1)。
const ACK_HDR: usize = 1 + 4 + 4 + 4 + 2 + 4 + 1;
/// ACK 里 nack_seq 的偏移。
const ACK_NACK_SEQ_OFF: usize = 1 + 4 + 4 + 4 + 2;
/// ACK 里 nack_cnt 的偏移。
const ACK_NACK_CNT_OFF: usize = ACK_NACK_SEQ_OFF + 4;
/// 控制包长度（PING/PONG）。
const CTRL_LEN: usize = 1 + 4 + 4;

/// 单次 ACK 最多报告的缺失分片数（限制 ACK 体积）。
const MAX_NACK: usize = 64;

/// 空闲多久发一次保活探测。
pub const PING_IDLE: Duration = Duration::from_secs(1);
/// 多久没收到对方任何包就判死（recv_timeout 返回 TimedOut）的**默认值**。
///
/// 6 秒：全丢场景下 10 秒才判死链，用户感知是"卡住不动"；6 秒更接近 RDP/ToDesk 的手感。
/// 需要更激进/更宽松可以用环境变量 [`DEAD_ENV`]（毫秒）覆盖。
pub const DEAD_AFTER: Duration = Duration::from_secs(6);
/// 覆盖判死时长的环境变量名（毫秒，正整数；非法值忽略，用默认值）。
pub const DEAD_ENV: &str = "LRC_UDP_DEAD_MS";

/// 解析判死时长：`None`/非法/0 一律回落到默认 [`DEAD_AFTER`]（纯函数，方便单测）。
fn parse_dead_ms(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEAD_AFTER)
}

/// 运行期的判死时长（读一次 `LRC_UDP_DEAD_MS` 并缓存）。
pub fn dead_after() -> Duration {
    static CACHE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let raw = std::env::var(DEAD_ENV).ok();
        let value = parse_dead_ms(raw.as_deref());
        if value != DEAD_AFTER {
            log::info!("lrc-udp: 判死时长由环境变量 {DEAD_ENV} 覆盖为 {} ms", value.as_millis());
        }
        value
    })
}
/// 最小 RTO。
/// 最小 RTO：跨公网 RTT 就 80-95ms，100ms 会在 ACK 还在路上时就重传（放大元凶）。
const MIN_RTO: Duration = Duration::from_millis(200);
/// RTO 指数退避上限。
const MAX_RTO: Duration = Duration::from_secs(1);
/// recv_timeout 的轮询间隔（锁外 sleep）。
const POLL: Duration = Duration::from_millis(1);
/// 最小 ACK 间隔下限（实际取 max(这个值, SRTT/8)）：
/// ACK 也要走中继、也算钱，2ms 一个太奢侈。
const ACK_INTERVAL_MIN: Duration = Duration::from_millis(5);
/// NACK 最小间隔的下限（实际间隔取 max(这个值, SRTT)：快链路补得快，慢链路别催）。
const NACK_INTERVAL_MIN: Duration = Duration::from_millis(20);
/// 一次 ACK 周期里额外报告几条乱序消息的 NACK（轮转，避免只通知队头 → 其它消息只能靠 RTO 全量重传）。
const EXTRA_NACK: usize = 3;
/// 一条消息停滞多久后，才把"尾部缺口"当成真丢包去 NACK。
const NACK_STALE: Duration = Duration::from_millis(300);
/// 单次持锁内最多抽多少个包（保证单次持锁 < 1ms）。
const PUMP_BATCH: usize = 256;
/// 单次 drive 里最多处理几条超时消息（避免重传风暴）。
const DRIVE_BATCH: usize = 1;
/// 单次 flush 最多发多少个数据报（软限速：突发打得太快会把对端 socket 缓冲打爆，
/// UDP 丢了就得靠重传补，比限速贵得多）。4 个/次 ≈ 数 MB/s 上限，足够 2-8Mbps 视频。
const FLUSH_BURST: usize = 4;
/// 单次 RTO 重传最多补多少个分片（RTO 只做"轻探测"，精确补洞交给 NACK；
/// 32 片会让 RTO 与 NACK 双补，是跨公网放大的第二个来源）。
const RTO_BURST: usize = 8;
/// 待发包队列上限（超了丢最旧；丢掉的会被 RTO 重传补上）。
const OUT_QUEUE_MAX: usize = 16384;
/// 在途消息窗口（消息数）：必须 ≤ ACK 的 32 位窗口，否则 ACK 位图覆盖不到，
/// 发送端会以为没收到而反复 RTO 重传 —— 这就是跨公网放大 4 倍的主因之一。
const MAX_INFLIGHT_MSGS: usize = 32;
/// 在途字节上限（大消息不能因为条数少就无限灌）。
/// 在途字节上限：约等于 BDP 的几倍。设太大（1MB）会在 2Mbps 链路上堆进 4 秒的缓冲，
/// 中继/对端 socket 缓冲一溢出就是成片丢包 → 重传放大。256KB 在 2~10Mbps 都合适。
const MAX_INFLIGHT_BYTES: usize = 256 * 1024;
/// socket 收发缓冲目标值（Windows 默认可能只有 64 KB，视频大帧突发会被内核直接丢掉）。
const SOCKET_BUFFER: i32 = 4 * 1024 * 1024;

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

    /// 出站包该发到哪个地址。
    fn send_addr(&self, peer: SocketAddr) -> SocketAddr {
        match self {
            PeerMode::Direct => peer,
            PeerMode::Relay { relay, .. } => *relay,
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
    /// 线上发出的总字节（含重传、ACK、PING；放大率分子用）
    pub sent_bytes: u64,
    /// 应用层 payload 字节（放大率分母用）
    pub data_bytes: u64,
    /// RTO 触发的重传分片数
    pub rto_retrans: u64,
    /// NACK 触发的重传分片数
    pub nack_retrans: u64,
    /// 发出的 ACK 包数
    pub acks_sent: u64,
    /// 发出的 NACK 条目数
    pub nacks_sent: u64,
    /// RTT 抖动（RFC6298 的 RTTVAR）
    pub rtt_var_ms: f32,
}

impl UdpStats {
    /// 线上字节 / 应用层字节（1.0 = 没有额外开销；跨公网 relay 场景要看这个）。
    pub fn amplification(&self) -> f64 {
        if self.data_bytes == 0 {
            return 0.0;
        }
        self.sent_bytes as f64 / self.data_bytes as f64
    }
}

/// 发送中的一条消息。
struct SendMsg {
    /// 消息序号（在 send 里就分配好，排 backlog 时也保持顺序）
    seq: u32,
    /// 预组好的数据报（已含 relay 信封），下标即 frag
    frags: Vec<Vec<u8>>,
    /// 每片是否已被确认
    acked: Vec<bool>,
    /// 首次发出的时刻（算 RTT 用）
    first_sent: Instant,
    /// 最近一次（重）传时刻
    last_sent: Instant,
    /// 当前 RTO（指数退避）
    rto: Duration,
    /// 重传轮数
    retries: u32,
    /// RTO 重传的轮转起点（分批补洞，避免一条大消息每轮全量重发）
    resend_cursor: usize,
    /// 入队时发送队列是空的（= 立即出网），这样算出的 RTT 才没有排队时间
    timely: bool,
    /// 所有分片的线上总字节（在途字节窗口用）
    wire_bytes: usize,

}

/// 接收中的一条消息。
struct RecvMsg {
    frags: Vec<Option<Vec<u8>>>,
    got: u32,
    total: u32,
    last_seen: Instant,
    /// 已见到的最高分片下标 + 1（NACK 只报它之前的缺口）
    max_frag: usize,
    /// 该分片是否已经被 NACK 报过一次（丢包统计只数唯一缺口，避免同一片每轮 ACK 重复计数）
    notified: Vec<bool>,
    /// 收到这条消息第一个分片的时刻（NACK 要等一个 RTT 之后再报，别把"还在路上"的片当丢了）
    first_seen: Instant,
}

impl RecvMsg {
    fn new(total: u32, now: Instant) -> Self {
        Self {
            frags: vec![None; total as usize],
            got: 0,
            total,
            last_seen: now,
            max_frag: 0,
            notified: vec![false; total as usize],
            first_seen: now,
        }
    }

    fn complete(&self) -> bool {
        self.got == self.total
    }

    /// 缺片下标（最多 MAX_NACK 个）。
    ///
    /// 只报"已见到的最高分片"之前的缺口：它之后的片很可能还在发送端队列里没发出来，
    /// 报上去只会换来一次重复发送，反而浪费带宽。
    fn missing(&self, now: Instant) -> Vec<u16> {
        let mut out: Vec<u16> = self.frags
            .iter()
            .enumerate()
            .take(self.max_frag)
            .filter(|(_, f)| f.is_none())
            .map(|(i, _)| i as u16)
            .take(MAX_NACK)
            .collect();
        // 尾部（>= max_frag）的缺口平时多半还在对端队列里，只有整条消息停滞 NACK_STALE
        // 之后才当成真丢了去要（否则会造成大量重复发送）
        if out.len() < MAX_NACK && now.saturating_duration_since(self.last_seen) >= NACK_STALE {
            for (i, f) in self.frags.iter().enumerate().skip(self.max_frag) {
                if f.is_none() && out.len() < MAX_NACK {
                    out.push(i as u16);
                }
            }
        }
        out
    }
}

/// 内部状态（全部由外层 Mutex 保护）。
struct Inner {
    socket: UdpSocket,
    peer: SocketAddr,
    mode: PeerMode,
    conn_id: u32,

    // ---- 发送侧 ----
    next_seq: u32,
    unacked: BTreeMap<u32, SendMsg>,
    /// 因为窗口满而还没发出去的消息（send 不阻塞，排队等 ACK 腾窗口）
    backlog: VecDeque<SendMsg>,
    /// 在途字节数（unacked 里所有分片的线上字节和）
    inflight_bytes: usize,
    out_queue: VecDeque<Vec<u8>>,
    sent_frags: u64,
    retransmits: u64,

    // ---- 接收侧 ----
    expect_seq: u32,
    recv_msgs: HashMap<u32, RecvMsg>,
    ready: VecDeque<Vec<u8>>,
    recv_frags: u64,
    nack_missing: u64,
    sent_bytes: u64,
    data_bytes: u64,
    rto_retrans: u64,
    nack_retrans: u64,
    acks_sent: u64,
    nacks_sent: u64,
    rttvar_ms: f32,
    /// 最近一次收到 ACK 的时刻（RTO 只在"对端 ACK 停了"时才触发）
    last_ack_recv: Option<Instant>,

    // ---- 时间/保活 ----
    last_send_any: Instant,
    last_recv_any: Instant,
    srtt_ms: f32,
    /// 最近一次发 ACK 的时刻（限流用）
    last_ack_at: Option<Instant>,
    /// 每条消息最近一次发 NACK 的时刻（限流用，按 seq 分别计时）
    nack_times: HashMap<u32, Instant>,
    /// 额外 NACK 的轮转游标
    nack_cursor: usize,
    ping_nonce: u32,
    /// 最近一次发 PING 的时刻（保活节拍，忙时也要发）
    last_ping_at: Instant,
    /// 最近一次 PING 发出的时刻（算 RTT 用）
    ping_sent: Option<Instant>,
    dead: bool,
    queue_dropped: u64,
}

impl Inner {
    fn send_addr(&self) -> SocketAddr {
        self.mode.send_addr(self.peer)
    }

    /// 把一包按当前模式封好（relay 模式加信封）。
    fn wrap(&self, inner: &[u8]) -> Vec<u8> {
        match self.mode {
            PeerMode::Direct => inner.to_vec(),
            PeerMode::Relay { dst_tag, .. } => {
                let mut out = Vec::with_capacity(RELAY_HDR + inner.len());
                out.push(RELAY_OUT);
                out.extend_from_slice(&dst_tag.to_be_bytes());
                out.extend_from_slice(inner);
                out
            }
        }
    }

    /// 入队一个已组好的数据报并尽力刷出去。
    fn emit(&mut self, datagram: Vec<u8>) {
        if self.out_queue.len() >= OUT_QUEUE_MAX {
            self.out_queue.pop_front();
            self.queue_dropped += 1;
        }
        self.out_queue.push_back(datagram);
        self.flush();
    }

    /// 组一个带当前模式信封的控制/ACK 包发出去。
    fn emit_inner(&mut self, inner: Vec<u8>) {
        let datagram = self.wrap(&inner);
        self.emit(datagram);
    }

    fn flush(&mut self) {
        let addr = self.send_addr();
        let mut sent = 0usize;
        while let Some(front) = self.out_queue.front() {
            if sent >= FLUSH_BURST {
                return;
            }
            sent += 1;
            match self.socket.send_to(front, addr) {
                Ok(_) => {
                    self.sent_bytes += front.len() as u64;
                    self.out_queue.pop_front();
                    self.last_send_any = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    // 非致命（例如 Windows 上 ICMP 端口不可达的回灌）：丢掉这一包，交给重传
                    self.out_queue.pop_front();
                    log::debug!("udp send 失败（丢弃该包）: {e}");
                }
            }
        }
    }

    /// 抽干 socket（非阻塞），最多 PUMP_BATCH 个包。
    fn pump(&mut self) {
        let mut buf = [0u8; DATA_HDR + MTU_PAYLOAD + RELAY_HDR + 8];
        for _ in 0..PUMP_BATCH {
            match self.socket.recv_from(&mut buf) {
                Ok((n, _from)) => {
                    let packet = buf[..n].to_vec();
                    self.handle_packet(&packet);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    log::debug!("udp recv 失败: {e}");
                    return;
                }
            }
        }
    }

    fn handle_packet(&mut self, raw: &[u8]) {
        // relay 入站信封：剥壳
        let inner: &[u8] = if raw.len() >= RELAY_HDR && raw[0] == RELAY_IN {
            &raw[RELAY_HDR..]
        } else {
            raw
        };
        if inner.len() < CTRL_LEN {
            return;
        }
        let now = Instant::now();
        self.last_recv_any = now;
        match inner[0] {
            TYPE_DATA => self.on_data(inner, now),
            TYPE_ACK => self.on_ack(inner, now),
            TYPE_PING => {
                let nonce = read_u32(&inner[5..9]);
                self.send_control(TYPE_PONG, nonce);
            }
            TYPE_PONG => {
                let nonce = read_u32(&inner[5..9]);
                if nonce == self.ping_nonce {
                    // 用 PING 发出的时刻算 RTT（不能用 started，那是整条连接的年龄）
                    if let Some(sent) = self.ping_sent.take() {
                        self.update_rtt(now.saturating_duration_since(sent));
                    }
                }
            }
            other => log::debug!("udp: 未知包类型 {other:#04x}"),
        }
    }

    fn send_control(&mut self, kind: u8, nonce: u32) {
        let mut p = Vec::with_capacity(CTRL_LEN);
        p.push(kind);
        p.extend_from_slice(&self.conn_id.to_be_bytes());
        p.extend_from_slice(&nonce.to_be_bytes());
        self.emit_inner(p);
    }

    fn update_rtt(&mut self, rtt: Duration) {
        let ms = rtt.as_secs_f32() * 1000.0;
        if self.srtt_ms <= 0.0 {
            self.srtt_ms = ms;
            return;
        }
        // RFC6298：RTTVAR 用 |SRTT - 样本| 的指数平均
        let diff = (self.srtt_ms - ms).abs();
        self.rttvar_ms = if self.rttvar_ms <= 0.0 {
            diff
        } else {
            self.rttvar_ms * 0.75 + diff * 0.25
        };
        self.srtt_ms = self.srtt_ms * 0.875 + ms * 0.125;
    }

    fn rto_for(&self) -> Duration {
        // RTO 必须显著大于真实 RTT：跨公网 RTT≈90ms 时，100ms 的 RTO 会在 ACK 还在路上
        // 就先重传一遍（这是"放大 4 倍"的主因之一）。
        // 取 2*SRTT + max(20ms, 4*RTTVAR)，下限 MIN_RTO、上限 MAX_RTO。
        if self.srtt_ms <= 0.0 {
            return MIN_RTO;
        }
        let jitter = (self.rttvar_ms * 4.0).max(20.0);
        let ms = self.srtt_ms * 2.0 + jitter;
        let base = Duration::from_millis(if ms.is_finite() && ms > 0.0 {
            ms as u64
        } else {
            MIN_RTO.as_millis() as u64
        });
        base.max(MIN_RTO).min(MAX_RTO)
    }

    fn on_data(&mut self, p: &[u8], now: Instant) {
        if p.len() < DATA_HDR {
            return;
        }
        let seq = read_u32(&p[5..9]);
        let frag = read_u16(&p[9..11]) as usize;
        let fcnt = read_u16(&p[11..13]) as usize;
        let len = read_u16(&p[13..15]) as usize;
        if fcnt == 0 || fcnt > MAX_FRAGS || frag >= fcnt || p.len() < DATA_HDR + len {
            return;
        }
        // 已经交付过的消息：只回 ACK，不再入缓冲
        let mut is_new = false;
        if !seq_lt(seq, self.expect_seq) {
            let entry = self
                .recv_msgs
                .entry(seq)
                .or_insert_with(|| RecvMsg::new(fcnt as u32, now));
            entry.max_frag = entry.max_frag.max(frag + 1);
            if entry.total as usize == fcnt && entry.frags[frag].is_none() {
                entry.frags[frag] = Some(p[DATA_HDR..DATA_HDR + len].to_vec());
                entry.got += 1;
                entry.last_seen = now;
                is_new = true;
            }
        }
        if is_new {
            // 只数"唯一的"分片：重复/迟到分片不进损失率分母
            self.recv_frags += 1;
        }
        let before = self.expect_seq;
        self.deliver_ready();
        // 有消息完成交付时立刻回 ACK（跳过限流），让发送端马上释放 inflight
        let advanced = self.expect_seq != before;
        self.send_ack(advanced);
    }

    /// 按 seq 顺序把完整消息搬进 ready 队列。
    fn deliver_ready(&mut self) {
        loop {
            let Some(msg) = self.recv_msgs.get(&self.expect_seq) else {
                return;
            };
            if !msg.complete() {
                return;
            }
            let Some(mut msg) = self.recv_msgs.remove(&self.expect_seq) else {
                return;
            };
            let total: usize = msg.frags.iter().map(|f| f.as_ref().map_or(0, Vec::len)).sum();
            let mut assembled = Vec::with_capacity(total);
            for f in msg.frags.iter_mut() {
                if let Some(part) = f.take() {
                    assembled.extend_from_slice(&part);
                }
            }
            self.ready.push_back(assembled);
            self.nack_times.remove(&self.expect_seq);
            self.expect_seq = self.expect_seq.wrapping_add(1);
        }
    }

    /// 这条消息现在允许 NACK 吗？
    ///
    /// 跨公网 RTT≈90ms：如果收到第一片就立刻报"缺 5 片"，那 5 片其实还在路上，
    /// 重传一遍就是纯放大。所以至少要等 1.5 个 RTT 再报缺口。
    /// 同一条消息两次 NACK 的最小间隔：至少一个 RTT，避免"重传还没到就再催一遍"。
    fn nack_interval(&self) -> Duration {
        if self.srtt_ms > 0.0 {
            Duration::from_millis(self.srtt_ms as u64).max(NACK_INTERVAL_MIN)
        } else {
            NACK_INTERVAL_MIN
        }
    }

    fn nack_ready(&self, msg: &RecvMsg, now: Instant) -> bool {
        let delay = if self.srtt_ms > 0.0 {
            Duration::from_millis((f64::from(self.srtt_ms) * 1.5) as u64)
                .max(Duration::from_millis(8))
        } else {
            Duration::from_millis(100)
        };
        now.saturating_duration_since(msg.first_seen) >= delay
    }

    /// 回一个 ACK：ack/ack_bits 是消息级累积确认，NACK 清单直说"缺哪几片"。
    ///
    /// force：有消息刚好完成交付时必须立刻发（否则发送端要等一个 RTO 才释放 inflight）。
    fn send_ack(&mut self, force: bool) {
        let mut bits: u32 = 0;
        for i in 0..32u32 {
            let want = self.expect_seq.wrapping_add(1 + i);
            if let Some(m) = self.recv_msgs.get(&want) {
                if m.complete() {
                    bits |= 1u32 << (31 - i);
                }
            }
        }
        // NACK 的是阻塞交付的那条（队头），它是唯一挡住有序交付的消息；
        // 但必须限流：否则"每收到一片就 NACK 一次"会和重传形成放大环。
        let now = Instant::now();
        let nack_seq = self.expect_seq;
        let mut missing = self
            .recv_msgs
            .get(&nack_seq)
            .filter(|m| self.nack_ready(m, now))
            .map(|m| m.missing(now))
            .unwrap_or_default();
        if !missing.is_empty() {
            let interval = self.nack_interval();
            let due = match self.nack_times.get(&nack_seq) {
                Some(at) => now.saturating_duration_since(*at) >= interval,
                None => true,
            };
            if due {
                self.nack_times.insert(nack_seq, now);
                let newly = self.mark_notified(nack_seq, &missing);
                self.nack_missing += newly;
            } else {
                missing.clear();
            }
        }

        // ACK 本身也限流：一个 1MB 消息有 800+ 片，每片回一个 ACK 纯属浪费
        let ack_interval = if self.srtt_ms > 0.0 {
            Duration::from_millis((self.srtt_ms / 8.0) as u64).max(ACK_INTERVAL_MIN)
        } else {
            ACK_INTERVAL_MIN
        };
        let ack_due = match self.last_ack_at {
            Some(at) => now.saturating_duration_since(at) >= ack_interval,
            None => true,
        };
        if !force && !ack_due && missing.is_empty() {
            return;
        }
        self.last_ack_at = Some(now);

        self.acks_sent += 1;
        self.nacks_sent += missing.len() as u64;
        let p = self.ack_packet(bits, nack_seq, &missing);
        self.emit_inner(p);

        // 顺便把其它乱序消息的缺口也告诉对端（轮转几条），否则它们只能等 RTO 全量重传
        let mut keys: Vec<u32> = self
            .recv_msgs
            .iter()
            .filter(|(seq, m)| **seq != nack_seq && !m.complete())
            .map(|(seq, _)| *seq)
            .collect();
        if keys.is_empty() {
            return;
        }
        keys.sort_unstable();
        let take = keys.len().min(EXTRA_NACK);
        let start = self.nack_cursor % keys.len();
        let mut extras: Vec<(u32, Vec<u16>)> = Vec::new();
        for off in 0..take {
            let seq = keys[(start + off) % keys.len()];
            let interval = self.nack_interval();
            let due = match self.nack_times.get(&seq) {
                Some(at) => now.saturating_duration_since(*at) >= interval,
                None => true,
            };
            if !due {
                continue;
            }
            if let Some(m) = self.recv_msgs.get(&seq) {
                if !self.nack_ready(m, now) {
                    continue;
                }
                let miss = m.missing(now);
                if !miss.is_empty() {
                    self.nack_times.insert(seq, now);
                    let newly = self.mark_notified(seq, &miss);
                    self.nack_missing += newly;
                    self.acks_sent += 1;
                    self.nacks_sent += miss.len() as u64;
                    extras.push((seq, miss));
                }
            }
        }
        self.nack_cursor = self.nack_cursor.wrapping_add(take.max(1));
        for (seq, miss) in extras {
            let p = self.ack_packet(bits, seq, &miss);
            self.emit_inner(p);
        }
    }

    /// 标记这些缺口已经报过，返回**本次新增**的缺口数（丢包统计只数唯一缺口）。
    fn mark_notified(&mut self, seq: u32, missing: &[u16]) -> u64 {
        let Some(m) = self.recv_msgs.get_mut(&seq) else {
            return 0;
        };
        let mut newly = 0u64;
        for &f in missing {
            if let Some(slot) = m.notified.get_mut(f as usize) {
                if !*slot {
                    *slot = true;
                    newly += 1;
                }
            }
        }
        newly
    }

    /// 组一个 ACK 包（ack/ack_bits 始终是当前累积值，nack_seq 指本条要报告的消息）。
    fn ack_packet(&self, bits: u32, nack_seq: u32, missing: &[u16]) -> Vec<u8> {
        let mut p = Vec::with_capacity(ACK_HDR + missing.len() * 2);
        p.push(TYPE_ACK);
        p.extend_from_slice(&self.conn_id.to_be_bytes());
        p.extend_from_slice(&self.expect_seq.to_be_bytes());
        p.extend_from_slice(&bits.to_be_bytes());
        let wnd = self.recv_msgs.len().min(u16::MAX as usize) as u16;
        p.extend_from_slice(&wnd.to_be_bytes());
        p.extend_from_slice(&nack_seq.to_be_bytes());
        p.push(missing.len() as u8);
        for f in missing {
            p.extend_from_slice(&f.to_be_bytes());
        }
        p
    }

    fn on_ack(&mut self, p: &[u8], now: Instant) {
        if p.len() < ACK_HDR {
            return;
        }
        let ack = read_u32(&p[5..9]);
        let bits = read_u32(&p[9..13]);
        self.last_ack_recv = Some(now);
        let nack_seq = read_u32(&p[ACK_NACK_SEQ_OFF..ACK_NACK_SEQ_OFF + 4]);
        let nack_cnt = p[ACK_NACK_CNT_OFF] as usize;

        // 1) 累积确认 + 窗口位图 → 收全的消息直接释放
        let mut done: Vec<u32> = Vec::new();
        for &seq in self.unacked.keys() {
            if seq_lt(seq, ack) {
                done.push(seq);
            } else if seq != ack && seq_lt(seq, ack.wrapping_add(32)) {
                let i = seq.wrapping_sub(ack).wrapping_sub(1) as usize;
                if i < 32 && (bits >> (31 - i)) & 1 == 1 {
                    done.push(seq);
                }
            }
        }
        for seq in done {
            if let Some(msg) = self.unacked.remove(&seq) {
                self.inflight_bytes = self.inflight_bytes.saturating_sub(msg.wire_bytes);
                // 只有"单片 + 没排队"的消息，其 ACK 时刻才≈真实 RTT；
                // 多片消息要等整条收齐才回 ACK，拿来当 RTT 样本会把 RTO 估偏。
                if msg.retries == 0 && msg.timely && msg.frags.len() == 1 {
                    let rtt = now.saturating_duration_since(msg.first_sent);
                    self.update_rtt(rtt);
                }
            }
        }

        // 2) 分片全到齐的消息直接摘掉：它已在对端缓冲区里，只等顺序交付，
        //    留在 unacked 里会导致"不重传也不再有 ACK"的假死。
        self.prune_fully_acked();

        // 3) NACK 清单 → 只重传这几片
        if nack_cnt > 0 && p.len() >= ACK_HDR + nack_cnt * 2 {
            let mut frags = Vec::with_capacity(nack_cnt);
            for i in 0..nack_cnt {
                let off = ACK_HDR + i * 2;
                frags.push(read_u16(&p[off..off + 2]) as usize);
            }
            let mut resend: Vec<Vec<u8>> = Vec::new();
            if let Some(msg) = self.unacked.get_mut(&nack_seq) {
                for f in frags {
                    if f < msg.frags.len() && !msg.acked[f] {
                        resend.push(msg.frags[f].clone());
                    }
                }
                if !resend.is_empty() {
                    // 补洞算一次"发送"，RTO 从这个点重新计时：
                    // 既不重复补，也不会像"完全压制 RTO"那样在弱网里卡死。
                    msg.last_sent = now;
                }
            }
            if !resend.is_empty() {
                self.retransmits += resend.len() as u64;
                self.nack_retrans += resend.len() as u64;
                for d in resend {
                    self.emit(d);
                }
            }
        }
    }

    /// 把 backlog 里排队的消息按窗口（条数 + 字节）放进在途集合并推到发送队列。
    ///
    /// 为什么要窗口：ACK 的 ack_bits 只有 32 位，在途消息一多，窗口外的消息永远
    /// 得不到确认 → 发送端只能靠 RTO 反复重传（跨公网 RTT 90ms 时就是 4 倍放大的主因）。
    fn pump_backlog(&mut self) {
        loop {
            let Some(front) = self.backlog.front() else {
                return;
            };
            let wire = front.wire_bytes;
            // 至少放一条进去，避免"单条大消息超过窗口"把自己卡死
            if !self.unacked.is_empty()
                && (self.unacked.len() >= MAX_INFLIGHT_MSGS
                    || self.inflight_bytes + wire > MAX_INFLIGHT_BYTES)
            {
                return;
            }
            let Some(mut msg) = self.backlog.pop_front() else {
                return;
            };
            // RTO 时钟从"真正出网"这一刻开始算：在 backlog 里排队的时间不是传输时间，
            // 否则窗口一满就会把还在排队的消息当成"超时未确认"重传一遍。
            let sent_at = Instant::now();
            msg.first_sent = sent_at;
            msg.last_sent = sent_at;
            msg.rto = self.rto_for();
            msg.timely = self.out_queue.is_empty();
            self.inflight_bytes += msg.wire_bytes;
            self.sent_frags += msg.frags.len() as u64;
            for d in &msg.frags {
                if self.out_queue.len() >= OUT_QUEUE_MAX {
                    self.out_queue.pop_front();
                    self.queue_dropped += 1;
                }
                self.out_queue.push_back(d.clone());
            }
            self.unacked.insert(msg.seq, msg);
        }
    }

    /// 摘掉"每片都已被确认"的消息（对端已经收全，只等有序交付）。
    fn prune_fully_acked(&mut self) {
        let done: Vec<u32> = self
            .unacked
            .iter()
            .filter(|(_, m)| m.acked.iter().all(|a| *a))
            .map(|(seq, _)| *seq)
            .collect();
        for seq in done {
            if let Some(msg) = self.unacked.remove(&seq) {
                self.inflight_bytes = self.inflight_bytes.saturating_sub(msg.wire_bytes);
            }
        }
    }

    /// 驱动重传与保活（不发数据也不会掉线）。
    fn drive(&mut self) {
        let now = Instant::now();
        // ACK 腾出窗口后，把 backlog 里的消息放进来
        self.pump_backlog();

        // 保活：每 1s 发一次 PING —— 不管有没有在发数据。
        // 只有空闲才发的话，忙的时候永远没有 RTT 样本，RTO 会一直停在最小值，
        // 跨公网 RTT 90ms 时就会"ACK 还在路上就开始重传"（放大主因）。
        if now.saturating_duration_since(self.last_ping_at) >= PING_IDLE {
            self.last_ping_at = now;
            self.ping_nonce = self.ping_nonce.wrapping_add(1);
            let nonce = self.ping_nonce;
            self.ping_sent = Some(now);
            self.send_control(TYPE_PING, nonce);
        }

        // 超时重传：只处理最老的几条（避免重传风暴），且只补缺失分片。
        // 队列里还有没发出去的数据时先不动：它们只是"还没发"，不是"丢了"。
        if !self.out_queue.is_empty() {
            if now.saturating_duration_since(self.last_recv_any) > dead_after() {
                self.dead = true;
            }
            self.flush();
            return;
        }
        let mut budget = DRIVE_BATCH;
        let mut due: Vec<u32> = Vec::new();
        for (&seq, msg) in self.unacked.iter() {
            if budget == 0 {
                break;
            }
            if now.saturating_duration_since(msg.last_sent) >= msg.rto {
                due.push(seq);
                budget -= 1;
            }
        }
        let mut resend: Vec<Vec<u8>> = Vec::new();
        for seq in due {
            let Some(msg) = self.unacked.get_mut(&seq) else {
                continue;
            };

            // 每轮最多补 RTO_BURST 片，并且从上次停下的位置继续（轮转补洞）
            let mut pkts: Vec<Vec<u8>> = Vec::new();
            let total = msg.frags.len();
            let start = msg.resend_cursor % total.max(1);
            for off in 0..total {
                let i = (start + off) % total;
                if !msg.acked[i] {
                    pkts.push(msg.frags[i].clone());
                    if pkts.len() >= RTO_BURST {
                        break;
                    }
                }
            }
            if pkts.is_empty() {
                continue;
            }
            msg.resend_cursor = (start + pkts.len()) % total.max(1);
            msg.last_sent = now;
            msg.rto = (msg.rto * 2).min(MAX_RTO);
            msg.retries += 1;
            resend.extend(pkts);
        }
        if !resend.is_empty() {
            self.retransmits += resend.len() as u64;
            self.rto_retrans += resend.len() as u64;
            for d in resend {
                self.emit(d);
            }
        }

        // 死链判定
        if now.saturating_duration_since(self.last_recv_any) > dead_after() {
            self.dead = true;
        }
        self.prune_fully_acked();
        self.flush();
    }
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn read_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// 把 socket 收发缓冲调大（Linux/Windows 都会把请求值砍到 sysctl/winsock 上限）。
///
/// 这不是"锦上添花"：1 MB 级别的突发在 64 KB 的接收缓冲上会直接被内核丢掉，
/// 属于真丢包 —— 调大 RCVBUF 比改统计口径更能改善弱网/大帧体验。
#[cfg(windows)]
fn tune_socket_buffers(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        setsockopt, SOCKET, SO_RCVBUF, SO_SNDBUF, SOL_SOCKET,
    };
    let raw = socket.as_raw_socket() as usize;
    let bytes = SOCKET_BUFFER.to_ne_bytes();
    for (opt, name) in [(SO_RCVBUF, "SO_RCVBUF"), (SO_SNDBUF, "SO_SNDBUF")] {
        let rc = unsafe { setsockopt(SOCKET(raw), SOL_SOCKET, opt, Some(&bytes)) };
        if rc != 0 {
            log::debug!("lrc-udp: 设置 {name} 失败（继续用系统默认值）");
        }
    }
}

/// Linux/其他平台：std 没有暴露 setsockopt，保持系统默认缓冲（Linux 默认 rcvbuf 约 208 KB，
/// relay 只转发控制流与中继包，够用；真要调就改 `net.core.rmem_max` + `net.core.rmem_default`）。
#[cfg(not(windows))]
fn tune_socket_buffers(_socket: &UdpSocket) {}

/// 序号环形比较：a 是否在 b 之前。
fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// 一条可靠 UDP 连接。
///
/// 线程安全：所有方法都取 &self，内部用 Mutex 保护状态；
/// 推荐上层写 Arc<UdpConn>，收线程调 recv_timeout、发线程调 send。
///
/// 注意：recv_timeout 内部**不会长时间持有锁** —— 锁内只做抽干 socket + 取一条消息，
/// 间隙 sleep 1ms 在锁外，实测单次持锁远小于 1ms。
pub struct UdpConn {
    inner: Mutex<Inner>,
}

impl UdpConn {
    /// 用一个**已经完成打洞/已确定路径**的 socket 建连接。
    ///
    /// peer：Direct 时是对端地址；Relay 时忽略（用模式里的 relay 地址）。
    /// 注意：内部会把 socket 设为非阻塞。
    pub fn new(socket: UdpSocket, peer: SocketAddr, mode: PeerMode) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        // std 的 UdpSocket 没有 set_recv/send_buffer_size（那是 TcpStream 的），
        // 走 setsockopt 把缓冲调大：大帧突发时内核接收缓冲太小会直接丢包。
        tune_socket_buffers(&socket);
        let now = Instant::now();
        let conn_id = now.elapsed().subsec_nanos()
            ^ (socket.local_addr().map_or(0, |a| u32::from(a.port())) << 16);
        let inner = Inner {
            socket,
            peer,
            mode,
            conn_id,
            next_seq: 0,
            unacked: BTreeMap::new(),
            backlog: VecDeque::new(),
            inflight_bytes: 0,
            out_queue: VecDeque::new(),
            sent_frags: 0,
            retransmits: 0,
            expect_seq: 0,
            recv_msgs: HashMap::new(),
            ready: VecDeque::new(),
            recv_frags: 0,
            nack_missing: 0,
            sent_bytes: 0,
            data_bytes: 0,
            rto_retrans: 0,
            nack_retrans: 0,
            acks_sent: 0,
            nacks_sent: 0,
            rttvar_ms: 0.0,
            last_ack_recv: None,
            last_send_any: now,
            last_recv_any: now,
            srtt_ms: 0.0,
            last_ack_at: None,
            nack_times: HashMap::new(),
            nack_cursor: 0,
            ping_nonce: 0,
            last_ping_at: now,
            ping_sent: None,
            dead: false,
            queue_dropped: 0,
        };
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 毒化只可能来自别的线程 panic；恢复内部状态继续用，比整条链路挂掉好
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 发送一条完整消息（内部自动分片入队；返回时**不保证**已经出网）。
    ///
    /// 取 &self（内部 Mutex）→ 上层可以 Arc<UdpConn> 让收/发两个线程共享同一个连接。
    pub fn send(&self, payload: &[u8]) -> io::Result<()> {
        if payload.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "udp: 不允许发送空消息",
            ));
        }
        if payload.len() > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("udp: 消息 {} 字节超过上限 {MAX_MESSAGE}", payload.len()),
            ));
        }
        let fcnt = payload.len().div_ceil(MTU_PAYLOAD);
        if fcnt > MAX_FRAGS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("udp: 需要 {fcnt} 个分片，超过上限 {MAX_FRAGS}"),
            ));
        }

        let mut g = self.lock();
        let now = Instant::now();
        g.data_bytes += payload.len() as u64;
        let seq = g.next_seq;
        g.next_seq = g.next_seq.wrapping_add(1);

        let mut frags = Vec::with_capacity(fcnt);
        for (i, chunk) in payload.chunks(MTU_PAYLOAD).enumerate() {
            let mut p = Vec::with_capacity(DATA_HDR + chunk.len());
            p.push(TYPE_DATA);
            p.extend_from_slice(&g.conn_id.to_be_bytes());
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&(i as u16).to_be_bytes());
            p.extend_from_slice(&(fcnt as u16).to_be_bytes());
            p.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            p.extend_from_slice(chunk);
            // relay 模式下每个包都带信封
            frags.push(g.wrap(&p));
        }

        let n = frags.len();
        let rto = g.rto_for();
        let timely = g.out_queue.is_empty() && g.backlog.is_empty() && g.unacked.is_empty();
        let wire_bytes = frags.iter().map(Vec::len).sum();
        g.backlog.push_back(SendMsg {
            seq,
            frags,
            acked: vec![false; n],
            first_sent: now,
            last_sent: now,
            rto,
            retries: 0,
            resend_cursor: 0,
            timely,
            wire_bytes,
        });
        // 窗口没满就立刻放出去；满了就排队等 ACK 腾窗口（send 不阻塞调用方）
        g.pump_backlog();
        if !g.backlog.is_empty() {
            // 窗口满：顺手抽一下 socket 把 ACK 收进来，避免"只发不收"的调用方把窗口堵死
            g.pump();
            g.drive();
        }
        g.flush();
        Ok(())
    }

    /// 阻塞最多 timeout 等一条完整消息。
    ///
    /// - Ok(Some(n)) 拿到一条（out 已被清空并填好）
    /// - Ok(None) 超时，没有完整消息
    /// - Err(TimedOut) 连续 DEAD_AFTER（默认 6s，`LRC_UDP_DEAD_MS` 可覆盖）没收到对端任何包（当成断线）
    /// - Err 其它致命 IO 错误
    pub fn recv_timeout(&self, out: &mut Vec<u8>, timeout: Duration) -> io::Result<Option<usize>> {
        let deadline = Instant::now() + timeout;
        loop {
            let (msg, dead) = {
                let mut g = self.lock();
                g.pump();
                g.drive();
                (g.ready.pop_front(), g.dead)
            };
            if let Some(m) = msg {
                let n = m.len();
                out.clear();
                out.extend_from_slice(&m);
                return Ok(Some(n));
            }
            if dead {
                let secs = dead_after().as_millis() as f64 / 1000.0;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("udp: 对端 {secs:.1} 秒无响应（NAT 映射可能已过期）"),
                ));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL);
        }
    }

    /// 驱动内部状态机（重传/保活/收包）。上层每 10–50 ms 调一次，
    /// 或者用 recv_timeout 内部自动驱动（实现里二选一，但要保证**不发数据也不会掉线**）。
    pub fn tick(&self) -> io::Result<()> {
        let mut g = self.lock();
        g.pump();
        g.drive();
        Ok(())
    }

    pub fn stats(&self) -> UdpStats {
        let g = self.lock();
        let expected = g.recv_frags + g.nack_missing;
        let loss = if expected == 0 {
            0
        } else {
            ((g.nack_missing.saturating_mul(1000)) / expected).min(1000) as u16
        };
        UdpStats {
            rtt_ms: g.srtt_ms,
            loss_permille: loss,
            sent_frags: g.sent_frags,
            recv_frags: g.recv_frags,
            retransmits: g.retransmits,
            inflight: g.unacked.len() as u32,
            pending: g.recv_msgs.len() as u32,
            sent_bytes: g.sent_bytes,
            data_bytes: g.data_bytes,
            rto_retrans: g.rto_retrans,
            nack_retrans: g.nack_retrans,
            acks_sent: g.acks_sent,
            nacks_sent: g.nacks_sent,
            rtt_var_ms: g.rttvar_ms,
        }
    }

    /// 本端因发包队列溢出丢了多少包（被 RTO 补回，仅诊断用）。
    pub fn queue_dropped(&self) -> u64 {
        self.lock().queue_dropped
    }

    pub fn mode(&self) -> PeerMode {
        self.lock().mode
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.lock().socket.local_addr()
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        let g = self.lock();
        if g.peer.ip().is_unspecified() && g.peer.port() == 0 {
            None
        } else {
            Some(g.peer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn bind() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1:0")
    }

    /// 一对直连的 UdpConn（回环）。
    fn pair() -> (UdpConn, UdpConn) {
        let a = bind();
        let b = bind();
        let a_addr = a.local_addr().expect("a addr");
        let b_addr = b.local_addr().expect("b addr");
        let ca = UdpConn::new(a, b_addr, PeerMode::Direct).expect("conn a");
        let cb = UdpConn::new(b, a_addr, PeerMode::Direct).expect("conn b");
        (ca, cb)
    }

    /// 造一条可校验的消息：前 4 字节是序号，后面是可复算的模式。
    fn make_msg(idx: u32, len: usize) -> Vec<u8> {
        let len = len.max(8);
        let mut v = vec![0u8; len];
        v[..4].copy_from_slice(&idx.to_be_bytes());
        for (i, b) in v.iter_mut().enumerate().skip(4) {
            *b = ((idx as usize).wrapping_mul(31).wrapping_add(i * 7) & 0xFF) as u8;
        }
        v
    }

    fn check_msg(buf: &[u8], idx: u32) {
        assert!(buf.len() >= 8, "消息太短: {}", buf.len());
        let got = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(got, idx, "交付顺序错了（第 {idx} 条收到的是 {got}）");
        for (i, b) in buf.iter().enumerate().skip(4) {
            let want = ((idx as usize).wrapping_mul(31).wrapping_add(i * 7) & 0xFF) as u8;
            assert_eq!(*b, want, "内容错了 idx={idx} off={i}");
        }
    }

    /// 收满 expect 条（或超时），保持交付顺序。
    fn drain(conn: &UdpConn, expect: usize, timeout: Duration) -> Vec<Vec<u8>> {
        let mut got = Vec::with_capacity(expect);
        let deadline = Instant::now() + timeout;
        let mut buf: Vec<u8> = Vec::new();
        while got.len() < expect && Instant::now() < deadline {
            match conn.recv_timeout(&mut buf, Duration::from_millis(50)) {
                Ok(Some(_)) => got.push(buf.clone()),
                Ok(None) => {}
                Err(e) => panic!("recv 失败: {e}"),
            }
        }
        got
    }

    /// 一边猛发、一边驱动重传（不驱动就没人补包了）。
    fn drive_until_drained(conn: &UdpConn, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if conn.stats().inflight == 0 {
                return true;
            }
            conn.tick().expect("tick");
            thread::sleep(Duration::from_millis(1));
        }
        conn.stats().inflight == 0
    }

    // ---------------------------------------------------------------- 1) 回环无丢包

    #[test]
    fn loopback_1000_messages_no_loss() {
        let (a, b) = pair();
        let a = Arc::new(a);
        let b = Arc::new(b);
        const HALF: u32 = 500;
        const BIG: usize = 1024 * 1024;

        let t0 = Instant::now();
        let mut bytes = 0usize;

        // ---- A → B：500 条随机长度 + 1 条 1MB ----
        let a_tx = Arc::clone(&a);
        let sender = thread::spawn(move || {
            let mut sent_bytes = 0usize;
            for i in 0..HALF {
                let len = 8 + (i as usize * 37) % 3000;
                let msg = make_msg(i, len);
                sent_bytes += msg.len();
                a_tx.send(&msg).expect("send");
            }
            let mut big = vec![0u8; BIG];
            big[..4].copy_from_slice(&HALF.to_be_bytes());
            for (i, byte) in big.iter_mut().enumerate().skip(4) {
                *byte = ((i.wrapping_mul(13)) & 0xFF) as u8;
            }
            sent_bytes += big.len();
            a_tx.send(&big).expect("send big");
            let ok = drive_until_drained(&a_tx, Duration::from_secs(30));
            (sent_bytes, ok)
        });
        let got = drain(&b, HALF as usize + 1, Duration::from_secs(60));
        let deliver_elapsed = t0.elapsed();
        // 收够之后 B 还要继续跑：ACK 得刷出去，A 的 inflight 才会归零（真实收线程一直在跑）
        let settle = Instant::now() + Duration::from_secs(30);
        while a.stats().inflight > 0 && Instant::now() < settle {
            b.tick().expect("tick b");
            thread::sleep(Duration::from_millis(1));
        }
        let (sent_a, drained_a) = sender.join().expect("join sender a");
        assert!(drained_a, "A 侧还有未确认消息（inflight != 0）");
        assert_eq!(got.len(), HALF as usize + 1, "A→B 少收了消息");
        for (i, msg) in got.iter().take(HALF as usize).enumerate() {
            check_msg(msg, i as u32);
        }
        let big_got = &got[HALF as usize];
        assert_eq!(big_got.len(), BIG, "1MB 消息长度不对");
        for (i, byte) in big_got.iter().enumerate().skip(4) {
            assert_eq!(*byte, ((i.wrapping_mul(13)) & 0xFF) as u8, "1MB 消息内容错了 off={i}");
        }
        bytes += sent_a;

        // ---- B → A：500 条随机长度 ----
        let b_tx = Arc::clone(&b);
        let sender_b = thread::spawn(move || {
            let mut sent_bytes = 0usize;
            for i in 0..HALF {
                let len = 8 + (i as usize * 53) % 5000;
                let msg = make_msg(i, len);
                sent_bytes += msg.len();
                b_tx.send(&msg).expect("send");
            }
            let ok = drive_until_drained(&b_tx, Duration::from_secs(30));
            (sent_bytes, ok)
        });
        let got_back = drain(&a, HALF as usize, Duration::from_secs(60));
        let settle = Instant::now() + Duration::from_secs(30);
        while b.stats().inflight > 0 && Instant::now() < settle {
            a.tick().expect("tick a");
            thread::sleep(Duration::from_millis(1));
        }
        let (sent_b, drained_b) = sender_b.join().expect("join sender b");
        assert!(drained_b, "B 侧还有未确认消息");
        assert_eq!(got_back.len(), HALF as usize, "B→A 少收了消息");
        for (i, msg) in got_back.iter().enumerate() {
            check_msg(msg, i as u32);
        }
        bytes += sent_b;

        // 吞吐按"发完→收齐"这段算（settle 只是等 ACK 刷完，不算传输耗时）
        let elapsed = deliver_elapsed;
        let mbps = bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
        let sa = a.stats();
        let sb = b.stats();
        println!(
            "[1 回环无丢包] 1000 条 + 1MB，{} 字节 / {:.2}s = {:.1} MB/s；A sent_frags={} recv_frags={} rtt={:.2}ms inflight={}；B sent_frags={} recv_frags={}",
            bytes,
            elapsed.as_secs_f64(),
            mbps,
            sa.sent_frags,
            sa.recv_frags,
            sa.rtt_ms,
            sa.inflight,
            sb.sent_frags,
            sb.recv_frags
        );
        assert!(mbps > 1.0, "吞吐太低: {mbps:.2} MB/s");
        // 回环上 1MB 突发仍可能撑爆 socket 缓冲（UDP 没有背压），允许少量重传
        let retrans_ratio = sa.retransmits as f64 / sa.sent_frags.max(1) as f64;
        println!("[1 回环无丢包] 重传 {} / {} = {:.2}%", sa.retransmits, sa.sent_frags, retrans_ratio * 100.0);
        assert!(retrans_ratio < 0.05, "回环重传率不该超过 5%: {:.2}%", retrans_ratio * 100.0);
    }

    // ---------------------------------------------------------------- 2) 注入 10% 丢包

    /// 中间放一个丢包代理：每 10 个包丢 1 个（两个方向都丢）。
    fn spin_lossy_proxy(
        proxy: UdpSocket,
        a_addr: SocketAddr,
        b_addr: SocketAddr,
    ) -> (Arc<AtomicU64>, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let dropped = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let d = Arc::clone(&dropped);
        let s = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            proxy
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("set_read_timeout");
            let mut buf = [0u8; 2048];
            let mut n: u64 = 0;
            while !s.load(Ordering::Relaxed) {
                let Ok((len, from)) = proxy.recv_from(&mut buf) else {
                    continue;
                };
                n += 1;
                if n % 10 == 0 {
                    d.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let dst = if from == a_addr { b_addr } else { a_addr };
                let _ = proxy.send_to(&buf[..len], dst);
            }
        });
        (dropped, stop, handle)
    }

    #[test]
    fn lossy_10_percent_still_delivers_all_in_order() {
        const COUNT: u32 = 1000;
        let pa = bind();
        let pb = bind();
        let px = bind();
        let a_addr = pa.local_addr().expect("a addr");
        let b_addr = pb.local_addr().expect("b addr");
        let px_addr = px.local_addr().expect("proxy addr");
        let (dropped, stop, proxy) = spin_lossy_proxy(px, a_addr, b_addr);

        let a = Arc::new(UdpConn::new(pa, px_addr, PeerMode::Direct).expect("conn a"));
        let b = Arc::new(UdpConn::new(pb, px_addr, PeerMode::Direct).expect("conn b"));

        let t0 = Instant::now();
        let a_tx = Arc::clone(&a);
        let sender = thread::spawn(move || {
            for i in 0..COUNT {
                // 8..8084 字节 → 1..7 个分片，保证会丢到"碎片"而不是整条消息
                let len = 8 + (i as usize * 53) % 8000;
                a_tx.send(&make_msg(i, len)).expect("send");
            }
            drive_until_drained(&a_tx, Duration::from_secs(120))
        });

        let got = drain(&b, COUNT as usize, Duration::from_secs(120));
        let drained = sender.join().expect("join sender");
        stop.store(true, Ordering::Relaxed);
        proxy.join().expect("join proxy");

        assert_eq!(got.len(), COUNT as usize, "丢包场景下少收了消息");
        for (i, msg) in got.iter().enumerate() {
            check_msg(msg, i as u32);
        }
        let sa = a.stats();
        let lost = dropped.load(Ordering::Relaxed);
        let elapsed = t0.elapsed();
        println!(
            "[2 注入 10% 丢包] {COUNT} 条全部有序到达；代理丢包 {lost} 个；A 重传 {} 片（sent_frags={}，重传率 {:.1}%）；用时 {:.2}s；loss_permille={}",
            sa.retransmits,
            sa.sent_frags,
            if sa.sent_frags == 0 { 0.0 } else { sa.retransmits as f64 * 100.0 / sa.sent_frags as f64 },
            elapsed.as_secs_f64(),
            sa.loss_permille
        );
        assert!(drained, "A 侧还有未确认消息");
        assert!(lost > 0, "代理没丢到包，测试没意义");
        assert!(sa.retransmits > 0, "应该有重传");
    }

    // ---------------------------------------------------------------- 3) 两线程共享

    #[test]
    fn shared_arc_send_and_recv_threads() {
        let (a, b) = pair();
        let a = Arc::new(a);
        let b = Arc::new(b);
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));

        let tx = {
            let a = Arc::clone(&a);
            let stop = Arc::clone(&stop);
            let sent = Arc::clone(&sent);
            thread::spawn(move || {
                let mut i: u32 = 0;
                while !stop.load(Ordering::Relaxed) {
                    if a.send(&make_msg(i, 200)).is_ok() {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    i = i.wrapping_add(1);
                    if i % 32 == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            })
        };

        let mut count = 0usize;
        let mut buf = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            match b.recv_timeout(&mut buf, Duration::from_millis(20)) {
                Ok(Some(_)) => count += 1,
                Ok(None) => {}
                Err(e) => panic!("recv 失败: {e}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let t_join = Instant::now();
        tx.join().expect("join sender");
        let join_ms = t_join.elapsed().as_millis();

        let sent_n = sent.load(Ordering::Relaxed);
        println!(
            "[3 两线程共享 Arc] 2s 内发 {sent_n} 条、收 {count} 条；发送线程 join {join_ms}ms；A inflight={} queue_dropped={}",
            a.stats().inflight,
            a.queue_dropped()
        );
        assert!(sent_n > 100, "发送太少: {sent_n}");
        assert!(count > 100, "接收太少: {count}");
        assert!(join_ms < 2000, "发送线程退出太慢（可能卡在锁上）: {join_ms}ms");
    }

    // ---------------------------------------------------------------- 4) 保活

    #[test]
    fn keepalive_keeps_link_alive_after_idle() {
        let (a, b) = pair();
        // 3 秒空闲：两边只 tick，不发业务数据
        let idle = Duration::from_secs(3);
        let t0 = Instant::now();
        while t0.elapsed() < idle {
            a.tick().expect("tick a");
            b.tick().expect("tick b");
            thread::sleep(Duration::from_millis(20));
        }
        // 空闲后不该被判死，也不该丢包
        a.send(&make_msg(7, 64)).expect("send after idle");
        let got = drain(&b, 1, Duration::from_secs(3));
        assert_eq!(got.len(), 1, "空闲 3 秒后消息没能送达");
        check_msg(&got[0], 7);

        let sa = a.stats();
        let sb = b.stats();
        println!(
            "[4 保活] 空闲 {}s 后仍可收发；A rtt={:.2}ms sent_frags={} recv_frags={}；B recv_frags={}",
            idle.as_secs(),
            sa.rtt_ms,
            sa.sent_frags,
            sa.recv_frags,
            sb.recv_frags
        );
        assert!(sa.rtt_ms > 0.0, "应该采到 PING/PONG 的 RTT");
        assert!(sb.recv_frags > 0, "对端应该收到了保活包");
    }

    // ---------------------------------------------------------------- 5) 参数校验

    #[test]
    fn rejects_empty_and_oversized_messages() {
        let (a, _b) = pair();
        let err = a.send(&[]).expect_err("空消息应返回错误");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        println!("[5 参数校验] 空消息 → {err}");

        let big = vec![0u8; MAX_MESSAGE + 1];
        let err = a.send(&big).expect_err("超过 MAX_MESSAGE 应返回错误");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        println!("[5 参数校验] {} 字节 → {err}", big.len());

        // 边界：正好 1 字节可以
        assert!(a.send(&[1u8]).is_ok());
        // 边界：MAX_FRAGS 对应的最大分片数不出错
        let max_frag_bytes = MAX_FRAGS * MTU_PAYLOAD;
        assert!(max_frag_bytes >= MAX_MESSAGE, "MAX_FRAGS 必须能覆盖 MAX_MESSAGE");
    }

    // ---------------------------------------------------------------- 6) relay 信封

    /// 假 relay：把 [0x10][dst_tag] 换成 [0x90][src_tag] 后转给对方。
    #[test]
    fn relay_mode_wraps_and_unwraps_envelope() {
        let pa = bind();
        let pb = bind();
        let px = bind();
        let a_addr = pa.local_addr().expect("a addr");
        let b_addr = pb.local_addr().expect("b addr");
        let px_addr = px.local_addr().expect("relay addr");
        const TAG_A: u32 = 0xA1;
        const TAG_B: u32 = 0xB2;

        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let relay = thread::spawn(move || {
            px.set_read_timeout(Some(Duration::from_millis(50))).expect("t");
            let mut buf = [0u8; 2048];
            let mut envelopes = 0u64;
            while !s.load(Ordering::Relaxed) {
                let Ok((len, from)) = px.recv_from(&mut buf) else {
                    continue;
                };
                if len < RELAY_HDR || buf[0] != RELAY_OUT {
                    continue;
                }
                envelopes += 1;
                let dst_tag = read_u32(&buf[1..5]);
                let (to, src_tag) = if from == a_addr {
                    (b_addr, TAG_A)
                } else {
                    (a_addr, TAG_B)
                };
                assert_eq!(dst_tag, if from == a_addr { TAG_B } else { TAG_A }, "dst_tag 不对");
                let mut out = Vec::with_capacity(len);
                out.push(RELAY_IN);
                out.extend_from_slice(&src_tag.to_be_bytes());
                out.extend_from_slice(&buf[RELAY_HDR..len]);
                let _ = px.send_to(&out, to);
            }
            envelopes
        });

        let a = UdpConn::new(
            pa,
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            PeerMode::Relay { relay: px_addr, src_tag: TAG_A, dst_tag: TAG_B },
        )
        .expect("conn a");
        let b = UdpConn::new(
            pb,
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            PeerMode::Relay { relay: px_addr, src_tag: TAG_B, dst_tag: TAG_A },
        )
        .expect("conn b");

        let a_tx = Arc::new(a);
        let a2 = Arc::clone(&a_tx);
        let sender = thread::spawn(move || {
            for i in 0..10u32 {
                a2.send(&make_msg(i, 3000)).expect("send relay");
            }
            drive_until_drained(&a2, Duration::from_secs(20))
        });
        let got = drain(&b, 10, Duration::from_secs(20));
        let settle = Instant::now() + Duration::from_secs(20);
        while a_tx.stats().inflight > 0 && Instant::now() < settle {
            b.tick().expect("tick b");
            thread::sleep(Duration::from_millis(1));
        }
        let drained = sender.join().expect("join sender");
        stop.store(true, Ordering::Relaxed);
        let envelopes = relay.join().expect("join relay");

        assert!(drained, "relay 模式还有未确认消息");
        assert_eq!(got.len(), 10, "relay 模式少收了消息");
        for (i, msg) in got.iter().enumerate() {
            check_msg(msg, i as u32);
        }
        println!(
            "[6 relay 信封] 10 条全部到达；relay 收到 {envelopes} 个 [0x10][dst_tag] 信封；mode={}",
            a_tx.mode().kind()
        );
        assert_eq!(a_tx.mode().kind(), "relay_udp");
        assert!(envelopes > 0, "relay 没收到信封");
    }

    // ---------------------------------------------------------------- 7) relay dst_tag = 0

    /// 约定：dst_tag = 0 表示"同房间另一端"，由 relay 按房间路由；
    /// 本端必须原样写出去（不能假设非 0），收到的 src_tag 只当信息不参与路由。
    #[test]
    fn relay_zero_dst_tag_means_same_room() {
        let sock = bind();
        let local = sock.local_addr().expect("local addr");
        let relay_sock = bind();
        let relay_addr = relay_sock.local_addr().expect("relay addr");
        let conn = UdpConn::new(
            sock,
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            PeerMode::Relay { relay: relay_addr, src_tag: 0x11, dst_tag: 0 },
        )
        .expect("conn");

        // 出站：dst_tag = 0 必须原样出现在信封里
        conn.send(&make_msg(3, 50)).expect("send");
        relay_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 2048];
        let (n, _from) = relay_sock.recv_from(&mut buf).expect("relay recv");
        assert!(n > RELAY_HDR, "relay 收到的包太短: {n}");
        assert_eq!(buf[0], RELAY_OUT, "信封类型不对");
        assert_eq!(read_u32(&buf[1..5]), 0, "dst_tag=0 必须原样写出去");
        assert_eq!(buf[RELAY_HDR], TYPE_DATA, "信封里应该是数据分片");

        // 入站：relay 用 [0x90][src_tag] 把对端的包送回来，剥壳后当普通分片处理
        let payload = make_msg(9, 40);
        let mut inner = Vec::new();
        inner.push(TYPE_DATA);
        inner.extend_from_slice(&0x1234_5678u32.to_be_bytes()); // conn_id（接收端不校验）
        inner.extend_from_slice(&0u32.to_be_bytes()); // seq 0
        inner.extend_from_slice(&0u16.to_be_bytes()); // frag 0
        inner.extend_from_slice(&1u16.to_be_bytes()); // fcnt 1
        inner.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        inner.extend_from_slice(&payload);
        let mut env = Vec::new();
        env.push(RELAY_IN);
        env.extend_from_slice(&0x77u32.to_be_bytes()); // src_tag：只当信息
        env.extend_from_slice(&inner);
        relay_sock.send_to(&env, local).expect("send back");

        let mut out = Vec::new();
        let n = conn
            .recv_timeout(&mut out, Duration::from_secs(3))
            .expect("recv")
            .expect("应该有消息");
        assert_eq!(n, payload.len());
        assert_eq!(out, payload, "剥壳后的内容不对");
        println!(
            "[7 relay dst_tag=0] 信封按 0 原样发出；入站 0x90+src_tag 剥壳后正常交付 {} 字节",
            n
        );
    }

    // ---------------------------------------------------------------- 8) 判死时长

    #[test]
    fn dead_after_defaults_to_6s_and_env_parses() {
        assert_eq!(DEAD_AFTER, Duration::from_secs(6), "默认判死时长应为 6 秒");
        // 没设 / 非法 / 0 → 一律回落默认值
        assert_eq!(parse_dead_ms(None), Duration::from_secs(6));
        assert_eq!(parse_dead_ms(Some("")), Duration::from_secs(6));
        assert_eq!(parse_dead_ms(Some("   ")), Duration::from_secs(6));
        assert_eq!(parse_dead_ms(Some("abc")), Duration::from_secs(6));
        assert_eq!(parse_dead_ms(Some("0")), Duration::from_secs(6));
        assert_eq!(parse_dead_ms(Some("-5")), Duration::from_secs(6));
        // 合法值毫米级生效
        assert_eq!(parse_dead_ms(Some("3000")), Duration::from_millis(3000));
        assert_eq!(parse_dead_ms(Some(" 4500 ")), Duration::from_millis(4500));
        println!(
            "[8 判死时长] 默认 {}ms；LRC_UDP_DEAD_MS=3000 → {}ms（非法值回默认）",
            DEAD_AFTER.as_millis(),
            parse_dead_ms(Some("3000")).as_millis()
        );
    }

    /// 对端完全不回包时，应该在 ~6 秒（不是 10 秒）判死链。
    #[test]
    fn dead_peer_is_detected_after_6s() {
        let mine = bind();
        // 一个"哑巴"对端：绑上端口但从不读、从不回包
        let silent = bind();
        let peer = silent.local_addr().expect("silent addr");
        let conn = UdpConn::new(mine, peer, PeerMode::Direct).expect("conn");

        let started = Instant::now();
        let mut out = Vec::new();
        let mut msg = String::new();
        let mut dead_ms = 0u128;
        while started.elapsed() < Duration::from_secs(12) {
            match conn.recv_timeout(&mut out, Duration::from_millis(100)) {
                Ok(None) => {}
                Ok(Some(_)) => panic!("哑巴对端不该有消息"),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    dead_ms = started.elapsed().as_millis();
                    msg = e.to_string();
                    break;
                }
                Err(e) => panic!("意外的错误: {e}"),
            }
        }
        assert!(dead_ms > 0, "12 秒内都没判死链");
        println!("[9 判死] {dead_ms}ms 后返回 TimedOut：{msg}");
        assert!(
            dead_ms >= 5_500 && dead_ms <= 9_000,
            "判死时刻应在 6 秒附近（5.5~9s），实际 {dead_ms}ms"
        );
        assert!(msg.contains("6.0"), "错误信息应写明 6.0 秒，实际: {msg}");
    }

    // ---------------------------------------------------------------- 10) 丢包统计口径

    /// 干净 loopback 上不该报出明显丢包（视频帧大小的消息也不该）。
    #[test]
    fn clean_loopback_reports_no_loss() {
        let (a, b) = pair();
        let a = Arc::new(a);
        let b = Arc::new(b);
        let a_tx = Arc::clone(&a);
        let sender = thread::spawn(move || {
            for i in 0..200u32 {
                a_tx.send(&make_msg(i, 300)).expect("send");
            }
            // 视频帧量级：32 KB ≈ 28 个分片
            a_tx.send(&make_msg(200, 32 * 1024)).expect("send 32KB");
            drive_until_drained(&a_tx, Duration::from_secs(15))
        });
        let got = drain(&b, 201, Duration::from_secs(20));
        let settle = Instant::now() + Duration::from_secs(15);
        while a.stats().inflight > 0 && Instant::now() < settle {
            b.tick().expect("tick b");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(sender.join().expect("join"), "还有未确认消息");
        assert_eq!(got.len(), 201, "少收了消息");
        for (i, msg) in got.iter().take(200).enumerate() {
            check_msg(msg, i as u32);
        }
        let sb = b.stats();
        let sa = a.stats();
        println!(
            "[10 丢包统计] 干净 loopback：B recv_frags={} loss_permille={}‰；A sent_frags={} 重传={}",
            sb.recv_frags, sb.loss_permille, sa.sent_frags, sa.retransmits
        );
        assert!(
            sb.loss_permille <= 5,
            "干净链路 loss 应接近 0，实际 {}‰（口径又算歪了）",
            sb.loss_permille
        );
    }

    // ---------------------------------------------------------------- 11) WAN 仿真放大率

    /// 带单向延迟的代理（模拟跨公网 RTT），可注入丢包。
    fn spin_wan_proxy(
        px: UdpSocket,
        a_addr: SocketAddr,
        b_addr: SocketAddr,
        delay_ms: u64,
        drop_every: u32,
    ) -> (Arc<AtomicU64>, Arc<AtomicBool>, thread::JoinHandle<u64>) {
        let dropped = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let d = Arc::clone(&dropped);
        let s = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            px.set_read_timeout(Some(Duration::from_millis(2))).expect("t");
            let mut queue: VecDeque<(Instant, SocketAddr, Vec<u8>)> = VecDeque::new();
            let mut buf = [0u8; 2048];
            let mut n: u64 = 0;
            let mut forwarded: u64 = 0;
            while !s.load(Ordering::Relaxed) {
                if let Ok((len, from)) = px.recv_from(&mut buf) {
                    n += 1;
                    if drop_every > 0 && n % u64::from(drop_every) == 0 {
                        d.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let dst = if from == a_addr { b_addr } else { a_addr };
                        queue.push_back((
                            Instant::now() + Duration::from_millis(delay_ms),
                            dst,
                            buf[..len].to_vec(),
                        ));
                    }
                }
                let now = Instant::now();
                while let Some((due, _, _)) = queue.front() {
                    if *due > now {
                        break;
                    }
                    if let Some((_, dst, data)) = queue.pop_front() {
                        let _ = px.send_to(&data, dst);
                        forwarded += 1;
                    }
                }
            }
            forwarded
        });
        (dropped, stop, handle)
    }

    fn wan_case(label: &str, drop_every: u32, count: u32) {
        let pa = bind();
        let pb = bind();
        let px = bind();
        let a_addr = pa.local_addr().expect("a");
        let b_addr = pb.local_addr().expect("b");
        let px_addr = px.local_addr().expect("px");
        // 单向 45ms → RTT ≈ 90ms（和实测 87~94ms 对齐）
        let (dropped, stop, proxy) = spin_wan_proxy(px, a_addr, b_addr, 45, drop_every);
        let a = Arc::new(UdpConn::new(pa, px_addr, PeerMode::Direct).expect("conn a"));
        let b = Arc::new(UdpConn::new(pb, px_addr, PeerMode::Direct).expect("conn b"));

        let a_tx = Arc::clone(&a);
        let sender = thread::spawn(move || {
            for i in 0..count {
                // 3 KB ≈ 3 个分片，接近块模式一帧的常见大小
                a_tx.send(&make_msg(i, 3000)).expect("send");
            }
            drive_until_drained(&a_tx, Duration::from_secs(90))
        });
        let got = drain(&b, count as usize, Duration::from_secs(90));
        let settle = Instant::now() + Duration::from_secs(30);
        while a.stats().inflight > 0 && Instant::now() < settle {
            b.tick().expect("tick b");
            thread::sleep(Duration::from_millis(1));
        }
        let drained = sender.join().expect("join sender");
        stop.store(true, Ordering::Relaxed);
        let _ = proxy.join();

        assert!(drained, "还有未确认消息");
        assert_eq!(got.len(), count as usize, "少收了消息");
        for (i, msg) in got.iter().enumerate() {
            check_msg(msg, i as u32);
        }
        let sa = a.stats();
        let lost = dropped.load(Ordering::Relaxed);
        println!(
            "[11 WAN 仿真 {label}] data={} B wire={} B 放大={:.2}x | 重传 RTO={} NACK={} | ACK={} NACK条目={} | rtt={:.1}ms jitter={:.1}ms | 代理丢包 {lost}",
            sa.data_bytes,
            sa.sent_bytes,
            sa.amplification(),
            sa.rto_retrans,
            sa.nack_retrans,
            sa.acks_sent,
            sa.nacks_sent,
            sa.rtt_ms,
            sa.rtt_var_ms
        );
        assert!(
            sa.amplification() < 1.3,
            "{label}: 放大率应 < 1.3x，实际 {:.2}x",
            sa.amplification()
        );
    }

    /// 目标：RTT≈90ms、丢包 0%/3% 时，线上字节 / 应用层字节 < 1.3x。
    #[test]
    fn wan_like_amplification_stays_low() {
        wan_case("0% 丢包", 0, 300);
        wan_case("~3% 丢包", 34, 300);
    }
}
