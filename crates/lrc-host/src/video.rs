//! M2 视频通道：块模式 ⇄ 视频模式自动切换 + NVENC 编码线程。
//!
//! 设计要点：
//! - 编码器在**会话建立时**创建并预热一次（探测 NVENC 是否可用），不在每帧 new；
//! - 编码跑在独立线程，采集线程只把最新的整帧丢进单槽邮箱（满了覆盖 = 丢旧帧，不排队）；
//! - 切模式用"世代号"：切走之后旧世代在途/待编码的 AU 直接丢弃，客户端不会收到
//!   跨越两次模式切换的脏码流；
//! - 编码器任何一次失败 → 打印一次告警并永久回块模式，不重试。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use lrc_capture::Frame;
use lrc_proto::{Message, VideoAu};
use lrc_video::{EncoderConfig, FfmpegEncoder, VideoEncoder};

use crate::cli::{Args, VideoChoice};
use crate::session::Shared;

/// 进视频模式：变化块比例 > ENTER_RATIO 连续 ENTER_FRAMES 帧
const ENTER_RATIO: f32 = 0.45;
const ENTER_FRAMES: u32 = 3;
/// 回块模式：变化块比例 < EXIT_RATIO 连续 EXIT_FRAMES 帧
const EXIT_RATIO: f32 = 0.10;
const EXIT_FRAMES: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Blocks,
    Video,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Blocks => "blocks",
            Mode::Video => "video",
        }
    }
}

/// 状态机的动作。调用方据此发 IDR / 回块模式发整帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEvent {
    None,
    /// 刚进入视频模式：需要请求一个 IDR
    EnterVideo,
    /// 刚回到块模式：需要发一整帧块模式消息（FULL|KEY）
    ExitToBlocks,
}

/// 块/视频模式判定（纯逻辑，方便单测）。
pub struct ModeSwitch {
    video_available: bool,
    mode: Mode,
    high_streak: u32,
    low_streak: u32,
}

impl ModeSwitch {
    pub fn new(video_available: bool) -> Self {
        Self {
            video_available,
            mode: Mode::Blocks,
            high_streak: 0,
            low_streak: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 视频通道是否可用（编码器探测通过且没被永久禁用）。
    pub fn video_available(&self) -> bool {
        self.video_available
    }

    /// 强制进入视频模式（块模式码率判据用，并联于 changed_ratio 判据）。
    /// 调用方负责请求 IDR / 换世代。
    pub fn force_video(&mut self) {
        if !self.video_available {
            return;
        }
        self.mode = Mode::Video;
        self.high_streak = 0;
        self.low_streak = 0;
    }

    /// 强制回块模式（控制器/缩放要求退出视频时用，保持状态机与实际发送路径一致）。
    pub fn force_blocks(&mut self) {
        self.mode = Mode::Blocks;
        self.high_streak = 0;
        self.low_streak = 0;
    }

    /// 编码器不可用（探测失败 / 运行期失败）：永久回块模式。
    pub fn disable_video(&mut self) {
        self.video_available = false;
        self.mode = Mode::Blocks;
        self.high_streak = 0;
        self.low_streak = 0;
    }

    /// 观察一帧的变化块比例，返回需要执行的动作。
    pub fn observe(&mut self, changed: usize, total: usize) -> ModeEvent {
        if !self.video_available || total == 0 {
            return ModeEvent::None;
        }
        let ratio = changed as f32 / total as f32;
        if ratio > ENTER_RATIO {
            self.high_streak = self.high_streak.saturating_add(1);
            self.low_streak = 0;
        } else if ratio < EXIT_RATIO {
            self.low_streak = self.low_streak.saturating_add(1);
            self.high_streak = 0;
        } else {
            // 中间地带：两边的"连续"都断了
            self.high_streak = 0;
            self.low_streak = 0;
        }

        match self.mode {
            Mode::Blocks if self.high_streak >= ENTER_FRAMES => {
                self.mode = Mode::Video;
                self.high_streak = 0;
                self.low_streak = 0;
                ModeEvent::EnterVideo
            }
            Mode::Video if self.low_streak >= EXIT_FRAMES => {
                self.mode = Mode::Blocks;
                self.high_streak = 0;
                self.low_streak = 0;
                ModeEvent::ExitToBlocks
            }
            _ => ModeEvent::None,
        }
    }
}

/// 视频通道的累计计数（被控端 --stats 用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoCounters {
    /// 发出的 AU 数
    pub aus: u64,
    /// 发出的线上字节（含 4 字节长度前缀）
    pub bytes: u64,
}

struct Job {
    frame: Arc<Frame>,
    seq: u32,
    generation: u32,
    /// 抓帧完成时刻的 GetTickCount64() 低 32 位（客户端用同一时钟算 lat_ms）
    ts_ms: u32,
}

/// 编码线程与采集线程共享的状态。
struct Pipe {
    /// 单槽邮箱：新帧覆盖旧帧（丢旧帧比堆队列好）
    slot: Mutex<Option<Job>>,
    cv: Condvar,
    stop: AtomicBool,
    /// 模式世代：切模式 +1，worker 发之前比对
    generation: AtomicU32,
    /// 下一个编码帧要 IDR
    keyframe_req: AtomicBool,
    /// 编码失败 → 永久回块模式
    broken: AtomicBool,
    /// 写 socket 失败 → 会话该结束了
    send_failed: AtomicBool,
    /// 自适应控制器要求的新码率（0 = 无请求）；由编码线程在主循环里应用
    bitrate_req: AtomicU32,
    aus: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
}

impl Pipe {
    fn lock_slot(&self) -> std::sync::MutexGuard<'_, Option<Job>> {
        match self.slot.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }
}

/// 视频流水线：一个编码器 + 一个编码线程。
pub struct VideoPipeline {
    pipe: Arc<Pipe>,
    handle: Option<std::thread::JoinHandle<()>>,
    codec_name: String,
    /// 编码器创建时的画面尺寸（切显示输出后尺寸不一致就不能再用）
    frame_size: (u32, u32),
}

impl VideoPipeline {
    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    /// 编码器创建时的画面尺寸。
    pub fn frame_size(&self) -> (u32, u32) {
        self.frame_size
    }

    /// 请求改视频码率（编码线程在下一帧前用 set_bitrate 应用；会重启编码进程 ~100-300ms，
    /// 新进程第一帧必为 IDR，客户端能立刻接上）。
    pub fn request_bitrate(&self, bps: u32) {
        self.pipe.bitrate_req.store(bps.max(1), Ordering::Relaxed);
    }

    /// 请求下一个编码帧为 IDR。
    pub fn request_keyframe(&self) {
        self.pipe.keyframe_req.store(true, Ordering::Relaxed);
    }

    /// 开始一个新的模式世代，返回新世代号；旧世代的在途 AU 会被丢掉。
    pub fn new_generation(&self) -> u32 {
        self.pipe
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// 把整帧交给编码线程（Arc 复用，不拷贝像素）。邮箱里已有的旧帧会被丢掉。
    ///
    /// ts_ms 是**抓帧完成时刻**的 GetTickCount64 低 32 位（不是发送时刻）：
    /// 这样客户端算出的 lat_ms 才包含"排队 + 编码 + 网络 + 解码"的整条管线耗时。
    pub fn submit(&self, frame: Arc<Frame>, seq: u32, generation: u32, ts_ms: u32) {
        {
            let mut slot = self.pipe.lock_slot();
            if slot.is_some() {
                self.pipe.dropped.fetch_add(1, Ordering::Relaxed);
            }
            *slot = Some(Job {
                frame,
                seq,
                generation,
                ts_ms,
            });
        }
        self.pipe.cv.notify_one();
    }

    pub fn counters(&self) -> VideoCounters {
        VideoCounters {
            aus: self.pipe.aus.load(Ordering::Relaxed),
            bytes: self.pipe.bytes.load(Ordering::Relaxed),
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.pipe.dropped.load(Ordering::Relaxed)
    }

    /// 编码器挂了（运行期失败），调用方应回块模式并永久禁用视频。
    pub fn is_broken(&self) -> bool {
        self.pipe.broken.load(Ordering::Relaxed)
    }

    /// 编码线程写 socket 失败（对端断开等），会话该收尾了。
    pub fn send_failed(&self) -> bool {
        self.pipe.send_failed.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.pipe.stop.store(true, Ordering::Relaxed);
        self.pipe.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                log::warn!("视频编码线程 panic");
            }
        }
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 会话建立时调用：创建编码器 + 预热探测 + 启动编码线程。
///
/// 返回 None 表示视频通道不可用（调用方应当永久留在块模式，不要重试）。
pub fn start(args: &Args, width: u32, height: u32, shared: Arc<Shared>) -> Option<VideoPipeline> {
    if args.video.is_off() {
        return None;
    }
    let ffmpeg_codec = match args.video {
        VideoChoice::H264 => "h264_nvenc",
        // auto / hevc 都先用 hevc_nvenc；探测失败就整条通道不可用（不会退回 h264）
        _ => "hevc_nvenc",
    };
    let Some(tag) = lrc_video::codec_tag_of(ffmpeg_codec) else {
        log::warn!("视频通道不可用（未知编码器 {ffmpeg_codec}），永久使用块模式");
        return None;
    };
    let fps = args.video_max_fps.max(1);
    let cfg = EncoderConfig {
        width,
        height,
        bitrate_bps: args.video_bitrate,
        // 2 秒一个 IDR：既保证可恢复，又不会吃掉太多码率
        gop: fps.saturating_mul(2).max(1),
        fps,
        codec: ffmpeg_codec.to_string(),
        ffmpeg_path: None,
    };

    // new() 内部会先 probe（真跑一次最短编码）：不可用时返回 Unsupported，这里一次性告警后永久块模式
    let t = Instant::now();
    let encoder = match FfmpegEncoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("视频通道不可用（永久使用块模式）：{e}");
            return None;
        }
    };
    log::info!(
        "视频通道就绪：{} {}x{} @{}fps {}kbps（探测+启动 {} ms）",
        ffmpeg_codec,
        width,
        height,
        fps,
        args.video_bitrate / 1000,
        t.elapsed().as_millis()
    );

    let pipe = Arc::new(Pipe {
        slot: Mutex::new(None),
        cv: Condvar::new(),
        stop: AtomicBool::new(false),
        generation: AtomicU32::new(0),
        keyframe_req: AtomicBool::new(false),
        broken: AtomicBool::new(false),
        send_failed: AtomicBool::new(false),
        bitrate_req: AtomicU32::new(0),
        aus: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });

    let spawned = {
        let pipe = Arc::clone(&pipe);
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("lrc-host-video".to_string())
            .spawn(move || encode_loop(encoder, pipe, shared, tag))
    };
    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            log::warn!("视频通道不可用（启动编码线程失败，永久使用块模式）：{e}");
            return None;
        }
    };

    Some(VideoPipeline {
        pipe,
        handle: Some(handle),
        codec_name: ffmpeg_codec.to_string(),
        frame_size: (width, height),
    })
}

fn encode_loop(mut encoder: FfmpegEncoder, pipe: Arc<Pipe>, shared: Arc<Shared>, codec: u8) {
    loop {
        // 取最新一帧；没有就睡在条件变量上
        let job = {
            let mut slot = pipe.lock_slot();
            loop {
                if pipe.stop.load(Ordering::Relaxed) {
                    encoder.shutdown();
                    return;
                }
                if let Some(job) = slot.take() {
                    break job;
                }
                slot = match pipe.cv.wait(slot) {
                    Ok(g) => g,
                    Err(poison) => poison.into_inner(),
                };
            }
        };

        // 切模式之后剩下的旧帧直接丢
        if job.generation != pipe.generation.load(Ordering::Relaxed) {
            continue;
        }

        let want_bitrate = pipe.bitrate_req.swap(0, Ordering::Relaxed);
        if want_bitrate != 0 && want_bitrate != encoder.bitrate() {
            match encoder.set_bitrate(want_bitrate) {
                Ok(()) => log::info!(
                    "视频：码率切到 {}kbps（重启 {}us，新进程首帧为 IDR）",
                    want_bitrate / 1000,
                    encoder.restart_cost_us()
                ),
                Err(e) => log::warn!("视频：切换码率失败：{e}"),
            }
        }
        if pipe.keyframe_req.swap(false, Ordering::Relaxed) {
            if let Err(e) = encoder.request_keyframe() {
                log::warn!("视频：请求 IDR 失败：{e}");
            }
        }

        let aus = match encoder.encode(&job.frame.bgra, job.frame.stride) {
            Ok(aus) => aus,
            Err(e) => {
                log::warn!("视频编码失败，永久回退块模式：{e}");
                pipe.broken.store(true, Ordering::Relaxed);
                encoder.shutdown();
                return;
            }
        };
        let key = encoder.last_au_was_keyframe();
        for au in aus {
            if job.generation != pipe.generation.load(Ordering::Relaxed) {
                break;
            }
            if au.len() > lrc_proto::message::MAX_VIDEO_AU {
                log::warn!(
                    "视频 AU 过大 {} > {}，丢弃",
                    au.len(),
                    lrc_proto::message::MAX_VIDEO_AU
                );
                continue;
            }
            let msg = Message::VideoAu(VideoAu {
                // Lead 要求：GetTickCount64 低 32 位（和客户端 lat_ms 的 now 同源）。
                // ⚠ 只有同机测试才有意义（跨机时钟不同源，lat_ms 只能作参考）。
                ts_ms: job.ts_ms,
                seq: job.seq,
                codec,
                flags: if key { 1 } else { 0 },
                data: &au,
            });
            let payload = msg.encode();
            if let Err(e) = shared.send(&payload) {
                log::warn!("视频：发送码流失败：{e}");
                pipe.send_failed.store(true, Ordering::Relaxed);
                encoder.shutdown();
                return;
            }
            shared.note_send(job.seq);
            pipe.aus.fetch_add(1, Ordering::Relaxed);
            pipe.bytes
                .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total() -> usize {
        510
    }

    #[test]
    fn enters_video_after_three_high_frames() {
        let mut sw = ModeSwitch::new(true);
        assert_eq!(sw.mode(), Mode::Blocks);
        assert_eq!(sw.observe(400, total()), ModeEvent::None);
        assert_eq!(sw.observe(400, total()), ModeEvent::None);
        assert_eq!(sw.mode(), Mode::Blocks);
        assert_eq!(sw.observe(400, total()), ModeEvent::EnterVideo);
        assert_eq!(sw.mode(), Mode::Video);
    }

    #[test]
    fn high_streak_broken_by_middle_frame() {
        let mut sw = ModeSwitch::new(true);
        sw.observe(400, total());
        sw.observe(400, total());
        // 中间地带（10%~45%）打断连续
        sw.observe(150, total());
        assert_eq!(sw.observe(400, total()), ModeEvent::None);
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn exits_video_after_thirty_low_frames() {
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(400, total());
        }
        assert_eq!(sw.mode(), Mode::Video);
        for _ in 0..29 {
            assert_eq!(sw.observe(0, total()), ModeEvent::None);
        }
        assert_eq!(sw.mode(), Mode::Video);
        assert_eq!(sw.observe(0, total()), ModeEvent::ExitToBlocks);
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn low_streak_broken_by_big_frame() {
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(400, total());
        }
        for _ in 0..20 {
            sw.observe(1, total());
        }
        sw.observe(400, total());
        for _ in 0..20 {
            assert_eq!(sw.observe(1, total()), ModeEvent::None);
        }
        assert_eq!(sw.mode(), Mode::Video);
    }

    #[test]
    fn unavailable_video_never_enters() {
        let mut sw = ModeSwitch::new(false);
        for _ in 0..100 {
            assert_eq!(sw.observe(500, total()), ModeEvent::None);
        }
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn disable_video_forces_blocks_and_stops_events() {
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(400, total());
        }
        assert_eq!(sw.mode(), Mode::Video);
        sw.disable_video();
        assert_eq!(sw.mode(), Mode::Blocks);
        for _ in 0..100 {
            assert_eq!(sw.observe(500, total()), ModeEvent::None);
        }
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn re_entry_after_exit_needs_three_again() {
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(400, total());
        }
        for _ in 0..30 {
            sw.observe(0, total());
        }
        assert_eq!(sw.mode(), Mode::Blocks);
        assert_eq!(sw.observe(400, total()), ModeEvent::None);
        assert_eq!(sw.observe(400, total()), ModeEvent::None);
        assert_eq!(sw.observe(400, total()), ModeEvent::EnterVideo);
    }

    #[test]
    fn zero_total_is_ignored() {
        let mut sw = ModeSwitch::new(true);
        for _ in 0..10 {
            assert_eq!(sw.observe(0, 0), ModeEvent::None);
        }
        assert_eq!(sw.mode(), Mode::Blocks);
    }

    #[test]
    fn ratio_boundaries() {
        // 恰好 45% 不算「> 45%」
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(229, 510); // 0.4490
        }
        assert_eq!(sw.mode(), Mode::Blocks);
        // 46% 算
        let mut sw = ModeSwitch::new(true);
        for _ in 0..3 {
            sw.observe(235, 510); // 0.4608
        }
        assert_eq!(sw.mode(), Mode::Video);
    }
}
