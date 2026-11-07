//! 被控端：采集 → 差分 → 编码 → 发送；回放输入。
//!
//! 用法（手写 CLI，见 cli.rs）：
//! lrc-host --listen 0.0.0.0:7800 --password 1234 [--display 0] [--fps 30] [--quality 85] [--stats]
//!
//! 线程模型（std 线程，不用 tokio）：
//! - 主线程：TcpListener::accept → 握手鉴权 → Conn::split() → 采集/差分/编码/发送循环
//! - 接收线程：阻塞读 Input / VideoAck / RequestFull / Ping（Ping 走共享写端回 Pong）
//!
//! 单流点对点：同一时刻只服务一个观看者，断开后再接受下一条连接。

mod audio;
mod auth;
mod cli;
mod input;
mod path;
mod session;
mod stats;
mod video;

use std::net::TcpListener;
use std::process::ExitCode;
use std::time::Duration;

use crate::cli::{Args, Cli};

fn main() -> ExitCode {
    // 必须最先做：之后所有坐标都当物理像素处理，否则 GetCursorPos / SendInput 会被缩放
    enable_dpi_awareness();
    init_logger();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env_password = std::env::var("LRC_PASSWORD").ok();
    let args = match cli::parse_with(&argv, env_password.as_deref()) {
        Ok(Cli::Run(a)) => a,
        Ok(Cli::Help) => {
            println!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("lrc-host: {e}");
            eprintln!();
            eprintln!("{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    match serve(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lrc-host: {e}");
            ExitCode::from(1)
        }
    }
}

/// PER_MONITOR_AWARE_V2；失败（例如宿主进程已经设置过）就忽略。
///
/// 采集和光标坐标都要求物理像素：不设的话 GetCursorInfo 拿到的是 DPI 虚拟化坐标，
/// 多屏时 CursorPos 会错位。统一走 lrc-capture 的实现，保证全进程只设一次同一口径。
fn enable_dpi_awareness() {
    lrc_capture::set_dpi_aware();
}

fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
}

/// 接入链路并逐条服务会话（永不返回，除非配置要求失败即退出）。
///
/// 路径优先级：P2P 直连 → relay → TCP（具体取舍见 crate::path）。
fn serve(args: &Args) -> Result<(), String> {
    // direct/relay 模式失败即报错，不需要 TCP 监听；auto/off 需要 TCP 兜底
    let need_tcp = path::need_tcp_listener(args.p2p, args.signal);
    let listener = if need_tcp {
        Some(
            TcpListener::bind(args.listen.as_str())
                .map_err(|e| format!("绑定 {} 失败：{e}", args.listen))?,
        )
    } else {
        None
    };
    if let Some(l) = listener.as_ref() {
        let local = match l.local_addr() {
            Ok(addr) => addr.to_string(),
            Err(_) => args.listen.clone(),
        };
        log::info!(
            "lrc-host 监听 TCP {local}（display={} fps={} quality={} stats={}，P2P 失败时的兜底）",
            args.display,
            args.fps,
            args.quality,
            args.stats
        );
    } else {
        log::info!(
            "lrc-host 不监听 TCP（--p2p {}）display={} fps={} quality={} stats={}",
            args.p2p.label(),
            args.display,
            args.fps,
            args.quality,
            args.stats
        );
    }
    match (args.signal, args.p2p) {
        (Some(sig), mode) if mode != cli::P2pMode::Off => {
            log::info!("P2P 打开：信令 {sig}，房间 {}，模式 {}", args.room, mode.label())
        }
        (Some(sig), _) => log::info!("给了 --signal {sig} 但 --p2p off：只走 TCP"),
        (None, _) => log::info!("未配置 --signal：只走 TCP"),
    }

    let mut nonce_seq: u64 = 0;
    let mut accepted: u64 = 0;
    loop {
        let link = match path::acquire_link(&path::LinkConfig {
            listener: listener.as_ref(),
            signal: args.signal,
            room: args.room,
            p2p: args.p2p,
        }) {
            Ok(l) => l,
            Err(e) => {
                if path::can_retry(args.p2p) && listener.is_some() {
                    log::warn!("接入失败（{e}），200ms 后重试");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                return Err(e);
            }
        };
        accepted += 1;
        nonce_seq = nonce_seq.wrapping_add(1);
        let path_name = link.path;
        log::info!("第 {accepted} 条链路接入：path={path_name}");

        if let Err(e) = session::run(link, args, nonce_seq) {
            log::error!("会话异常结束：{e}");
        }
        log::info!("第 {accepted} 条链路结束（path={path_name}），继续接入");
    }
}
