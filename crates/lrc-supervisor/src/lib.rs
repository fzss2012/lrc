//! 常驻监督进程 + 控制命令行（A 档：登录后可用，不碰系统服务）。
//!
//! 为什么要有这一层：lrc-host 和反向隧道都是"会自己悄悄坏掉"的进程——
//! host 可能被换成了旧 exe，ssh 隧道断了以后远端还会留一个"能连但不转发"的
//! 僵死监听。这些问题都不是 GUI 能解决的，必须有一个常驻的东西负责看见并修好。
//! 前端（托盘/窗口/网页）因此可以做得非常薄：读状态、发命令，坏了也不影响业务。
//!
//! 分层：
//! ```text
//! lrc-supervisord（常驻，无界面）
//!   ├─ 管 lrc-host：启动/监控/异常重启/报告 exe 身份
//!   ├─ 管反向隧道：启动/监控/启动前清远端残留端口/重连
//!   └─ 127.0.0.1 上的行协议 IPC
//!             ↑
//!      lrc-ctl（命令行）      前端（可选，随便换）
//! ```
//!
//! 刻意不引入 serde/tokio：协议就是一行一条 `key=value`，手写解析 30 行，
//! 和仓库其它部分（手写 CLI、手写 Win32 窗口）的取舍保持一致。

pub mod autostart;
pub mod config;
pub mod config_file;
pub mod daemon;
pub mod exe;
pub mod ipc;
pub mod probe;
pub mod status;

pub use config::Config;
pub use daemon::Daemon;
pub use status::{HostStatus, Status, TunnelStatus};

/// 一次 IPC 请求的最大字节数（一行命令而已，给足余量）。
pub const MAX_REQUEST_BYTES: usize = 4096;
