//! 长度前缀消息收发。M1 用 TCP，M2 换 QUIC（同一套 API）。
//!
//! 帧格式：u32 payload_len（小端，不含自身）+ payload；单条 payload 上限
//! lrc_proto::MAX_PAYLOAD，收发两边超限都返回 io::ErrorKind::InvalidData。
//!
//! 用法约定：
//! - 一个线程收、一个线程发时，先 Conn::split() 拿 (ConnWriter, ConnReader)。
//! - try_recv 给 poll 循环用：不足一整条时返回 Ok(None)，半包字节留在内部 rx
//!   里不会丢；对端已关闭且没有完整消息时返回 UnexpectedEof，方便跳出循环。
//!   注意它会在调用期间把套接字临时切成非阻塞（返回前恢复）。
//! - sent_bytes / recv_bytes 统计线上字节数（含 4 字节长度前缀）。

pub mod link;

use std::io::{self, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};

use lrc_proto::MAX_PAYLOAD;

/// 长度前缀的字节数。
const LEN_PREFIX: usize = 4;
/// 每次非阻塞读最多吃进的字节数。
const READ_CHUNK: usize = 64 * 1024;
/// 临时被切成非阻塞时，写遇到 WouldBlock 的重试次数（每次退避 200us）。
const WOULD_BLOCK_SPINS: u32 = 100;

/// 一条连接的两端。
pub struct Conn {
    stream: TcpStream,
    rx: Vec<u8>,
    sent_bytes: u64,
    recv_bytes: u64,
}

impl Conn {
    /// 主动连接。
    pub fn connect(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Self::from_tcp(stream)
    }

    /// 从已建立的 TCP 流构造。
    pub fn from_tcp(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            rx: Vec::with_capacity(64 * 1024),
            sent_bytes: 0,
            recv_bytes: 0,
        })
    }

    /// 接受一条连接（阻塞）。
    pub fn accept(listener: &TcpListener) -> io::Result<Self> {
        let (stream, _peer) = listener.accept()?;
        Self::from_tcp(stream)
    }

    /// 发送一条已编码的消息（payload 首字节为 type）。
    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        write_frame(&mut self.stream, payload, &mut self.sent_bytes)
    }

    /// 阻塞读一条完整消息到 out（先清空）。
    ///
    /// 返回 Ok(None) 表示对端正常关闭。
    pub fn recv(&mut self, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        recv_blocking(&mut self.stream, &mut self.rx, out, &mut self.recv_bytes)
    }

    /// 非阻塞地取出已经缓冲的一条完整消息（配合 poll 使用）。
    pub fn try_recv(&mut self, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        try_recv_nonblocking(&mut self.stream, &mut self.rx, out, &mut self.recv_bytes)
    }

    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    pub fn recv_bytes(&self) -> u64 {
        self.recv_bytes
    }

    /// 拆成读/写两半，供"一个线程收、一个线程发"的用法（被控端/主控端都这么用）。
    pub fn split(self) -> io::Result<(ConnWriter, ConnReader)> {
        let reader = self.stream.try_clone()?;
        let _ = reader.set_nodelay(true);
        Ok((
            ConnWriter {
                stream: self.stream,
                sent_bytes: self.sent_bytes,
            },
            ConnReader {
                stream: reader,
                // 已经缓冲的字节要跟着读半边走，不能丢。
                rx: self.rx,
                recv_bytes: self.recv_bytes,
            },
        ))
    }
}

/// 只写半边。send 必须整条消息原子写入（TCP 上即 len+payload）。
pub struct ConnWriter {
    stream: TcpStream,
    sent_bytes: u64,
}

impl ConnWriter {
    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        write_frame(&mut self.stream, payload, &mut self.sent_bytes)
    }

    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// 设置写超时，避免对端卡死时永远阻塞。
    pub fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(timeout)
    }
}

/// 只读半边。
pub struct ConnReader {
    stream: TcpStream,
    rx: Vec<u8>,
    recv_bytes: u64,
}

impl ConnReader {
    /// 阻塞读一条完整消息到 out（先清空）；Ok(None) 表示对端关闭。
    pub fn recv(&mut self, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        recv_blocking(&mut self.stream, &mut self.rx, out, &mut self.recv_bytes)
    }

    /// 非阻塞取出已缓冲的一条完整消息；不够一整条时返回 Ok(None)。
    ///
    /// 内部会先 read 一次可用字节（非阻塞），再尝试切包。
    /// 注意：对端已关闭且没有完整消息时返回 io::ErrorKind::UnexpectedEof，
    /// 这样 poll 循环能区分"暂时没数据"和"连接没了"。
    pub fn try_recv(&mut self, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        try_recv_nonblocking(&mut self.stream, &mut self.rx, out, &mut self.recv_bytes)
    }

    pub fn recv_bytes(&self) -> u64 {
        self.recv_bytes
    }

    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }
}

// ---------------------------------------------------------------- 内部实现

/// 写满一段数据。套接字默认是阻塞的；只有在别的线程调用 try_recv 期间被临时
/// 切成非阻塞时才会碰到 WouldBlock，这里短暂退避重试掉，不影响上层语义。
fn write_full(stream: &mut TcpStream, mut data: &[u8], written: &mut u64) -> io::Result<()> {
    let mut spins = 0u32;
    while !data.is_empty() {
        match stream.write(data) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "TCP 写入返回 0 字节",
                ));
            }
            Ok(n) => {
                *written += n as u64;
                data = &data[n..];
                spins = 0;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && spins < WOULD_BLOCK_SPINS => {
                spins += 1;
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 写一条帧：u32 长度前缀 + payload。
fn write_frame(stream: &mut TcpStream, payload: &[u8], sent: &mut u64) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("消息 {} 字节，超过上限 {MAX_PAYLOAD}", payload.len()),
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "消息长度超过 u32"))?;
    let header = len.to_le_bytes();
    let total = LEN_PREFIX + payload.len();

    // 先试一次 vectored 写，让长度前缀和内容进同一个 TCP 段（小消息省一个包）。
    let slices = [IoSlice::new(&header), IoSlice::new(payload)];
    let mut written = 0u64;
    let first = match stream.write_vectored(&slices) {
        Ok(n) => n,
        Err(e)
            if e.kind() == io::ErrorKind::Interrupted || e.kind() == io::ErrorKind::WouldBlock =>
        {
            0
        }
        Err(e) => return Err(e),
    };
    written += first as u64;
    let mut done = first.min(total);

    // 部分写（大消息常见）：把剩下的按"前缀 → 内容"顺序写完。
    if done < LEN_PREFIX {
        write_full(stream, &header[done..], &mut written)?;
        done = LEN_PREFIX;
    }
    if done < total {
        write_full(stream, &payload[done - LEN_PREFIX..], &mut written)?;
    }

    *sent += written;
    Ok(())
}

/// 解析长度前缀（调用前保证 rx.len() >= 4）。
fn parse_len(rx: &[u8]) -> io::Result<usize> {
    let len = u32::from_le_bytes([rx[0], rx[1], rx[2], rx[3]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("对端声明消息 {len} 字节，超过上限 {MAX_PAYLOAD}"),
        ));
    }
    Ok(len)
}

/// 从 rx 里切出一条完整消息；不够一整条返回 Ok(None)，半个包留在 rx 里不丢。
fn peel(rx: &mut Vec<u8>, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
    if rx.len() < LEN_PREFIX {
        return Ok(None);
    }
    let len = parse_len(rx)?;
    let total = LEN_PREFIX + len;
    if rx.len() < total {
        return Ok(None);
    }
    out.clear();
    out.extend_from_slice(&rx[LEN_PREFIX..total]);
    if rx.len() == total {
        rx.clear();
    } else {
        rx.drain(..total);
    }
    Ok(Some(len))
}

/// 阻塞读到一条完整消息。优先从 rx 里切，不够再读；读满一条即返回。
///
/// 关键约定：**读到的字节一律留在 rx 里**，包括读超时/出错之前已经到达的那部分。
/// 以前这里用 `read_exact` 直接补消息体，一旦碰上读超时就把 "已读进 rx 的半个包"
/// 连同缓冲一起 truncate 掉——那批字节在 socket 里已经没了，后续永远等不齐，
/// 会话就此半死不活（客户端继续发，host 一直等到天荒地老）。
fn recv_blocking(
    stream: &mut TcpStream,
    rx: &mut Vec<u8>,
    out: &mut Vec<u8>,
    recv_bytes: &mut u64,
) -> io::Result<Option<usize>> {
    loop {
        if let Some(len) = peel(rx, out)? {
            return Ok(Some(len));
        }
        // 还差多少：知道长度就只要补齐这一条，否则按块读。
        let want = if rx.len() >= LEN_PREFIX {
            let len = parse_len(rx)?;
            (LEN_PREFIX + len).saturating_sub(rx.len()).min(READ_CHUNK)
        } else {
            READ_CHUNK
        }
        .max(1);
        let have = rx.len();
        rx.resize(have + want, 0);
        match stream.read(&mut rx[have..]) {
            Ok(0) => {
                // 对端关闭：截掉没填上的尾巴，再按"已经攒到哪一步"给结论。
                rx.truncate(have);
                if have == 0 {
                    // 一条没攒到：干净关闭。
                    return Ok(None);
                }
                if have < LEN_PREFIX {
                    // 长度头都没齐：对端在头中途关掉。
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "对端在长度前缀中途关闭",
                    ));
                }
                // 长度头齐了但消息体没齐：截断。
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("消息体被截断：只有 {have} 字节"),
                ));
            }
            Ok(n) => {
                *recv_bytes += n as u64;
                rx.truncate(have + n);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                rx.truncate(have);
            }
            Err(e) => {
                // 超时 / 出错：这批新字节没收下，但之前缓冲的半个包必须原样保留。
                rx.truncate(have);
                return Err(e);
            }
        }
    }
}

/// 临时把套接字切成非阻塞，离开作用域无条件切回来（包括 ? 提前返回和 panic）。
///
/// 用 &mut 借用持有套接字：读操作走 scope.stream，作用域一结束就恢复阻塞模式。
struct NonBlockingScope<'a> {
    stream: &'a mut TcpStream,
}

impl<'a> NonBlockingScope<'a> {
    fn begin(stream: &'a mut TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream })
    }
}

impl Drop for NonBlockingScope<'_> {
    fn drop(&mut self) {
        let _ = self.stream.set_nonblocking(false);
    }
}

/// 非阻塞地读一批数据进 rx，并切出一条完整消息。
///
/// 注意：调用期间套接字是临时非阻塞的；如果同时有另一个线程在写同一个连接，
/// 极端情况下它的 write 会拿到 WouldBlock（write_frame 里已经带退避重试）。
fn try_recv_nonblocking(
    stream: &mut TcpStream,
    rx: &mut Vec<u8>,
    out: &mut Vec<u8>,
    recv_bytes: &mut u64,
) -> io::Result<Option<usize>> {
    if let Some(len) = peel(rx, out)? {
        return Ok(Some(len));
    }

    let scope = NonBlockingScope::begin(stream)?;
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match scope.stream.read(&mut buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "对端已关闭连接",
                ));
            }
            Ok(n) => {
                *recv_bytes += n as u64;
                rx.extend_from_slice(&buf[..n]);
                if let Some(len) = peel(rx, out)? {
                    return Ok(Some(len));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    // ------------------------------------------------------------ 测试工具

    /// 极简 xorshift64*，只给测试造随机长度用。
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed | 1)
        }

        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
        }
    }

    fn bind_local() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// 接受一条连接并设好读超时，免得实现有 bug 时测试永远挂着。
    fn accept_with_timeout(listener: TcpListener) -> Conn {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Conn::from_tcp(stream).unwrap()
    }

    /// 1000 条随机长度消息 + 8 MiB 大消息，附边界长度（0/1）。
    fn build_messages() -> Vec<Vec<u8>> {
        let mut rng = Rng::new(0x1234_5678);
        let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(1003);
        msgs.push(Vec::new());
        msgs.push(vec![0x7F]);
        for i in 0..1000usize {
            let len = match i % 7 {
                0 => 4096,
                1 => 1,
                _ => (rng.next_u32() % 4096) as usize,
            };
            let mut m = vec![0u8; len];
            for (k, b) in m.iter_mut().enumerate() {
                *b = (rng.next_u32() ^ (k as u32).wrapping_mul(31)) as u8;
            }
            msgs.push(m);
        }
        msgs.push(vec![0x5A; 8 * 1024 * 1024]);
        msgs
    }

    // ------------------------------------------------------------ TCP 往返

    #[test]
    fn tcp_roundtrip_1000_messages_and_8mib() {
        let (listener, addr) = bind_local();
        let messages = build_messages();
        let count = messages.len();
        let total_bytes: u64 = messages.iter().map(|m| (LEN_PREFIX + m.len()) as u64).sum();

        let receiver = thread::spawn(move || {
            let mut conn = accept_with_timeout(listener);
            let mut out = Vec::new();
            let mut got: Vec<Vec<u8>> = Vec::with_capacity(count);
            while got.len() < count {
                match conn.recv(&mut out).unwrap() {
                    Some(len) => {
                        assert_eq!(len, out.len(), "返回长度应与 out 一致");
                        got.push(out.clone());
                    }
                    None => panic!("对端提前关闭，只收到 {} 条", got.len()),
                }
            }
            (got, conn.recv_bytes())
        });

        let mut conn = Conn::connect(&addr.to_string()).unwrap();
        for m in &messages {
            conn.send(m).unwrap();
        }
        assert_eq!(conn.sent_bytes(), total_bytes, "sent_bytes 应等于线上字节数");

        let (got, recv_total) = receiver.join().unwrap();
        assert_eq!(got.len(), messages.len());
        for (i, (a, b)) in got.iter().zip(messages.iter()).enumerate() {
            assert_eq!(a.len(), b.len(), "第 {i} 条长度不一致");
            assert!(a == b, "第 {i} 条内容不一致");
        }
        assert_eq!(recv_total, total_bytes, "recv_bytes 应等于线上字节数");
        println!(
            "[tcp] {} 条消息（含 8 MiB）往返 OK，单向 {} 字节",
            messages.len(),
            total_bytes
        );
    }

    #[test]
    fn split_halves_carry_both_directions() {
        let (listener, addr) = bind_local();
        let n = 500usize;

        let server = thread::spawn(move || {
            let conn = accept_with_timeout(listener);
            let (mut writer, mut reader) = conn.split().unwrap();
            let mut out = Vec::new();
            let mut got = Vec::new();
            for i in 0..n {
                let len = reader.recv(&mut out).unwrap().expect("应还有消息");
                assert_eq!(len, out.len());
                got.push(out.clone());
                if i == n - 1 {
                    break;
                }
            }
            for i in 0..n {
                writer.send(format!("server-{i}").as_bytes()).unwrap();
            }
            (got, reader.recv_bytes(), writer.sent_bytes())
        });

        let conn = Conn::connect(&addr.to_string()).unwrap();
        let (mut writer, mut reader) = conn.split().unwrap();

        let mut sent: Vec<Vec<u8>> = Vec::with_capacity(n);
        for i in 0..n {
            let m = format!("client-{i}-{}", "x".repeat(i % 17)).into_bytes();
            writer.send(&m).unwrap();
            sent.push(m);
        }
        assert_eq!(
            writer.sent_bytes(),
            sent.iter().map(|m| (LEN_PREFIX + m.len()) as u64).sum::<u64>()
        );

        let mut out = Vec::new();
        for i in 0..n {
            let len = reader.recv(&mut out).unwrap().expect("应还有消息");
            assert_eq!(len, out.len());
            assert_eq!(out, format!("server-{i}").into_bytes());
        }

        let (got, server_recv, server_sent) = server.join().unwrap();
        assert_eq!(got, sent, "被控端收到的内容/顺序必须一致");
        assert_eq!(server_recv, writer.sent_bytes());
        assert_eq!(server_sent, reader.recv_bytes());
        println!("[split] 双向各 {n} 条消息 OK");
    }

    // ------------------------------------------------------------ try_recv 半包

    #[test]
    fn try_recv_half_frame_keeps_bytes() {
        let (listener, addr) = bind_local();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let server = thread::spawn(move || {
            let mut conn = accept_with_timeout(listener);
            let mut out = Vec::new();
            // 1) 对端还没发东西：应为 None
            let first = conn.try_recv(&mut out).unwrap();
            ready_tx.send(()).unwrap();

            // 2) 半包期间应一直 None；凑齐后整条吐出来
            let mut none_count = 0usize;
            let (len, payload) = loop {
                match conn.try_recv(&mut out).unwrap() {
                    Some(len) => break (len, out.clone()),
                    None => {
                        none_count += 1;
                        assert!(none_count < 5000, "半包阶段一直等不到完整消息");
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            };

            // 3) 一次写入 3 条消息：应逐条取出且顺序正确
            let mut got3 = Vec::new();
            while got3.len() < 3 {
                match conn.try_recv(&mut out).unwrap() {
                    Some(len) => got3.push((len, out.clone())),
                    None => thread::sleep(Duration::from_millis(2)),
                }
            }
            (
                first.is_none(),
                none_count,
                len,
                payload,
                got3,
                conn.recv_bytes(),
            )
        });

        let mut raw = TcpStream::connect(addr).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let header = (payload.len() as u32).to_le_bytes();
        raw.write_all(&header).unwrap();
        raw.write_all(&payload[..300]).unwrap();
        raw.flush().unwrap();
        thread::sleep(Duration::from_millis(80));
        raw.write_all(&payload[300..]).unwrap();
        raw.flush().unwrap();

        // 3 条消息拼成一次写入
        let mut batched: Vec<u8> = Vec::new();
        let mut expected3: Vec<Vec<u8>> = Vec::new();
        for i in 0..3u8 {
            let m = vec![i; 10 + usize::from(i)];
            batched.extend_from_slice(&(m.len() as u32).to_le_bytes());
            batched.extend_from_slice(&m);
            expected3.push(m);
        }
        raw.write_all(&batched).unwrap();
        raw.flush().unwrap();

        let (first_none, none_count, len, got, got3, recv_bytes) = server.join().unwrap();
        assert!(first_none, "刚连上时 try_recv 应为 Ok(None)");
        assert!(none_count > 0, "半包期间应该至少有一次 None");
        assert_eq!(len, payload.len());
        assert_eq!(got, payload, "半包不能丢已消费字节");
        assert_eq!(got3.len(), 3);
        for (i, (len, m)) in got3.iter().enumerate() {
            assert_eq!(*len, expected3[i].len());
            assert_eq!(m, &expected3[i], "批量到达的第 {i} 条不一致");
        }
        let expected_total = (LEN_PREFIX + payload.len()) as u64
            + expected3
                .iter()
                .map(|m| (LEN_PREFIX + m.len()) as u64)
                .sum::<u64>();
        assert_eq!(recv_bytes, expected_total, "try_recv 的 recv_bytes 要准");
        println!("[try_recv] 半包 {none_count} 次 None 后收齐，批量 3 条顺序正确");
    }

    // ------------------------------------------------------------ 非法输入

    #[test]
    fn oversized_length_header_is_rejected() {
        for bogus_len in [u32::MAX, (MAX_PAYLOAD as u32) + 1] {
            let (listener, addr) = bind_local();
            let server = thread::spawn(move || {
                let mut conn = accept_with_timeout(listener);
                let mut out = Vec::new();
                let err = conn.recv(&mut out).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidData, "错误类型: {err:?}");
                assert!(out.is_empty(), "出错时不应交出半条消息");
            });

            let mut raw = TcpStream::connect(addr).unwrap();
            raw.write_all(&bogus_len.to_le_bytes()).unwrap();
            raw.flush().unwrap();
            // 连接先别关，确认上层一看到长度头就报错，而不是傻等 payload
            thread::sleep(Duration::from_millis(50));
            server.join().unwrap();
            drop(raw);
        }
        println!("[transport] 超限长度头（u32::MAX / MAX_PAYLOAD+1）均返回 InvalidData");
    }

    #[test]
    fn truncated_frame_reports_unexpected_eof() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let mut conn = accept_with_timeout(listener);
            let mut out = Vec::new();
            let err = conn.recv(&mut out).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "错误类型: {err:?}");
        });

        let mut raw = TcpStream::connect(addr).unwrap();
        raw.write_all(&1000u32.to_le_bytes()).unwrap();
        raw.write_all(&[0xAB; 10]).unwrap();
        raw.flush().unwrap();
        drop(raw); // 对端关闭 → 消息体被截断
        server.join().unwrap();
        println!("[transport] 截断的消息体报 UnexpectedEof（不 panic / 不 OOM）");
    }

    #[test]
    fn clean_close_returns_none() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let mut conn = Conn::accept(&listener).unwrap();
            let mut out = Vec::new();
            assert_eq!(conn.recv(&mut out).unwrap(), None, "对端正常关闭应返回 Ok(None)");
            assert_eq!(conn.recv_bytes(), 0);
        });

        let raw = TcpStream::connect(addr).unwrap();
        drop(raw);
        server.join().unwrap();
    }

    #[test]
    fn try_recv_reports_eof_instead_of_spinning() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let mut conn = accept_with_timeout(listener);
            let mut out = Vec::new();
            let mut saw_eof = false;
            for _ in 0..100 {
                match conn.try_recv(&mut out) {
                    Ok(Some(_)) => panic!("不该有消息"),
                    Ok(None) => thread::sleep(Duration::from_millis(2)),
                    Err(e) => {
                        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
                        saw_eof = true;
                        break;
                    }
                }
            }
            assert!(saw_eof, "对端关闭后 try_recv 必须报 UnexpectedEof，不能永远 None");
        });

        let raw = TcpStream::connect(addr).unwrap();
        thread::sleep(Duration::from_millis(30));
        drop(raw);
        server.join().unwrap();
    }

    #[test]
    fn send_rejects_oversized_payload() {
        let (listener, addr) = bind_local();
        let waiter = thread::spawn(move || {
            let _ = listener.accept();
        });

        let mut conn = Conn::connect(&addr.to_string()).unwrap();
        let big = vec![0u8; MAX_PAYLOAD + 1];
        match conn.send(&big) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData),
            Ok(()) => panic!("超过 MAX_PAYLOAD 的消息必须被拒绝"),
        }
        assert_eq!(conn.sent_bytes(), 0, "被拒绝的消息不应计入 sent_bytes");
        waiter.join().unwrap();
    }
}
