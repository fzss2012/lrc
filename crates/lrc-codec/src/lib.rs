//! 块编码：无损（zstd + 左预测）/ 有损（JPEG）/ 原样。
//!
//! 契约见 docs/DESIGN.md §3。**本文件是三方共享的接口，签名不要随意改**，
//! 需要改先跟 Lead 对齐。

use std::borrow::Cow;
use std::fmt;

pub use lrc_proto::codec_tag;

/// 无损块头长度：mode(1) + w(2) + h(2)。
const LOSSLESS_HEADER: usize = 5;
/// 有损块头长度：quality(1) + w(2) + h(2)。
const JPEG_HEADER: usize = 5;
/// 无损模式：左预测后 zstd。
const LOSSLESS_MODE_PREDICT: u8 = 0;
/// 无损模式：原样 zstd（偶尔比左预测更小，两种都试）。
const LOSSLESS_MODE_RAW: u8 = 1;

use std::cell::RefCell;

thread_local! {
    /// zstd 的 CCtx/DCtx 创建一次要上百微秒（比压 64x64 块本身还贵），按线程缓存复用。
    /// 实测复用后单块压缩 109us -> 12us。
    static ZSTD_COMPRESSOR: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
    static ZSTD_DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// 复用线程内的 zstd 压缩上下文（按 level 缓存）。
fn zstd_compress(data: &[u8], level: i32) -> CodecResult<Vec<u8>> {
    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell
            .try_borrow_mut()
            .map_err(|_| CodecError::Encode("zstd 压缩器重入".to_string()))?;
        let reuse = matches!(slot.as_ref(), Some((cached, _)) if *cached == level);
        if !reuse {
            let compressor = zstd::bulk::Compressor::new(level)
                .map_err(|e| CodecError::Encode(format!("zstd 压缩器初始化 level={level}: {e}")))?;
            *slot = Some((level, compressor));
        }
        let compressor = match slot.as_mut() {
            Some((_, compressor)) => compressor,
            None => return Err(CodecError::Encode("zstd 压缩器不可用".to_string())),
        };
        compressor
            .compress(data)
            .map_err(|e| CodecError::Encode(format!("zstd 压缩 level={level}: {e}")))
    })
}

/// 复用线程内的 zstd 解压上下文。
fn zstd_decompress(data: &[u8], capacity: usize) -> CodecResult<Vec<u8>> {
    ZSTD_DECOMPRESSOR.with(|cell| {
        let mut slot = cell
            .try_borrow_mut()
            .map_err(|_| CodecError::Decode("zstd 解压器重入".to_string()))?;
        if slot.is_none() {
            *slot = Some(
                zstd::bulk::Decompressor::new()
                    .map_err(|e| CodecError::Decode(format!("zstd 解压器初始化: {e}")))?,
            );
        }
        let decompressor = match slot.as_mut() {
            Some(decompressor) => decompressor,
            None => return Err(CodecError::Decode("zstd 解压器不可用".to_string())),
        };
        decompressor
            .decompress(data, capacity)
            .map_err(|e| CodecError::Decode(format!("zstd 解压: {e}")))
    })
}

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

/// 编码单块。src 为 BGRA，stride 为字节步长。
///
/// 返回实际使用的 codec_tag（可能比期望的更低，例如整块纯色直接无损）。
/// out 会被清空后写入编码数据。
pub fn encode_block(
    src: &[u8],
    stride: usize,
    w: u32,
    h: u32,
    opts: &Options,
    out: &mut Vec<u8>,
) -> CodecResult<u8> {
    out.clear();

    let (row, h_us, total) = check_geometry(w, h, stride, src.len())?;
    if row == 0 || h_us == 0 {
        // 空块：没有内容可编，直接按 raw 返回空数据。
        return Ok(codec_tag::RAW);
    }

    // 紧排行缓冲（JPEG 编码器要求无步长填充）。
    let tight: Cow<'_, [u8]> = if stride == row && src.len() >= total {
        Cow::Borrowed(&src[..total])
    } else {
        let mut v = Vec::with_capacity(total);
        for y in 0..h_us {
            let s = y * stride;
            v.extend_from_slice(&src[s..s + row]);
        }
        Cow::Owned(v)
    };
    let raw_len = total;

    // 1) 无损：左预测 + zstd，取两种模式里更小的。
    let lossless = encode_lossless(&tight, row, h_us, w, h, opts.zstd_level)?;
    let lossless_len = lossless.len();
    let limit = if opts.lossless_ratio_limit.is_finite() {
        opts.lossless_ratio_limit.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let gate = raw_len as f64 * f64::from(limit);
    if (lossless_len as f64) <= gate {
        out.extend_from_slice(&lossless);
        return Ok(codec_tag::LOSSLESS);
    }

    // 2) 无损不够好：试 JPEG，两者取小；都不如 raw 就 raw。
    let jpeg = encode_jpeg(&tight, w, h, opts.jpeg_quality)?;
    let (tag, best) = if lossless_len <= jpeg.len() {
        (codec_tag::LOSSLESS, &lossless)
    } else {
        (codec_tag::JPEG, &jpeg)
    };
    if best.len() >= raw_len {
        out.extend_from_slice(&tight);
        return Ok(codec_tag::RAW);
    }
    out.extend_from_slice(best);
    Ok(tag)
}

/// 解码单块到 BGRA 目标缓冲。
pub fn decode_block(
    codec: u8,
    data: &[u8],
    w: u32,
    h: u32,
    dst: &mut [u8],
    dst_stride: usize,
) -> CodecResult<()> {
    let (row, h_us, raw_len) = check_geometry(w, h, dst_stride, dst.len())?;
    if row == 0 || h_us == 0 {
        return Ok(());
    }

    match codec {
        codec_tag::LOSSLESS => {
            if data.len() < LOSSLESS_HEADER {
                return Err(CodecError::Decode(format!(
                    "无损块头不足 {LOSSLESS_HEADER} 字节（实际 {}）",
                    data.len()
                )));
            }
            let mode = data[0];
            let dw = u32::from(u16::from_le_bytes([data[1], data[2]]));
            let dh = u32::from(u16::from_le_bytes([data[3], data[4]]));
            if dw != w || dh != h {
                return Err(CodecError::Decode(format!(
                    "无损块尺寸 {dw}x{dh} 与期望 {w}x{h} 不符"
                )));
            }
            let mut buf = zstd_decompress(&data[LOSSLESS_HEADER..], raw_len)?;
            if buf.len() != raw_len {
                return Err(CodecError::Decode(format!(
                    "zstd 输出 {} 字节，期望 {raw_len}",
                    buf.len()
                )));
            }
            match mode {
                LOSSLESS_MODE_PREDICT => undo_predict(&mut buf, row, h_us),
                LOSSLESS_MODE_RAW => {}
                other => {
                    return Err(CodecError::Decode(format!("未知无损模式 {other}")));
                }
            }
            copy_rows(&buf, row, h_us, dst, dst_stride);
        }
        codec_tag::JPEG => {
            if data.len() < JPEG_HEADER {
                return Err(CodecError::Decode(format!(
                    "JPEG 块头不足 {JPEG_HEADER} 字节（实际 {}）",
                    data.len()
                )));
            }
            let dw = u32::from(u16::from_le_bytes([data[1], data[2]]));
            let dh = u32::from(u16::from_le_bytes([data[3], data[4]]));
            if dw != w || dh != h {
                return Err(CodecError::Decode(format!(
                    "JPEG 块尺寸 {dw}x{dh} 与期望 {w}x{h} 不符"
                )));
            }
            decode_jpeg_to_bgra(&data[JPEG_HEADER..], row, h_us, dst, dst_stride)?;
        }
        codec_tag::RAW => {
            if data.len() != raw_len {
                return Err(CodecError::Decode(format!(
                    "raw 块 {} 字节，期望 {raw_len}",
                    data.len()
                )));
            }
            copy_rows(data, row, h_us, dst, dst_stride);
        }
        other => return Err(CodecError::UnknownCodec(other)),
    }
    Ok(())
}

/// 校验并返回 BGRA 缓冲的行字节数。
pub fn row_bytes(w: u32) -> Option<usize> {
    usize::try_from(w).ok()?.checked_mul(4)
}

/// 校验一块 BGRA 缓冲（行步长 stride、长度 len）装不装得下 w×h 的块。
///
/// 返回 (行字节数, 行数, 紧排总字节数)；w 或 h 为 0 时三者都退化为 0。
fn check_geometry(w: u32, h: u32, stride: usize, len: usize) -> CodecResult<(usize, usize, usize)> {
    let bad = || CodecError::BadGeometry { w, h, stride, len };
    let row = row_bytes(w).ok_or_else(bad)?;
    let h_us = usize::try_from(h).map_err(|_| bad())?;
    if row == 0 || h_us == 0 {
        return Ok((row, h_us, 0));
    }
    if stride < row {
        return Err(bad());
    }
    // 最后一行可以只到行宽，不要求补齐步长填充。
    let span = stride
        .checked_mul(h_us - 1)
        .and_then(|v| v.checked_add(row))
        .ok_or_else(bad)?;
    if len < span {
        return Err(bad());
    }
    let total = row.checked_mul(h_us).ok_or_else(bad)?;
    Ok((row, h_us, total))
}

// ---------------------------------------------------------------- 无损

/// 左预测：每行首像素（4 字节）原样，其后逐字节 p[i] - p[i-1]（环绕）。
fn apply_predict(src: &[u8], row: usize, h: usize, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(src.len());
    for y in 0..h {
        let line = &src[y * row..(y + 1) * row];
        let first = line.len().min(4);
        out.extend_from_slice(&line[..first]);
        for i in first..line.len() {
            out.push(line[i].wrapping_sub(line[i - 1]));
        }
    }
}

/// 左预测的逆运算（就地）。
fn undo_predict(buf: &mut [u8], row: usize, h: usize) {
    for y in 0..h {
        let start = y * row;
        let line = &mut buf[start..start + row];
        for i in 4..line.len() {
            line[i] = line[i].wrapping_add(line[i - 1]);
        }
    }
}

/// 无损编码：u8 mode + u16 w + u16 h + zstd(payload)，两种模式取小。
fn encode_lossless(
    tight: &[u8],
    row: usize,
    h: usize,
    w: u32,
    hh: u32,
    level: i32,
) -> CodecResult<Vec<u8>> {
    let w16 = u16::try_from(w).map_err(|_| CodecError::Encode(format!("块宽 {w} 超过 u16 上限")))?;
    let h16 =
        u16::try_from(hh).map_err(|_| CodecError::Encode(format!("块高 {hh} 超过 u16 上限")))?;

    let mut predicted = Vec::with_capacity(tight.len());
    apply_predict(tight, row, h, &mut predicted);
    let z_pred = zstd_compress(&predicted, level)
        .map_err(|e| CodecError::Encode(format!("无损(左预测): {e}")))?;
    let z_raw = zstd_compress(tight, level)
        .map_err(|e| CodecError::Encode(format!("无损(原样): {e}")))?;
    let (mode, payload) = if z_pred.len() <= z_raw.len() {
        (LOSSLESS_MODE_PREDICT, z_pred)
    } else {
        (LOSSLESS_MODE_RAW, z_raw)
    };

    let mut out = Vec::with_capacity(LOSSLESS_HEADER + payload.len());
    out.push(mode);
    out.extend_from_slice(&w16.to_le_bytes());
    out.extend_from_slice(&h16.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

// ---------------------------------------------------------------- 有损

/// 有损编码：u8 quality + u16 w + u16 h + 基线 JPEG（4:4:4）。
fn encode_jpeg(tight: &[u8], w: u32, h: u32, quality: u8) -> CodecResult<Vec<u8>> {
    let w16 = u16::try_from(w).map_err(|_| CodecError::Encode(format!("块宽 {w} 超过 u16 上限")))?;
    let h16 = u16::try_from(h).map_err(|_| CodecError::Encode(format!("块高 {h} 超过 u16 上限")))?;
    let q = quality.clamp(1, 100);

    let mut jpeg = Vec::with_capacity(tight.len() / 2);
    let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, q);
    // 桌面文字讲究锐利：固定 4:4:4，别让色度被下采样糊掉。
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    encoder
        .encode(tight, w16, h16, jpeg_encoder::ColorType::Bgra)
        .map_err(|e| CodecError::Encode(format!("jpeg: {e}")))?;

    let mut out = Vec::with_capacity(JPEG_HEADER + jpeg.len());
    out.push(q);
    out.extend_from_slice(&w16.to_le_bytes());
    out.extend_from_slice(&h16.to_le_bytes());
    out.extend_from_slice(&jpeg);
    Ok(out)
}

/// 解码 JPEG 载荷（RGB24 / L8）并写成 BGRA，按 dst_stride 排布。
fn decode_jpeg_to_bgra(
    payload: &[u8],
    row: usize,
    h: usize,
    dst: &mut [u8],
    dst_stride: usize,
) -> CodecResult<()> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(payload);
    // 先只读头部，按声明尺寸校验，避免被伪造的巨型 JPEG 撑爆内存。
    decoder
        .read_info()
        .map_err(|e| CodecError::Decode(format!("jpeg 头部: {e}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| CodecError::Decode("jpeg 缺少头部信息".to_string()))?;
    let (iw, ih) = (usize::from(info.width), usize::from(info.height));
    if iw * 4 != row || ih != h {
        return Err(CodecError::Decode(format!(
            "JPEG 尺寸 {iw}x{ih} 与期望 {}x{h} 不符",
            row / 4
        )));
    }
    let pixels = decoder
        .decode()
        .map_err(|e| CodecError::Decode(format!("jpeg 解码: {e}")))?;

    match info.pixel_format {
        PixelFormat::RGB24 => {
            let wpx = row / 4;
            let need = wpx * h * 3;
            if pixels.len() < need {
                return Err(CodecError::Decode(format!(
                    "JPEG 输出 {} 字节，期望至少 {need}",
                    pixels.len()
                )));
            }
            for y in 0..h {
                let s = y * wpx * 3;
                let d = y * dst_stride;
                let line = &mut dst[d..d + row];
                let src = &pixels[s..s + wpx * 3];
                for x in 0..wpx {
                    line[x * 4] = src[x * 3 + 2];
                    line[x * 4 + 1] = src[x * 3 + 1];
                    line[x * 4 + 2] = src[x * 3];
                    line[x * 4 + 3] = 255;
                }
            }
        }
        PixelFormat::L8 => {
            let wpx = row / 4;
            if pixels.len() < wpx * h {
                return Err(CodecError::Decode(format!(
                    "JPEG 灰度输出 {} 字节，期望至少 {}",
                    pixels.len(),
                    wpx * h
                )));
            }
            for y in 0..h {
                let d = y * dst_stride;
                let line = &mut dst[d..d + row];
                let src = &pixels[y * wpx..(y + 1) * wpx];
                for x in 0..wpx {
                    let g = src[x];
                    line[x * 4] = g;
                    line[x * 4 + 1] = g;
                    line[x * 4 + 2] = g;
                    line[x * 4 + 3] = 255;
                }
            }
        }
        other => {
            return Err(CodecError::Decode(format!(
                "不支持的 JPEG 像素格式 {other:?}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 公共小工具

/// 紧排（行宽 = row）的数据拷进带步长的目标缓冲。
fn copy_rows(tight: &[u8], row: usize, h: usize, dst: &mut [u8], dst_stride: usize) {
    if dst_stride == row {
        dst[..row * h].copy_from_slice(&tight[..row * h]);
    } else {
        for y in 0..h {
            let s = y * row;
            let d = y * dst_stride;
            dst[d..d + row].copy_from_slice(&tight[s..s + row]);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // ------------------------------------------------------------ 测试素材

    /// 极简 xorshift64*，只给测试造数据用。
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed | 1)
        }

        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
        }

        fn next_range(&mut self, n: u32) -> u32 {
            if n == 0 {
                0
            } else {
                self.next_u32() % n
            }
        }
    }

    fn set_px(buf: &mut [u8], w: usize, x: usize, y: usize, c: [u8; 4]) {
        let i = (y * w + x) * 4;
        buf[i..i + 4].copy_from_slice(&c);
    }

    fn fill_rect(buf: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, rw: usize, rh: usize, c: [u8; 4]) {
        for y in y0..(y0 + rh).min(h) {
            for x in x0..(x0 + rw).min(w) {
                set_px(buf, w, x, y, c);
            }
        }
    }

    /// "代码编辑器风格"：大块深色背景 + 1px 细线"文字"。
    fn editor_block(w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![0u8; w * h * 4];
        fill_rect(&mut buf, w, h, 0, 0, w, h, [0x1E, 0x1E, 0x1E, 0xFF]);
        let mut rng = Rng::new(0x5EED_1234);
        let mut y = 3usize;
        while y + 1 < h {
            let mut x = 2 + rng.next_range(8) as usize;
            while x + 2 < w {
                let run = 1 + rng.next_range(5) as usize;
                let color = if rng.next_range(8) == 0 {
                    [0x56, 0x9C, 0xD6, 0xFF] // 关键字蓝
                } else if rng.next_range(8) == 0 {
                    [0x6A, 0x99, 0x55, 0xFF] // 注释绿
                } else {
                    [0xD4, 0xD4, 0xD4, 0xFF] // 普通文字灰
                };
                for dx in 0..run.min(w - x) {
                    set_px(&mut buf, w, x + dx, y, color);
                }
                x += run + 1 + rng.next_range(3) as usize;
            }
            y += 6;
        }
        buf
    }

    /// "照片风格"：中等幅度随机噪声（zstd 压不动、JPEG 能压）。
    fn photo_block(w: usize, h: usize) -> Vec<u8> {
        let mut rng = Rng::new(0xC0FF_EE11);
        let mut buf = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let n = |r: &mut Rng| 128i32 + (r.next_range(121) as i32 - 60);
                let b = n(&mut rng) as u8;
                let g = n(&mut rng) as u8;
                let r = n(&mut rng) as u8;
                set_px(&mut buf, w, x, y, [b, g, r, 0xFF]);
            }
        }
        buf
    }

    /// 合成 1920x1080 桌面：壁纸渐变 + 3 个窗口（标题栏 + 文字行）。
    fn desktop_1080p() -> (Vec<u8>, usize, usize) {
        let (w, h) = (1920usize, 1080usize);
        let mut buf = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let b = (40 + (x * 20) / w) as u8;
                let g = (44 + (y * 30) / h) as u8;
                let r = (52 + ((x + y) * 16) / (w + h)) as u8;
                set_px(&mut buf, w, x, y, [b, g, r, 0xFF]);
            }
        }
        let windows: [(usize, usize, usize, usize, [u8; 4]); 3] = [
            (120, 90, 900, 620, [0xF3, 0xF3, 0xF3, 0xFF]),
            (1010, 260, 760, 460, [0x2D, 0x2D, 0x2D, 0xFF]),
            (420, 720, 640, 320, [0xFF, 0xFF, 0xFF, 0xFF]),
        ];
        let mut rng = Rng::new(0xABCD_1234);
        for (x0, y0, ww, hh, body) in windows {
            fill_rect(&mut buf, w, h, x0, y0, ww, hh, body);
            fill_rect(&mut buf, w, h, x0, y0, ww, 28, [0x3C, 0x6E, 0xA5, 0xFF]);
            // 标题
            for x in (x0 + 10)..(x0 + 110) {
                set_px(&mut buf, w, x, y0 + 12, [0xFF, 0xFF, 0xFF, 0xFF]);
            }
            // 正文细线文字
            let mut y = y0 + 44;
            while y + 1 < y0 + hh - 8 {
                let mut x = x0 + 14 + rng.next_range(30) as usize;
                while x + 3 < x0 + ww - 10 {
                    let run = 1 + rng.next_range(6) as usize;
                    let c = if body[0] > 0xC0 {
                        [0x30, 0x30, 0x30, 0xFF]
                    } else {
                        [0xD8, 0xD8, 0xD8, 0xFF]
                    };
                    for dx in 0..run {
                        set_px(&mut buf, w, x + dx, y, c);
                    }
                    x += run + 1 + rng.next_range(3) as usize;
                }
                y += 5;
            }
        }
        (buf, w, h)
    }

    // ------------------------------------------------------------ 1. 无损往返

    #[test]
    fn lossless_roundtrip_is_byte_exact() {
        let opts = Options::default();
        let (w, h) = (64usize, 64usize);
        let block = editor_block(w, h);

        let mut enc = Vec::new();
        let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
        assert_eq!(tag, codec_tag::LOSSLESS, "文字块应该走无损");

        let mut dec = vec![0u8; w * h * 4];
        decode_block(tag, &enc, w as u32, h as u32, &mut dec, w * 4).unwrap();
        assert_eq!(dec, block, "无损往返必须逐字节一致");
    }

    #[test]
    fn lossless_roundtrip_with_padded_stride() {
        let opts = Options::default();
        let (w, h, stride) = (37usize, 23usize, 37 * 4 + 24);
        let src_block = editor_block(w, h);
        // 铺进带填充的源缓冲
        let mut src = vec![0xABu8; stride * h];
        for y in 0..h {
            src[y * stride..y * stride + w * 4].copy_from_slice(&src_block[y * w * 4..(y + 1) * w * 4]);
        }

        let mut enc = Vec::new();
        let tag = encode_block(&src, stride, w as u32, h as u32, &opts, &mut enc).unwrap();
        assert_eq!(tag, codec_tag::LOSSLESS);

        // 目标也带填充
        let dst_stride = w * 4 + 64;
        let mut dst = vec![0x11u8; dst_stride * h];
        decode_block(tag, &enc, w as u32, h as u32, &mut dst, dst_stride).unwrap();
        for y in 0..h {
            assert_eq!(
                &dst[y * dst_stride..y * dst_stride + w * 4],
                &src_block[y * w * 4..(y + 1) * w * 4],
                "第 {y} 行不一致"
            );
            // 填充字节必须原样保留
            assert!(dst[y * dst_stride + w * 4..(y + 1) * dst_stride].iter().all(|&b| b == 0x11));
        }
    }

    #[test]
    fn solid_block_is_tiny_lossless() {
        let opts = Options::default();
        let (w, h) = (64usize, 64usize);
        let block = vec![0x20u8; w * h * 4];
        let mut enc = Vec::new();
        let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
        assert_eq!(tag, codec_tag::LOSSLESS);
        assert!(enc.len() < 64, "纯色块应压到极小，实际 {} 字节", enc.len());
    }

    // ------------------------------------------------------------ 2. JPEG 往返

    #[test]
    fn jpeg_roundtrip_error_is_bounded() {
        let opts = Options::default();
        let (w, h) = (64usize, 64usize);
        let block = photo_block(w, h);

        let mut enc = Vec::new();
        let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
        assert_eq!(tag, codec_tag::JPEG, "噪声块应该走 JPEG");

        let mut dec = vec![0u8; w * h * 4];
        decode_block(tag, &enc, w as u32, h as u32, &mut dec, w * 4).unwrap();
        assert_eq!(dec.len(), block.len());

        let mut sum = 0i64;
        let mut max = 0i32;
        let mut n = 0i64;
        for (a, b) in block.chunks_exact(4).zip(dec.chunks_exact(4)) {
            for c in 0..3 {
                let d = i32::from(a[c]) - i32::from(b[c]);
                sum += i64::from(d.abs());
                max = max.max(d.abs());
                n += 1;
            }
        }
        let mean = sum as f64 / n as f64;
        println!("[jpeg] 64x64 噪声块 q85: {} 字节, 平均误差 {:.2}, 最大误差 {max}", enc.len(), mean);
        assert!(mean < 20.0, "JPEG 平均误差 {mean} 过大");
        assert!(max < 110, "JPEG 最大误差 {max} 过大");
        // JPEG 没有 alpha，解码后统一补 255
        assert!(dec.chunks_exact(4).all(|p| p[3] == 255));
    }

    // ------------------------------------------------------------ 3. 体积对比

    #[test]
    fn size_compare_text_block_lossless_vs_jpeg() {
        let opts = Options::default();
        let (w, h) = (64usize, 64usize);
        let block = editor_block(w, h);
        let raw = w * h * 4;

        let lossless = encode_lossless(&block, w * 4, h, w as u32, h as u32, opts.zstd_level).unwrap();
        let jpeg = encode_jpeg(&block, w as u32, h as u32, 85).unwrap();
        let mut policied = Vec::new();
        let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut policied).unwrap();

        println!(
            "[64x64 文字块] raw={raw} B | LOSSLESS={} B ({:.2}%) | JPEG(q85)={} B ({:.2}%) | 策略={} B tag={tag} | 无损/JPEG={:.2}x",
            lossless.len(),
            lossless.len() as f64 * 100.0 / raw as f64,
            jpeg.len(),
            jpeg.len() as f64 * 100.0 / raw as f64,
            policied.len(),
            lossless.len() as f64 / jpeg.len() as f64
        );
        assert_eq!(tag, codec_tag::LOSSLESS);
        assert!(
            lossless.len() < jpeg.len(),
            "文字块无损应更小：无损 {} vs JPEG {}",
            lossless.len(),
            jpeg.len()
        );
        assert_eq!(policied.len(), lossless.len());
    }

    #[test]
    fn size_compare_1080p_screen() {
        let opts = Options::default();
        let (screen, w, h) = desktop_1080p();
        let stride = w * 4;
        let bs = 64usize;
        let bx = (w + bs - 1) / bs;
        let by = (h + bs - 1) / bs;

        let mut out = Vec::new();
        let mut policy_total = 0usize;
        let mut lossless_total = 0usize;
        let mut jpeg_total = 0usize;
        let mut raw_total = 0usize;
        let mut hist = [0usize; 3];

        for by_i in 0..by {
            for bx_i in 0..bx {
                let bw = (w - bx_i * bs).min(bs);
                let bh = (h - by_i * bs).min(bs);
                let off = by_i * bs * stride + bx_i * bs * 4;

                let tag = encode_block(&screen[off..], stride, bw as u32, bh as u32, &opts, &mut out).unwrap();
                policy_total += out.len();
                hist[usize::from(tag)] += 1;

                // 拼出紧排块，再分别强制无损 / 强制 JPEG，做公平对比
                let mut tile = Vec::with_capacity(bw * bh * 4);
                for y in 0..bh {
                    let s = off + y * stride;
                    tile.extend_from_slice(&screen[s..s + bw * 4]);
                }
                raw_total += tile.len();
                lossless_total +=
                    encode_lossless(&tile, bw * 4, bh, bw as u32, bh as u32, opts.zstd_level).unwrap().len();
                jpeg_total += encode_jpeg(&tile, bw as u32, bh as u32, opts.jpeg_quality).unwrap().len();
            }
        }

        let tiles = bx * by;
        println!(
            "[1080p {w}x{h}] {tiles} 个 {bs}x{bs} 块 | raw={raw_total} B | 策略={policy_total} B ({:.2}%, {:.3} Mbps@30fps) | 全无损={lossless_total} B ({:.2}%) | 全JPEG(q85)={jpeg_total} B ({:.2}%) | tag 分布 LOSSLESS={} JPEG={} RAW={}",
            policy_total as f64 * 100.0 / raw_total as f64,
            policy_total as f64 * 8.0 * 30.0 / 1.0e6,
            lossless_total as f64 * 100.0 / raw_total as f64,
            jpeg_total as f64 * 100.0 / raw_total as f64,
            hist[0], hist[1], hist[2]
        );

        assert!(policy_total <= lossless_total, "策略不应比全无损更大");
        assert!(policy_total <= jpeg_total, "策略不应比全 JPEG 更大");
        assert!(lossless_total < jpeg_total, "UI 整屏应该无损比 JPEG 更小");
        assert!(
            policy_total * 100 < raw_total * 5,
            "策略应该压到 5% 以下，实际 {policy_total} B / {raw_total} B"
        );
    }

    // ------------------------------------------------------------ 4. 性能

    #[test]
    fn timing_64x64_block() {
        let opts = Options::default();
        let (w, h) = (64usize, 64usize);
        let block = editor_block(w, h);
        let iters = 300u32;

        let mut enc = Vec::new();
        let t0 = Instant::now();
        for _ in 0..iters {
            let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
            assert_eq!(tag, codec_tag::LOSSLESS);
        }
        let enc_us = t0.elapsed().as_secs_f64() * 1.0e6 / f64::from(iters);

        let mut dec = vec![0u8; w * h * 4];
        let t1 = Instant::now();
        for _ in 0..iters {
            decode_block(codec_tag::LOSSLESS, &enc, w as u32, h as u32, &mut dec, w * 4).unwrap();
        }
        let dec_us = t1.elapsed().as_secs_f64() * 1.0e6 / f64::from(iters);

        let photo = photo_block(w, h);
        let t2 = Instant::now();
        for _ in 0..iters {
            let tag = encode_block(&photo, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
            assert_eq!(tag, codec_tag::JPEG);
        }
        let photo_enc_us = t2.elapsed().as_secs_f64() * 1.0e6 / f64::from(iters);
        let t3 = Instant::now();
        for _ in 0..iters {
            decode_block(codec_tag::JPEG, &enc, w as u32, h as u32, &mut dec, w * 4).unwrap();
        }
        let photo_dec_us = t3.elapsed().as_secs_f64() * 1.0e6 / f64::from(iters);

        println!(
            "[perf] 64x64 文字块 encode={enc_us:.1}us decode={dec_us:.1}us | 噪声块(JPEG) encode={photo_enc_us:.1}us decode={photo_dec_us:.1}us"
        );

        let limit = if cfg!(debug_assertions) { 20_000.0 } else { 1_000.0 };
        assert!(enc_us < limit, "64x64 编码 {enc_us}us 超预算");
        assert!(dec_us < limit, "64x64 解码 {dec_us}us 超预算");
    }

    // ------------------------------------------------------------ 5. 非法输入

    #[test]
    fn decode_rejects_broken_input() {
        let (w, h) = (16usize, 16usize);
        let mut dst = vec![0u8; w * h * 4];

        // 未知 codec
        match decode_block(9, &[0u8; 8], w as u32, h as u32, &mut dst, w * 4) {
            Err(CodecError::UnknownCodec(9)) => {}
            other => panic!("未知 codec 应报 UnknownCodec，实际 {other:?}"),
        }

        // 截断的块头
        assert!(decode_block(codec_tag::LOSSLESS, &[0, 1, 2], w as u32, h as u32, &mut dst, w * 4).is_err());
        assert!(decode_block(codec_tag::JPEG, &[85, 0], w as u32, h as u32, &mut dst, w * 4).is_err());

        // 头部尺寸与期望不符
        let mut bogus = vec![LOSSLESS_MODE_PREDICT, 32, 0, 32, 0];
        bogus.extend_from_slice(&[0u8; 8]);
        assert!(decode_block(codec_tag::LOSSLESS, &bogus, w as u32, h as u32, &mut dst, w * 4).is_err());

        // 未知无损模式
        let mut bogus_mode = vec![7u8, w as u8, 0, h as u8, 0];
        bogus_mode.extend_from_slice(&[0u8; 8]);
        assert!(decode_block(codec_tag::LOSSLESS, &bogus_mode, w as u32, h as u32, &mut dst, w * 4).is_err());

        // 损坏的 zstd 数据
        let block = editor_block(w, h);
        let opts = Options::default();
        let mut enc = Vec::new();
        let tag = encode_block(&block, w * 4, w as u32, h as u32, &opts, &mut enc).unwrap();
        assert_eq!(tag, codec_tag::LOSSLESS);
        let mut corrupt = enc.clone();
        for b in corrupt[LOSSLESS_HEADER..].iter_mut() {
            *b ^= 0xA5;
        }
        assert!(decode_block(codec_tag::LOSSLESS, &corrupt, w as u32, h as u32, &mut dst, w * 4).is_err());

        // raw 长度不符
        assert!(decode_block(codec_tag::RAW, &[0u8; 7], w as u32, h as u32, &mut dst, w * 4).is_err());

        // 目标缓冲太小 / 步长太小
        assert!(encode_block(&block, 4, w as u32, h as u32, &opts, &mut enc).is_err());
        let mut small = vec![0u8; 16];
        assert!(decode_block(codec_tag::RAW, &block, w as u32, h as u32, &mut small, w * 4).is_err());

        // 伪造的巨型 JPEG 头部不能把我们撑爆
        let mut fake_jpeg = vec![85u8, w as u8, 0, h as u8, 0];
        fake_jpeg.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46]);
        assert!(decode_block(codec_tag::JPEG, &fake_jpeg, w as u32, h as u32, &mut dst, w * 4).is_err());
    }


    #[test]
    fn encode_rejects_bad_geometry() {
        let opts = Options::default();
        let block = editor_block(8, 8);
        let mut out = Vec::new();
        // 步长不够
        assert!(encode_block(&block, 8, 8, 8, &opts, &mut out).is_err());
        // 数据不够
        assert!(encode_block(&block[..40], 32, 8, 8, &opts, &mut out).is_err());
        // 空块不 panic
        let tag = encode_block(&[], 0, 0, 0, &opts, &mut out).unwrap();
        assert_eq!(tag, codec_tag::RAW);
        assert!(out.is_empty());
    }
}
