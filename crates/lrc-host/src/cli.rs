//! 命令行解析（手写，不引入 clap）。

use std::fmt;

/// 默认监听地址。
pub const DEFAULT_LISTEN: &str = "0.0.0.0:7800";
/// 默认发送帧率上限。
pub const DEFAULT_FPS: u32 = 30;
/// 默认 JPEG 质量。
pub const DEFAULT_QUALITY: u8 = 85;
/// 帧率上限的硬顶。
pub const MAX_FPS: u32 = 240;
/// 视频通道默认码率（bit/s）。
pub const DEFAULT_VIDEO_BITRATE: u32 = 4_000_000;
/// 视频模式默认帧率上限。
pub const DEFAULT_VIDEO_MAX_FPS: u32 = 30;
/// 视频码率的合理范围。
pub const VIDEO_BITRATE_RANGE: std::ops::RangeInclusive<u32> = 100_000..=80_000_000;

/// P2P / relay 路径选择（M4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P2pMode {
    /// 先打洞，失败走 relay，relay 也不可用再回退 TCP
    Auto,
    /// 只用 TCP（= 没有 P2P 时的行为）
    Off,
    /// 打洞失败就报错退出（不降级）
    Direct,
    /// 跳过打洞，直接走 relay（relay 不可用报错，不回退 TCP）
    Relay,
}

impl P2pMode {
    pub fn label(self) -> &'static str {
        match self {
            P2pMode::Auto => "auto",
            P2pMode::Off => "off",
            P2pMode::Direct => "direct",
            P2pMode::Relay => "relay",
        }
    }
}

/// 视频通道选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoChoice {
    /// 探测到 NVENC 就用 hevc，否则永久块模式
    Auto,
    /// 完全不用视频通道
    Off,
    Hevc,
    H264,
}

impl VideoChoice {
    pub fn label(self) -> &'static str {
        match self {
            VideoChoice::Auto => "auto",
            VideoChoice::Off => "off",
            VideoChoice::Hevc => "hevc",
            VideoChoice::H264 => "h264",
        }
    }

    /// 显式关了视频就不该去探测/起编码器。
    pub fn is_off(self) -> bool {
        matches!(self, VideoChoice::Off)
    }
}

pub const USAGE: &str = "用法: lrc-host --password <PASS> [--listen <IP:PORT>] [--display N] [--fps N] [--quality N] [--stats] [--video auto|off|hevc|h264] [--video-bitrate B] [--video-max-fps N] [--budget-bps B] [--audio on|off] [--signal IP:PORT --room N --p2p auto|off|direct|relay]";

pub const HELP: &str = "\
lrc-host — lrc 被控端（单流点对点）

用法:
  lrc-host --password <PASS> [--listen <IP:PORT>] [--display N] [--fps N] [--quality N] [--stats]

必填:
  --password <PASS>   共享密码，即 HMAC-SHA256 的 key（也可用环境变量 LRC_PASSWORD）

选项:
  --listen <ADDR>     监听地址，默认 0.0.0.0:7800
  --display <N>       采集第 N 个输出（DXGI 输出序号，0 起），默认 0
  --fps <N>           块模式发送帧率上限 1..=240，默认 30
  --quality <N>       JPEG 质量 1..=100，默认 85
  --stats             每秒往 stdout 打一行统计
  --video <MODE>      视频通道 auto|off|hevc|h264，默认 auto（探测到 NVENC 就用 hevc）
  --video-bitrate <B> 视频码率 bit/s，默认 4000000
  --video-max-fps <N> 视频模式帧率上限 1..=240，默认 30
  --budget-bps <B>    链路预算 bit/s；>0 时启用自适应控制器（降帧率/抬QP/降分辨率/调码率），
                      默认 0 = 关闭（参数固定，行为与不加此参数前完全一致）
  --audio on|off      系统声音回环（Opus 32kbps），默认 on；设备不可用时自动永久关闭
  --signal <IP:PORT>  relay 信令地址；不给则不启用 P2P（行为与 M3 完全一致）
  --room <N>          房间号，默认 1
  --p2p <MODE>        auto|off|direct|relay，默认 auto：
                        off=只用 TCP；direct=打洞失败报错；relay=跳过打洞直接走 relay；
                        auto=先打洞，失败走 relay，再失败回退 TCP
  -h, --help          显示本帮助

统计行:
  t=<秒> fps= blocks= sent_frames= bytes= kbps= idle_frames= dropped= sec_bytes=
  cursor_msgs= full_resends= mode=blocks|video video_kbps= video_fps=
  level=<档位> stage=<阶段> scale=<缩放>（--budget-bps 生效时才有意义）
";

/// 解析后的启动参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub listen: String,
    pub password: String,
    pub display: usize,
    pub fps: u32,
    pub quality: u8,
    pub stats: bool,
    /// 视频通道（M2）
    pub video: VideoChoice,
    /// 视频码率（bit/s）
    pub video_bitrate: u32,
    /// 视频模式帧率上限
    pub video_max_fps: u32,
    /// 链路预算（bit/s）；0 = 不使用自适应控制器（保持固定参数）
    pub budget_bps: u32,
    /// 是否启用系统声音回环（M3 音频通道），默认 true
    pub audio: bool,
    /// relay 信令地址；None = 不启用 P2P（行为与 M3 完全一致）
    pub signal: Option<std::net::SocketAddr>,
    /// 房间号
    pub room: u32,
    /// P2P 路径选择
    pub p2p: P2pMode,
}

/// 解析结果：要么是帮助，要么是运行参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    Help,
    Run(Args),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// 选项后面没有值
    MissingValue(String),
    /// 不认识的选项
    UnknownFlag(String),
    /// 数值越界/不是数字
    BadValue {
        flag: String,
        value: String,
        expect: &'static str,
    },
    /// 既没给 --password 也没有 LRC_PASSWORD
    MissingPassword,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::MissingValue(flag) => write!(f, "选项 {flag} 缺少取值"),
            CliError::UnknownFlag(flag) => write!(f, "未知选项 {flag}"),
            CliError::BadValue {
                flag,
                value,
                expect,
            } => write!(f, "选项 {flag} 的取值 [{value}] 非法，应为{expect}"),
            CliError::MissingPassword => {
                write!(f, "缺少 --password（或环境变量 LRC_PASSWORD）")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// 解析命令行；`env_password` 用于在缺省 `--password` 时兜底（避免密码出现在进程列表里）。
pub fn parse_with(argv: &[String], env_password: Option<&str>) -> Result<Cli, CliError> {
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut password: Option<String> = None;
    let mut display = 0usize;
    let mut fps = DEFAULT_FPS;
    let mut quality = DEFAULT_QUALITY;
    let mut stats = false;
    let mut video = VideoChoice::Auto;
    let mut video_bitrate = DEFAULT_VIDEO_BITRATE;
    let mut video_max_fps = DEFAULT_VIDEO_MAX_FPS;
    let mut budget_bps = 0u32;
    let mut audio = true;
    let mut signal: Option<std::net::SocketAddr> = None;
    let mut room: u32 = 1;
    let mut p2p = P2pMode::Auto;

    let mut i = 0usize;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg, None),
        };
        match name {
            "-h" | "--help" | "/?" => return Ok(Cli::Help),
            "--stats" => {
                stats = !matches!(inline.as_deref(), Some("0") | Some("false") | Some("no"));
            }
            "--no-stats" => stats = false,
            "--listen" | "--password" | "--display" | "--fps" | "--quality" | "--video"
            | "--video-bitrate" | "--video-max-fps" | "--budget-bps" | "--audio" | "--signal"
            | "--room" | "--p2p" => {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        match argv.get(i) {
                            Some(v) => v.clone(),
                            None => return Err(CliError::MissingValue(name.to_string())),
                        }
                    }
                };
                match name {
                    "--audio" => {
                        audio = match value.as_str() {
                            "on" | "1" | "true" | "yes" => true,
                            "off" | "0" | "false" | "no" => false,
                            _ => {
                                return Err(CliError::BadValue {
                                    flag: name.to_string(),
                                    value,
                                    expect: "on|off",
                                })
                            }
                        }
                    }
                    "--signal" => {
                        signal = Some(value.parse::<std::net::SocketAddr>().map_err(|_| {
                            CliError::BadValue {
                                flag: name.to_string(),
                                value,
                                expect: "IP:PORT（例如 1.2.3.4:7801）",
                            }
                        })?);
                    }
                    "--room" => {
                        let v = parse_usize(name, &value)?;
                        room = u32::try_from(v).map_err(|_| CliError::BadValue {
                            flag: name.to_string(),
                            value,
                            expect: "0..=4294967295",
                        })?;
                    }
                    "--p2p" => {
                        p2p = match value.as_str() {
                            "auto" => P2pMode::Auto,
                            "off" | "none" | "0" => P2pMode::Off,
                            "direct" => P2pMode::Direct,
                            "relay" => P2pMode::Relay,
                            _ => {
                                return Err(CliError::BadValue {
                                    flag: name.to_string(),
                                    value,
                                    expect: "auto|off|direct|relay",
                                })
                            }
                        };
                    }
                    "--listen" => {
                        if value.trim().is_empty() {
                            return Err(CliError::BadValue {
                                flag: name.to_string(),
                                value,
                                expect: "IP:PORT",
                            });
                        }
                        listen = value;
                    }
                    "--password" => password = Some(value),
                    "--display" => display = parse_usize(name, &value)?,
                    "--fps" => {
                        let v = parse_usize(name, &value)?;
                        if v == 0 || v > MAX_FPS as usize {
                            return Err(CliError::BadValue {
                                flag: name.to_string(),
                                value,
                                expect: "1..=240",
                            });
                        }
                        fps = v as u32;
                    }
                    "--quality" => {
                        let v = parse_usize(name, &value)?;
                        if !(1..=100).contains(&v) {
                            return Err(CliError::BadValue {
                                flag: name.to_string(),
                                value,
                                expect: "1..=100",
                            });
                        }
                        quality = v as u8;
                    }
                    "--video" => {
                        video = match value.as_str() {
                            "auto" => VideoChoice::Auto,
                            "off" | "none" | "0" => VideoChoice::Off,
                            "hevc" | "h265" | "hevc_nvenc" => VideoChoice::Hevc,
                            "h264" | "avc" | "h264_nvenc" => VideoChoice::H264,
                            _ => {
                                return Err(CliError::BadValue {
                                    flag: name.to_string(),
                                    value,
                                    expect: "auto|off|hevc|h264",
                                })
                            }
                        };
                    }
                    "--video-bitrate" => {
                        let v = parse_usize(name, &value)?;
                        let v = u32::try_from(v).ok().filter(|v| {
                            VIDEO_BITRATE_RANGE.contains(v)
                        });
                        match v {
                            Some(v) => video_bitrate = v,
                            None => {
                                return Err(CliError::BadValue {
                                    flag: name.to_string(),
                                    value,
                                    expect: "100000..=80000000",
                                })
                            }
                        }
                    }
                    "--budget-bps" => {
                        let v = parse_usize(name, &value)?;
                        let v = u32::try_from(v).ok().filter(|v| {
                            *v == 0 || VIDEO_BITRATE_RANGE.contains(v)
                        });
                        match v {
                            Some(v) => budget_bps = v,
                            None => {
                                return Err(CliError::BadValue {
                                    flag: name.to_string(),
                                    value,
                                    expect: "0（关闭）或 100000..=80000000",
                                })
                            }
                        }
                    }
                    "--video-max-fps" => {
                        let v = parse_usize(name, &value)?;
                        if v == 0 || v > MAX_FPS as usize {
                            return Err(CliError::BadValue {
                                flag: name.to_string(),
                                value,
                                expect: "1..=240",
                            });
                        }
                        video_max_fps = v as u32;
                    }
                    _ => unreachable!(),
                }
            }
            other => return Err(CliError::UnknownFlag(other.to_string())),
        }
        i += 1;
    }

    let password = match password.or_else(|| env_password.map(str::to_string)) {
        Some(p) if !p.is_empty() => p,
        _ => return Err(CliError::MissingPassword),
    };

    Ok(Cli::Run(Args {
        listen,
        password,
        display,
        fps,
        quality,
        stats,
        video,
        video_bitrate,
        video_max_fps,
        budget_bps,
        audio,
        signal,
        room,
        p2p,
    }))
}

fn parse_usize(flag: &str, value: &str) -> Result<usize, CliError> {
    value
        .trim()
        .parse::<usize>()
        .map_err(|_| CliError::BadValue {
            flag: flag.to_string(),
            value: value.to_string(),
            expect: "非负整数",
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn parse(items: &[String]) -> Result<Cli, CliError> {
        parse_with(items, None)
    }

    fn run(items: &[&str]) -> Args {
        match parse(&argv(items)) {
            Ok(Cli::Run(a)) => a,
            other => panic!("期待 Run，得到 {other:?}"),
        }
    }

    #[test]
    fn defaults_with_only_password() {
        let a = run(&["--password", "1234"]);
        assert_eq!(a.listen, DEFAULT_LISTEN);
        assert_eq!(a.password, "1234");
        assert_eq!(a.display, 0);
        assert_eq!(a.fps, DEFAULT_FPS);
        assert_eq!(a.quality, DEFAULT_QUALITY);
        assert!(!a.stats);
        assert_eq!(a.video, VideoChoice::Auto);
        assert_eq!(a.video_bitrate, DEFAULT_VIDEO_BITRATE);
        assert_eq!(a.video_max_fps, DEFAULT_VIDEO_MAX_FPS);
        assert_eq!(a.budget_bps, 0);
        assert!(a.audio, "音频默认开启");
    }

    #[test]
    fn p2p_flags() {
        let a = run(&["--password", "1"]);
        assert_eq!(a.signal, None, "不给 --signal 就不启用 P2P");
        assert_eq!(a.room, 1);
        assert_eq!(a.p2p, P2pMode::Auto);

        let a = run(&[
            "--password", "1", "--signal", "127.0.0.1:7801", "--room", "42", "--p2p", "relay",
        ]);
        assert_eq!(a.signal, Some("127.0.0.1:7801".parse().expect("addr")));
        assert_eq!(a.room, 42);
        assert_eq!(a.p2p, P2pMode::Relay);

        // 别名 + equals 形式
        assert_eq!(run(&["--password", "1", "--p2p=off"]).p2p, P2pMode::Off);
        assert_eq!(run(&["--password", "1", "--p2p", "none"]).p2p, P2pMode::Off);
        assert_eq!(run(&["--password", "1", "--p2p", "direct"]).p2p, P2pMode::Direct);

        // 非法值
        assert!(matches!(
            parse(&argv(&["--password", "1", "--p2p", "holepunch"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--signal", "not-an-addr"])),
            Err(CliError::BadValue { .. })
        ));
    }

    #[test]
    fn audio_flag() {
        assert!(!run(&["--password", "1", "--audio", "off"]).audio);
        assert!(!run(&["--password", "1", "--audio=off"]).audio);
        assert!(run(&["--password", "1", "--audio", "on"]).audio);
        assert!(matches!(
            parse(&argv(&["--password", "1", "--audio", "loud"])),
            Err(CliError::BadValue { .. })
        ));
    }

    #[test]
    fn budget_flag() {
        let a = run(&["--password", "1", "--budget-bps", "2000000"]);
        assert_eq!(a.budget_bps, 2_000_000);
        let a = run(&["--password", "1", "--budget-bps=0"]);
        assert_eq!(a.budget_bps, 0);
        assert!(matches!(
            parse(&argv(&["--password", "1", "--budget-bps", "1234"])),
            Err(CliError::BadValue { .. })
        ));
    }

    #[test]
    fn video_flags() {
        let a = run(&["--password", "1", "--video", "off"]);
        assert_eq!(a.video, VideoChoice::Off);
        assert!(a.video.is_off());
        let a = run(&["--password", "1", "--video=h264", "--video-bitrate", "8000000", "--video-max-fps", "60"]);
        assert_eq!(a.video, VideoChoice::H264);
        assert_eq!(a.video_bitrate, 8_000_000);
        assert_eq!(a.video_max_fps, 60);
        // 别名
        assert_eq!(run(&["--password", "1", "--video", "h265"]).video, VideoChoice::Hevc);
        assert_eq!(run(&["--password", "1", "--video", "avc"]).video, VideoChoice::H264);
        // 非法值
        assert!(matches!(
            parse(&argv(&["--password", "1", "--video", "vp9"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--video-bitrate", "10"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--video-max-fps", "0"])),
            Err(CliError::BadValue { .. })
        ));
    }

    #[test]
    fn full_command_line() {
        let a = run(&[
            "--listen",
            "127.0.0.1:9000",
            "--password",
            "p@ss",
            "--display",
            "1",
            "--fps",
            "60",
            "--quality",
            "70",
            "--stats",
        ]);
        assert_eq!(a.listen, "127.0.0.1:9000");
        assert_eq!(a.password, "p@ss");
        assert_eq!(a.display, 1);
        assert_eq!(a.fps, 60);
        assert_eq!(a.quality, 70);
        assert!(a.stats);
    }

    #[test]
    fn equals_form_is_accepted() {
        let a = run(&["--password=1234", "--fps=24", "--display=2", "--quality=50"]);
        assert_eq!(a.password, "1234");
        assert_eq!(a.fps, 24);
        assert_eq!(a.display, 2);
        assert_eq!(a.quality, 50);
    }

    #[test]
    fn help_variants() {
        for flag in ["-h", "--help", "/?"] {
            assert_eq!(parse(&argv(&[flag])), Ok(Cli::Help));
        }
    }

    #[test]
    fn unknown_flag_rejected() {
        assert_eq!(
            parse(&argv(&["--password", "1", "--bogus"])),
            Err(CliError::UnknownFlag("--bogus".to_string()))
        );
    }

    #[test]
    fn missing_value_rejected() {
        assert_eq!(
            parse(&argv(&["--password"])),
            Err(CliError::MissingValue("--password".to_string()))
        );
    }

    #[test]
    fn out_of_range_rejected() {
        assert!(matches!(
            parse(&argv(&["--password", "1", "--fps", "0"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--fps", "241"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--quality", "101"])),
            Err(CliError::BadValue { .. })
        ));
        assert!(matches!(
            parse(&argv(&["--password", "1", "--fps", "abc"])),
            Err(CliError::BadValue { .. })
        ));
    }

    #[test]
    fn password_from_env() {
        let a = match parse_with(&argv(&["--fps", "20"]), Some("envpw")) {
            Ok(Cli::Run(a)) => a,
            other => panic!("期待 Run，得到 {other:?}"),
        };
        assert_eq!(a.password, "envpw");
        assert_eq!(a.fps, 20);

        // 命令行优先于环境变量
        let a = match parse_with(&argv(&["--password", "cli"]), Some("envpw")) {
            Ok(Cli::Run(a)) => a,
            other => panic!("期待 Run，得到 {other:?}"),
        };
        assert_eq!(a.password, "cli");

        // 都没有 → 报错
        assert_eq!(parse_with(&argv(&[]), None), Err(CliError::MissingPassword));
        assert_eq!(
            parse_with(&argv(&[]), Some("")),
            Err(CliError::MissingPassword)
        );
    }

    #[test]
    fn stats_can_be_disabled_explicitly() {
        let a = run(&["--password", "1", "--stats=false"]);
        assert!(!a.stats);
        let a = run(&["--password", "1", "--stats", "--no-stats"]);
        assert!(!a.stats);
    }
}
