//! 控制命令行：跟常驻监督进程说话，自己不含任何业务逻辑。

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use lrc_supervisor::config::DEFAULT_IPC_PORT;
use lrc_supervisor::ipc;

const USAGE: &str = "\
用法: lrc-ctl <命令> [--port N]

命令:
  status            显示 host / 隧道 / 监督进程状态
  start             启动 host
  stop              停止 host
  restart           重启 host
  tunnel-start      启动反向隧道
  tunnel-stop       停止反向隧道
  connect           允许新客户端接入（accept on）
  disconnect        不再接受新客户端（accept off）
  ping              探活
  shutdown          让监督进程退出（连同子进程）

选项:
  --port <N>        IPC 端口，默认 17899
  -h, --help        显示本帮助
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut port = DEFAULT_IPC_PORT;
    let mut action: Option<String> = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--port" => {
                let Some(v) = it.next() else {
                    eprintln!("--port 缺少取值");
                    return ExitCode::from(2);
                };
                match v.parse() {
                    Ok(p) => port = p,
                    Err(_) => {
                        eprintln!("--port 需要 1..=65535");
                        return ExitCode::from(2);
                    }
                }
            }
            other if action.is_none() => action = Some(other.to_string()),
            other => {
                eprintln!("多余的参数：{other}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(action) = action else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };

    let remote = match action.as_str() {
        "status" => "status",
        "start" => "start-host",
        "stop" => "stop-host",
        "restart" => "restart-host",
        "tunnel-start" => "start-tunnel",
        "tunnel-stop" => "stop-tunnel",
        "connect" => "accept on",
        "disconnect" => "accept off",
        "ping" => "ping",
        "shutdown" => "shutdown",
        other => {
            eprintln!("未知命令：{other}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match ipc::call(addr, remote, Duration::from_secs(8)) {
        Ok(text) => {
            if action == "status" {
                print_human(&text);
            } else {
                print!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lrc-ctl: {e}");
            eprintln!("（监督进程没在跑？用 lrc-supervisord 启动）");
            ExitCode::FAILURE
        }
    }
}

/// 把 key=value 渲染成对齐的人读表格。
fn print_human(text: &str) {
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        rows.push((k.to_string(), v.to_string()));
    }
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in rows {
        println!("{k:<width$}  {v}");
    }
}
