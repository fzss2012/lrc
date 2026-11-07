//! 视频通道（M2）：把整帧 BGRA 送进硬件编码器（NVENC），产出低延迟码流；
//! 客户端侧为对应解码器。目标是替换"视频/照片类内容"的块 JPEG 路径。
//!
//! 由 windows-gfx 实现（spike 阶段）。接口先放这里，host/client 下一轮接入。

pub mod ffmpeg;

pub use ffmpeg::{DecoderConfig, FfmpegDecoder, FfmpegEncoder, resolve_ffmpeg};

use std::fmt;

/// 编码器配置。
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// 目标码率（比特/秒）
    pub bitrate_bps: u32,
    /// 关键帧间隔（帧）
    pub gop: u32,
    /// 目标帧率上限
    pub fps: u32,
    /// 编码器名（ffmpeg 的 -c:v），默认 "hevc_nvenc"
    pub codec: String,
    /// ffmpeg 可执行文件；None 表示走 PATH
    pub ffmpeg_path: Option<String>,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            gop: 60,
            fps: 30,
            codec: "hevc_nvenc".to_string(),
            ffmpeg_path: None,
        }
    }
}

#[derive(Debug)]
pub enum VideoError {
    Io(String),
    Spawn(String),
    Protocol(String),
    Unsupported(&'static str),
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoError::Io(m) => write!(f, "video: IO 错误 {m}"),
            VideoError::Spawn(m) => write!(f, "video: 启动编码器失败 {m}"),
            VideoError::Protocol(m) => write!(f, "video: 码流格式错误 {m}"),
            VideoError::Unsupported(m) => write!(f, "video: 不支持 {m}"),
        }
    }
}

impl std::error::Error for VideoError {}

pub type VideoResult<T> = Result<T, VideoError>;

/// 编码器：输入一帧 BGRA，输出零或多个"访问单元"（完整可解码帧）。
pub trait VideoEncoder: Send {
    /// 已编码的码流 keyframe 间隔外的参考帧情况等由实现决定。
    fn encode(&mut self, bgra: &[u8], stride: usize) -> VideoResult<Vec<Vec<u8>>>;
    /// 请求下一帧为 IDR。
    fn request_keyframe(&mut self) -> VideoResult<()>;
    fn codec_name(&self) -> &str;
    fn shutdown(&mut self);
}

/// 解码器：输入一个访问单元，输出 BGRA 帧。
pub trait VideoDecoder: Send {
    fn decode(&mut self, au: &[u8]) -> VideoResult<Option<Vec<u8>>>;
    fn shutdown(&mut self);
}
