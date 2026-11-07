//! lrc-relay 服务端：房间登记 + 地址交换 + UDP 中继。
//!
//! 用法：
//! ```text
//! lrc-relay --udp 0.0.0.0:7801 [--room-ttl 60] [--max-rooms 64] [--stats]
//! ```
//!
//! 协议见 lib.rs 顶部表格。要点：
//! - 单线程 `recv_from` 循环即可（自用场景最多 1–2 个房间）
//! - 房间状态：`room -> { host_addr, host_tag, client_addr, client_tag, last_seen, relayed_bytes }`
//! - 收到 `RELAY_OUT{dst_tag}` → 查 tag 找到对端地址 → 回发 `RELAT_IN{src_tag, payload}`
//! - `--stats`：每 5 秒打一行 `rooms=… reachable=… relayed_bytes=… relayed_pkts=…`

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use lrc_relay::server::{Server, ServerConfig};

const HELP: &str = "\
lrc-relay —— P2P 打洞的信令/中继服务端

用法:
  lrc-relay [--udp <addr>] [--room-ttl <sec>] [--max-rooms <n>] [--stats] [--verbose]

选项:
  --udp <addr>        监听地址（默认 0.0.0.0:7801）
  --room-ttl <sec>    房间空闲多久过期（默认 60 秒）
  --max-rooms <n>     最多房间数（默认 64）
  --stats             每 5 秒打印一行统计
  --verbose           每个信令包打一行日志
  -h, --help          显示本帮助
";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut cfg = ServerConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--udp" => match args.next().and_then(|v| v.parse::<SocketAddr>().ok()) {
                Some(v) => cfg.bind = v,
                None => {
                    eprintln!("lrc-relay: --udp 需要合法地址，例如 0.0.0.0:7801");
                    return ExitCode::from(2);
                }
            },
            "--room-ttl" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => cfg.room_ttl = Duration::from_secs(v),
                None => {
                    eprintln!("lrc-relay: --room-ttl 需要秒数");
                    return ExitCode::from(2);
                }
            },
            "--max-rooms" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) => cfg.max_rooms = v.max(1),
                None => {
                    eprintln!("lrc-relay: --max-rooms 需要整数");
                    return ExitCode::from(2);
                }
            },
            "--stats" => cfg.stats_interval = Some(Duration::from_secs(5)),
            "--verbose" => cfg.verbose = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("lrc-relay: 未知参数 {other}（--help 看用法）");
                return ExitCode::from(2);
            }
        }
    }

    let mut srv = match Server::bind(cfg.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lrc-relay: 绑定 {} 失败: {e}", cfg.bind);
            return ExitCode::from(1);
        }
    };
    let local = srv.local_addr().unwrap_or(cfg.bind);
    println!("lrc-relay 监听 {local}（room_ttl={:?}）", cfg.room_ttl);

    // 服务端不主动退出：Ctrl-C 结束进程即可
    let stop = AtomicBool::new(false);
    match srv.run(&stop) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lrc-relay: 跑挂了: {e}");
            ExitCode::from(1)
        }
    }
}
