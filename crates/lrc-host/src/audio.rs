//! M3 音频通道（host 半）：系统回环采集 → Opus(32kbps) → Message::AudioFrame。
//!
//! 独立线程 + 有界队列语义：采集/编码在音频线程里跑，主采集循环完全不受影响；
//! 网络拥塞时**直接丢音频包**（不排队），WASAPI 的环形缓冲自己会覆盖旧数据。
//! 设备/编码器不可用只告警一次并永久关闭音频，不重试。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lrc_audio::{AudioFormat, LoopbackCapturer, OpusEncoder};
use lrc_proto::{AudioFrame, Message};
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::session::Shared;

/// 目标码率（bps）。
pub const AUDIO_BITRATE_BPS: u32 = 32_000;
/// 未确认字节超过这个量就暂停发音频（音频不值得排队）。
const CONGEST_SKIP_BYTES: u64 = 4 * 1024 * 1024;
/// 拥塞时歇一下，别空转。
const CONGEST_SLEEP: Duration = Duration::from_millis(5);

#[derive(Debug, Default)]
struct Counters {
    packets: AtomicU64,
    bytes: AtomicU64,
}

/// 音频流水线句柄。Drop 时停止线程。
pub struct AudioPipeline {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    counters: Arc<Counters>,
    format: AudioFormat,
}

impl AudioPipeline {
    /// 已发出的音频包数 / 字节数 / 格式。
    pub fn stats(&self) -> (u64, u64, AudioFormat) {
        (
            self.counters.packets.load(Ordering::Relaxed),
            self.counters.bytes.load(Ordering::Relaxed),
            self.format,
        )
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                log::warn!("音频线程 panic");
            }
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 会话建立时调用。返回 None 表示音频不可用（已经告警，调用方不要再重试）。
pub fn start(shared: Arc<Shared>) -> Option<AudioPipeline> {
    let format = AudioFormat::default();
    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Counters::default());
    let spawned = {
        let stop = Arc::clone(&stop);
        let counters = Arc::clone(&counters);
        std::thread::Builder::new()
            .name("lrc-host-audio".to_string())
            .spawn(move || {
                // WASAPI 与 ffmpeg 的句柄不是 Send，所以设备/编码器都在音频线程里创建。
                // 不可用就在这里告警一次并退出（调用方不需要重试）。
                let capturer = match LoopbackCapturer::new(format) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("音频通道不可用（设备错误，本次会话永久关闭）：{e}");
                        return;
                    }
                };
                let encoder = match OpusEncoder::new(format, AUDIO_BITRATE_BPS, None) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("音频通道不可用（Opus 编码器启动失败，永久关闭）：{e}");
                        return;
                    }
                };
                log::info!(
                    "音频通道就绪：{}Hz/{}ch → Opus {}kbps",
                    format.sample_rate,
                    format.channels,
                    AUDIO_BITRATE_BPS / 1000
                );
                audio_loop(capturer, encoder, format, shared, stop, counters);
            })
    };
    match spawned {
        Ok(handle) => Some(AudioPipeline {
            stop,
            handle: Some(handle),
            counters,
            format,
        }),
        Err(e) => {
            log::warn!("音频通道不可用（启动线程失败，永久关闭）：{e}");
            None
        }
    }
}

fn audio_loop(
    mut capturer: LoopbackCapturer,
    mut encoder: OpusEncoder,
    format: AudioFormat,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    let block = format.bytes_per_20ms();
    if block == 0 {
        log::warn!("音频格式异常（每 20ms 0 字节），关闭音频");
        return;
    }
    let mut pcm = vec![0u8; block];
    let mut acc: Vec<u8> = Vec::with_capacity(block * 4);
    let mut chunk = vec![0u8; block];
    let mut out: Vec<u8> = Vec::with_capacity(1024);
    let mut seq: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        // 有界队列语义：拥塞就丢，绝不排队
        if shared.ack_fresh() && shared.inflight_bytes() > CONGEST_SKIP_BYTES {
            std::thread::sleep(CONGEST_SLEEP);
            continue;
        }
        let got = match capturer.read(&mut pcm) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("音频采集失败（永久关闭音频）：{e}");
                return;
            }
        };
        if got == 0 {
            // 静音/没新数据：顺手把编码器尾巴抽出来
            out.clear();
            if encoder.flush(&mut out).is_ok() && !out.is_empty() {
                let bytes = send_audio(&shared, &mut seq, &out);
                if bytes > 0 {
                    counters.packets.fetch_add(1, Ordering::Relaxed);
                    counters.bytes.fetch_add(bytes, Ordering::Relaxed);
                }
            }
            continue;
        }
        acc.extend_from_slice(&pcm[..got.min(pcm.len())]);
        while acc.len() >= block {
            chunk.copy_from_slice(&acc[..block]);
            acc.drain(..block);
            out.clear();
            if let Err(e) = encoder.encode(&chunk, &mut out) {
                log::warn!("音频编码失败（永久关闭音频）：{e}");
                return;
            }
            if out.is_empty() {
                continue;
            }
            let key = unsafe { GetTickCount64() } as u32;
            let msg = Message::AudioFrame(AudioFrame {
                ts_ms: key,
                seq,
                // bit0 = Opus
                flags: 1,
                sample_rate: u16::try_from(format.sample_rate).unwrap_or(48_000),
                channels: u8::try_from(format.channels).unwrap_or(2),
                data: &out,
            });
            let payload = msg.encode();
            if let Err(e) = shared.send(&payload) {
                log::warn!("发送音频帧失败（结束音频线程）：{e}");
                return;
            }
            seq = seq.wrapping_add(1);
            counters.packets.fetch_add(1, Ordering::Relaxed);
            counters
                .bytes
                .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        }
    }
}

/// 发一个音频包，返回线上字节数（0 = 没发出去）。
fn send_audio(shared: &Shared, seq: &mut u32, data: &[u8]) -> u64 {
    let msg = Message::AudioFrame(AudioFrame {
        ts_ms: unsafe { GetTickCount64() } as u32,
        seq: *seq,
        flags: 1,
        sample_rate: 48_000,
        channels: 2,
        data,
    });
    let payload = msg.encode();
    if shared.send(&payload).is_err() {
        return 0;
    }
    *seq = seq.wrapping_add(1);
    payload.len() as u64 + 4
}
