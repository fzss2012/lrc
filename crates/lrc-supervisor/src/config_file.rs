//! 配置持久化：手写 key=value 文件（和 IPC 同一套格式，零依赖）。
//!
//! 位置：%APPDATA%\lrc\config.txt。第一次运行自动生成一个带注释的默认文件，
//! 用户改完重启面板即可生效。密码不写进命令行（不出现在进程列表里）。

use std::path::{Path, PathBuf};

use crate::config::{Config, TunnelConfig};

/// 配置文件名。
pub const FILE_NAME: &str = "config.txt";

/// 默认配置内容（面向普通用户的中文说明）。
pub const DEFAULT_TEMPLATE: &str = concat!(
    "# ============================================", "\n",
    "#  远程控制服务 - 设置", "\n",
    "#  改完请关掉程序再重新打开，设置才会生效", "\n",
    "# ============================================", "\n",
    "", "\n",
    "# 连接密码：对方连接时需要输入这个密码", "\n",
    "# 建议改成你自己记得住的密码，不要用默认的 1234", "\n",
    "password=1234", "\n",
    "", "\n",
    "# 本机端口，一般不用改", "\n",
    "listen_port=7805", "\n",
    "", "\n",
    "# 采集哪块屏幕（多显示器时：主屏填 0，第二块填 1）", "\n",
    "display=0", "\n",
    "", "\n",
    "# 远程中转服务器的 SSH 主机名", "\n",
    "# 也就是「对方通过哪台服务器找到这台电脑」", "\n",
    "# 不需要远程连接就留空", "\n",
    "ssh_host=yanfa2", "\n",
    "", "\n",
    "# SSH 配置文件路径", "\n",
    "# 留空 = 自动使用 ~/.ssh/config", "\n",
    "ssh_config=", "\n",
    "", "\n",
    "# 远程服务器上使用的端口，一般不用改", "\n",
    "remote_port=17800", "\n",
    "", "\n",
    "# 程序内部通信用，不要修改", "\n",
    "ipc_port=17899", "\n",
);

/// 自动找 ssh 配置文件：优先 lrc 专用配置，其次系统默认。
///
/// 用户在自己电脑上跑 ssh 时，配置文件名五花八门（config / lrc-ssh-config …），
/// 与其让用户去填绝对路径，不如按常见顺序试一遍。
fn detect_ssh_config() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let ssh_dir = PathBuf::from(home).join(".ssh");
    for name in ["lrc-ssh-config", "config"] {
        let p = ssh_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 取配置目录：%APPDATA%\lrc，没有 APPDATA 就用当前目录。
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("lrc");
        }
    }
    PathBuf::from(".")
}

pub fn config_path() -> PathBuf {
    config_dir().join(FILE_NAME)
}

/// 读配置；文件不存在时创建默认模板并返回默认值。
pub fn load_or_create() -> Result<Config, String> {
    let path = config_path();
    if !path.exists() {
        create_default(&path)?;
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取配置 {} 失败: {e}", path.display()))?;
    parse(&text)
}

/// 创建默认配置文件（含注释）。
pub fn create_default(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置目录 {} 失败: {e}", parent.display()))?;
    }
    std::fs::write(path, DEFAULT_TEMPLATE)
        .map_err(|e| format!("写默认配置 {} 失败: {e}", path.display()))
}

/// 解析配置文本：跳过 # 注释和空行，key=value。
pub fn parse(text: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut password = String::new();
    let mut ssh_host = String::new();
    let mut ssh_config = String::new();
    let mut listen_port = crate::config::DEFAULT_HOST_PORT;
    let mut remote_port = crate::config::DEFAULT_REMOTE_PORT;
    let mut ipc_port = crate::config::DEFAULT_IPC_PORT;

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("第 {} 行不是 key=value: {line}", i + 1));
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "password" => password = v.to_string(),
            "listen_port" => listen_port = v.parse().map_err(|_| format!("第 {} 行 listen_port 非法", i + 1))?,
            "remote_port" => remote_port = v.parse().map_err(|_| format!("第 {} 行 remote_port 非法", i + 1))?,
            "ipc_port" => ipc_port = v.parse().map_err(|_| format!("第 {} 行 ipc_port 非法", i + 1))?,
            "display" => cfg.display = v.parse().map_err(|_| format!("第 {} 行 display 非法", i + 1))?,
            "ssh_host" => ssh_host = v.to_string(),
            "ssh_config" => ssh_config = v.to_string(),
            other => return Err(format!("第 {} 行未知配置项 {other}", i + 1)),
        }
    }
    if password.is_empty() {
        return Err(String::from("配置里 password 不能为空"));
    }
    cfg.password = password;

    // 单 exe 模式下没有 --host-exe 参数：默认取面板自己所在目录的 lrc-host.exe。
    // 必须是绝对路径，否则 identify() 会拿相对路径去读、拿不到身份，
    // 面板上"版本状态"就永远显示"未知"，旧 exe 检测形同虚设。
    if cfg.host_exe == std::path::PathBuf::from("lrc-host.exe") {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                cfg.host_exe = dir.join("lrc-host.exe");
            }
        }
    }
    cfg.listen = format!("127.0.0.1:{listen_port}");
    cfg.ipc_port = ipc_port;

    if !ssh_host.is_empty() {
        // ssh_config 留空时自动探测：先找 lrc 专用配置，再退回系统默认。
        // 注意顺序：必须在这里（构造 tunnel 时）决定，不能提前——
        // 提前的话 tunnel 还是 None，探测永远不生效（踩过）。
        let resolved = if ssh_config.is_empty() {
            detect_ssh_config()
        } else {
            Some(PathBuf::from(&ssh_config))
        };
        cfg.tunnel = Some(TunnelConfig {
            ssh_host,
            remote_port,
            local_port: listen_port,
            ssh_config: resolved,
        });
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_into_defaults() {
        let cfg = parse(DEFAULT_TEMPLATE).expect("默认模板必须能解析");
        assert_eq!(cfg.password, "1234");
        assert_eq!(cfg.listen, "127.0.0.1:7805");
        assert_eq!(cfg.ipc_port, 17899);
        let t = cfg.tunnel.expect("模板里有 ssh_host");
        assert_eq!(t.ssh_host, "yanfa2");
        assert_eq!(t.remote_port, 17800);
        assert_eq!(t.local_port, 7805);
    }

    #[test]
    fn empty_ssh_host_disables_tunnel() {
        let text = "password=p\nssh_host=\n";
        let cfg = parse(text).unwrap();
        assert!(cfg.tunnel.is_none());
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let text = "# hello\n\npassword=abc\n# tail\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.password, "abc");
    }

    #[test]
    fn empty_ssh_config_gets_auto_detected() {
        // 本机有 ~/.ssh/lrc-ssh-config 或 ~/.ssh/config 时，留空应被自动填上
        let text = "password=p\nssh_host=yanfa2\nssh_config=\n";
        let cfg = parse(text).unwrap();
        let t = cfg.tunnel.expect("应启用隧道");
        // 探测不到时允许为 None（干净环境），但探测到了就必须真实存在
        if let Some(p) = t.ssh_config.as_ref() {
            assert!(!p.as_os_str().is_empty(), "不能是空路径");
            assert!(p.is_file(), "探测到的路径必须真实存在: {}", p.display());
        }
    }

    #[test]
    fn explicit_ssh_config_is_respected() {
        let text = "password=p\nssh_host=x\nssh_config=/tmp/custom_ssh\n";
        let cfg = parse(text).unwrap();
        assert_eq!(
            cfg.tunnel.unwrap().ssh_config,
            Some(PathBuf::from("/tmp/custom_ssh"))
        );
    }

    #[test]
    fn empty_password_is_rejected() {
        assert!(parse("password=\n").is_err());
    }
}
