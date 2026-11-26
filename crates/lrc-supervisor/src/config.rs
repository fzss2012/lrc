//! 监督进程的配置：纯数据 + 手写解析，方便单测，也方便以后接前端。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// 默认 IPC 地址：只绑回环，外部扫不到。
pub const DEFAULT_IPC_PORT: u16 = 17899;
/// 默认 host 监听端口。
pub const DEFAULT_HOST_PORT: u16 = 7805;
/// 默认反向隧道远端端口。
pub const DEFAULT_REMOTE_PORT: u16 = 17800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// lrc-host 可执行文件
    pub host_exe: PathBuf,
    /// host 监听地址
    pub listen: String,
    /// 共享密码
    pub password: String,
    /// 采集的显示输出序号
    pub display: usize,
    /// 是否常驻（false = 只启动 host，不守）
    pub supervise_host: bool,
    /// 隧道配置；None = 不启用隧道
    pub tunnel: Option<TunnelConfig>,
    /// IPC 监听端口（固定 127.0.0.1）
    pub ipc_port: u16,
    /// 健康探测间隔
    pub probe_interval: Duration,
    /// 子进程异常退出后的重启退避起点
    pub restart_backoff: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelConfig {
    /// SSH 配置里的主机名（例如 yanfa2）
    pub ssh_host: String,
    /// 远端监听端口
    pub remote_port: u16,
    /// 本地被转发端口
    pub local_port: u16,
    /// 可选：ssh -F 指定的配置文件
    pub ssh_config: Option<PathBuf>,
}

impl Config {
    pub fn ipc_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.ipc_port)
    }

    /// 应用层健康探测的目标：host 自己的监听端口。
    pub fn host_addr(&self) -> String {
        // listen 形如 127.0.0.1:7805 或 0.0.0.0:7805；探测时一律连回环。
        match self.listen.rsplit_once(':') {
            Some((_, port)) => format!("127.0.0.1:{port}"),
            None => format!("127.0.0.1:{}", DEFAULT_HOST_PORT),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host_exe: PathBuf::from("lrc-host.exe"),
            listen: format!("127.0.0.1:{DEFAULT_HOST_PORT}"),
            password: String::new(),
            display: 0,
            supervise_host: true,
            tunnel: None,
            ipc_port: DEFAULT_IPC_PORT,
            probe_interval: Duration::from_secs(2),
            restart_backoff: Duration::from_secs(2),
        }
    }
}
