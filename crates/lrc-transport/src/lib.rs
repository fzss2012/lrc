//! 长度前缀消息收发。M1 用 TCP，M2 换 QUIC（同一套 API）。
//!
//! 帧格式：`u32 payload_len`（小端）+ payload。

use std::io;
use std::net::{TcpListener, TcpStream};

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
    pub fn send(&mut self, _payload: &[u8]) -> io::Result<()> {
        todo!("T2 实现：写 u32 长度前缀 + 内容，累计 sent_bytes")
    }

    /// 阻塞读一条完整消息到 `out`（先清空）。
    ///
    /// 返回 `Ok(None)` 表示对端正常关闭。
    pub fn recv(&mut self, _out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        todo!("T2 实现：读满 4 字节长度、校验上限、读满 payload")
    }

    /// 非阻塞地取出已经缓冲的一条完整消息（配合 `poll` 使用）。
    pub fn try_recv(&mut self, _out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        todo!("T2 实现：从 rx 缓冲里切一条完整消息，不足返回 Ok(None)")
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
                sent_bytes: 0,
            },
            ConnReader {
                stream: reader,
                rx: Vec::with_capacity(64 * 1024),
                recv_bytes: 0,
            },
        ))
    }
}

/// 只写半边。`send` 必须整条消息原子写入（TCP 上即 len+payload）。
pub struct ConnWriter {
    stream: TcpStream,
    sent_bytes: u64,
}

impl ConnWriter {
    pub fn send(&mut self, _payload: &[u8]) -> io::Result<()> {
        todo!("T2 实现：u32 长度前缀 + payload，累计 sent_bytes")
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
    /// 阻塞读一条完整消息到 `out`（先清空）；`Ok(None)` 表示对端关闭。
    pub fn recv(&mut self, _out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        todo!("T2 实现：读满 4 字节长度 → 校验 MAX_PAYLOAD → 读满 payload")
    }

    /// 非阻塞取出已缓冲的一条完整消息；不够一整条时返回 `Ok(None)`。
    ///
    /// 内部会先 `read` 一次可用字节（非阻塞），再尝试切包。
    pub fn try_recv(&mut self, _out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        todo!("T2 实现")
    }

    pub fn recv_bytes(&self) -> u64 {
        self.recv_bytes
    }

    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }
}
