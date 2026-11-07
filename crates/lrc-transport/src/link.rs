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

    /// 带超时的读；超时返回 `Ok(None)`。
    pub fn recv_timeout(&mut self, out: &mut Vec<u8>, timeout: Duration) -> io::Result<Option<usize>> {
        match self {
            LinkReader::Tcp(r) => {
                r.set_read_timeout(Some(timeout))?;
                match r.recv(out) {
                    Ok(v) => Ok(v),
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
