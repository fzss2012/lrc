//! 应用层健康探测。
//!
//! 为什么不能用"端口在监听"当健康标准：yanfa2 那次事故里，17800 明明在监听，
//! 但 ssh 转发会话已经僵死，一个字节都不往下传——端口检查完全看不出问题。
//! 所以探测必须真的连上去、发一条 lrc 协议消息、等一个能识别的回应。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub ok: bool,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl ProbeResult {
    fn ok(elapsed: Duration) -> Self {
        Self {
            ok: true,
            elapsed_ms: elapsed.as_millis() as u64,
            error: None,
        }
    }

    fn fail(elapsed: Duration, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            elapsed_ms: elapsed.as_millis() as u64,
            error: Some(error.into()),
        }
    }
}

/// lrc 协议的 ClientHello：type(1) + version(2) + client_id(16) + nonce_c(16)。
///
/// 只要对端是活的 lrc-host，它一定会回 ServerHello 或 AuthResult/版本不匹配；
/// 我们能读到任意一条带长度前缀的帧，就证明"真的能通"。
fn client_hello() -> Vec<u8> {
    let payload_len: u32 = 1 + 2 + 16 + 16;
    let mut out = Vec::with_capacity(4 + payload_len as usize);
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.push(0x01); // MSG_CLIENT_HELLO
    out.extend_from_slice(&1u16.to_le_bytes()); // PROTO_VERSION
    out.extend_from_slice(&[0xAB; 16]); // client_id
    out.extend_from_slice(&[0xCD; 16]); // nonce_c
    out
}

/// 对该地址做一次协议级探测。
///
/// `timeout` 是整个探测的总预算（连接 + 收发），不是每一步各给一次。
pub fn probe_host(addr: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let Ok(sock) = addr.parse::<SocketAddr>() else {
        return ProbeResult::fail(started.elapsed(), format!("地址非法: {addr}"));
    };
    let deadline = started + timeout;

    let mut stream = match TcpStream::connect_timeout(&sock, timeout) {
        Ok(s) => s,
        Err(e) => return ProbeResult::fail(started.elapsed(), format!("连接失败: {e}")),
    };
    let _ = stream.set_nodelay(true);
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return ProbeResult::fail(started.elapsed(), "连接超时");
    }
    if let Err(e) = stream.set_read_timeout(Some(remaining)) {
        return ProbeResult::fail(started.elapsed(), format!("设置超时失败: {e}"));
    }
    if let Err(e) = stream.set_write_timeout(Some(remaining)) {
        return ProbeResult::fail(started.elapsed(), format!("设置超时失败: {e}"));
    }

    if let Err(e) = stream.write_all(&client_hello()) {
        return ProbeResult::fail(started.elapsed(), format!("发送失败: {e}"));
    }

    // 只读 4 字节长度前缀：端口通、协议对，就足够了。
    // 不读完整帧是刻意的——探测不该把 host 拖进一次真实会话的建采集器开销里。
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {
            let len = u32::from_le_bytes(header) as usize;
            if len == 0 || len > 64 * 1024 * 1024 {
                return ProbeResult::fail(
                    started.elapsed(),
                    format!("对端回了非法长度 {len}（不是 lrc-host？）"),
                );
            }
            ProbeResult::ok(started.elapsed())
        }
        Err(e) => ProbeResult::fail(started.elapsed(), format!("等待回应失败: {e}")),
    }
}

/// 纯 TCP 连通性（隧道这种"不一定是 lrc 协议"的场景用）。
pub fn probe_tcp(addr: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let Ok(sock) = addr.parse::<SocketAddr>() else {
        return ProbeResult::fail(started.elapsed(), format!("地址非法: {addr}"));
    };
    match TcpStream::connect_timeout(&sock, timeout) {
        Ok(_) => ProbeResult::ok(started.elapsed()),
        Err(e) => ProbeResult::fail(started.elapsed(), format!("连接失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn probe_succeeds_against_real_lrc_frame_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // 假 host：读完 ClientHello，回一条 4 字节长度头 + 假 ServerHello
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf);
            let payload = [0x02u8, 0x01, 0x00];
            let mut out = (payload.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(&payload);
            let _ = s.write_all(&out);
        });
        let r = probe_host(&addr, Duration::from_secs(2));
        assert!(r.ok, "应当探测成功: {r:?}");
        assert!(r.error.is_none());
        server.join().unwrap();
    }

    /// 关键回归：端口 accept 了但永远不回数据（yanfa2 那种"黑洞"）必须判定为不健康。
    #[test]
    fn probe_fails_on_black_hole_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(1200)); // 收下连接，什么都不回
            drop(s);
        });
        let started = Instant::now();
        let r = probe_host(&addr, Duration::from_millis(400));
        assert!(!r.ok, "黑洞必须判定为不健康");
        assert!(r.error.is_some());
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "探测必须按预算超时返回，不能挂住"
        );
        server.join().unwrap();
    }

    #[test]
    fn probe_fails_when_nothing_listens() {
        // 先占一个端口再释放，拿到一个几乎肯定没人监听的端口号
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let r = probe_tcp(&format!("127.0.0.1:{port}"), Duration::from_millis(300));
        assert!(!r.ok, "没人监听时必须失败");
    }
}
