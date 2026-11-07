//! 音频通道（M3）：系统声音回环采集 → Opus 编码 → 传输 → 解码 → 播放。
//!
//! 由 wire-codec 实现。M3 只做**单向**（被控端 → 主控端），48 kHz / 16bit / 2ch。
//! 本机没有 cmake，所以 **Opus 走 ffmpeg 子进程**（与 lrc-video 同一套路），
//! 不引入需要本地编译的 libopus 绑定。
//!
//! # 线格式约定（M3）
//!
//! ffmpeg 的 opus 封装只有 Ogg（这个 ffmpeg 构建**没有 opus 解封装器**，
//! 所以解码必须用 -f ogg 而不能用 -f opus）。为了让每 20ms 一次 encode 就能拿到
//! 数据、且解码端能边收边解，本实现：
//!
//! - 编码器用  -page_duration 20000 让 ffmpeg **每 20ms 输出一个 Ogg 页**；
//! - 线上每个"音频包"就是**一个完整的 Ogg 页**（含 27 字节页头 + 段表 + Opus 载荷）；
//! - 解码器把收到的页原样喂给  -f ogg -i pipe:0，不需要自己拼容器。
//!
//! 代价是每包多 ~28 字节容器开销（32kbps/20ms 的包载荷 ~80 字节，约 +35%；
//! 64kbps 约 +17%）。要省掉它得在两端自己拆/拼 Ogg 页（页头 + CRC + 序号），
//! 属于 M3 之后的优化，先把链路跑通。
//!
//! # 设备
//!
//! 采集/播放都走 WASAPI 共享模式，使用设备的**混音格式**（通常是 48kHz 立体声
//! float32），内部转换成 48kHz/16bit/2ch。设备采样率不是 48kHz、或没有渲染
//! 端点时返回 AudioError::Device / Unsupported，调用方据此关闭音频通道。

use std::fmt;

pub(crate) mod ogg;
pub(crate) mod opus;
pub(crate) mod proc;
pub(crate) mod wasapi;

pub use opus::{OpusDecoder, OpusEncoder};
pub use wasapi::{LoopbackCapturer, Player};

/// 采集/播放格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

impl AudioFormat {
    /// 每 20ms 的采样点数
    pub fn frames_per_20ms(&self) -> usize {
        (self.sample_rate as usize / 1000) * 20
    }

    /// 每 20ms 的 s16le 字节数
    pub fn bytes_per_20ms(&self) -> usize {
        self.frames_per_20ms() * self.channels as usize * 2
    }

    /// 每秒 s16le 字节数
    pub fn bytes_per_sec(&self) -> usize {
        self.sample_rate as usize * self.channels as usize * 2
    }

    /// 每帧（一个采样点，含所有声道）的字节数
    pub fn block_align(&self) -> usize {
        self.channels as usize * 2
    }

    /// 只支持 48kHz / 立体声 / 16bit（M3 固定口径）。
    pub fn ensure_supported(&self) -> AudioResult<()> {
        if self.sample_rate != 48_000 {
            return Err(AudioError::Unsupported("只支持 48kHz"));
        }
        if self.channels != 1 && self.channels != 2 {
            return Err(AudioError::Unsupported("只支持 1/2 声道"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum AudioError {
    /// 设备不可用
    Device(String),
    /// 编解码失败
    Codec(String),
    /// 子进程/IO 失败
    Io(String),
    Unsupported(&'static str),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::Device(m) => write!(f, "audio: 设备错误 {m}"),
            AudioError::Codec(m) => write!(f, "audio: 编解码错误 {m}"),
            AudioError::Io(m) => write!(f, "audio: IO 错误 {m}"),
            AudioError::Unsupported(m) => write!(f, "audio: 不支持 {m}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<std::io::Error> for AudioError {
    fn from(e: std::io::Error) -> Self {
        AudioError::Io(e.to_string())
    }
}

pub type AudioResult<T> = Result<T, AudioError>;

/// 16bit 单声道/立体声（我们对外统一的 PCM 口径）。
pub(crate) fn s16_samples(pcm: &[u8]) -> impl Iterator<Item = i16> + '_ {
    pcm.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
}

/// 一段 s16le PCM 的 RMS（0..=1 归一化）、峰值、非零采样占比——自测/日志用。
pub fn pcm_stats(pcm: &[u8]) -> (f32, f32, f32) {
    let mut sum = 0.0f64;
    let mut peak = 0.0f32;
    let mut nonzero = 0usize;
    let mut n = 0usize;
    for s in s16_samples(pcm) {
        let v = f32::from(s) / 32768.0;
        sum += f64::from(v) * f64::from(v);
        if v.abs() > peak {
            peak = v.abs();
        }
        if s != 0 {
            nonzero += 1;
        }
        n += 1;
    }
    if n == 0 {
        return (0.0, 0.0, 0.0);
    }
    let rms = (sum / n as f64).sqrt() as f32;
    (rms, peak, nonzero as f32 / n as f32)
}

/// 两段 s16le PCM 的归一化互相关（在 max_lag 内找最佳对齐），返回 (相关, 滞后采样数)。
/// 自测用：编解码往返后判断"声音还在、只是有损"。
pub fn best_correlation(a: &[u8], b: &[u8], max_lag: usize) -> (f32, i32) {
    let sa: Vec<f32> = s16_samples(a).map(|v| f32::from(v) / 32768.0).collect();
    let sb: Vec<f32> = s16_samples(b).map(|v| f32::from(v) / 32768.0).collect();
    if sa.is_empty() || sb.is_empty() {
        return (0.0, 0);
    }
    // 只取中间一段做相关，避免首尾静音/裁切影响
    let win = sa.len().min(sb.len()).min(48_000);
    let a0 = (sa.len() - win) / 2;
    let b0 = (sb.len() - win) / 2;
    let mut best = (f32::NEG_INFINITY, 0i32);
    for lag in 0..=max_lag as i64 * 2 {
        let lag = lag - max_lag as i64;
        let mut dot = 0.0f64;
        let mut na = 0.0f64;
        let mut nb = 0.0f64;
        for i in 0..win {
            let bi = b0 as i64 + i as i64 + lag;
            if bi < 0 || bi >= sb.len() as i64 {
                continue;
            }
            let x = f64::from(sa[a0 + i]);
            let y = f64::from(sb[bi as usize]);
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        let denom = (na * nb).sqrt();
        let corr = if denom > 0.0 { (dot / denom) as f32 } else { 0.0 };
        if corr > best.0 {
            best = (corr, lag as i32);
        }
    }
    best
}
