//! 命令行解析（手写，不引 clap）。

pub const USAGE: &str = "\
用法: lrc-client --password <密码> [--connect <host:port> | --signal <IP:PORT>] [--title <标题>] [--stats]

  --connect <addr>   被控端 TCP 地址，例如 127.0.0.1:7800（连不上会每秒重试，窗口保持打开）；
                     给了 --signal 且 --p2p != off 时可省略——但省略后 P2P 失败无法回退 TCP
  --password <pwd>   认证密码，作为 HMAC-SHA256 的 key
  --title <text>     窗口标题，默认 \"lrc viewer\"
  --audio on|off     音频通道，默认 on（设备不可用会自动降级为 off，不影响视频）
  --audio-dump <path> 把解码后的 PCM(s16le 48k/2ch) 追加写到文件，调试用
  --signal <IP:PORT> relay 信令地址（不给 = 不启用 P2P，只走 TCP；给了就 P2P 优先）
  --room <N>         房间号（host/client 相同），默认 1
  --p2p auto|off|direct|relay  P2P 模式
  --stats            每秒打印一行 fps= recv_kbps= decoded_blocks= ...
  -h, --help         显示本帮助
";

/// P2P 传输模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum P2pMode {
    /// 先打洞，失败降级 relay（默认）
    #[default]
    Auto,
    /// 关闭 P2P，只走 TCP 直连
    Off,
    /// 必须打洞成功（失败按 P2P 失败处理，回退 TCP）
    Direct,
    /// 强制走 relay（不打洞）
    Relay,
}

impl P2pMode {
    /// 是否启用 UDP 路径（`off` 表示纯 TCP）
    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// 主控端运行参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// TCP 直连地址（--connect）；给了 --signal 且 --p2p != off 时可以不给（P2P 优先）
    pub connect: Option<String>,
    pub password: String,
    pub title: String,
    pub stats: bool,
    /// 音频通道开关（--audio on|off，默认 on）
    pub audio: bool,
    /// 解码 PCM 落盘路径（--audio-dump，调试用）
    pub audio_dump: Option<String>,
    /// relay 信令地址（--signal；None = 不启用 P2P）
    pub signal: Option<String>,
    /// 房间号（--room，默认 1）
    pub room: u32,
    /// P2P 模式（--p2p）
    pub p2p: P2pMode,
    /// 窗口客户区默认宽度（物理像素）
    pub width: u32,
    /// 窗口客户区默认高度（物理像素）
    pub height: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connect: None,
            password: String::new(),
            title: String::from("lrc viewer"),
            stats: false,
            audio: true,
            audio_dump: None,
            signal: None,
            room: 1,
            p2p: P2pMode::Auto,
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
                    cfg.connect = Some(take_value(inline, &mut it, "--connect")?);
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
                "--audio-dump" => {
                    cfg.audio_dump = Some(take_value(inline, &mut it, "--audio-dump")?);
                }
                "--signal" => {
                    cfg.signal = Some(take_value(inline, &mut it, "--signal")?);
                }
                "--room" => {
                    let value = take_value(inline, &mut it, "--room")?;
                    cfg.room = value.parse().map_err(|e| format!("--room 需要整数: {e}"))?;
                }
                "--p2p" => {
                    let value = take_value(inline, &mut it, "--p2p")?;
                    cfg.p2p = match value.to_ascii_lowercase().as_str() {
                        "auto" => P2pMode::Auto,
                        "off" => P2pMode::Off,
                        "direct" => P2pMode::Direct,
                        "relay" => P2pMode::Relay,
                        other => {
                            return Err(format!(
                                "--p2p 只接受 auto|off|direct|relay（收到 {other}）"
                            ))
                        }
                    };
                }
                other => return Err(format!("未知参数 {other}")),
            }
        }

        if !has_password {
            return Err(String::from("缺少 --password <密码>"));
        }
        // --connect 可选：给了 --signal 且 --p2p != off 时 P2P 优先，TCP 只是兜底
        let p2p_available = cfg.signal.is_some() && cfg.p2p.enabled();
        if !has_connect && !p2p_available {
            return Err(String::from(
                "缺少 --connect <host:port>（没给 --signal 时必须提供 TCP 直连地址）",
            ));
        }
        if matches!(cfg.connect.as_deref(), Some("")) {
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
                assert_eq!(cfg.connect.as_deref(), Some("127.0.0.1:7800"));
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
                assert_eq!(cfg.connect.as_deref(), Some("a:1"));
                assert_eq!(cfg.password, "p");
                assert_eq!(cfg.title, "lrc viewer");
                assert!(!cfg.stats);
            }
            Parsed::Help => panic!("should run"),
        }
    }

    #[test]
    fn parses_p2p_options() {
        let parsed = parse(&[
            "--connect",
            "a:1",
            "--password",
            "p",
            "--signal",
            "127.0.0.1:7801",
            "--room",
            "7",
            "--p2p",
            "relay",
        ])
        .expect("parse");
        match parsed {
            Parsed::Run(cfg) => {
                assert_eq!(cfg.signal.as_deref(), Some("127.0.0.1:7801"));
                assert_eq!(cfg.room, 7);
                assert_eq!(cfg.p2p, P2pMode::Relay);
            }
            Parsed::Help => panic!("should run"),
        }
        // 默认：不启用 P2P、room=1、p2p=auto
        let parsed = parse(&["--connect", "a:1", "--password", "p"]).expect("parse");
        match parsed {
            Parsed::Run(cfg) => {
                assert!(cfg.signal.is_none());
                assert_eq!(cfg.room, 1);
                assert_eq!(cfg.p2p, P2pMode::Auto);
                assert!(cfg.p2p.enabled());
            }
            Parsed::Help => panic!("should run"),
        }
        assert!(parse(&["--connect", "a:1", "--password", "p", "--p2p", "bogus"]).is_err());
        assert!(parse(&["--connect", "a:1", "--password", "p", "--room", "x"]).is_err());
    }

    #[test]
    fn connect_is_optional_when_p2p_enabled() {
        // 给了 --signal 且 p2p != off：可以不给 --connect
        let parsed = parse(&["--password", "p", "--signal", "127.0.0.1:7801"]).expect("parse");
        match parsed {
            Parsed::Run(cfg) => {
                assert!(cfg.connect.is_none());
                assert_eq!(cfg.p2p, P2pMode::Auto);
            }
            Parsed::Help => panic!("should run"),
        }
        // relay/direct 模式同样可以省略 --connect
        assert!(parse(&["--password", "p", "--signal", "1.2.3.4:7801", "--p2p", "relay"]).is_ok());
        assert!(parse(&["--password", "p", "--signal", "1.2.3.4:7801", "--p2p", "direct"]).is_ok());
        // --p2p off：P2P 不可用，仍然必须有 --connect
        assert!(parse(&["--password", "p", "--signal", "1.2.3.4:7801", "--p2p", "off"]).is_err());
        // 两者都不给：报错
        assert!(parse(&["--password", "p"]).is_err());
        // 空 --connect 也要报错
        assert!(parse(&["--password", "p", "--signal", "1.2.3.4:7801", "--connect", ""]).is_err());
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
