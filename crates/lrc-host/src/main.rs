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

/// 绑定监听并逐条服务连接（永不返回，除非绑定/监听彻底失败）。
fn serve(args: &Args) -> Result<(), String> {
    let listener =
        TcpListener::bind(args.listen.as_str()).map_err(|e| format!("绑定 {} 失败：{e}", args.listen))?;
    let local = match listener.local_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => args.listen.clone(),
    };
    log::info!(
        "lrc-host 监听 {local}（display={} fps={} quality={} stats={}）",
        args.display,
        args.fps,
        args.quality,
        args.stats
    );
    log::info!("等待客户端连接……");

    let mut nonce_seq: u64 = 0;
    let mut accepted: u64 = 0;
    loop {
        let conn = match lrc_transport::Conn::accept(&listener) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("accept 失败：{e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        accepted += 1;
        nonce_seq = nonce_seq.wrapping_add(1);
        log::info!("第 {accepted} 条连接接入");

        if let Err(e) = session::run(conn, args, nonce_seq) {
            log::error!("会话异常结束：{e}");
        }
        log::info!("第 {accepted} 条连接结束，继续监听");
    }
}
