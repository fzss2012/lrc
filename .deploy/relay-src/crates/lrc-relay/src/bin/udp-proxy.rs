//! UDP 有损链路模拟器：夹在两个 UDP 端点之间，按概率丢包 / 加延迟 / 乱序。
//!
//! 用途：验收 `lrc-udp` 的重传与有序交付（本机一条命令就能造弱网）。
//!
//! 用法：
//! ```text
//! udp-proxy --listen 127.0.0.1:7900 --target 127.0.0.1:7801 --loss 5 --delay-ms 20 --reorder 2
//! ```
//! 语义：本工具在 listen 上收包（来自一端），转发给 target（另一端）；
//! target 回包再转发回最近一次发来数据的客户端。**只适合单会话调试**。
//!
//! 统计：每秒一行 `forwarded= dropped= loss=…%`；退出（--duration-sec / --count 触发）时再打一行汇总。
//!
//! 由 windows-gfx 实现。

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const HELP: &str = "\
udp-proxy —— UDP 有损链路模拟器（单会话）

用法:
  udp-proxy --listen <addr> --target <addr> [--loss <pct>] [--delay-ms <ms>] [--reorder <pct>]
            [--duration-sec <n>] [--count <n>] [--quiet]

选项:
  --listen <addr>     监听地址（默认 127.0.0.1:7900）
  --target <addr>     转发目标（默认 127.0.0.1:7801）
  --loss <pct>        丢包率百分比（0–100，默认 0）
  --delay-ms <ms>     固定延迟毫秒（默认 0）
  --reorder <pct>     轻微乱序概率（0–100，默认 0）
  --duration-sec <n>  跑 n 秒后退出并打印汇总
  --count <n>         转发 n 个包后退出并打印汇总
  --quiet             不打每秒统计行
  -h, --help          显示本帮助
";

/// xorshift32：确定性、够快，不需要额外依赖。
struct Rng(u32);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// 返回 [0,100) 的百分比
    fn pct(&mut self) -> f64 {
        (self.next_u32() % 10_000) as f64 / 100.0
    }
}

struct Pending {
    at: Instant,
    data: Vec<u8>,
    to: SocketAddr,
}

fn main() -> ExitCode {
    let mut listen: SocketAddr = "127.0.0.1:7900".parse().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7900)));
    let mut target: SocketAddr = "127.0.0.1:7801".parse().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7801)));
    let mut loss = 0.0f64;
    let mut delay = Duration::ZERO;
    let mut reorder = 0.0f64;
    let mut duration: Option<Duration> = None;
    let mut count_target: Option<u64> = None;
    let mut quiet = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => listen = v,
                None => {
                    eprintln!("udp-proxy: --listen 需要地址");
                    return ExitCode::from(2);
                }
            },
            "--target" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => target = v,
                None => {
                    eprintln!("udp-proxy: --target 需要地址");
                    return ExitCode::from(2);
                }
            },
            "--loss" => match args.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) => loss = v.clamp(0.0, 100.0),
                None => {
                    eprintln!("udp-proxy: --loss 需要百分比");
                    return ExitCode::from(2);
                }
            },
            "--delay-ms" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => delay = Duration::from_millis(v),
                None => {
                    eprintln!("udp-proxy: --delay-ms 需要毫秒数");
                    return ExitCode::from(2);
                }
            },
            "--reorder" => match args.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) => reorder = v.clamp(0.0, 100.0),
                None => {
                    eprintln!("udp-proxy: --reorder 需要百分比");
                    return ExitCode::from(2);
                }
            },
            "--duration-sec" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => duration = Some(Duration::from_secs(v)),
                None => {
                    eprintln!("udp-proxy: --duration-sec 需要秒数");
                    return ExitCode::from(2);
                }
            },
            "--count" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => count_target = Some(v),
                None => {
                    eprintln!("udp-proxy: --count 需要整数");
                    return ExitCode::from(2);
                }
            },
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("udp-proxy: 未知参数 {other}（--help 看用法）");
                return ExitCode::from(2);
            }
        }
    }

    let sock = match UdpSocket::bind(listen) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("udp-proxy: 绑定 {listen} 失败: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = sock.set_read_timeout(Some(Duration::from_millis(2))) {
        eprintln!("udp-proxy: set_read_timeout 失败: {e}");
        return ExitCode::from(1);
    }
    println!(
        "udp-proxy 监听 {listen} → {target}  loss={loss}% delay={}ms reorder={reorder}%",
        delay.as_millis()
    );

    let mut rng = Rng(0x9E37_79B9);
    let mut queue: VecDeque<Pending> = VecDeque::new();
    let mut buf = vec![0u8; 65536];
    let mut last_client: Option<SocketAddr> = None;
    let mut forwarded: u64 = 0;
    let mut dropped: u64 = 0;
    let mut decided: u64 = 0;
    let start = Instant::now();
    let mut last_report = Instant::now();

    loop {
        if let Some(d) = duration {
            if start.elapsed() >= d {
                break;
            }
        }
        if let Some(c) = count_target {
            if forwarded >= c {
                break;
            }
        }

        // 1) 收包 → 判定丢包 → 入延迟队列
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let to = if from == target {
                    last_client
                } else {
                    last_client = Some(from);
                    Some(target)
                };
                decided += 1;
                let drop = rng.pct() < loss;
                if drop {
                    dropped += 1;
                } else if let Some(to) = to {
                    let at = Instant::now() + delay;
                    if reorder > 0.0 && rng.pct() < reorder && !queue.is_empty() {
                        // 轻微乱序：插到队列中随机靠前的位置（可能抢在别的包前面发出）
                        let len = queue.len();
                        let pos = (rng.next_u32() as usize) % len;
                        queue.insert(pos, Pending {
                            at,
                            data: buf[..n].to_vec(),
                            to,
                        });
                    } else {
                        queue.push_back(Pending {
                            at,
                            data: buf[..n].to_vec(),
                            to,
                        });
                    }
                }
            }
            // Windows 上 UDP 收到 ICMP 端口不可达会在 recv 返回 WSAECONNRESET(10054)，
            // 对代理来说只是"这一段没包"，忽略继续转发即可。
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionAborted
                ) => {}
            Err(e) => {
                eprintln!("udp-proxy: recv_from 失败: {e}");
                break;
            }
        }

        // 2) 发送到期包
        let now = Instant::now();
        while queue.front().map(|p| p.at <= now).unwrap_or(false) {
            if let Some(p) = queue.pop_front() {
                match sock.send_to(&p.data, p.to) {
                    Ok(_) => forwarded += 1,
                    Err(e) => eprintln!("udp-proxy: 转发给 {} 失败: {e}", p.to),
                }
            }
        }

        // 3) 每秒统计
        if !quiet && last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            report(forwarded, dropped, decided);
        }
    }

    // 把还在队列里的包发完（最多等一个延迟周期）
    let drain_deadline = Instant::now() + delay + Duration::from_millis(50);
    while Instant::now() < drain_deadline {
        let now = Instant::now();
        while queue.front().map(|p| p.at <= now).unwrap_or(false) {
            if let Some(p) = queue.pop_front() {
                if sock.send_to(&p.data, p.to).is_ok() {
                    forwarded += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    report(forwarded, dropped, decided);
    ExitCode::SUCCESS
}

fn report(forwarded: u64, dropped: u64, decided: u64) {
    let pct = if decided > 0 {
        dropped as f64 * 100.0 / decided as f64
    } else {
        0.0
    };
    println!("forwarded={forwarded} dropped={dropped} 丢包实测 {pct:.1}%");
}
