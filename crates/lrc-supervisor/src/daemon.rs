//! 监督主循环：拉起/看守 host 与隧道，处理 IPC 命令。
//!
//! 单线程事件循环 + 非阻塞 accept。命令都很短，不值得为它引异步运行时——
//! 和仓库其它部分（std 线程、手写轮询）保持同一套取舍。

use std::io::{BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, TunnelConfig};
use crate::exe::{freshness, identify, Freshness};
use crate::ipc::{self, Command as IpcCommand};
use crate::probe;
use crate::status::{HostStatus, Status, TunnelStatus};

/// 主循环单次睡眠：决定命令响应延迟。
const TICK: Duration = Duration::from_millis(100);
/// 健康探测总预算。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
/// 重启退避上限。
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// 准备重启前的等待，避免"启动即失败"死循环打满 CPU。
const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// 一个被监督的子进程。
struct Slot {
    child: Option<Child>,
    pid: Option<u32>,
    /// 实际在跑的那个二进制的身份（前端拿它和磁盘上的比，识别"跑着旧 exe"）
    exe: Option<crate::status::ExeIdentity>,
    /// 端口上已经有别人起的实例，我们只是接管观测
    adopted: bool,
    healthy: bool,
    /// 正在为某个客户端服务（host 是单流的：有人连上时探测必然失败，
    /// 那不是故障，是它在干活）。
    busy: bool,
    probe_ms: Option<u64>,
    last_error: Option<String>,
    restarts: u32,
    /// 下次允许启动的时刻（退避）
    next_start: Option<Instant>,
    backoff: Duration,
}

impl Slot {
    fn new() -> Self {
        Self {
            child: None,
            pid: None,
            exe: None,
            adopted: false,
            healthy: false,
            busy: false,
            probe_ms: None,
            last_error: None,
            restarts: 0,
            next_start: None,
            backoff: MIN_BACKOFF,
        }
    }

    /// 子进程还活着吗？顺带把退出码记进 last_error。
    fn poll_exit(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                self.pid = None;
                self.exe = None;
                self.healthy = false;
                self.busy = false;
                let code = status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| String::from("signal"));
                self.last_error = Some(format!("进程退出（code={code}）"));
                self.restarts = self.restarts.saturating_add(1);
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                self.next_start = Some(Instant::now() + self.backoff);
            }
            Ok(None) => {}
            Err(e) => {
                self.last_error = Some(format!("等待子进程失败: {e}"));
            }
        }
    }

    fn note_started(&mut self, child: Child) {
        self.pid = child.id().into();
        self.child = Some(child);
        self.healthy = false;
        self.busy = false;
        self.adopted = false;
        self.next_start = None;
    }

    fn reset_backoff(&mut self) {
        self.backoff = MIN_BACKOFF;
    }
}

pub struct Daemon {
    cfg: Config,
    started: Instant,
    accept_clients: bool,
    host: Slot,
    tunnel: Slot,
    /// 期望的 host exe 身份（磁盘上的），用于前端比对
    expected_exe: Option<crate::status::ExeIdentity>,
    shutting_down: bool,
}

impl Daemon {
    pub fn new(cfg: Config) -> Self {
        let expected_exe = identify(&cfg.host_exe);
        Self {
            cfg,
            started: Instant::now(),
            accept_clients: true,
            host: Slot::new(),
            tunnel: Slot::new(),
            expected_exe,
            shutting_down: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// 跑起来就不返回（除非收到 shutdown）。
    pub fn run(&mut self) -> Result<(), String> {
        let listener = ipc::bind(self.cfg.ipc_addr())?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("IPC 监听设为非阻塞失败: {e}"))?;
        log::info!("监督进程就绪：IPC={}", self.cfg.ipc_addr());
        log::info!(
            "host={} listen={} display={} 隧道={}",
            self.cfg.host_exe.display(),
            self.cfg.listen,
            self.cfg.display,
            match self.cfg.tunnel.as_ref() {
                Some(t) => format!("{}:{}<->{}", t.ssh_host, t.remote_port, t.local_port),
                None => String::from("关闭"),
            }
        );

        // 启动阶段：先看一眼端口上是不是已经有实例在跑
        self.startup_host();
        if self.cfg.tunnel.is_some() {
            self.start_tunnel();
        }

        let mut last_probe = Instant::now();
        while !self.shutting_down {
            self.handle_ipc(&listener);
            self.host.poll_exit();
            self.tunnel.poll_exit();

            if last_probe.elapsed() >= self.cfg.probe_interval {
                last_probe = Instant::now();
                self.probe_all();
            }
            self.supervise();

            std::thread::sleep(TICK);
        }
        self.shutdown_children();
        log::info!("监督进程退出");
        Ok(())
    }

    /// 启动阶段：端口上已有实例就接管观测，不重复拉起。
    fn startup_host(&mut self) {
        let addr = self.cfg.host_addr();
        let r = probe::probe_host(&addr, PROBE_TIMEOUT);
        if r.ok {
            log::warn!("{addr} 上已有 lrc-host 在跑（非本进程拉起），接管观测；不会重复启动");
            self.host.adopted = true;
            self.host.healthy = true;
            self.host.probe_ms = Some(r.elapsed_ms);
            self.host.pid = find_pid_on_port(&addr);
            // 外部实例的 exe 路径尽量查出来；查不到就留空，前端显示为 unknown
            self.host.exe = process_image_path(self.host.pid).and_then(|p| identify(std::path::Path::new(&p)));
            return;
        }
        self.spawn_host();
    }

    fn spawn_host(&mut self) {
        if let Some(at) = self.host.next_start {
            if Instant::now() < at {
                return;
            }
        }
        let mut cmd = Command::new(&self.cfg.host_exe);
        cmd.arg("--listen")
            .arg(&self.cfg.listen)
            .arg("--password")
            .arg(&self.cfg.password)
            .arg("--display")
            .arg(self.cfg.display.to_string())
            .arg("--stats")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let spawned_for = self.cfg.host_exe.clone();
        match cmd.spawn() {
            Ok(child) => {
                log::info!("已启动 lrc-host（pid={}）", child.id());
                self.host.exe = identify(&spawned_for);
                self.host.note_started(child);
                self.host.reset_backoff();
            }
            Err(e) => {
                log::error!("启动 lrc-host 失败: {e}");
                self.host.last_error = Some(format!("启动失败: {e}"));
                self.host.next_start = Some(Instant::now() + self.host.backoff);
                self.host.backoff = (self.host.backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    fn start_tunnel(&mut self) {
        let Some(t) = self.cfg.tunnel.clone() else {
            return;
        };
        if let Some(at) = self.tunnel.next_start {
            if Instant::now() < at {
                return;
            }
        }
        // 关键：清掉远端可能残留的"能连但不转发"的僵死监听。
        // 不清的话 ExitOnForwardFailure=yes 会让 ssh 每轮都绑不上、永远起不来。
        if let Err(e) = clear_remote_stale(&t) {
            log::warn!("清理远端残留监听失败（不阻塞启动）: {e}");
        }
        let child = spawn_tunnel(&t);
        match child {
            Ok(child) => {
                log::info!(
                    "已启动隧道 {}:{} <-> 127.0.0.1:{}（pid={}）",
                    t.ssh_host,
                    t.remote_port,
                    t.local_port,
                    child.id()
                );
                self.tunnel.note_started(child);
                self.tunnel.reset_backoff();
            }
            Err(e) => {
                log::error!("启动隧道失败: {e}");
                self.tunnel.last_error = Some(format!("启动失败: {e}"));
                self.tunnel.next_start = Some(Instant::now() + self.tunnel.backoff);
                self.tunnel.backoff = (self.tunnel.backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    /// 健康探测 + 该死就让它死。
    fn probe_all(&mut self) {
        // 每次探测都重读磁盘身份：用户重建了 exe 时，前端才能看到 stale。
        self.expected_exe = identify(&self.cfg.host_exe);
        if self.host.child.is_some() || self.host.adopted {
            let addr = self.cfg.host_addr();
            let r = probe::probe_host(&addr, PROBE_TIMEOUT);
            self.host.probe_ms = Some(r.elapsed_ms);
            if r.ok {
                self.host.healthy = true;
                self.host.busy = false;
                self.host.last_error = None;
            } else {
                // host 是单流的：一个客户端占着连接时，探测连不进来、
                // 或者连进来也拿不到握手回应——这是"正在服务"，不是"坏了"。
                //
                // 之前没区分这两种情况，探测一失败就当成 host 挂了并重启，
                // 等于把用户正在用的远程会话直接掐断（实测复现）。
                let serving = self.host_alive() && port_has_established(&addr);
                self.host.busy = serving;
                self.host.healthy = serving;
                self.host.last_error = if serving {
                    None
                } else {
                    r.error.clone()
                };
                if !serving {
                    log::warn!("host 探测失败: {:?}", r.error);
                }
            }
        }
        // 隧道：本地端口通只说明 ssh 客户端还活着；真正的判据是**远端在监听**。
        // yanfa2 那次事故里本地一切正常、远端端口被僵死会话占着不转发，
        // 只看本地会把黑洞当健康。所以这里去远端查一次（频率低于本地探测）。
        if let Some(t) = self.cfg.tunnel.clone() {
            let local = format!("127.0.0.1:{}", t.local_port);
            let local_ok = probe::probe_tcp(&local, PROBE_TIMEOUT);
            if !local_ok.ok {
                self.tunnel.healthy = false;
                self.tunnel.probe_ms = Some(local_ok.elapsed_ms);
                self.tunnel.last_error = local_ok.error.clone();
            } else {
                let started = Instant::now();
                let remote_ok = remote_port_listening(&t);
                self.tunnel.probe_ms = Some(started.elapsed().as_millis() as u64);
                self.tunnel.healthy = remote_ok;
                self.tunnel.last_error = if remote_ok {
                    None
                } else {
                    Some(format!("远端 {}:{} 未监听", t.ssh_host, t.remote_port))
                };
            }
        }
    }

    /// 监督：死了就按退避重启（暂停状态下完全不拉起）。
    fn supervise(&mut self) {
        if !self.accept_clients {
            return;
        }
        // host
        if self.cfg.supervise_host && !self.host.adopted && self.host.child.is_none() {
            let due = self.host.next_start.map(|t| Instant::now() >= t).unwrap_or(true);
            if due {
                log::info!("host 不在运行，尝试拉起");
                self.spawn_host();
            }
        }
        // host 被接管后进程可能被别人杀掉。
        // 判据必须是"进程真的没了"——不能看探测结果：它在服务客户端时
        // 探测一定失败，那时候重拉等于把用户的会话踢掉。
        if self.host.adopted && !self.host_alive() {
            log::warn!("接管的 host 进程已不存在，转为自行拉起");
            self.host.adopted = false;
            self.host.pid = None;
            self.host.healthy = false;
            self.host.busy = false;
            self.host.next_start = Some(Instant::now());
        }
        // tunnel
        if self.cfg.tunnel.is_some() && self.tunnel.child.is_none() {
            let due = self.tunnel.next_start.map(|t| Instant::now() >= t).unwrap_or(true);
            if due {
                self.start_tunnel();
            }
        }
    }

    fn stop_host(&mut self) {
        if let Some(mut child) = self.host.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            log::info!("已停止 lrc-host");
        }
        if let Some(pid) = self.host.pid.take() {
            kill_pid(pid);
        }
        self.host.adopted = false;
        self.host.healthy = false;
        self.host.pid = None;
        self.host.next_start = None;
    }

    fn stop_tunnel(&mut self) {
        if let Some(mut child) = self.tunnel.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            log::info!("已停止隧道");
        }
        self.tunnel.healthy = false;
        self.tunnel.pid = None;
        self.tunnel.next_start = None;
    }

    fn shutdown_children(&mut self) {
        self.stop_host();
        self.stop_tunnel();
    }

    fn handle_ipc(&mut self, listener: &TcpListener) {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(e) => {
                log::warn!("IPC accept 失败: {e}");
                return;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("IPC 复制句柄失败: {e}");
                return;
            }
        });
        let line = match ipc::read_command(&mut reader) {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(e) => {
                log::warn!("IPC 读命令失败: {e}");
                return;
            }
        };
        let mut out = stream;
        let reply = match IpcCommand::parse(&line) {
            Ok(cmd) => self.dispatch(cmd),
            Err(e) => format!("error={e}\n"),
        };
        let _ = out.write_all(reply.as_bytes());
        let _ = out.flush();
    }

    fn dispatch(&mut self, cmd: IpcCommand) -> String {
        match cmd {
            IpcCommand::Ping => String::from("ok=pong\n"),
            IpcCommand::Status => self.render_status(),
            IpcCommand::StartHost => {
                if self.host.child.is_none() && !self.host.adopted {
                    self.host.next_start = Some(Instant::now());
                    self.spawn_host();
                }
                self.render_status()
            }
            IpcCommand::StopHost => {
                self.stop_host();
                self.render_status()
            }
            IpcCommand::RestartHost => {
                self.stop_host();
                self.host.next_start = Some(Instant::now());
                self.spawn_host();
                self.render_status()
            }
            IpcCommand::StartTunnel => {
                if self.tunnel.child.is_none() {
                    self.tunnel.next_start = Some(Instant::now());
                    self.start_tunnel();
                }
                self.render_status()
            }
            IpcCommand::StopTunnel => {
                self.stop_tunnel();
                self.render_status()
            }
            // 主开关（面板上的"启动服务/暂停服务"）：
            // 关闭时必须真的停掉 host 和隧道，并且阻止监督循环自动重启，
            // 否则用户点了"暂停"、100ms 后服务又自己活过来——那是骗人的按钮。
            IpcCommand::SetAccept(v) => {
                self.accept_clients = v;
                if v {
                    log::info!("服务已开启");
                    self.host.next_start = Some(Instant::now());
                    self.tunnel.next_start = Some(Instant::now());
                    if self.cfg.supervise_host {
                        self.spawn_host();
                    }
                    if self.cfg.tunnel.is_some() {
                        self.start_tunnel();
                    }
                } else {
                    log::info!("服务已暂停：停止 host 与隧道，并停止自动重启");
                    self.stop_host();
                    self.stop_tunnel();
                    self.host.next_start = None;
                    self.tunnel.next_start = None;
                }
                self.render_status()
            }
            IpcCommand::Shutdown => {
                self.shutting_down = true;
                String::from("ok=shutdown\n")
            }
        }
    }

    pub fn status(&self) -> Status {
        Status {
            host: HostStatus {
                running: self.host.child.is_some() || self.host.adopted,
                pid: self.host.pid,
                exe: self.host.exe.clone(),
                healthy: self.host.healthy,
                busy: self.host.busy,
                probe_ms: self.host.probe_ms,
                last_error: self.host.last_error.clone(),
                restarts: self.host.restarts,
            },
            tunnel: TunnelStatus {
                enabled: self.cfg.tunnel.is_some(),
                running: self.tunnel.child.is_some(),
                pid: self.tunnel.pid,
                remote: self
                    .cfg
                    .tunnel
                    .as_ref()
                    .map(|t| format!("{}:{}", t.ssh_host, t.remote_port))
                    .unwrap_or_default(),
                remote_listening: self.tunnel.healthy,
                last_error: self.tunnel.last_error.clone(),
                reconnects: self.tunnel.restarts,
            },
            supervisor_pid: std::process::id(),
            uptime: self.started.elapsed(),
            accept_clients: self.accept_clients,
        }
    }

    /// 把状态渲染成 `key=value` 行：前端和命令行共用同一份数据。
    pub fn render_status(&self) -> String {
        let s = self.status();
        let mut out = String::new();
        out.push_str(&format!("host.running={}\n", s.host.running));
        out.push_str(&format!(
            "host.pid={}\n",
            s.host.pid.map(|p| p.to_string()).unwrap_or_else(|| String::from("-"))
        ));
        out.push_str(&format!("host.healthy={}\n", s.host.healthy));
        out.push_str(&format!("host.busy={}\n", s.host.busy));
        out.push_str(&format!(
            "host.probe_ms={}\n",
            s.host.probe_ms.map(|v| v.to_string()).unwrap_or_else(|| String::from("-"))
        ));
        out.push_str(&format!("host.restarts={}\n", s.host.restarts));
        out.push_str(&format!(
            "host.exe={}\n",
            s.host.exe.as_ref().map(|e| e.path.clone()).unwrap_or_else(|| String::from("-"))
        ));
        out.push_str(&format!(
            "host.exe_built={}\n",
            crate::exe::format_time(s.host.exe.as_ref().and_then(|e| e.modified))
        ));
        out.push_str(&format!(
            "host.exe_len={}\n",
            s.host.exe.as_ref().map(|e| e.len.to_string()).unwrap_or_else(|| String::from("-"))
        ));
        out.push_str(&format!(
            "host.last_error={}\n",
            s.host.last_error.as_deref().unwrap_or("-")
        ));
        out.push_str(&format!("tunnel.enabled={}\n", s.tunnel.enabled));
        out.push_str(&format!("tunnel.running={}\n", s.tunnel.running));
        out.push_str(&format!(
            "tunnel.pid={}\n",
            s.tunnel.pid.map(|p| p.to_string()).unwrap_or_else(|| String::from("-"))
        ));
        out.push_str(&format!("tunnel.remote={}\n", s.tunnel.remote));
        out.push_str(&format!("tunnel.healthy={}\n", s.tunnel.remote_listening));
        out.push_str(&format!("tunnel.reconnects={}\n", s.tunnel.reconnects));
        out.push_str(&format!(
            "tunnel.last_error={}\n",
            s.tunnel.last_error.as_deref().unwrap_or("-")
        ));
        out.push_str(&format!("supervisor.pid={}\n", s.supervisor_pid));
        out.push_str(&format!("supervisor.uptime_s={}\n", s.uptime.as_secs()));
        out.push_str(&format!("accept_clients={}\n", s.accept_clients));
        // 同一个状态的两个名字：accept_clients 给老命令行用，
        // service_enabled 给面板用（面板显示"服务运行中/已暂停"）。
        out.push_str(&format!("service_enabled={}\n", s.accept_clients));
        // 磁盘上（期望）vs 实际在跑的：不一致说明"换了 exe 没重启"，前端应标红
        out.push_str(&format!(
            "host.exe_freshness={}\n",
            match freshness(self.expected_exe.as_ref(), s.host.exe.as_ref()) {
                Freshness::Match => "match",
                Freshness::Stale => "stale",
                Freshness::Unknown => "unknown",
            }
        ));
        out.push_str(&format!(
            "host.exe_expected_built={}\n",
            crate::exe::format_time(self.expected_exe.as_ref().and_then(|e| e.modified))
        ));
        out
    }
}

/// 去远端确认隧道端口真的在监听。
///
/// 注意用的是 `ss -tln`（真的有监听），而不是"能连上"——黑洞端口也能连上。
fn remote_port_listening(t: &TunnelConfig) -> bool {
    let script = format!(
        "ss -tln | grep -q ':{} '",
        t.remote_port
    );
    let mut cmd = Command::new("ssh");
    if let Some(cfg) = t.ssh_config.as_ref() {
        cmd.arg("-F").arg(cfg);
    }
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg(&t.ssh_host)
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// 远端清残留：只在隧道主机上执行，失败不影响启动。
fn clear_remote_stale(t: &TunnelConfig) -> Result<(), String> {
    let script = format!(
        "for p in $(sudo -n ss -tlnp 2>/dev/null | grep ':{} ' | grep -oP 'pid=\\K[0-9]+' | sort -u); do sudo -n kill $p 2>/dev/null; done",
        t.remote_port
    );
    let mut cmd = Command::new("ssh");
    if let Some(cfg) = t.ssh_config.as_ref() {
        cmd.arg("-F").arg(cfg);
    }
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"])
        .arg(&t.ssh_host)
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| format!("调用 ssh 失败: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

fn spawn_tunnel(t: &TunnelConfig) -> Result<Child, String> {
    let mut cmd = Command::new("ssh");
    if let Some(cfg) = t.ssh_config.as_ref() {
        cmd.arg("-F").arg(cfg);
    }
    cmd.arg("-N")
        .args(["-o", "ServerAliveInterval=15"])
        .args(["-o", "ServerAliveCountMax=3"])
        .args(["-o", "TCPKeepAlive=yes"])
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(["-o", "BatchMode=yes"])
        .arg("-R")
        .arg(format!(
            "{}:127.0.0.1:{}",
            t.remote_port, t.local_port
        ))
        .arg(&t.ssh_host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().map_err(|e| format!("spawn ssh 失败: {e}"))
}

impl Daemon {
    /// host 进程本身是否还在（不管它健不健康）。
    fn host_alive(&self) -> bool {
        if self.host.child.is_some() {
            return true;
        }
        if !self.host.adopted {
            return false;
        }
        // 接管的实例：看 PID 还在不在
        match self.host.pid {
            Some(pid) => pid_running(pid),
            None => find_pid_on_port(&self.cfg.host_addr()).is_some(),
        }
    }
}

/// 某端口上是否存在 ESTABLISHED 连接（说明有客户端正连着）。
///
/// host 是单流设计：有客户端占用时，健康探测必然失败。
/// 靠这个把"正在服务"和"真的挂了"区分开。
fn port_has_established(addr: &str) -> bool {
    let Some(port) = addr.rsplit(':').next() else {
        return false;
    };
    let Ok(out) = Command::new("netstat").args(["-ano"]).output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if !line.contains("ESTABLISHED") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        if cols[1].rsplit(':').next() == Some(port) {
            return true;
        }
    }
    false
}

/// PID 对应的进程是否还活着。
fn pid_running(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

/// 从 `netstat -ano` 里找监听某端口的 PID（接管外部已启动实例用）。
pub fn find_pid_on_port(addr: &str) -> Option<u32> {
    let port = addr.rsplit(':').next()?;
    let out = Command::new("netstat").args(["-ano"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if !line.contains("LISTENING") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        let local = cols[1];
        if local.rsplit(':').next() == Some(port) {
            if let Ok(pid) = cols[4].parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// 查某个 PID 的可执行文件路径（接管外部实例时用）。
fn process_image_path(pid: Option<u32>) -> Option<String> {
    let pid = pid?;
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).Path"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            host_exe: std::path::PathBuf::from("lrc-host.exe"),
            listen: String::from("127.0.0.1:17805"),
            password: String::from("p"),
            display: 0,
            // 单测绝不能真的拉起子进程：SetAccept(true) 会 spawn lrc-host，
            // 测试进程结束后它就变成孤儿，占着端口和 CPU（实测泄漏过）。
            supervise_host: false,
            tunnel: None,
            ipc_port: 0,
            probe_interval: Duration::from_secs(2),
            restart_backoff: Duration::from_secs(2),
        }
    }

    #[test]
    fn host_addr_rewrites_wildcard_to_loopback() {
        let mut c = cfg();
        c.listen = String::from("0.0.0.0:7805");
        assert_eq!(c.host_addr(), "127.0.0.1:7805");
        c.listen = String::from("127.0.0.1:9000");
        assert_eq!(c.host_addr(), "127.0.0.1:9000");
    }

    #[test]
    fn status_renders_all_expected_keys() {
        let d = Daemon::new(cfg());
        let text = d.render_status();
        for key in [
            "host.running=",
            "host.pid=",
            "host.healthy=",
            "host.exe=",
            "host.exe_built=",
            "tunnel.enabled=",
            "tunnel.remote=",
            "supervisor.pid=",
            "accept_clients=",
        ] {
            assert!(text.contains(key), "缺少字段 {key}\n{text}");
        }
    }

    #[test]
    fn dispatch_set_accept_toggles_flag() {
        let mut d = Daemon::new(cfg());
        assert!(d.status().accept_clients);
        d.dispatch(IpcCommand::SetAccept(false));
        assert!(!d.status().accept_clients, "accept off 必须生效");
        d.dispatch(IpcCommand::SetAccept(true));
        assert!(d.status().accept_clients);
        // 这个用例只验证开关状态；cfg() 里 supervise_host=false，
        // 所以不会真的 spawn 子进程。
        assert!(d.status().host.pid.is_none(), "测试不该拉起真实 host 进程");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut s = Slot::new();
        s.backoff = MIN_BACKOFF;
        for _ in 0..10 {
            s.backoff = (s.backoff * 2).min(MAX_BACKOFF);
        }
        assert_eq!(s.backoff, MAX_BACKOFF, "退避必须有上限，不能无限增长");
    }
}
