//! 主控端：窗口 + 收流 → 解码 → 呈现；采集本地输入回传。
//!
//! 用法：
//! ```text
//! lrc-client --connect 127.0.0.1:7800 --password 1234 [--title "lrc viewer"] [--stats]
//! ```

mod cli;
mod mapping;
mod net;
mod state;
mod window;

use std::process::ExitCode;

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // DPI 感知必须在创建任何窗口之前设置，否则窗口客户区坐标会被系统按 96 DPI 虚化，
    // 映射到被控端就会"点不准"。老系统上调用失败不影响功能，只记录一条日志。
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            log::warn!("SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) 失败: {e}");
        }
    }

    let cfg = match cli::Config::parse(std::env::args().skip(1)) {
        Ok(cli::Parsed::Run(cfg)) => cfg,
        Ok(cli::Parsed::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("参数错误：{e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    match window::run(&cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lrc-client 失败：{e:#}");
            ExitCode::FAILURE
        }
    }
}
