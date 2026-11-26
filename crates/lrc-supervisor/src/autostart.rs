//! 登录后自启动（A 档：不装系统服务）。
//!
//! 用当前用户的 Run 注册表项：登录即启动、只在该用户会话里、不需要管理员。
//! 采集和输入注入都必须在交互式会话里跑，所以"登录后可用"正好够用。

use std::path::Path;

/// 注册表位置（当前用户）。
#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// 启动项名称。
pub const VALUE_NAME: &str = "lrc-panel";

/// 自启动状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Autostart {
    /// 已登记，且指向这个路径
    Enabled { command: String },
    /// 没登记
    Disabled,
    /// 登记了别的东西
    Other { command: String },
}

/// 查询当前自启动配置。非 Windows 上永远是 Disabled（A 档只管 Windows）。
pub fn query() -> Autostart {
    #[cfg(windows)]
    {
        match read_run_value() {
            Some(cmd) if cmd.contains("lrc-panel") => Autostart::Enabled { command: cmd },
            Some(cmd) => Autostart::Other { command: cmd },
            None => Autostart::Disabled,
        }
    }
    #[cfg(not(windows))]
    {
        Autostart::Disabled
    }
}

/// 生成要写进注册表的命令行。
///
/// 注意：不能带 `--config` —— 那是"打开配置文件夹"的意思（见面板 CLI），
/// 会把自启动变成每次登录弹一个资源管理器窗口。配置固定在 %APPDATA%\lrc，
/// 所以自启动只需要直接运行 exe。
pub fn command_line(exe: &Path, _config: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(windows)]
fn read_run_value() -> Option<String> {
    // 用 reg.exe 而不是 windows crate：这里只需要读一个字符串，
    // 引一个 feature 不划算（和仓库其它部分"能不引就不引"的取舍一致）。
    let out = std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 输出形如：    lrc-panel    REG_SZ    "C:\path\lrc-panel.exe"
    text.lines()
        .find(|l| l.contains(VALUE_NAME))
        .and_then(|l| l.split("REG_SZ").nth(1))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 登记自启动。
#[cfg(windows)]
pub fn enable(exe: &Path, config: &Path) -> Result<(), String> {
    let cmd = command_line(exe, config);
    let out = std::process::Command::new("reg")
        .args([
            "add",
            RUN_KEY,
            "/v",
            VALUE_NAME,
            "/t",
            "REG_SZ",
            "/d",
            &cmd,
            "/f",
        ])
        .output()
        .map_err(|e| format!("调用 reg.exe 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "写注册表失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 取消自启动。
#[cfg(windows)]
pub fn disable() -> Result<(), String> {
    let out = std::process::Command::new("reg")
        .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
        .output()
        .map_err(|e| format!("调用 reg.exe 失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // 本来就没有 = 已经是目标状态
        if err.contains("unable to find") || err.contains("找不到") {
            return Ok(());
        }
        return Err(format!("删除注册表项失败: {}", err.trim()));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn enable(_exe: &Path, _config: &Path) -> Result<(), String> {
    Err(String::from("自启动目前只支持 Windows（A 档）"))
}

#[cfg(not(windows))]
pub fn disable() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        let cmd = command_line(
            Path::new(r"C:\Program Files\lrc\lrc-panel.exe"),
            Path::new(r"C:\Users\me\lrc.toml"),
        );
        assert!(cmd.starts_with('"'), "带空格的路径必须加引号: {cmd}");
        assert!(!cmd.contains("--config"), "自启动不能带 --config（那会打开文件夹）: {cmd}");
    }

    #[test]
    fn query_never_panics() {
        // 只是确认能在任何环境下安全调用（无注册表权限时返回 Disabled/Other 均可）
        let _ = query();
    }
}
