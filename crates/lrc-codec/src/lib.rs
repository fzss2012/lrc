//! 块编码：无损（zstd + 左预测）/ 有损（JPEG）/ 原样。
//!
//! 契约见 `docs/DESIGN.md` §3。**本文件是三方共享的接口，签名不要随意改**，
//! 需要改先跟 Lead 对齐。

use std::fmt;

pub use lrc_proto::codec_tag;

#[derive(Debug)]
pub enum CodecError {
    /// 输入尺寸/步长不匹配
    BadGeometry { w: u32, h: u32, stride: usize, len: usize },
    /// 编码失败
    Encode(String),
    /// 解码失败（数据损坏）
    Decode(String),
    /// 未知 codec 标签
    UnknownCodec(u8),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::BadGeometry { w, h, stride, len } => {
                write!(f, "codec: 几何不匹配 {w}x{h} stride={stride} len={len}")
            }
            CodecError::Encode(m) => write!(f, "codec: 编码失败 {m}"),
            CodecError::Decode(m) => write!(f, "codec: 解码失败 {m}"),
            CodecError::UnknownCodec(t) => write!(f, "codec: 未知标签 {t}"),
        }
    }
}

impl std::error::Error for CodecError {}

pub type CodecResult<T> = Result<T, CodecError>;

/// 编码参数。
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// JPEG 质量 1..=100
    pub jpeg_quality: u8,
    /// zstd 等级
    pub zstd_level: i32,
    /// 无损结果若大于 raw 的这个倍数就改试有损
    pub lossless_ratio_limit: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            jpeg_quality: 85,
            zstd_level: 3,
            lossless_ratio_limit: 0.55,
        }
    }
}

/// 编码单块。`src` 为 BGRA，`stride` 为字节步长。
///
/// 返回实际使用的 `codec_tag`（可能比期望的更低，例如整块纯色直接无损）。
/// `out` 会被清空后写入编码数据。
pub fn encode_block(
    _src: &[u8],
    _stride: usize,
    _w: u32,
    _h: u32,
    _opts: &Options,
    _out: &mut Vec<u8>,
) -> CodecResult<u8> {
    todo!("T2 实现：无损 zstd+左预测 / JPEG / raw 三者取最小")
}

/// 解码单块到 BGRA 目标缓冲。
pub fn decode_block(
    _codec: u8,
    _data: &[u8],
    _w: u32,
    _h: u32,
    _dst: &mut [u8],
    _dst_stride: usize,
) -> CodecResult<()> {
    todo!("T2 实现：与 encode_block 对应的解码")
}

/// 校验并返回 BGRA 缓冲的行字节数。
pub fn row_bytes(w: u32) -> Option<usize> {
    usize::try_from(w).ok()?.checked_mul(4)
}
