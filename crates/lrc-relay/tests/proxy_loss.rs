//! 集成测试：真跑 `udp-proxy` 二进制，验证丢包率符合设定。

use std::net::{SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct Proxy {
    child: Child,
    listen: SocketAddr,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind temp");
    let a = s.local_addr().expect("local_addr");
    drop(s);
    a
}

fn start_proxy(target: SocketAddr, loss: u32, delay_ms: u64) -> Proxy {
    let listen = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_udp-proxy"))
        .args([
            "--listen",
            &listen.to_string(),
            "--target",
            &target.to_string(),
            "--loss",
            &loss.to_string(),
            "--delay-ms",
            &delay_ms.to_string(),
            "--quiet",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn udp-proxy");
    // 等它绑好端口
    thread::sleep(Duration::from_millis(300));
    Proxy { child, listen }
}

/// 通过 proxy 打 1000 个包，返回目标端实际收到的数量。
fn measure(loss: u32) -> usize {
    let target = UdpSocket::bind("127.0.0.1:0").expect("bind target");
    target
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("rt");
    let target_addr = target.local_addr().expect("target addr");
    let proxy = start_proxy(target_addr, loss, 5);

    let recv = Arc::new(AtomicU64::new(0));
    let r2 = recv.clone();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match target.recv_from(&mut buf) {
                Ok((n, _)) if n >= 4 => {
                    r2.fetch_add(1, Ordering::Relaxed);
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    });

    let client = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    let n = 1000usize;
    for i in 0..n {
        let payload = (i as u32).to_le_bytes();
        let _ = client.send_to(&payload, proxy.listen);
        // 控制发送速率，别把 UDP 缓冲打爆
        thread::sleep(Duration::from_micros(200));
    }
    let _ = reader.join();
    drop(proxy);
    recv.load(Ordering::Relaxed) as usize
}

#[test]
fn proxy_loss_matches_configured_rate() {
    let n = 1000;
    // 0% 丢包：basline（允许极少量系统级丢包）
    let got0 = measure(0);
    println!("loss=0%   → {got0}/{n} 到达");

    // 10% 丢包：应在 10% ± 2%
    let got10 = measure(10);
    let loss_pct = (n - got10) as f64 * 100.0 / n as f64;
    println!("loss=10%  → {got10}/{n} 到达，实测丢包 {loss_pct:.1}%");
    assert!(
        (8.0..=12.0).contains(&loss_pct),
        "丢包率 {loss_pct:.1}% 不在 10%±2% 区间（到达 {got10}/{n}）"
    );

    // 0% 档也要基本不丢（否则说明是缓冲溢出而非模拟丢包）
    let loss0_pct = (n - got0) as f64 * 100.0 / n as f64;
    assert!(
        loss0_pct <= 1.0,
        "loss=0 时不该丢包，实测丢 {loss0_pct:.1}%"
    );
}
