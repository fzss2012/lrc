//! 命令行解析（手写，不引 clap）。

pub const USAGE: &str = "\
用法: lrc-client --connect <host:port> --password <密码> [--title <标题>] [--stats]

  --connect <addr>   被控端地址，例如 127.0.0.1:7800（连不上会每秒重试，窗口保持打开）
  --password <pwd>   认证密码，作为 HMAC-SHA256 的 key
  --title <text>     窗口标题，默认 \"lrc viewer\"
  --audio on|off     音频通道，默认 on（设备不可用会自动降级为 off，不影响视频）
  --stats            每秒打印一行 fps= recv_kbps= decoded_blocks= ...
  -h, --help         显示本帮助
";

/// 主控端运行参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub connect: String,
    pub password: String,
    pub title: String,
    pub stats: bool,
    /// 音频通道开关（--audio on|off，默认 on）
    pub audio: bool,
    /// 窗口客户区默认宽度（物理像素）
    pub width: u32,
    /// 窗口客户区默认高度（物理像素）
    pub height: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connect: String::new(),
            password: String::new(),
            title: String::from("lrc viewer"),
            stats: false,
            audio: true,
            width: 1280,
            height: 720,
        }
    }
}

/// 解析结果：要么跑，要么打印帮助。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Config),
    Help,
}

impl Config {
    /// 解析 `--flag value` 与 `--flag=value` 两种写法。
    pub fn parse<I>(args: I) -> Result<Parsed, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut cfg = Self::default();
        let mut has_connect = false;
        let mut has_password = false;
        let mut it = args.into_iter();

        while let Some(arg) = it.next() {
            let (key, inline) = match arg.split_once('=') {
                Some((k, v)) => (String::from(k), Some(String::from(v))),
                None => (arg, None),
            };

            match key.as_str() {
                "-h" | "--help" => return Ok(Parsed::Help),
                "--connect" => {
                    cfg.connect = take_value(inline, &mut it, "--connect")?;
                    has_connect = true;
                }
                "--password" => {
                    cfg.password = take_value(inline, &mut it, "--password")?;
                    has_password = true;
                }
                "--title" => {
                    cfg.title = take_value(inline, &mut it, "--title")?;
                }
                "--stats" => {
                    if inline.is_some() {
                        return Err(String::from("--stats 不接受取值"));
                    }
                    cfg.stats = true;
                }
                "--audio" => {
                    let value = take_value(inline, &mut it, "--audio")?;
                    cfg.audio = match value.to_ascii_lowercase().as_str() {
                        "on" | "1" | "true" => true,
                        "off" | "0" | "false" => false,
                        other => return Err(format!("--audio 只接受 on|off（收到 {other}）")),
                    };
                }
                other => return Err(format!("未知参数 {other}")),
            }
        }

        if !has_connect {
            return Err(String::from("缺少 --connect <host:port>"));
        }
        if !has_password {
            return Err(String::from("缺少 --password <密码>"));
        }
        if cfg.connect.is_empty() {
            return Err(String::from("--connect 不能为空"));
        }
        Ok(Parsed::Run(cfg))
    }
}

fn take_value<T>(inline: Option<String>, it: &mut T, flag: &str) -> Result<String, String>
where
    T: Iterator<Item = String>,
{
    match inline {
        Some(v) => Ok(v),
        None => it
            .next()
            .ok_or_else(|| format!("参数 {flag} 缺少取值")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, String> {
        Config::parse(args.iter().map(|s| String::from(*s)))
    }

    #[test]
    fn parses_required_and_optional_flags() {
        let parsed = parse(&[
            "--connect",
            "127.0.0.1:7800",
            "--password",
            "1234",
            "--title",
            "lrc viewer",
            "--stats",
        ])
        .expect("parse");
        match parsed {
            Parsed::Run(cfg) => {
                assert_eq!(cfg.connect, "127.0.0.1:7800");
                assert_eq!(cfg.password, "1234");
                assert_eq!(cfg.title, "lrc viewer");
                assert!(cfg.stats);
                assert_eq!((cfg.width, cfg.height), (1280, 720));
            }
            Parsed::Help => panic!("should run"),
        }
    }

    #[test]
    fn parses_equals_form_and_default_title() {
        let parsed = parse(&["--connect=a:1", "--password=p"]).expect("parse");
        match parsed {
            Parsed::Run(cfg) => {
                assert_eq!(cfg.connect, "a:1");
                assert_eq!(cfg.password, "p");
                assert_eq!(cfg.title, "lrc viewer");
                assert!(!cfg.stats);
            }
            Parsed::Help => panic!("should run"),
        }
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse(&["--help"]).expect("parse"), Parsed::Help);
    }

    #[test]
    fn rejects_missing_values_and_unknown_flags() {
        assert!(parse(&["--connect", "a:1"]).is_err());
        assert!(parse(&["--connect"]).is_err());
        assert!(parse(&["--stats", "--password", "p", "--bogus"]).is_err());
        assert!(parse(&[]).is_err());
    }
}
