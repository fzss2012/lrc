//! 常驻监督进程入口。

use std::path::PathBuf;
use std::process::ExitCode;

use lrc_supervisor::config::TunnelConfig;
use lrc_supervisor::{Config, Daemon};

const USAGE: &str = "\
用法: lrc-supervisord --password <PASS> [选项]

必填:
  --password <PASS>      共享密码（也可用环境变量 LRC_PASSWORD）

选项:
  --host-exe <PATH>      lrc-host 可执行文件，默认 lrc-host.exe（同目录）
  --listen <IP:PORT>     host 监听地址，默认 127.0.0.1:7805
  --display <N>          采集显示输出序号，默认 0
  --ipc-port <PORT>      IPC 监听端口（固定 127.0.0.1），默认 17899
  --no-supervise         只启动 host，退出后不自动重启
  --ssh-host <HOST>      启用反向隧道，并给出 ssh 目标主机名
  --ssh-config <PATH>    可选，ssh -F 使用的配置文件
  --remote-port <PORT>   隧道在远端的监听端口，默认 17800
  --local-port <PORT>    隧道转发的本地端口，默认等于 host 端口
  --install-autostart    把自己登记到当前用户登录启动项
  --uninstall-autostart  取消登录自启动
  -h, --help             显示本帮助
";

struct Args {
    cfg: Config,
    install_autostart: bool,
    uninstall_autostart: bool,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut cfg = Config::default();
    let mut host_exe: Option<PathBuf> = None;
    let mut ssh_host: Option<String> = None;
    let mut ssh_config: Option<PathBuf> = None;
    let mut remote_port = lrc_supervisor::config::DEFAULT_REMOTE_PORT;
    let mut local_port: Option<u16> = None;
    let mut install_autostart = false;
    let mut uninstall_autostart = false;
    let mut password: Option<String> = None;

    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut take = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            it.next().ok_or_else(|| format!("{what} 缺少取值"))
        };
        match name.as_str() {
            "-h" | "--help" => return Ok(None),
            "--password" => password = Some(take("--password")?),
            "--host-exe" => host_exe = Some(PathBuf::from(take("--host-exe")?)),
            "--listen" => cfg.listen = take("--listen")?,
            "--display" => {
                cfg.display = take("--display")?
                    .parse()
                    .map_err(|_| String::from("--display 需要非负整数"))?
            }
            "--ipc-port" => {
                cfg.ipc_port = take("--ipc-port")?
                    .parse()
                    .map_err(|_| String::from("--ipc-port 需要 1..=65535"))?
            }
            "--no-supervise" => cfg.supervise_host = false,
            "--ssh-host" => ssh_host = Some(take("--ssh-host")?),
            "--ssh-config" => ssh_config = Some(PathBuf::from(take("--ssh-config")?)),
            "--remote-port" => {
                remote_port = take("--remote-port")?
                    .parse()
                    .map_err(|_| String::from("--remote-port 需要 1..=65535"))?
            }
            "--local-port" => {
                local_port = Some(
                    take("--local-port")?
                        .parse()
                        .map_err(|_| String::from("--local-port 需要 1..=65535"))?,
                )
            }
            "--install-autostart" => install_autostart = true,
            "--uninstall-autostart" => uninstall_autostart = true,
            other => return Err(format!("未知选项 {other}")),
        }
    }

    let password = password
        .or_else(|| std::env::var("LRC_PASSWORD").ok())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| String::from("缺少 --password（或环境变量 LRC_PASSWORD）"))?;
    cfg.password = password;

    // 默认：host 可执行文件放在监督进程自己旁边
    if host_exe.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                host_exe = Some(dir.join("lrc-host.exe"));
            }
        }
    }
    if let Some(p) = host_exe {
        cfg.host_exe = p;
    }

    let local_port = local_port.unwrap_or_else(|| {
        cfg.listen
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
            .unwrap_or(lrc_supervisor::config::DEFAULT_HOST_PORT)
    });
    if let Some(host) = ssh_host {
        cfg.tunnel = Some(TunnelConfig {
            ssh_host: host,
            remote_port,
            local_port,
            ssh_config,
        });
    }

    Ok(Some(Args {
        cfg,
        install_autostart,
        uninstall_autostart,
    }))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(argv) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("lrc-supervisord: {e}");
            eprintln!();
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    if args.uninstall_autostart {
        match lrc_supervisor::autostart::disable() {
            Ok(()) => log::info!("已取消登录自启动"),
            Err(e) => {
                eprintln!("取消自启动失败：{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if args.install_autostart {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lrc-supervisord.exe"));
        let cfg_path = PathBuf::from("lrc-supervisor.cfg");
        match lrc_supervisor::autostart::enable(&exe, &cfg_path) {
            Ok(()) => log::info!("已登记登录自启动：{}", exe.display()),
            Err(e) => {
                eprintln!("登记自启动失败：{e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut daemon = Daemon::new(args.cfg);
    match daemon.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lrc-supervisord 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_sane() {
        let a = parse_args(argv(&["--password", "p"])).unwrap().unwrap();
        assert_eq!(a.cfg.listen, "127.0.0.1:7805");
        assert_eq!(a.cfg.ipc_port, 17899);
        assert!(a.cfg.tunnel.is_none());
        assert!(a.cfg.supervise_host);
    }

    #[test]
    fn tunnel_flags_build_config() {
        let a = parse_args(argv(&[
            "--password", "p", "--ssh-host", "yanfa2", "--remote-port", "17800",
            "--local-port", "7805",
        ]))
        .unwrap()
        .unwrap();
        let t = a.cfg.tunnel.expect("应启用隧道");
        assert_eq!(t.ssh_host, "yanfa2");
        assert_eq!(t.remote_port, 17800);
        assert_eq!(t.local_port, 7805);
    }

    #[test]
    fn missing_password_is_an_error() {
        assert!(parse_args(argv(&[])).is_err());
    }

    #[test]
    fn help_returns_none() {
        assert!(parse_args(argv(&["--help"])).unwrap().is_none());
    }
}
