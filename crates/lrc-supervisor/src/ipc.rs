//! 行协议 IPC：`127.0.0.1:port`，一行一条请求，一行一条响应。
//!
//! 为什么是回环 TCP 而不是命名管道：std 就能做到零依赖、跨平台、能像普通
//! socket 一样用 `connect_timeout` 判断"监督进程在不在"。命名管道要引
//! windows crate 的一堆 feature，收益不大。绑定地址固定 127.0.0.1，外网扫不到。

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use crate::MAX_REQUEST_BYTES;

/// 连接/读写的默认预算。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Status,
    StartHost,
    StopHost,
    RestartHost,
    StartTunnel,
    StopTunnel,
    /// 允许/禁止新客户端接入（前端"连接/断开"按钮）
    SetAccept(bool),
    Shutdown,
    Ping,
}

impl Command {
    pub fn parse(line: &str) -> Result<Self, String> {
        let line = line.trim();
        match line {
            "status" => Ok(Self::Status),
            "start-host" => Ok(Self::StartHost),
            "stop-host" => Ok(Self::StopHost),
            "restart-host" => Ok(Self::RestartHost),
            "start-tunnel" => Ok(Self::StartTunnel),
            "stop-tunnel" => Ok(Self::StopTunnel),
            "shutdown" => Ok(Self::Shutdown),
            "ping" => Ok(Self::Ping),
            other => match other.strip_prefix("accept ") {
                Some(v) => match v.trim() {
                    "on" | "1" | "true" => Ok(Self::SetAccept(true)),
                    "off" | "0" | "false" => Ok(Self::SetAccept(false)),
                    _ => Err(format!("accept 的取值应为 on|off，收到 {v}")),
                },
                None => Err(format!("未知命令: {other}")),
            },
        }
    }
}

/// 发送一条命令并拿到整段响应。
pub fn call(addr: SocketAddr, cmd: &str, timeout: Duration) -> Result<String, String> {
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("连不上监督进程: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("发送失败: {e}"))?;
    stream.flush().map_err(|e| format!("发送失败: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("半关闭失败: {e}"))?;

    let mut out = String::new();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .map_err(|e| format!("读取响应失败: {e}"))?
        > 0
    {
        out.push_str(&line);
        line.clear();
        if out.len() > MAX_REQUEST_BYTES * 16 {
            return Err(String::from("响应过长，疑似协议错乱"));
        }
    }
    Ok(out)
}

/// 读一行命令（服务端用）。超过上限直接报错，避免被无边界输入打爆。
pub fn read_command(reader: &mut BufReader<TcpStream>) -> Result<Option<String>, String> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| format!("读取请求失败: {e}"))?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_REQUEST_BYTES {
        return Err(String::from("请求过长"));
    }
    Ok(Some(line))
}

/// 绑定 IPC 监听。
pub fn bind(addr: SocketAddr) -> Result<TcpListener, String> {
    TcpListener::bind(addr).map_err(|e| format!("绑定 IPC {addr} 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_commands() {
        assert_eq!(Command::parse("status"), Ok(Command::Status));
        assert_eq!(Command::parse("  ping  "), Ok(Command::Ping));
        assert_eq!(Command::parse("accept on"), Ok(Command::SetAccept(true)));
        assert_eq!(Command::parse("accept off"), Ok(Command::SetAccept(false)));
        assert_eq!(Command::parse("accept 1"), Ok(Command::SetAccept(true)));
        assert!(Command::parse("accept maybe").is_err());
        assert!(Command::parse("nope").is_err());
    }

    #[test]
    fn roundtrip_over_loopback() {
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let cmd = read_command(&mut reader).unwrap().unwrap();
            let mut w = s;
            let _ = w.write_all(format!("echo:{}\n", cmd.trim()).as_bytes());
        });
        let out = call(addr, "status", Duration::from_secs(2)).unwrap();
        assert_eq!(out.trim(), "echo:status");
        server.join().unwrap();
    }

    #[test]
    fn rejects_oversized_request() {
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(s);
            read_command(&mut reader)
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let big = "x".repeat(MAX_REQUEST_BYTES + 10);
        let _ = c.write_all(big.as_bytes());
        let _ = c.write_all(b"\n");
        let res = server.join().unwrap();
        assert!(res.is_err(), "超大请求必须被拒绝");
    }
}
