//! 对外可见的状态。前端只读这个结构，不关心怎么采集到的。

use std::time::{Duration, SystemTime};

/// 一个可执行文件的"身份"：路径 + 大小 + 构建时间。
///
/// P0 那次的教训是"跑着的是旧 exe"肉眼看不出来，所以状态里必须带上这个，
/// 前端拿它和期望值一比就能标红。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeIdentity {
    pub path: String,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub exe: Option<ExeIdentity>,
    /// 应用层探测：能否连上并完成一次协议级交互
    pub healthy: bool,
    /// 最近一次探测耗时（毫秒）
    pub probe_ms: Option<u64>,
    /// 最近一次探测失败原因
    pub last_error: Option<String>,
    /// 已重启次数
    pub restarts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStatus {
    pub enabled: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub remote: String,
    /// 远端端口是否真的在监听（在隧道主机上探）
    pub remote_listening: bool,
    pub last_error: Option<String>,
    pub reconnects: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub host: HostStatus,
    pub tunnel: TunnelStatus,
    /// 监督进程自己的 PID
    pub supervisor_pid: u32,
    /// 已运行时长
    pub uptime: Duration,
    /// 是否允许接入（前端"连接/断开"按钮控制的就是这个）
    pub accept_clients: bool,
}
