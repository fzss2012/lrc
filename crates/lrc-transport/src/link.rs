//! 统一连接抽象：把 TCP（`lrc-transport`）与可靠 UDP（`lrc-udp`）收敛成同一套读写接口，
//! 让 host/client 的上层协议（握手、VideoFrame、AudioFrame…）**完全不感知底层走哪条路**。
//!
//! 由 Lead 维护（跨 crate 共享契约）。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::{Conn, ConnReader, ConnWriter};
use lrc_udp::{UdpConn, UdpStats};

/// 路径名字（直接进 `--stats` 的 `path=` 字段）。
pub const PATH_TCP: &str = "tcp";
pub const PATH_DIRECT_UDP: &str = "direct_udp";
pub const PATH_RELAY_UDP: &str = "relay_udp";

fn udp_path(mode: lrc_udp::PeerMode) -> &'static str {
    match mode {
        lrc_udp::PeerMode::Direct => PATH_DIRECT_UDP,
        lrc_udp::PeerMode::Relay { .. } => PATH_RELAY_UDP,
    }
}

/// 写半边。
pub enum LinkWriter {
    Tcp(ConnWriter),
    Udp(Arc<UdpConn>),
}

impl LinkWriter {
    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        match self {
            LinkWriter::Tcp(w) => w.send(payload),
            LinkWriter::Udp(u) => u.send(payload),
        }
    }

    pub fn shutdown(&self) {
        match self {
            LinkWriter::Tcp(w) => w.shutdown(),
            LinkWriter::Udp(_) => {}
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            LinkWriter::Tcp(w) => w.set_write_timeout(timeout),
            LinkWriter::Udp(_) => Ok(()),
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            LinkWriter::Tcp(_) => PATH_TCP,
            LinkWriter::Udp(u) => udp_path(u.mode()),
        }
    }
}

/// 读半边。
pub enum LinkReader {
    Tcp(ConnReader),
    Udp(Arc<UdpConn>),
}

impl LinkReader {
    /// 阻塞读一条完整消息；`Ok(None)` 表示对端关闭（UDP 下不会返回 None）。
    pub fn recv(&mut self, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        match self {
            LinkReader::Tcp(r) => r.recv(out),
            LinkReader::Udp(u) => loop {
                match u.recv_timeout(out, Duration::from_millis(200)) {
                    Ok(Some(n)) => return Ok(Some(n)),
                    Ok(None) => continue,
                    Err(e) => return Err(e),
                }
            },
        }
    }

    /// 带超时的读：**只有真的超时**才返回 `Ok(None)`。
    ///
    /// 这条区分是整个抽象里最容易被写错、也最贵的一处（P0）：
    /// Windows 上对端正常断开时 `read` 返回 `Ok(0)`（EOF），而读超时返回
    /// `Err(TimedOut)`。如果把两者都翻译成 `Ok(None)`，调用方的 poll 循环
    /// 就会把"对端已经没了"当成"再等一会儿"，于是 `Ok(0)` 立刻返回、循环里
    /// 没有任何阻塞点 → 单核 100% 空转，且永远不会退出会话去 accept 下一条连接。
    /// 所以这里：超时 → `Ok(None)`；对端关闭 → `Err(UnexpectedEof)`。
    pub fn recv_timeout(&mut self, out: &mut Vec<u8>, timeout: Duration) -> io::Result<Option<usize>> {
        match self {
            LinkReader::Tcp(r) => {
                r.set_read_timeout(Some(timeout))?;
                match r.recv(out) {
                    // recv_blocking 的 Ok(None) 只在"一条没攒到且对端已关闭"时出现，
                    // 那是 EOF，不是超时。
                    Ok(None) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "对端已关闭连接",
                    )),
                    Ok(Some(n)) => Ok(Some(n)),
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            LinkReader::Udp(u) => u.recv_timeout(out, timeout),
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            LinkReader::Tcp(_) => PATH_TCP,
            LinkReader::Udp(u) => udp_path(u.mode()),
        }
    }

    /// UDP 路径下返回链路质量；TCP 路径返回 `None`（由上层用 ack 估算）。
    pub fn udp_stats(&self) -> Option<UdpStats> {
        match self {
            LinkReader::Tcp(_) => None,
            LinkReader::Udp(u) => Some(u.stats()),
        }
    }
}

/// 从已建立的 TCP 连接拆出读写两半。
pub fn from_tcp(conn: Conn) -> io::Result<(LinkWriter, LinkReader)> {
    let (w, r) = conn.split()?;
    Ok((LinkWriter::Tcp(w), LinkReader::Tcp(r)))
}

/// 从一个可靠的 UDP 连接拆出读写两半（共享同一个 `Arc<UdpConn>`）。
pub fn from_udp(conn: UdpConn) -> (LinkWriter, LinkReader) {
    let shared = Arc::new(conn);
    (LinkWriter::Udp(shared.clone()), LinkReader::Udp(shared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    fn bind_local() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr.to_string())
    }

    /// 服务端拿到一条链路，并设好读超时，避免实现有 bug 时测试永远挂着。
    fn accept_link(listener: TcpListener) -> (LinkWriter, LinkReader) {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let conn = Conn::from_tcp(stream).unwrap();
        from_tcp(conn).unwrap()
    }

    /// P0 回归：对端正常关闭时，`recv_timeout` 必须报 UnexpectedEof，**不能**返回 Ok(None)。
    ///
    /// 返回 Ok(None) 会让调用方的 poll 循环把"对端已经没了"当成"再等一会儿"：
    /// 每次 read 都立刻拿到 0 字节 → 循环里没有任何阻塞点 → 单核 100% 空转，
    /// 且永远不会判定会话结束，也就再也回不到 accept（Windows host 的实际故障）。
    #[test]
    fn recv_timeout_reports_eof_instead_of_timeout() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let (writer, mut reader) = accept_link(listener);
            let mut out = Vec::new();
            let started = Instant::now();
            let err = reader
                .recv_timeout(&mut out, Duration::from_secs(2))
                .expect_err("对端已关闭必须报错，不能当成超时的 Ok(None)");
            let elapsed = started.elapsed();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "错误类型: {err:?}");
            assert!(out.is_empty(), "不该交出半条消息");
            assert!(
                elapsed < Duration::from_secs(1),
                "对端关闭应立即返回，实际用了 {elapsed:?}"
            );
            // 反复调用也必须一直是 Err，而不是 Ok(None) 的忙循环
            for _ in 0..5 {
                let again = reader.recv_timeout(&mut out, Duration::from_millis(200));
                assert_eq!(
                    again.expect_err("关闭后必须持续报错").kind(),
                    io::ErrorKind::UnexpectedEof
                );
            }
            // 写端 keep 到断言之后：只靠对端的 FIN 触发 EOF
            drop(writer);
        });

        let raw = std::net::TcpStream::connect(addr).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(raw); // 干净关闭（FIN）→ 对端 read 得到 0 字节
        server.join().unwrap();
    }

    /// 真超时仍必须是 `Ok(None)`：修复不能把"对端还活着只是没数据"也判成断线。
    #[test]
    fn recv_timeout_still_returns_none_on_real_timeout() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let (_writer, mut reader) = accept_link(listener);
            let mut out = Vec::new();
            let started = Instant::now();
            let got = reader
                .recv_timeout(&mut out, Duration::from_millis(150))
                .expect("对端还活着，真超时不该报错");
            assert!(got.is_none(), "超时应返回 Ok(None)");
            assert!(
                started.elapsed() >= Duration::from_millis(120),
                "返回得太早，不像真超时"
            );
            // 连接还好着：之后能正常收到消息
            let n = reader
                .recv_timeout(&mut out, Duration::from_secs(5))
                .expect("对端仍在，后续读必须成功")
                .expect("应收到消息");
            assert_eq!(n, out.len());
            out.clone()
        });

        let mut raw = std::net::TcpStream::connect(addr).unwrap();
        thread::sleep(Duration::from_millis(400)); // 先什么都不发，制造一次真超时
        raw.write_all(&(5u32).to_le_bytes()).unwrap();
        raw.write_all(b"hello").unwrap();
        raw.flush().unwrap();

        assert_eq!(server.join().unwrap(), b"hello");
    }

    /// 读超时之前已经到达的字节**不能丢**。
    ///
    /// 这是老实现真正丢数据的路径：它用 `read_exact` 直接往 rx 里补消息体，
    /// 一旦这次 read 只拿到一部分就超时，那些刚读进 rx 的字节会被 `truncate` 掉，
    /// 而它们在 socket 里已经被消费——消息永远凑不齐，会话从此半死不活。
    /// 所以这里让消息体**一块一块慢慢到**，每块之间都跨过一次读超时。
    #[test]
    fn partial_reads_survive_repeated_timeouts() {
        let (listener, addr) = bind_local();
        let server = thread::spawn(move || {
            let (_writer, mut reader) = accept_link(listener);
            let mut out = Vec::new();
            let mut timeouts = 0usize;
            let n = loop {
                match reader.recv_timeout(&mut out, Duration::from_millis(100)) {
                    Ok(Some(n)) => break n,
                    Ok(None) => {
                        timeouts += 1;
                        assert!(timeouts < 200, "消息一直凑不齐（已超时 {timeouts} 次）");
                    }
                    Err(e) => panic!("读失败：{e}"),
                }
            };
            assert_eq!(n, out.len());
            (timeouts, out.clone())
        });

        let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
        let mut raw = std::net::TcpStream::connect(addr).unwrap();
        raw.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
        // 消息体切成 6 块，每块之间停 150ms —— 比读端 100ms 的超时更长，
        // 于是每次 read 都会"读到一点 → 等不全 → 超时"。
        for chunk in payload.chunks(1000) {
            raw.write_all(chunk).unwrap();
            raw.flush().unwrap();
            thread::sleep(Duration::from_millis(150));
        }

        let (timeouts, got) = server.join().unwrap();
        assert!(timeouts > 0, "这条用例应当至少撞到一次读超时");
        assert_eq!(got.len(), payload.len(), "超时把已到达的字节丢了");
        assert!(got == payload, "拼接后的内容不一致");
    }
}
