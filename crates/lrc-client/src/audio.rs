//! 音频播放：接收线程只负责把 Opus 包塞进有界队列，独立音频线程解码 + WASAPI 播放。
//!
//! 设计要点（见 DESIGN / Lead 要求）：
//! - **独立线程**：20ms 一次的播放不能拖累视频解码/接收；
//! - **有界队列**（10 包 ≈ 200ms）：满了丢最旧的包，音频宁可掉一小段也不能越积越卡；
//! - **seq 不连续**：丢掉当前包并重新对齐（不重排、不等待），避免一个丢包拖垮整条音频；
//! - 解码器/播放设备不可用：**打一次告警后关闭音频通道**（audio=err），不重试、不影响视频。

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use lrc_audio::{pcm_stats, AudioFormat, OpusDecoder, Player};
use lrc_proto::AudioFrame;

use crate::state::{audio_state, Shared};

/// 队列深度：10 包 × 20ms ≈ 200ms 上限
const QUEUE_DEPTH: usize = 10;

#[derive(Clone, Debug)]
struct AudioPacket {
    seq: u32,
    sample_rate: u32,
    channels: u16,
    data: Vec<u8>,
}

struct QueueInner {
    packets: VecDeque<AudioPacket>,
    closed: bool,
}

/// 有界音频包队列（生产者=接收线程，消费者=音频线程）。
struct AudioQueue {
    inner: Mutex<QueueInner>,
    ready: Condvar,
}

impl AudioQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                packets: VecDeque::with_capacity(QUEUE_DEPTH),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 入队；队列满时丢最旧的包并返回 true（调用方记一次丢包）。
    fn push(&self, packet: AudioPacket) -> bool {
        let mut inner = self.lock();
        let dropped = if inner.packets.len() >= QUEUE_DEPTH {
            inner.packets.pop_front().is_some()
        } else {
            false
        };
        inner.packets.push_back(packet);
        self.ready.notify_one();
        dropped
    }

    /// 阻塞取一个包；队列关闭且取空后返回 None（音频线程据此退出）。
    fn pop(&self) -> Option<AudioPacket> {
        let mut inner = self.lock();
        loop {
            if let Some(packet) = inner.packets.pop_front() {
                return Some(packet);
            }
            if inner.closed {
                return None;
            }
            match self.ready.wait_timeout(inner, Duration::from_millis(200)) {
                Ok((guard, _timeout)) => inner = guard,
                Err(poisoned) => inner = poisoned.into_inner().0,
            }
        }
    }

    fn clear(&self) {
        self.lock().packets.clear();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }
}

/// 音频通道句柄（接收线程持有）：把包塞进队列，队列/线程生命周期归它管。
pub struct AudioSink {
    queue: Arc<AudioQueue>,
    shared: Arc<Shared>,
    last_seq: Option<u32>,
}

impl AudioSink {
    /// 启动音频线程。\`enabled=false\`（--audio off）时不建线程，返回 None。
    pub fn start(enabled: bool, shared: Arc<Shared>, dump_path: Option<String>) -> Option<Self> {
        if !enabled {
            shared.set_audio_state(audio_state::OFF);
            return None;
        }
        let queue = Arc::new(AudioQueue::new());
        let worker_queue = queue.clone();
        let worker_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name(String::from("lrc-client-audio"))
            .spawn(move || worker(worker_shared, worker_queue, dump_path));
        match spawned {
            Ok(_) => Some(Self {
                queue,
                shared,
                last_seq: None,
            }),
            Err(e) => {
                log::warn!("音频线程启动失败：{e}");
                shared.set_audio_state(audio_state::ERR);
                None
            }
        }
    }

    /// 新会话（含重连）：清队列与 seq 连续性状态，避免旧包串台。
    pub fn reset(&mut self) {
        self.last_seq = None;
        self.queue.clear();
    }

    /// 收到一个 AudioFrame：seq 不连续就丢弃当前包（音频宁可静音一小段）。
    pub fn push_frame(&mut self, frame: &AudioFrame<'_>) {
        if self.shared.audio_state() == audio_state::ERR {
            return;
        }
        if let Some(last) = self.last_seq {
            if frame.seq != last.wrapping_add(1) {
                self.last_seq = Some(frame.seq);
                self.shared.audio_drop.fetch_add(1, Ordering::Relaxed);
                log::debug!("音频 seq 跳变（{last} → {}），丢弃当前包", frame.seq);
                return;
            }
        }
        self.last_seq = Some(frame.seq);
        if !frame.is_opus() {
            // M3 只有 Opus；裸 PCM 调试包先忽略
            self.shared.audio_drop.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let packet = AudioPacket {
            seq: frame.seq,
            sample_rate: u32::from(frame.sample_rate),
            channels: u16::from(frame.channels),
            data: frame.data.to_vec(),
        };
        if self.queue.push(packet) {
            self.shared.audio_drop.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// 音频线程：解码 + 播放，失败就关闭通道（不重试）。
///
/// 每解出一个 20ms 的 PCM 块都会算 RMS 汇报给窗口线程（--stats 的 audio_rms），
/// 这样同机回环自激时也能用数字证明"真的收到/解码/播放了声音"。
fn worker(shared: Arc<Shared>, queue: Arc<AudioQueue>, dump_path: Option<String>) {
    let mut decoder: Option<OpusDecoder> = None;
    let mut player: Option<Player> = None;
    let mut format = AudioFormat::default();
    let mut pcm: Vec<u8> = Vec::new();
    let mut dump = match dump_path.as_deref() {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => {
                log::info!("音频 PCM 将写到 {path}（s16le 48k/2ch）");
                Some(file)
            }
            Err(e) => {
                log::warn!("音频 PCM 落盘失败（{path}）：{e}");
                None
            }
        },
        None => None,
    };
    if let Some(file) = dump.as_mut() {
        use std::io::Write as _;
        let _ = file.flush();
    }

    while let Some(packet) = queue.pop() {
        if shared.audio_state() == audio_state::ERR {
            continue;
        }
        // 格式（采样率/声道）变了：重建解码器和播放器
        if decoder.is_none()
            || format.sample_rate != packet.sample_rate
            || u32::from(format.channels) != u32::from(packet.channels)
        {
            let next = AudioFormat {
                sample_rate: packet.sample_rate,
                channels: packet.channels,
            };
            if let Err(e) = next.ensure_supported() {
                disable(&shared, &next, &e);
                continue;
            }
            match OpusDecoder::new(next, None) {
                Ok(dec) => {
                    if format != next {
                        log::info!(
                            "音频格式变化（{} Hz / {} ch）：重建解码器与播放器",
                            next.sample_rate,
                            next.channels
                        );
                    }
                    format = next;
                    decoder = Some(dec);
                    player = None;
                }
                Err(e) => {
                    disable(&shared, &next, &e);
                    continue;
                }
            }
        }
        let Some(dec) = decoder.as_mut() else {
            continue;
        };
        pcm.clear();
        if let Err(e) = dec.decode(&packet.data, &mut pcm) {
            shared.audio_drop.fetch_add(1, Ordering::Relaxed);
            log::debug!("音频解码失败 seq={}：{e}", packet.seq);
            continue;
        }
        if pcm.is_empty() {
            continue;
        }
        // 每个 20ms 包算一次 RMS：验收看 audio_rms > 0.01 就说明真的解出声音了
        let (rms, _peak, _nonzero) = pcm_stats(&pcm);
        shared.add_audio_rms(rms);
        if let Some(file) = dump.as_mut() {
            use std::io::Write as _;
            if let Err(e) = file.write_all(&pcm) {
                log::warn!("音频 PCM 落盘失败：{e}");
                dump = None;
            }
        }
        if player.is_none() {
            match Player::new(format) {
                Ok(p) => player = Some(p),
                Err(e) => {
                    disable(&shared, &format, &e);
                    continue;
                }
            }
        }
        let Some(out) = player.as_mut() else {
            continue;
        };
        match out.write(&pcm) {
            Ok(()) => {
                shared.audio_pkts.fetch_add(1, Ordering::Relaxed);
                let buffered = out.buffered_ms();
                shared.set_audio_buffered_ms(buffered);
                if shared.audio_state() != audio_state::ON {
                    shared.set_audio_state(audio_state::ON);
                    log::info!(
                        "音频通道已启用：{} Hz / {} ch（设备缓冲 {} ms）",
                        format.sample_rate,
                        format.channels,
                        out.device_buffer_ms()
                    );
                }
                if buffered > 120 {
                    log::debug!("音频积压 {buffered} ms（播放器满则丢旧）");
                }
            }
            Err(e) => disable(&shared, &format, &e),
        }
    }
}

/// 关闭音频通道：只告警一次，不重试，不影响视频。
fn disable(shared: &Shared, format: &AudioFormat, err: &lrc_audio::AudioError) {
    shared.set_audio_state(audio_state::ERR);
    log::warn!(
        "音频通道关闭（{} Hz / {} ch）：{err}（不影响视频）",
        format.sample_rate,
        format.channels
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u32) -> AudioPacket {
        AudioPacket {
            seq,
            sample_rate: 48_000,
            channels: 2,
            data: vec![0u8; 4],
        }
    }

    #[test]
    fn queue_drops_oldest_when_full() {
        let queue = AudioQueue::new();
        let mut dropped = 0;
        for seq in 0..(QUEUE_DEPTH as u32 + 5) {
            if queue.push(packet(seq)) {
                dropped += 1;
            }
        }
        assert_eq!(dropped, 5, "超出深度的 5 个包应丢最旧");
        // 剩下的应该是最新的 10 个；先 close 再 drain（否则 pop 会一直等新包）
        queue.close();
        let mut seqs = Vec::new();
        while let Some(p) = queue.pop() {
            seqs.push(p.seq);
        }
        assert_eq!(seqs.len(), QUEUE_DEPTH);
        assert_eq!(seqs.first().copied(), Some(5));
        assert_eq!(seqs.last().copied(), Some(14));
    }

    #[test]
    fn queue_close_wakes_consumer() {
        let queue = AudioQueue::new();
        queue.close();
        assert!(queue.pop().is_none(), "关闭后 pop 应返回 None");
        // push 之后仍能取到（close 只影响空队列退出）
        let queue2 = AudioQueue::new();
        queue2.push(packet(1));
        queue2.close();
        assert_eq!(queue2.pop().map(|p| p.seq), Some(1));
        assert!(queue2.pop().is_none());
    }
}
