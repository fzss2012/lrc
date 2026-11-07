//! Opus 编解码：包装 ffmpeg 子进程（Ogg 页直通，见 crate 顶部说明）。

use crate::proc::{resolve_ffmpeg, FfmpegChild};
use crate::{AudioError, AudioFormat, AudioResult};

/// Opus 编码器（ffmpeg 子进程）。
pub struct OpusEncoder {
    child: FfmpegChild,
    format: AudioFormat,
    bitrate_bps: u32,
    bytes_per_block: usize,
}

impl OpusEncoder {
    /// bitrate_bps 建议 32_000（语音）/ 64_000（音乐）。
    pub fn new(format: AudioFormat, bitrate_bps: u32, ffmpeg: Option<&str>) -> AudioResult<Self> {
        format.ensure_supported()?;
        if !(6_000..=512_000).contains(&bitrate_bps) {
            return Err(AudioError::Unsupported("Opus 码率请给 6k..512k"));
        }
        let ff = resolve_ffmpeg(ffmpeg);
        let args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            // 关键：关掉输入探测缓冲。默认 probesize=5MB / analyzeduration=5s 会让
            // ffmpeg 攒够 4~5 秒输入才开始编码（实测首次输出延迟 4.3 秒）。
            "-probesize".into(),
            "32".into(),
            "-analyzeduration".into(),
            "0".into(),
            "-f".into(),
            "s16le".into(),
            "-ar".into(),
            format.sample_rate.to_string(),
            "-ac".into(),
            format.channels.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-c:a".into(),
            "libopus".into(),
            "-b:a".into(),
            format!("{}k", (bitrate_bps + 500) / 1000),
            // 固定码率：ffmpeg 默认 VBR，实测纯音上会超目标 2 倍；CBR 才能保证
            // 「实际码率 ≈ 目标码率」，对带宽敏感的远控更合适。
            "-vbr".into(),
            "off".into(),
            "-frame_duration".into(),
            "20".into(),
            "-application".into(),
            "lowdelay".into(),
            // 每 20ms 输出一个 Ogg 页 → encode() 一次就能拿到一包，延迟可控
            "-page_duration".into(),
            "20000".into(),
            "-f".into(),
            "ogg".into(),
            "pipe:1".into(),
        ];
        let child = FfmpegChild::spawn(&ff, &args)?;
        log::info!(
            "audio: Opus 编码器就绪（{}Hz/{}ch/{}bps，pid={}）",
            format.sample_rate,
            format.channels,
            bitrate_bps,
            child.pid()
        );
        Ok(Self {
            child,
            format,
            bitrate_bps,
            bytes_per_block: format.bytes_per_20ms(),
        })
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// ffmpeg 子进程 pid（自测清理用）。
    pub fn child_pid(&self) -> u32 {
        self.child.pid()
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// 输入一块 20ms 的 s16le PCM，往 out 追加音频包。
    ///
    /// 注意：本实现的"一个包"是**一个完整 Ogg 页**（前几个包会包含 OpusHead /
    /// OpusTags 容器头），调用方把 out 里的字节原样发出去即可；ffmpeg 还没产出时
    /// out 为空（正常节奏下 1 输入对 1 输出）。
    pub fn encode(&mut self, pcm: &[u8], out: &mut Vec<u8>) -> AudioResult<()> {
        out.clear();
        if pcm.len() != self.bytes_per_block {
            return Err(AudioError::Codec(format!(
                "编码输入应为 {} 字节（20ms），实际 {}",
                self.bytes_per_block,
                pcm.len()
            )));
        }
        self.child.write(pcm)?;
        self.child.wait_page(40)?;
        self.child.take_pages(out);
        Ok(())
    }
}

/// Opus 解码器（ffmpeg 子进程）。
pub struct OpusDecoder {
    child: FfmpegChild,
    format: AudioFormat,
}

impl OpusDecoder {
    pub fn new(format: AudioFormat, ffmpeg: Option<&str>) -> AudioResult<Self> {
        format.ensure_supported()?;
        let ff = resolve_ffmpeg(ffmpeg);
        let args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            // 同样关掉探测缓冲，否则解码端也会攒一大段才吐 PCM
            "-probesize".into(),
            "32".into(),
            "-analyzeduration".into(),
            "0".into(),
            // 这个 ffmpeg 没有 opus 解封装器，Ogg 容器必须用 -f ogg
            "-f".into(),
            "ogg".into(),
            "-i".into(),
            "pipe:0".into(),
            "-f".into(),
            "s16le".into(),
            "-ar".into(),
            format.sample_rate.to_string(),
            "-ac".into(),
            format.channels.to_string(),
            "pipe:1".into(),
        ];
        let child = FfmpegChild::spawn(&ff, &args)?;
        log::info!(
            "audio: Opus 解码器就绪（{}Hz/{}ch，pid={}）",
            format.sample_rate,
            format.channels,
            child.pid()
        );
        Ok(Self { child, format })
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// ffmpeg 子进程 pid（自测清理用）。
    pub fn child_pid(&self) -> u32 {
        self.child.pid()
    }

    /// 输入一个音频包（本实现是一个 Ogg 页），往 out 追加解码出的 PCM。
    /// 解码器还在攒帧时 out 可能为空。
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<u8>) -> AudioResult<()> {
        out.clear();
        if packet.is_empty() {
            return Ok(());
        }
        self.child.write(packet)?;
        // 等一帧 PCM（20ms）出来；首包会因为 pre-skip 少几个采样，等不到就下次再取
        self.child.wait_bytes(40, self.format.bytes_per_20ms())?;
        self.child.take_all(out);
        Ok(())
    }
}
