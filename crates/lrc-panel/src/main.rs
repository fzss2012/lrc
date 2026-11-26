//! lrc 控制面板（产品化单 exe）。
//!
//! 双击即用：进程内直接跑监督器（不再需要单独启动 lrc-supervisord），
//! 配置存在 %APPDATA%\lrc\config.txt，关窗缩到托盘，托盘菜单可显示配置/退出。
//!
//! 命令行只保留几个排障开关：
//!   lrc-panel.exe            正常运行
//!   lrc-panel.exe --config   打开配置文件所在的文件夹
//!   lrc-panel.exe --no-tray  关窗即退出（调试用）
//!   lrc-panel.exe --reset-config  删除配置，下次运行重建默认模板

mod ui;

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use lrc_supervisor::{config_file, Daemon};
use windows::core::PCWSTR;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.iter().any(|a| a == "--reset-config") {
        let p = config_file::config_path();
        if p.exists() {
            if let Err(e) = std::fs::remove_file(&p) {
                eprintln!("删除配置失败: {e}");
                return ExitCode::FAILURE;
            }
        }
        println!("已删除配置 {}，下次运行会重建默认模板", p.display());
        return ExitCode::SUCCESS;
    }
    if argv.iter().any(|a| a == "--config") {
        let dir = config_file::config_dir();
        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        println!("配置目录: {}", dir.display());
        return ExitCode::SUCCESS;
    }
    let no_tray = argv.iter().any(|a| a == "--no-tray");

    // 0) 单实例：已经有一个面板在跑时，激活它的窗口然后退出，
    //    绝不启动第二套监督器（两个监督器会互相抢 IPC 端口/子元素）。
    let mutex_name = windows::core::w!("lrc-panel-single-instance");
    unsafe {
        let _mutex = match CreateMutexW(None, true, mutex_name) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("创建单实例互斥失败：{e}");
                return ExitCode::FAILURE;
            }
        };
        if GetLastError().0 == 183 {
            // ERROR_ALREADY_EXISTS：已有实例。把它的窗口叫到前台。
            let cls = windows::core::w!("lrc_panel_window_v3");
            let title = windows::core::w!("远程控制服务");
            if let Ok(hwnd) = FindWindowW(PCWSTR::from_raw(cls.as_ptr()), PCWSTR::from_raw(title.as_ptr())) {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            println!("远程控制服务已在运行");
            // 注意不要 CloseHandle：进程退出时系统自动回收；提前释放反而会让互斥失效。
            return ExitCode::SUCCESS;
        }
        // HANDLE 是 Copy 类型，不需要 forget；只要不 CloseHandle，
        // 互斥量会一直持有到本进程退出（系统自动回收）。
    }

    // 1) 读配置（第一次自动生成默认模板）
    let cfg = match config_file::load_or_create() {
        Ok(c) => c,
        Err(e) => {
            // 配置有问题时给出清楚的错误 + 配置文件路径，而不是直接崩
            let p = config_file::config_path();
            eprintln!("配置有误：{e}");
            eprintln!("请编辑：{}", p.display());
            let _ = std::process::Command::new("notepad").arg(&p).spawn();
            return ExitCode::FAILURE;
        }
    };
    let ipc_addr = cfg.ipc_addr();

    // 2) 启动监督器：优先连已有实例；连不上就在本进程里起一个
    if lrc_supervisor::ipc::call(ipc_addr, "ping", Duration::from_millis(600)).is_err() {
        // 先试着看清是不是别的进程占着 IPC 端口
        let mut daemon = Daemon::new(cfg);
        thread::Builder::new()
            .name("lrc-supervisor".to_string())
            .spawn(move || {
                if let Err(e) = daemon.run() {
                    log::error!("监督器退出：{e}");
                }
            })
            .expect("启动监督线程失败");
        // 等 IPC 就绪
        for _ in 0..50 {
            if lrc_supervisor::ipc::call(ipc_addr, "ping", Duration::from_millis(200)).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    // 3) 跑窗口
    match ui::run(ipc_addr, config_file::config_path(), !no_tray) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lrc-panel 失败：{e}");
            ExitCode::FAILURE
        }
    }
}
