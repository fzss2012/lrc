//! M4 路径选择：**P2P 直连优先 → relay 兜底 → TCP 最终兜底**。
//!
//! 这个模块只做"拿到一条可用链路"这一件事，上层会话（握手/推流）对走哪条路无感知：
//! 返回的 LinkWriter/LinkReader 把 TCP 与可靠 UDP 收敛成同一套 send/recv。

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use lrc_transport::link::{self, LinkReader, LinkWriter};
use lrc_udp::UdpConn;

use crate::cli::P2pMode;

/// 在 relay 登记房间后等客户端来连的上限。
pub const PEER_WAIT: Duration = Duration::from_secs(60);
/// 打洞时间上限。
pub const PUNCH_TIMEOUT: Duration = Duration::from_secs(2);

/// 一条已经就绪的链路。
pub struct AcquiredLink {
    pub writer: LinkWriter,
    pub reader: LinkReader,
    /// 进 --stats 的 path= 字段（tcp / direct_udp / relay_udp）
    pub path: &'static str,
    /// UDP 路径时带上（读 rtt/loss 给 stats 与自适应控制器）
    pub udp: Option<Arc<UdpConn>>,
}

/// P2P/relay 失败后的去处。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// 回退 TCP（auto）
    Tcp,
    /// 直接报错退出（direct / relay）
    Fatal,
}

/// 这次会话要不要尝试 P2P：必须给了 --signal 且 --p2p 不是 off。
pub fn should_try_p2p(mode: P2pMode, signal: Option<SocketAddr>) -> bool {
    signal.is_some() && mode != P2pMode::Off
}

/// P2P/relay 失败后怎么办。
pub fn fallback_on_failure(mode: P2pMode) -> Fallback {
    match mode {
        // auto：降级到 TCP；off：本来就走 TCP
        P2pMode::Auto | P2pMode::Off => Fallback::Tcp,
        // direct：打洞失败就报错；relay：relay 不可用就报错（都不回退 TCP）
        P2pMode::Direct | P2pMode::Relay => Fallback::Fatal,
    }
}

/// 打洞超时：relay 模式**跳过打洞**（传 0，契约里 punch 超时即返回 Failed，establish 直接走 relay）。
pub fn punch_timeout(mode: P2pMode) -> Duration {
    match mode {
        P2pMode::Relay => Duration::ZERO,
        _ => PUNCH_TIMEOUT,
    }
}

/// 本次进程需不需要绑定 TCP 监听（direct/relay 不需要：失败就直接报错）。
pub fn need_tcp_listener(mode: P2pMode, signal: Option<SocketAddr>) -> bool {
    if !should_try_p2p(mode, signal) {
        return true;
    }
    fallback_on_failure(mode) == Fallback::Tcp
}

/// 接入失败后能否重试（能回退 TCP 的会话，失败通常只是 accept 出错，重试即可）。
pub fn can_retry(mode: P2pMode) -> bool {
    fallback_on_failure(mode) == Fallback::Tcp
}

pub struct LinkConfig<'a> {
    /// P2P 不可用时的 TCP 监听（direct/relay 模式下是 None）
    pub listener: Option<&'a TcpListener>,
    pub signal: Option<SocketAddr>,
    pub room: u32,
    pub p2p: P2pMode,
}

/// 按优先级拿一条链路：P2P(direct/relay) → TCP。
pub fn acquire_link(cfg: &LinkConfig<'_>) -> Result<AcquiredLink, String> {
    if should_try_p2p(cfg.p2p, cfg.signal) {
        if let Some(relay) = cfg.signal {
            match establish_udp(relay, cfg.room, cfg.p2p) {
                Ok(link) => return Ok(link),
                Err(e) => match fallback_on_failure(cfg.p2p) {
                    Fallback::Tcp => {
                        log::warn!("P2P/relay 不可用（{e}），回退 TCP");
                    }
                    Fallback::Fatal => {
                        return Err(format!(
                            "P2P/relay 不可用（{e}），--p2p {} 不降级",
                            cfg.p2p.label()
                        ))
                    }
                },
            }
        }
    } else if cfg.signal.is_none() {
        log::debug!("未给 --signal，走 TCP");
    } else {
        log::debug!("--p2p off，走 TCP");
    }
    accept_tcp(cfg.listener)
}

fn establish_udp(relay: SocketAddr, room: u32, p2p: P2pMode) -> Result<AcquiredLink, String> {
    log::info!(
        "P2P：向 relay {relay} 登记房间 {room}（--p2p {}，等客户端 {}s，打洞超时 {}ms）",
        p2p.label(),
        PEER_WAIT.as_secs(),
        punch_timeout(p2p).as_millis()
    );
    let established = lrc_relay::establish(
        true, // host
        relay,
        room,
        None,
        PEER_WAIT,
        punch_timeout(p2p),
    )
    .map_err(|e| format!("relay 建立失败：{e}"))?;
    let peer = established.conn_peer();
    let lrc_relay::Established { socket, mode, .. } = established;
    let path = mode.kind();
    let conn = UdpConn::new(socket, peer, mode).map_err(|e| format!("UdpConn::new 失败：{e}"))?;
    let local = conn.local_addr().map(|a| a.to_string()).unwrap_or_default();
    log::info!("P2P 链路就绪：path={path} local={local} peer={peer}");
    let udp = Arc::new(conn);
    Ok(AcquiredLink {
        writer: LinkWriter::Udp(Arc::clone(&udp)),
        reader: LinkReader::Udp(Arc::clone(&udp)),
        path,
        udp: Some(udp),
    })
}

fn accept_tcp(listener: Option<&TcpListener>) -> Result<AcquiredLink, String> {
    let Some(listener) = listener else {
        return Err("需要 TCP 兜底，但本次进程没有绑定监听套接字".to_string());
    };
    log::info!("等待 TCP 客户端连接……");
    let conn = lrc_transport::Conn::accept(listener).map_err(|e| format!("accept 失败：{e}"))?;
    let (writer, reader) = link::from_tcp(conn).map_err(|e| format!("拆分流失败：{e}"))?;
    log::info!("TCP 链路就绪：path={}", link::PATH_TCP);
    Ok(AcquiredLink {
        writer,
        reader,
        path: link::PATH_TCP,
        udp: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> Option<SocketAddr> {
        Some("127.0.0.1:7801".parse().expect("addr"))
    }

    #[test]
    fn p2p_off_never_tries_p2p() {
        // --p2p off：即使给了 --signal 也必须走 TCP
        assert!(!should_try_p2p(P2pMode::Off, sig()));
        assert!(!should_try_p2p(P2pMode::Off, None));
        assert!(need_tcp_listener(P2pMode::Off, sig()));
        assert_eq!(fallback_on_failure(P2pMode::Off), Fallback::Tcp);
    }

    #[test]
    fn no_signal_means_tcp_only() {
        for mode in [
            P2pMode::Auto,
            P2pMode::Off,
            P2pMode::Direct,
            P2pMode::Relay,
        ] {
            assert!(!should_try_p2p(mode, None), "{mode:?} 没 signal 不该走 P2P");
            assert!(need_tcp_listener(mode, None), "{mode:?} 需要 TCP 监听");
        }
    }

    #[test]
    fn auto_attempts_p2p_and_falls_back_to_tcp() {
        assert!(should_try_p2p(P2pMode::Auto, sig()));
        assert_eq!(fallback_on_failure(P2pMode::Auto), Fallback::Tcp);
        assert!(need_tcp_listener(P2pMode::Auto, sig()), "auto 必须留 TCP 兜底");
        assert!(can_retry(P2pMode::Auto));
        assert_eq!(punch_timeout(P2pMode::Auto), PUNCH_TIMEOUT);
    }

    #[test]
    fn direct_does_not_fall_back() {
        assert!(should_try_p2p(P2pMode::Direct, sig()));
        assert_eq!(fallback_on_failure(P2pMode::Direct), Fallback::Fatal);
        assert!(!need_tcp_listener(P2pMode::Direct, sig()), "direct 不需要 TCP 监听");
        assert!(!can_retry(P2pMode::Direct));
        assert_eq!(punch_timeout(P2pMode::Direct), PUNCH_TIMEOUT);
    }

    #[test]
    fn relay_skips_punching() {
        assert!(should_try_p2p(P2pMode::Relay, sig()));
        // 关键：relay 模式打洞超时为 0 → 不 punch，直接走 relay
        assert_eq!(punch_timeout(P2pMode::Relay), Duration::ZERO);
        assert_eq!(fallback_on_failure(P2pMode::Relay), Fallback::Fatal);
        assert!(!need_tcp_listener(P2pMode::Relay, sig()));
    }

    #[test]
    fn tcp_accept_without_listener_is_an_error() {
        let cfg = LinkConfig {
            listener: None,
            signal: sig(),
            room: 1,
            p2p: P2pMode::Direct,
        };
        // direct 模式没有监听：一旦 establish 失败必须报错而不是 panic。
        // 这里没有真 relay，所以只会走到 error 分支。
        let err = match acquire_link(&cfg) {
            Ok(_) => panic!("不应该成功"),
            Err(e) => e,
        };
        assert!(err.contains("P2P/relay 不可用"), "err={err}");
    }
}
