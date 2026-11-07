//! ffmpeg 子进程实现（NVENC 硬编 / 硬解）。
//!
//! ## 码流分帧怎么解决的
//!
//! h264/hevc elementary stream 没有容器边界，编码器输出是一串 Annex-B 字节。做法：
//!
//! 1. 编码器进程带 `-zerolatency 1 -flush_packets 1 -bf 0`，**编码器内部零延迟**：
//!    写进去一帧，它就吐且只吐这一帧的字节（不会攒包）。
//! 2. 每次 `encode()` 写完这一帧后，把管道里**当前所有可用字节全部收完**：
//!    先阻塞等第一块数据，然后循环读到「连续 `idle_us` 微秒没有新数据」为止。
//!    因为每次 encode 结束都把管道抽干，所以这一批字节 = 这一帧的访问单元（AU）。
//! 3. 再按 Annex-B start code 扫一遍 NAL，做两个校验/标记：
//!    - 必须至少有一个 VCL NAL（否则说明这一帧没编出来）；
//!    - 是不是 IDR（keyframe），给上层做「客户端从哪里开始能解码」。
//!
//! 为了边界更明确，编码器还加了 `-bsf:v hevc_metadata=aud=insert`（h264 对应
//! `h264_metadata=aud=insert`）在每帧前插一个 AUD NAL——这样 AU 一定以
//! `00 00 01 46 01 50`（HEVC）/ `00 00 01 09 F0`（H.264）开头，可以断言。
//!
//! 解码方向不需要分帧：rawvideo 输出是定长的（W*H*4 字节），读满一帧就是一张图。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{EncoderConfig, VideoDecoder, VideoEncoder, VideoError, VideoResult};

/// 已知的 ffmpeg 安装位置（找不到再退回 PATH）。
const KNOWN_FFMPEG: &str = r"C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe";
/// 等第一块码流的上限（首帧要等 NVENC 初始化 ~300ms，之后每帧 10ms 内）
const FIRST_DATA_TIMEOUT_MS: u64 = 2000;
/// 解码一帧的上限
const DECODE_TIMEOUT_MS: u64 = 400;
/// 收尾时等子进程自己退出的时间，超时就 kill
const EXIT_GRACE_MS: u64 = 1500;

fn env_us(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// ffmpeg 可执行文件：显式配置 > 环境变量 LRC_FFMPEG > 已知路径 > PATH。
pub fn resolve_ffmpeg(configured: Option<&str>) -> PathBuf {
    if let Some(p) = configured {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("LRC_FFMPEG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let known = Path::new(KNOWN_FFMPEG);
    if known.is_file() {
        return known.to_path_buf();
    }
    PathBuf::from("ffmpeg")
}

fn is_hevc(codec: &str) -> bool {
    codec.contains("hevc") || codec.contains("265")
}

/// 编码器命令行（也用于 probe 打印，保证「我说用的参数」和「实际跑的」一致）。
pub fn encode_command_line(
    cfg: &EncoderConfig,
    width: u32,
    height: u32,
    ffmpeg: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "-s".into(),
        format!("{width}x{height}"),
        "-framerate".into(),
        cfg.fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-c:v".into(),
        cfg.codec.clone(),
        "-preset".into(),
        "p1".into(),
        "-tune".into(),
        "ll".into(),
        "-rc".into(),
        "cbr".into(),
        "-b:v".into(),
        cfg.bitrate_bps.to_string(),
        "-maxrate".into(),
        cfg.bitrate_bps.to_string(),
        "-bufsize".into(),
        (u64::from(cfg.bitrate_bps) / u64::from(cfg.fps.max(1))).max(1).to_string(),
        "-g".into(),
        cfg.gop.to_string(),
        "-bf".into(),
        "0".into(),
        "-zerolatency".into(),
        "1".into(),
        // 关键：nvenc 的 -delay 默认是 INT_MAX（= 最大异步深度），会让输出晚 2 帧才出来，
        // 表现就是"写进去两三帧才吐第一帧"。必须显式 0，才能做到写完一帧就出这一帧。
        "-delay".into(),
        "0".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-flush_packets".into(),
        "1".into(),
        "-fflags".into(),
        "+nobuffer".into(),
        "-flags".into(),
        "+low_delay".into(),
        "-f".into(),
        if is_hevc(&cfg.codec) { "hevc".into() } else { "h264".into() },
        "pipe:1".into(),
    ];
    // 第一个元素是 ffmpeg 自身路径
    a.insert(0, ffmpeg.to_string_lossy().into_owned());
    a
}

/// 解码器命令行。
pub fn decode_command_line(codec: &str, ffmpeg: &Path) -> Vec<String> {
    vec![
        ffmpeg.to_string_lossy().into_owned(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        // ⚠ 不能加 -fflags +nobuffer：解码端加了它会把第一个访问单元（IDR）在探测阶段丢掉，
        // 之后所有 P 帧都报 "Could not find ref with POC N"（实测 A/B：加了 0 帧，不加 1 帧）。
        // 缩短探测：不加 -analyzeduration 0 时解码器会等更多数据才出第一帧
        "-analyzeduration".into(),
        "0".into(),
        "-probesize".into(),
        "32".into(),
        // 关键：默认帧级多线程会让解码器先攒 N 帧才出画面（活管道上看起来就是"永远没输出"）
        "-threads".into(),
        "1".into(),
        // 注意：不要用 -c:v hevc_cuvid（NVDEC）。实测它内部攒帧，
        // 在"一次喂一个 AU、同步等画面"的模式下会卡住；软解 -threads 1 才是确定性的。
        "-f".into(),
        if is_hevc(codec) { "hevc".into() } else { "h264".into() },
        "-i".into(),
        "pipe:0".into(),
        // 输出侧保持最简：不要 -fps_mode/-r。-r/-fps_mode 在"输入无帧率 + 活管道"的组合下
        // 会把帧静默丢掉（实测 0 输出）；-r 与 -fps_mode 又会互相冲突。
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "pipe:1".into(),
    ]
}

enum Msg {
    Data(Vec<u8>),
    Eof(String),
}

/// 一个带 stdin/stdout 管道 + 后台读线程 + stderr 尾部的子进程。
struct PipeChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Option<Receiver<Msg>>,
    reader: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<String>>,
    what: String,
    closed: bool,
}

impl PipeChild {
    fn spawn(cmdline: &[String], what: &str) -> VideoResult<Self> {
        let (prog, args) = cmdline
            .split_first()
            .ok_or_else(|| VideoError::Spawn(format!("{what}: 空命令行")))?;
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // 不留控制台窗口
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| VideoError::Spawn(format!("{what}: 启动 {} 失败: {e}", prog)))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| VideoError::Spawn(format!("{what}: 拿不到 stdout")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| VideoError::Spawn(format!("{what}: 拿不到 stderr")))?;

        let (tx, rx) = channel::<Msg>();
        let reader = thread::Builder::new()
            .name(format!("{what}-out"))
            .spawn(move || {
                let mut stdout = stdout;
                let mut buf = vec![0u8; 128 * 1024];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(Msg::Eof("stdout EOF".into()));
                            break;
                        }
                        Ok(n) => {
                            if tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Eof(format!("读 stdout 失败: {e}")));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| VideoError::Spawn(format!("{what}: 起读线程失败: {e}")))?;

        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = stderr_tail.clone();
        let stderr_thread = thread::Builder::new()
            .name(format!("{what}-err"))
            .spawn(move || {
                let mut stderr = stderr;
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                            if let Ok(mut t) = tail.lock() {
                                t.push_str(&text);
                                if t.len() > 8192 {
                                    let cut = t.len().saturating_sub(4096);
                                    // 只保留尾部，避免无限增长
                                    let tail_str = t[cut..].to_string();
                                    *t = tail_str;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

        Ok(Self {
            child,
            stdin,
            rx: Some(rx),
            reader: Some(reader),
            stderr_thread,
            stderr_tail,
            what: what.to_string(),
            closed: false,
        })
    }

    fn stderr_snippet(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|t| t.trim().to_string())
            .unwrap_or_default()
    }

    fn write_frame(&mut self, data: &[u8]) -> VideoResult<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| VideoError::Io(format!("{}: stdin 已关闭", self.what)))?;
        stdin
            .write_all(data)
            .map_err(|e| VideoError::Io(format!("{}: 写 stdin 失败: {e}", self.what)))?;
        // rawvideo 必须整帧到齐，ffmpeg 才会处理；不 flush 会卡在 BufWriter 里。
        // ChildStdin 没有 BufWriter（是裸 File），write_all 已经直接进管道，无需 flush。
        Ok(())
    }

    /// 先阻塞等第一块，再把「连续 idle 时间内到达的」全部收完。
    fn read_burst(&mut self, idle: Duration, first_timeout: Duration) -> VideoResult<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let deadline = Instant::now() + first_timeout;
        let rx = self
            .rx
            .as_ref()
            .ok_or_else(|| VideoError::Io(format!("{}: 管道已关闭", self.what)))?;

        let mut got_first = false;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            let wait = if got_first { idle } else { remain };
            if wait.is_zero() {
                break;
            }
            match rx.recv_timeout(wait) {
                Ok(Msg::Data(d)) => {
                    out.extend_from_slice(&d);
                    got_first = true;
                }
                Ok(Msg::Eof(why)) => {
                    if out.is_empty() {
                        let tail = self.stderr_snippet();
                        return Err(VideoError::Io(format!(
                            "{}: 提前退出（{why}）{}",
                            self.what,
                            if tail.is_empty() {
                                String::new()
                            } else {
                                format!(": {tail}")
                            }
                        )));
                    }
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if !got_first && out.is_empty() {
            let tail = self.stderr_snippet();
            return Err(VideoError::Io(format!(
                "{}: {}ms 内没有输出{}",
                self.what,
                first_timeout.as_millis(),
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            )));
        }
        Ok(out)
    }

    /// 读满 n 字节（用于解码定长 rawvideo 输出）。
    fn read_exact_bytes(&mut self, n: usize, timeout: Duration) -> VideoResult<Vec<u8>> {
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let deadline = Instant::now() + timeout;
        let rx = self
            .rx
            .as_ref()
            .ok_or_else(|| VideoError::Io(format!("{}: 管道已关闭", self.what)))?;
        while out.len() < n {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                let tail = self.stderr_snippet();
                return Err(VideoError::Io(format!(
                    "{}: 解码超时（已有 {}/{} 字节）{}",
                    self.what,
                    out.len(),
                    n,
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(": {tail}")
                    }
                )));
            }
            match rx.recv_timeout(remain) {
                Ok(Msg::Data(d)) => out.extend_from_slice(&d),
                Ok(Msg::Eof(why)) => {
                    let tail = self.stderr_snippet();
                    return Err(VideoError::Io(format!(
                        "{}: 解码器退出（{why}, {}/{} 字节）{}",
                        self.what,
                        out.len(),
                        n,
                        if tail.is_empty() {
                            String::new()
                        } else {
                            format!(": {tail}")
                        }
                    )));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if out.len() != n {
            out.truncate(n);
        }
        Ok(out)
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // 1) 关 stdin：ffmpeg 读到 EOF 会自己收尾退出
        if let Some(mut si) = self.stdin.take() {
            let _ = si.flush();
            drop(si);
        }
        self.rx = None;
        // 2) 给它一点时间自己退
        let deadline = Instant::now() + Duration::from_millis(EXIT_GRACE_MS);
        let mut exited = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        // 3) 还不走就杀
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // 4) 回收线程（进程已死，管道会 EOF）
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PipeChild {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn au_is_keyframe(au: &[u8], hevc: bool) -> bool {
    let mut i = 0usize;
    let n = au.len();
    while i + 3 < n {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let hdr = i + 3;
            if hdr < n {
                if hevc {
                    let t = (au[hdr] >> 1) & 0x3F;
                    // 16..=21 都是 IRAP（BLA/IDR/CRA）
                    if (16..=21).contains(&t) {
                        return true;
                    }
                } else {
                    let t = au[hdr] & 0x1F;
                    if t == 5 {
                        return true;
                    }
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

fn au_has_vcl(au: &[u8], hevc: bool) -> bool {
    let mut i = 0usize;
    let n = au.len();
    while i + 3 < n {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let hdr = i + 3;
            if hdr < n {
                if hevc {
                    let t = (au[hdr] >> 1) & 0x3F;
                    if t < 32 {
                        return true;
                    }
                } else {
                    let t = au[hdr] & 0x1F;
                    if (1..=5).contains(&t) {
                        return true;
                    }
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EncoderConfig {
        EncoderConfig {
            bitrate_bps: 4_000_000,
            gop: 60,
            fps: 30,
            codec: "hevc_nvenc".into(),
            ffmpeg_path: None,
        }
    }

    #[test]
    fn encoder_cmdline_has_low_latency_flags() {
        let line = encode_command_line(&cfg(), 1920, 1080, Path::new("ffmpeg")).join(" ");
        // 这几个参数少一个延迟/帧率就会崩，必须钉住
        assert!(line.contains("-delay 0"), "缺 -delay 0（nvenc 异步深度）");
        assert!(line.contains("-zerolatency 1"));
        assert!(line.contains("-bf 0"));
        assert!(line.contains("-flush_packets 1"));
        assert!(line.contains("-s 1920x1080"));
        assert!(line.contains("-f hevc pipe:1"));
        assert!(!line.contains("aud=insert"), "AUD bsf 会改变分帧，不该再用");
    }

    #[test]
    fn decoder_cmdline_has_single_thread() {
        let line = decode_command_line("hevc", Path::new("ffmpeg")).join(" ");
        // -threads 1：帧级多线程会攒帧，活管道上看起来就是"永远没有输出"
        assert!(line.contains("-threads 1"));
        assert!(!line.contains("+nobuffer"), "解码端加 nobuffer 会丢第一个 AU");
        assert!(line.contains("-probesize 32"));
        assert!(line.contains("-f rawvideo"));
    }

    #[test]
    fn h264_and_hevc_use_different_demuxers() {
        let hevc = encode_command_line(&cfg(), 640, 480, Path::new("ffmpeg")).join(" ");
        let mut c = cfg();
        c.codec = "h264_nvenc".into();
        let h264 = encode_command_line(&c, 640, 480, Path::new("ffmpeg")).join(" ");
        assert!(hevc.contains("-f hevc"));
        assert!(h264.contains("-f h264"));
        assert!(decode_command_line("hevc", Path::new("f")).join(" ").contains("-f hevc"));
        assert!(decode_command_line("h264", Path::new("f")).join(" ").contains("-f h264"));
    }

    /// 一个最小的 HEVC AU：VPS/SPS/PPS + IDR slice
    fn hevc_au(idr: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for t in [32u8, 33, 34] {
            v.extend_from_slice(&[0, 0, 0, 1, t << 1, 0x01]);
        }
        let nal_type: u8 = if idr { 19 } else { 1 };
        v.extend_from_slice(&[0, 0, 0, 1, nal_type << 1, 0x01, 0xAA]);
        v
    }

    #[test]
    fn keyframe_and_vcl_detection() {
        let idr = hevc_au(true);
        let p = hevc_au(false);
        assert!(au_has_vcl(&idr, true));
        assert!(au_has_vcl(&p, true));
        assert!(au_is_keyframe(&idr, true));
        assert!(!au_is_keyframe(&p, true));
        // 只有参数集、没有 slice
        let params_only = vec![0u8, 0, 0, 1, 32 << 1, 1, 0, 0, 1, 33 << 1, 1];
        assert!(!au_has_vcl(&params_only, true));
    }
}

/// 用 ffmpeg + NVENC 的编码器。
pub struct FfmpegEncoder {
    proc: PipeChild,
    cfg: EncoderConfig,
    cmdline: Vec<String>,
    width: u32,
    height: u32,
    frame_bytes: usize,
    pack: Vec<u8>,
    idle: Duration,
    last_keyframe: bool,
    au_count: u64,
    force_keyframe: bool,
}

impl FfmpegEncoder {
    /// 注意：契约里的 @@EncoderConfig@@ 不含尺寸，所以宽高单独传（已在汇报里跟 Lead 提了要不要补进 config）。
    pub fn new(width: u32, height: u32, cfg: EncoderConfig) -> VideoResult<Self> {
        if width == 0 || height == 0 {
            return Err(VideoError::Unsupported("宽高不能为 0"));
        }
        let ffmpeg = resolve_ffmpeg(cfg.ffmpeg_path.as_deref());
        let cmdline = encode_command_line(&cfg, width, height, &ffmpeg);
        let proc = PipeChild::spawn(&cmdline, "encoder")?;
        let frame_bytes = width as usize * height as usize * 4;
        Ok(Self {
            proc,
            width,
            height,
            frame_bytes,
            cfg,
            cmdline,
            pack: Vec::new(),
            // 收尾静默窗口：太短会把一个 AU 切成两半，太长白加延迟
            idle: Duration::from_micros(env_us("LRC_VIDEO_IDLE_US", 600)),
            last_keyframe: false,
            au_count: 0,
            force_keyframe: false,
        })
    }

    pub fn command_line(&self) -> String {
        self.cmdline.join(" ")
    }

    /// 上一个 AU 是不是关键帧（决定客户端能不能从这里开始解码）。
    pub fn last_au_was_keyframe(&self) -> bool {
        self.last_keyframe
    }

    pub fn au_count(&self) -> u64 {
        self.au_count
    }

    pub fn stderr_tail(&self) -> String {
        self.proc.stderr_snippet()
    }

}

impl VideoEncoder for FfmpegEncoder {
    fn encode(&mut self, bgra: &[u8], stride: usize) -> VideoResult<Vec<Vec<u8>>> {
        // 「立刻要关键帧」的做法：重启编码进程（新进程第一帧必然是 IDR）。
        // 代价 ~300ms，只在客户端请求刷新时发生；spike 阶段够用。
        if std::mem::take(&mut self.force_keyframe) {
            self.proc.shutdown();
            self.proc = PipeChild::spawn(&self.cmdline, "encoder")?;
        }
        // stride == width*4 是常见路径，直接喂（不拷）；否则逐行打包到复用缓冲。
        let row = self.width as usize * 4;
        let need = self.frame_bytes;
        if stride == row {
            let data = bgra.get(..need).ok_or_else(|| {
                VideoError::Protocol(format!("帧数据不足: {} < {need}", bgra.len()))
            })?;
            self.proc.write_frame(data)?;
        } else {
            let mut pack = std::mem::take(&mut self.pack);
            pack.resize(need, 0);
            for y in 0..self.height as usize {
                let s = y * stride;
                let d = y * row;
                let Some(src) = bgra.get(s..s + row) else {
                    self.pack = pack;
                    return Err(VideoError::Protocol("帧数据不足（stride 打包）".into()));
                };
                pack[d..d + row].copy_from_slice(src);
            }
            self.proc.write_frame(&pack)?;
            self.pack = pack;
        }
        let first_ms = env_us("LRC_VIDEO_FIRST_MS", FIRST_DATA_TIMEOUT_MS);
        let t_first = Instant::now();
        let au = self.proc.read_burst(self.idle, Duration::from_millis(first_ms))?;
        if std::env::var("LRC_VIDEO_DEBUG").is_ok() {
            eprintln!(
                "[video-debug] 写帧→拿到首个 AU: {}us, {} 字节, idle={}us",
                t_first.elapsed().as_micros(),
                au.len(),
                self.idle.as_micros()
            );
        }
        if au.is_empty() {
            return Ok(Vec::new());
        }
        let hevc = is_hevc(&self.cfg.codec);
        if !au_has_vcl(&au, hevc) {
            return Err(VideoError::Protocol(format!(
                "AU 里没有 VCL NAL（{} 字节），分帧可能被切断，试试调大 LRC_VIDEO_IDLE_US",
                au.len()
            )));
        }
        self.last_keyframe = au_is_keyframe(&au, hevc);
        self.au_count = self.au_count.saturating_add(1);
        Ok(vec![au])
    }

    fn request_keyframe(&mut self) -> VideoResult<()> {
        self.force_keyframe = true;
        Ok(())
    }

    fn codec_name(&self) -> &str {
        &self.cfg.codec
    }

    fn shutdown(&mut self) {
        self.proc.shutdown();
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        self.proc.shutdown();
    }
}

/// 解码器配置。
#[derive(Clone, Debug)]
pub struct DecoderConfig {
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub ffmpeg_path: Option<String>,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            codec: "hevc".to_string(),
            ffmpeg_path: None,
        }
    }
}

/// 用 ffmpeg 解码的播放端解码器。
pub struct FfmpegDecoder {
    proc: PipeChild,
    cmdline: Vec<String>,
    frame_bytes: usize,
    frames: u64,
    timeout: Duration,
    hevc: bool,
    no_aud: bool,
    got_first: bool,
}

impl FfmpegDecoder {
    pub fn new(cfg: &DecoderConfig) -> VideoResult<Self> {
        let ffmpeg = resolve_ffmpeg(cfg.ffmpeg_path.as_deref());
        let cmdline = decode_command_line(&cfg.codec, &ffmpeg);
        let proc = PipeChild::spawn(&cmdline, "decoder")?;
        Ok(Self {
            proc,
            cmdline,
            frame_bytes: cfg.width as usize * cfg.height as usize * 4,
            frames: 0,
            timeout: Duration::from_millis(DECODE_TIMEOUT_MS),
            hevc: is_hevc(&cfg.codec),
            no_aud: std::env::var("LRC_VIDEO_NO_AUD").is_ok(),
            got_first: false,
        })
    }

    pub fn command_line(&self) -> String {
        self.cmdline.join(" ")
    }

    pub fn frame_count(&self) -> u64 {
        self.frames
    }

    pub fn stderr_tail(&self) -> String {
        self.proc.stderr_snippet()
    }
}

/// 追在 AU 末尾的 AUD（访问单元分隔符）。ffmpeg 的 h264/hevc **parser** 必须看到
/// 下一个 AU 的起点才肯把当前 AU 交给解码器——哪怕我们一次写进去的是一个完整 AU，
/// 它也会一直压着。写完 AU 再补一个 AUD NAL，parser 立刻就把当前 AU 吐出来了。
/// 那个 AUD 会被 parser 留作下个 packet 的前缀，正好是合法的 AU 开头。
const AUD_HEVC: [u8; 6] = [0x00, 0x00, 0x01, 0x46, 0x01, 0x50];
const AUD_H264: [u8; 5] = [0x00, 0x00, 0x01, 0x09, 0xF0];

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, au: &[u8]) -> VideoResult<Option<Vec<u8>>> {
        if au.is_empty() {
            return Ok(None);
        }
        let aud: &[u8] = if self.hevc { &AUD_HEVC } else { &AUD_H264 };
        self.proc.write_frame(au)?;
        if !self.no_aud {
            self.proc.write_frame(aud)?;
        }

        // ffmpeg 打开输入时会先跑一遍 avformat_find_stream_info：它会用一个**临时解码器**
        // 把这第一帧吃掉来拿参数，真正的解码器看不到这个 IDR，于是后面全部解不出来
        // （表现为 "Could not find ref with POC N" / 一直没输出）。
        // 对策：第一帧迟迟没出画面就把同一个 AU 再喂一次，让真解码器也拿到 IDR。
        if !self.got_first {
            match self.proc.read_exact_bytes(self.frame_bytes, Duration::from_millis(250)) {
                Ok(frame) => {
                    self.got_first = true;
                    self.frames = self.frames.saturating_add(1);
                    return Ok(Some(frame));
                }
                Err(_) => {
                    self.proc.write_frame(au)?;
                    if !self.no_aud {
                        self.proc.write_frame(aud)?;
                    }
                }
            }
        }
        let frame = self.proc.read_exact_bytes(self.frame_bytes, self.timeout)?;
        self.got_first = true;
        self.frames = self.frames.saturating_add(1);
        Ok(Some(frame))
    }

    fn shutdown(&mut self) {
        self.proc.shutdown();
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        self.proc.shutdown();
    }
}
