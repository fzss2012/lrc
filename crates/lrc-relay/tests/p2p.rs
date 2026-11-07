//! 端到端集成测试：真起一个 relay 服务端，跑打洞 / 降级 / 房间过期。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lrc_relay::server::{Server, ServerConfig};
use lrc_relay::{establish, ClientSession, HostSession, PunchResult, RelayError, RelayMsg};

fn start_server(ttl: Duration) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let cfg = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        room_ttl: ttl,
        max_rooms: 8,
        stats_interval: None,
        verbose: false,
    };
    let mut srv = Server::bind(cfg).expect("bind");
    let addr = srv.local_addr().expect("local_addr");
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    let h = thread::spawn(move || {
        let _ = srv.run(&st);
    });
    (addr, stop, h)
}

/// 两端同时 establish，punch_timeout=0 会让打洞必然失败 → 必须降级到 relay。
#[test]
fn establish_falls_back_to_relay_when_punch_fails() {
    let (relay, stop, h) = start_server(Duration::from_secs(30));

    let host_thread = thread::spawn(move || {
        establish(
            true,
            relay,
            11,
            None,
            Duration::from_secs(3),
            Duration::from_millis(0), // 立刻超时 = 打洞必失败
        )
    });
    // 给 host 一点时间先 register
    thread::sleep(Duration::from_millis(100));
    let client = establish(
        false,
        relay,
        11,
        None,
        Duration::from_secs(3),
        Duration::from_millis(0),
    )
    .expect("client establish");

    let host = host_thread.join().expect("join").expect("host establish");

    assert!(
        matches!(client.mode, lrc_udp::PeerMode::Relay { .. }),
        "client 应降级到 relay，实际 {:?}",
        client.mode
    );
    assert!(
        matches!(host.mode, lrc_udp::PeerMode::Relay { .. }),
        "host 应降级到 relay，实际 {:?}",
        host.mode
    );
    assert!(client.direct_peer.is_none() && host.direct_peer.is_none());
    assert_eq!(client.conn_peer(), relay);
    assert_eq!(host.conn_peer(), relay);

    // 验证 relay 真的能双向转发：两端用 socket 直接发 RELAY_OUT
    let cs = client.socket;
    let hs = host.socket;
    cs.set_read_timeout(Some(Duration::from_millis(800))).expect("rt");
    hs.set_read_timeout(Some(Duration::from_millis(800))).expect("rt");
    let mut buf = [0u8; 1024];

    for (from, to, tag) in [(&cs, &hs, "c2h"), (&hs, &cs, "h2c")] {
        let pkt = RelayMsg::RelayOut {
            dst_tag: 0,
            payload: tag.as_bytes().to_vec(),
        }
        .encode();
        from.send_to(&pkt, relay).expect("send RELAY_OUT");
        let (n, _) = to.recv_from(&mut buf).expect("对端应收到转发包");
        match RelayMsg::decode(&buf[..n]).expect("decode") {
            RelayMsg::RelayIn { payload, .. } => assert_eq!(payload, tag.as_bytes()),
            other => panic!("不是 RELAY_IN: {other:?}"),
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = h.join();
}

/// 打洞成功路径：两端 punch 都返回 Direct，且能直接互发裸包。
#[test]
fn punch_success_gives_direct_path() {
    let (relay, stop, h) = start_server(Duration::from_secs(30));

    let mut host = HostSession::register(relay, 12, None).expect("register");
    let mut client = ClientSession::connect(relay, 12, None).expect("connect");

    let host_thread = thread::spawn(move || {
        let peer = host.wait_peer(Duration::from_secs(3)).expect("wait_peer");
        let peer = peer.expect("应该等到 client");
        let t0 = Instant::now();
        let r = host.punch(peer, Duration::from_millis(2000)).expect("punch");
        (r, t0.elapsed(), host.take_socket())
    });

    let t0 = Instant::now();
    let cr = client.punch(Duration::from_millis(2000)).expect("punch");
    let client_elapsed = t0.elapsed();
    let (hr, host_elapsed, hsock) = host_thread.join().expect("join");
    let csock = client.take_socket();

    let PunchResult::Direct(peer) = cr else {
        panic!("client 打洞失败");
    };
    assert!(matches!(hr, PunchResult::Direct(_)), "host 打洞失败");
    println!(
        "打洞成功：client {:.1}ms host {:.1}ms peer={peer}",
        client_elapsed.as_secs_f64() * 1000.0,
        host_elapsed.as_secs_f64() * 1000.0
    );

    // 直连互发（不再经过 relay）
    csock.set_read_timeout(Some(Duration::from_millis(500))).expect("rt");
    hsock.set_read_timeout(Some(Duration::from_millis(500))).expect("rt");
    // 用 relay 告诉我们的 host 观测地址（host 绑的是 0.0.0.0，local_addr 不能直接发）
    csock.send_to(b"direct-ping", peer).expect("send");
    let mut buf = [0u8; 64];
    let (n, _) = hsock.recv_from(&mut buf).expect("host 应收到直连包");
    assert_eq!(&buf[..n], b"direct-ping");

    stop.store(true, Ordering::Relaxed);
    let _ = h.join();
}

/// 房间过期后 CONNECT 必须拿到 ROOM_EMPTY。
#[test]
fn expired_room_reports_empty() {
    let (relay, stop, h) = start_server(Duration::from_millis(300));
    let _host = HostSession::register(relay, 13, None).expect("register");
    thread::sleep(Duration::from_millis(800));
    let r = ClientSession::connect(relay, 13, None);
    assert!(
        matches!(r, Err(RelayError::RoomEmpty)),
        "过期房间应回 ROOM_EMPTY，实际 {:?}",
        r.err()
    );
    stop.store(true, Ordering::Relaxed);
    let _ = h.join();
}
