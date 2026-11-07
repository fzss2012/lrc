//! 接收线程 ↔ 窗口线程共享的状态。
//!
//! 窗口线程（主线程）是唯一碰 D3D/GDI 的地方；接收线程只解码并把"脏块"塞进队列，
//! 由 8ms 定时器在主线程 drain。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lrc_proto::{DisplayInfo, Grid};
use lrc_udp::UdpStats;
use windows::Win32::System::SystemInformation::GetTickCount64;

/// 音频通道状态（--stats 的 audio= 字段）。
pub mod audio_state {
    /// 未启用（--audio off，或还没收到音频包）
    pub const OFF: u8 = 0;
    /// 正在播放
    pub const ON: u8 = 1;
    /// 设备/解码不可用，通道已关闭（不重试、不影响视频）
    pub const ERR: u8 = 2;
}

/// 连接状态：窗口线程据此改标题栏提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    /// 没连上（首次连接中或断线重连中）；`attempt` 从 1 开始
    Reconnecting { attempt: u32 },
    Connected,
    /// 终止状态（认证失败/版本不符）：不再重连，窗口保留并显示提示
    Terminal { message: String },
}

/// 重连退避：第 1/2/3/4 次分别等 1s/2s/4s/8s，之后封顶 10s。
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let secs = 1u64 << shift; // 1, 2, 4, 8, 16
    Duration::from_secs(secs.min(10))
}

/// 待呈现队列上限：一帧 1080p 全屏也就 510 块，8192 已经是"主线程卡了很久"。
/// 超过就丢弃并请求整帧，避免无界内存增长。
pub const MAX_PENDING_BLOCKS: usize = 8192;

/// 一个待呈现的脏块（BGRA，行连续，stride = w*4）。
#[derive(Clone, Debug, Default)]
pub struct BlockUpdate {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// 被主控端拥有的光标位图（BGRA，行连续，长度 = 4*w*h）。
///
/// 线上 `CursorShape.rgba` 实际存的是 BGRA（proto 字段名不准），alpha 在第 4 字节。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CursorShapeOwned {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bgra: Vec<u8>,
}

/// 一帧解码出来的视频画面（BGRA 整帧）。只保留最新一帧。
#[derive(Clone, Debug, Default)]
pub struct VideoFrameUpdate {
    pub width: u32,
    pub height: u32,
    /// 行步长（= width*4）
    pub stride: usize,
    pub bgra: Vec<u8>,
    /// host 采集完成时刻（`VideoAu.ts_ms`），用来算管线延迟
    pub ts_ms: u32,
    pub seq: u32,
}

/// 系统开机毫秒（u32 截断）。
///
/// `VideoAu.ts_ms` 用它当时间基准时，同机 host/client 共享同一个系统时钟，
/// 算出来的管线延迟才有意义；**跨机时两端开机时间不同，差值只能当参考**
/// （甚至可能为负/巨大，这时 `lat_ms` 就只是个数字，不代表真实延迟）。
pub fn host_clock_ms() -> u32 {
    let uptime = unsafe { GetTickCount64() };
    (uptime & 0xFFFF_FFFF) as u32
}

/// 某一时刻的光标状态。
///
/// `x`/`y` 是**被控端帧像素坐标**（热点位置，与 VideoFrame 的块坐标同一坐标系）。
#[derive(Clone, Debug, Default)]
pub struct CursorSnapshot {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape: Option<Arc<CursorShapeOwned>>,
}

/// 加锁，但把"毒锁"当成正常情况处理（某个线程 panic 后仍要能继续显示画面）。
pub fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 跨线程共享状态。只用原子量和 Mutex，保证 `Send + Sync`。
pub struct Shared {
    /// 接收线程解好的脏块，等主线程取走
    pub pending: Mutex<Vec<BlockUpdate>>,
    /// 被控端屏幕物理像素尺寸（来自 ServerHello），0 表示未知
    pub remote_w: AtomicU32,
    /// 被控端屏幕物理像素尺寸（来自 ServerHello），0 表示未知
    pub remote_h: AtomicU32,
    /// 收到并解出的帧数
    pub frame_count: AtomicU64,
    /// 累计解码成功的块数
    pub decoded_blocks: AtomicU64,
    /// 累计收到的 payload 字节（不含 4 字节长度前缀）
    pub recv_bytes: AtomicU64,
    /// 最近一帧的 seq（用于 VideoAck）
    pub last_seq: AtomicU32,
    /// 队列积压时置位，主线程看到后发 RequestFull
    want_full: AtomicBool,
    /// 是否已经收到过整帧（flags FULL）—— 主线程据此做一次窗口宽高比自适应
    first_full_frame: AtomicBool,
    connected: AtomicBool,
    quit: AtomicBool,
    /// 当前连接状态（窗口线程据此改标题）
    status: Mutex<ConnStatus>,
    /// 成功重连次数（第一次连接不算）
    pub reconnects: AtomicU32,
    /// 当前会话的发送端；断线期间为 None
    out_tx: Mutex<Option<Sender<Vec<u8>>>>,
    /// 光标状态 + 版本号（版本变了主线程才重绘光标）
    cursor: Mutex<CursorSnapshot>,
    cursor_version: AtomicU64,
    /// 最近一次 Ping 的 (ts_ms, 发出时刻)，只保留一条（1 秒一个，RTT 远小于 1 秒）
    ping_pending: Mutex<Option<(u64, Instant)>>,
    /// 最近一次测得的 RTT（微秒），0 = 还没测到
    rtt_micros: AtomicU64,
    /// 单调时钟原点（Ping 的 ts_ms 用它算；两个线程共用）
    epoch: Instant,
    /// 最新解出的视频帧（窗口线程取走即清空；只保留最新一帧）
    video_frame: Mutex<Option<VideoFrameUpdate>>,
    /// 当前通道模式：0 = 块，1 = 视频
    mode: AtomicU8,
    /// 累计解出的视频帧数
    pub video_frames: AtomicU64,
    /// 视频解码失败次数
    pub decode_errors: AtomicU64,
    /// 管线延迟滑动窗口：累加毫秒 + 样本数（窗口线程每秒取走并清零）
    lat_sum_ms: AtomicU64,
    lat_samples: AtomicU64,
    /// 从视频模式切回块模式：要求窗口线程整屏重建 clean
    block_full_reset: AtomicBool,
    /// 音频通道状态（audio_state::*）
    audio: AtomicU8,
    /// 已成功写进播放器的音频包数
    pub audio_pkts: AtomicU64,
    /// 丢掉的音频包数（队列满 / seq 跳变 / 解码失败）
    pub audio_drop: AtomicU64,
    /// 最近一次播放器水位（毫秒）
    audio_ms: AtomicU32,
    /// 解码 PCM 的 RMS 累加（rms×1e6）与样本数，窗口线程每秒取一次平均
    audio_rms_acc: AtomicU64,
    audio_rms_n: AtomicU64,
    /// host 上报的显示输出列表（DisplayList）
    displays: Mutex<Vec<DisplayInfo>>,
    /// 当前链路路径（tcp / direct_udp / relay_udp），--stats 的 path= 用
    link_path: Mutex<&'static str>,
    /// UDP 链路质量（TCP 路径为 None）
    udp: Mutex<Option<UdpStats>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            remote_w: AtomicU32::new(0),
            remote_h: AtomicU32::new(0),
            frame_count: AtomicU64::new(0),
            decoded_blocks: AtomicU64::new(0),
            recv_bytes: AtomicU64::new(0),
            last_seq: AtomicU32::new(0),
            want_full: AtomicBool::new(false),
            first_full_frame: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            status: Mutex::new(ConnStatus::Reconnecting { attempt: 0 }),
            reconnects: AtomicU32::new(0),
            out_tx: Mutex::new(None),
            cursor: Mutex::new(CursorSnapshot::default()),
            cursor_version: AtomicU64::new(0),
            ping_pending: Mutex::new(None),
            rtt_micros: AtomicU64::new(0),
            epoch: Instant::now(),
            video_frame: Mutex::new(None),
            mode: AtomicU8::new(0),
            video_frames: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            lat_sum_ms: AtomicU64::new(0),
            lat_samples: AtomicU64::new(0),
            block_full_reset: AtomicBool::new(false),
            audio: AtomicU8::new(audio_state::OFF),
            audio_pkts: AtomicU64::new(0),
            audio_drop: AtomicU64::new(0),
            audio_ms: AtomicU32::new(0),
            audio_rms_acc: AtomicU64::new(0),
            audio_rms_n: AtomicU64::new(0),
            displays: Mutex::new(Vec::new()),
            link_path: Mutex::new("tcp"),
            udp: Mutex::new(None),
        }
    }

    // ---- 连接状态 / 发送端 ----

    pub fn set_status(&self, status: ConnStatus) {
        *lock_or_recover(&self.status) = status;
    }

    pub fn status(&self) -> ConnStatus {
        lock_or_recover(&self.status).clone()
    }

    pub fn bump_reconnects(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reconnect_count(&self) -> u32 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// 会话建立时窗口线程从这里拿发送端；断开时置 None。
    pub fn set_out_tx(&self, tx: Option<Sender<Vec<u8>>>) {
        *lock_or_recover(&self.out_tx) = tx;
    }

    /// 往当前会话发一条已编码消息；没连接就丢掉（断线期间的输入/ack 不需要排队）。
    pub fn send(&self, payload: Vec<u8>) -> bool {
        let tx = lock_or_recover(&self.out_tx).clone();
        match tx {
            Some(tx) => tx.send(payload).is_ok(),
            None => false,
        }
    }

    /// 会话结束/重连：清掉旧会话的脏块与视频帧，避免旧状态串台。
    pub fn clear_session_state(&self) {
        lock_or_recover(&self.pending).clear();
        *lock_or_recover(&self.video_frame) = None;
        self.want_full.store(false, Ordering::Relaxed);
        self.mode.store(0, Ordering::Relaxed);
        self.first_full_frame.store(false, Ordering::SeqCst);
        // 旧会话的"整屏重建"标记不该带到新会话（新会话会重新发 RequestFull）
        self.block_full_reset.store(false, Ordering::Relaxed);
    }

    /// 记录被控端尺寸；返回是否变化。
    pub fn set_remote_size(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        let old_w = self.remote_w.swap(w, Ordering::SeqCst);
        let old_h = self.remote_h.swap(h, Ordering::SeqCst);
        old_w != w || old_h != h
    }

    /// 用块帧网格校正画布尺寸（服务端降分辨率时 ServerHello 的宽高不再等于实际帧）。
    ///
    /// 网格尺寸是**向上取整到整块**的（1080 高 → 17×64 = 1088），所以只有
    /// "帧网格比宣布的画布**小**"才认为服务端降分辨率了；正常 1080p 的网格
    /// （1920x1088）不会被误改成 1088 高（否则末行块尺寸和 host 编码对不上）。
    /// 返回是否真的改了。
    pub fn reconcile_canvas_from_grid(&self, grid: Grid) -> bool {
        let width = u32::from(grid.blocks_x) * u32::from(grid.block_size);
        let height = u32::from(grid.blocks_y) * u32::from(grid.block_size);
        if width == 0 || height == 0 {
            return false;
        }
        match self.remote_size() {
            Some((sw, sh)) if width >= sw && height >= sh => false,
            _ => self.set_remote_size(width, height),
        }
    }

    /// 被控端尺寸；未知返回 `None`。
    pub fn remote_size(&self) -> Option<(u32, u32)> {
        let w = self.remote_w.load(Ordering::SeqCst);
        let h = self.remote_h.load(Ordering::SeqCst);
        if w == 0 || h == 0 {
            None
        } else {
            Some((w, h))
        }
    }

    /// 把一帧解出的脏块推进队列；积压超限则丢弃并请求整帧。
    pub fn push_blocks(&self, blocks: Vec<BlockUpdate>) {
        if blocks.is_empty() {
            return;
        }
        let mut pending = lock_or_recover(&self.pending);
        if pending.len() + blocks.len() > MAX_PENDING_BLOCKS {
            log::warn!(
                "脏块队列积压 {} 块，丢弃本帧 {} 块并请求整帧",
                pending.len(),
                blocks.len()
            );
            self.want_full.store(true, Ordering::Relaxed);
            return;
        }
        pending.extend(blocks);
    }

    /// 主线程取走所有待呈现块。
    pub fn take_blocks(&self) -> Vec<BlockUpdate> {
        let mut pending = lock_or_recover(&self.pending);
        if pending.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut *pending)
        }
    }

    /// 请求一帧整屏（尺寸变化 / 呈现面重建后）。
    pub fn request_full(&self) {
        self.want_full.store(true, Ordering::Relaxed);
    }

    /// 读取并清除"需要整帧"标记。
    pub fn take_want_full(&self) -> bool {
        self.want_full.swap(false, Ordering::Relaxed)
    }

    /// 收到整帧（flags FULL）时置位：主线程据此做首帧窗口宽高比自适应。
    pub fn mark_first_full_frame(&self) {
        self.first_full_frame.store(true, Ordering::SeqCst);
    }

    /// 是否已经收到过整帧。
    pub fn has_first_full_frame(&self) -> bool {
        self.first_full_frame.load(Ordering::SeqCst)
    }

    // ---- 光标 ----

    /// 更新光标位置/可见性（收到 CursorPos）。没变化就不动版本号。
    pub fn set_cursor_pos(&self, x: i32, y: i32, visible: bool) {
        {
            let mut cursor = lock_or_recover(&self.cursor);
            if cursor.x == x && cursor.y == y && cursor.visible == visible {
                return;
            }
            cursor.x = x;
            cursor.y = y;
            cursor.visible = visible;
        }
        self.cursor_version.fetch_add(1, Ordering::SeqCst);
    }

    /// 更新光标位图（收到 CursorShape）；`None` 表示隐藏/清空。
    pub fn set_cursor_shape(&self, shape: Option<CursorShapeOwned>) {
        {
            let mut cursor = lock_or_recover(&self.cursor);
            cursor.shape = shape.map(Arc::new);
        }
        self.cursor_version.fetch_add(1, Ordering::SeqCst);
    }

    /// 取光标快照 + 版本号。主线程只在版本变化时重绘光标。
    pub fn cursor_snapshot(&self) -> (CursorSnapshot, u64) {
        let version = self.cursor_version.load(Ordering::SeqCst);
        let snapshot = lock_or_recover(&self.cursor).clone();
        (snapshot, version)
    }

    // ---- RTT（Ping/Pong）----

    /// 单调时钟（毫秒），作为 Ping 的 `ts_ms`。
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// 记录一次 Ping，返回要发到线上的 `ts_ms`。
    pub fn begin_ping(&self) -> u64 {
        let ts = self.now_ms();
        *lock_or_recover(&self.ping_pending) = Some((ts, Instant::now()));
        ts
    }

    /// 收到 Pong：匹配上最近一次 Ping 就记下 RTT（微秒）并返回。
    pub fn finish_ping(&self, ts: u64) -> Option<u64> {
        let mut slot = lock_or_recover(&self.ping_pending);
        let elapsed = match slot.as_ref() {
            Some((sent_ts, sent_at)) if *sent_ts == ts => Some(sent_at.elapsed()),
            _ => None,
        };
        let elapsed = elapsed?;
        *slot = None;
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.rtt_micros.store(micros, Ordering::Relaxed);
        Some(micros)
    }

    /// 最近一次 RTT（微秒）；0 表示还没测到。
    pub fn rtt_micros(&self) -> u64 {
        self.rtt_micros.load(Ordering::Relaxed)
    }

    // ---- 视频通道 ----

    /// 放入最新解出的视频帧（覆盖上一帧：解码跟不上时只保留最新，旧帧直接丢）。
    pub fn put_video_frame(&self, frame: VideoFrameUpdate) {
        *lock_or_recover(&self.video_frame) = Some(frame);
    }

    /// 窗口线程取走最新视频帧。
    pub fn take_video_frame(&self) -> Option<VideoFrameUpdate> {
        lock_or_recover(&self.video_frame).take()
    }

    pub fn set_mode_block(&self) {
        self.mode.store(0, Ordering::Relaxed);
    }

    pub fn set_mode_video(&self) {
        self.mode.store(1, Ordering::Relaxed);
    }

    pub fn is_video_mode(&self) -> bool {
        self.mode.load(Ordering::Relaxed) == 1
    }

    /// 记一次视频帧的管线延迟（毫秒），进每秒滑动窗口。
    pub fn add_latency_ms(&self, ms: u64) {
        self.lat_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.lat_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// 取走最近一个窗口的平均延迟；这一秒没有样本返回 None。
    pub fn take_latency_avg_ms(&self) -> Option<f64> {
        let samples = self.lat_samples.swap(0, Ordering::Relaxed);
        let sum = self.lat_sum_ms.swap(0, Ordering::Relaxed);
        if samples == 0 {
            None
        } else {
            Some(sum as f64 / samples as f64)
        }
    }

    /// 从视频模式切回块模式：要求窗口线程用整帧重建 clean，清掉视频帧/光标残影。
    pub fn mark_block_full_reset(&self) {
        self.block_full_reset.store(true, Ordering::Relaxed);
    }

    pub fn take_block_full_reset(&self) -> bool {
        self.block_full_reset.swap(false, Ordering::Relaxed)
    }

    pub fn bump_decode_error(&self) {
        self.decode_errors.fetch_add(1, Ordering::Relaxed);
    }

    // ---- 音频 ----

    pub fn set_audio_state(&self, state: u8) {
        self.audio.store(state, Ordering::Relaxed);
    }

    pub fn audio_state(&self) -> u8 {
        self.audio.load(Ordering::Relaxed)
    }

    /// audio= 字段的文本形式
    pub fn audio_state_name(&self) -> &'static str {
        match self.audio_state() {
            audio_state::ON => "on",
            audio_state::ERR => "err",
            _ => "off",
        }
    }

    pub fn set_audio_buffered_ms(&self, ms: u32) {
        self.audio_ms.store(ms, Ordering::Relaxed);
    }

    pub fn audio_buffered_ms(&self) -> u32 {
        self.audio_ms.load(Ordering::Relaxed)
    }

    /// 记一次解码 PCM 的 RMS（每个 20ms 包一次），进每秒滑窗。
    pub fn add_audio_rms(&self, rms: f32) {
        let scaled = (f64::from(rms.max(0.0)) * 1_000_000.0) as u64;
        self.audio_rms_acc.fetch_add(scaled, Ordering::Relaxed);
        self.audio_rms_n.fetch_add(1, Ordering::Relaxed);
    }

    /// 取走最近一秒的 RMS 平均；这一秒没有样本返回 None。
    pub fn take_audio_rms(&self) -> Option<f32> {
        let samples = self.audio_rms_n.swap(0, Ordering::Relaxed);
        let sum = self.audio_rms_acc.swap(0, Ordering::Relaxed);
        if samples == 0 {
            None
        } else {
            Some((sum as f64 / samples as f64 / 1_000_000.0) as f32)
        }
    }

    // ---- 显示输出列表 ----

    pub fn set_displays(&self, list: Vec<DisplayInfo>) {
        *lock_or_recover(&self.displays) = list;
    }

    pub fn displays(&self) -> Vec<DisplayInfo> {
        lock_or_recover(&self.displays).clone()
    }

    // ---- 链路路径 / UDP 质量 ----

    /// 更新当前链路路径；切回 TCP 时清掉旧的 UDP 统计。
    pub fn set_link_path(&self, path: &'static str) {
        *lock_or_recover(&self.link_path) = path;
        if path == "tcp" {
            *lock_or_recover(&self.udp) = None;
        }
    }

    pub fn link_path(&self) -> &'static str {
        *lock_or_recover(&self.link_path)
    }

    pub fn set_udp_stats(&self, stats: UdpStats) {
        *lock_or_recover(&self.udp) = Some(stats);
    }

    pub fn udp_stats(&self) -> Option<UdpStats> {
        *lock_or_recover(&self.udp)
    }

    pub fn add_recv_bytes(&self, n: u64) {
        self.recv_bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_connected(&self, value: bool) {
        self.connected.store(value, Ordering::SeqCst);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// 窗口关闭：通知接收/发送线程收工。
    pub fn set_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
    }

    pub fn is_quit(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }


}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_size_roundtrip() {
        let shared = Shared::new();
        assert!(shared.remote_size().is_none());
        assert!(shared.set_remote_size(1920, 1080));
        assert_eq!(shared.remote_size(), Some((1920, 1080)));
        assert!(!shared.set_remote_size(1920, 1080));
        assert!(shared.set_remote_size(1280, 720));
        assert!(!shared.set_remote_size(0, 720));
        assert_eq!(shared.remote_size(), Some((1280, 720)));
    }

    #[test]
    fn push_and_take_blocks() {
        let shared = Shared::new();
        assert!(shared.take_blocks().is_empty());
        let block = BlockUpdate {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
            data: vec![0u8; 16],
        };
        shared.push_blocks(vec![block.clone(), block]);
        assert_eq!(shared.take_blocks().len(), 2);
        assert!(shared.take_blocks().is_empty());
        assert!(!shared.take_want_full());
    }

    #[test]
    fn queue_overflow_requests_full_frame() {
        let shared = Shared::new();
        let block = BlockUpdate {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            data: vec![0u8; 4],
        };
        shared.push_blocks(vec![block; MAX_PENDING_BLOCKS]);
        assert!(!shared.take_want_full());
        shared.push_blocks(vec![BlockUpdate {
            x: 4,
            y: 0,
            w: 1,
            h: 1,
            data: vec![0u8; 4],
        }]);
        assert!(shared.take_want_full());
        assert_eq!(shared.take_blocks().len(), MAX_PENDING_BLOCKS);
    }

    #[test]
    fn cursor_version_bumps_only_on_change() {
        let shared = Shared::new();
        let (snapshot, v0) = shared.cursor_snapshot();
        assert_eq!(v0, 0);
        assert!(!snapshot.visible);

        shared.set_cursor_pos(100, 200, true);
        let (snapshot, v1) = shared.cursor_snapshot();
        assert_eq!((snapshot.x, snapshot.y, snapshot.visible), (100, 200, true));
        assert_eq!(v1, v0 + 1);

        // 同样的位置不该再涨版本号（避免每帧重绘光标）
        shared.set_cursor_pos(100, 200, true);
        assert_eq!(shared.cursor_snapshot().1, v1);

        shared.set_cursor_pos(101, 200, true);
        assert_eq!(shared.cursor_snapshot().1, v1 + 1);

        let shape = CursorShapeOwned {
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra: vec![1u8; 16],
        };
        shared.set_cursor_shape(Some(shape));
        let (snapshot, v2) = shared.cursor_snapshot();
        assert!(snapshot.shape.is_some());
        assert_eq!(v2, v1 + 2);

        shared.set_cursor_shape(None);
        assert!(shared.cursor_snapshot().0.shape.is_none());
    }

    #[test]
    fn rtt_roundtrip() {
        let shared = Shared::new();
        assert_eq!(shared.rtt_micros(), 0);
        // 没有 Ping 时收到 Pong：忽略
        assert!(shared.finish_ping(123).is_none());

        let ts = shared.begin_ping();
        assert!(shared.now_ms() >= ts, "Ping ts 不应来自未来");
        let rtt = shared.finish_ping(ts).expect("rtt");
        assert_eq!(shared.rtt_micros(), rtt);
        // 重放同一个 Pong 不再更新（这一条 Ping 已经消费掉）
        assert!(shared.finish_ping(ts).is_none());
        // 不匹配的 ts 也忽略
        let _ = shared.begin_ping();
        assert!(shared.finish_ping(ts.wrapping_add(1)).is_none());
    }

    #[test]
    fn video_frame_keeps_only_latest() {
        let shared = Shared::new();
        assert!(shared.take_video_frame().is_none());
        let mk = |seq: u32| VideoFrameUpdate {
            width: 4,
            height: 2,
            stride: 16,
            bgra: vec![seq as u8; 32],
            ts_ms: 1000 + seq,
            seq,
        };
        shared.put_video_frame(mk(1));
        shared.put_video_frame(mk(2));
        let frame = shared.take_video_frame().expect("frame");
        assert_eq!(frame.seq, 2, "旧帧应被新帧覆盖");
        assert!(shared.take_video_frame().is_none());
    }

    #[test]
    fn mode_and_block_full_reset() {
        let shared = Shared::new();
        assert!(!shared.is_video_mode());
        shared.set_mode_video();
        assert!(shared.is_video_mode());
        shared.set_mode_block();
        assert!(!shared.is_video_mode());
        assert!(!shared.take_block_full_reset());
        shared.mark_block_full_reset();
        assert!(shared.take_block_full_reset());
        assert!(!shared.take_block_full_reset());
    }

    #[test]
    fn latency_window_average() {
        let shared = Shared::new();
        assert!(shared.take_latency_avg_ms().is_none());
        shared.add_latency_ms(30);
        shared.add_latency_ms(50);
        let avg = shared.take_latency_avg_ms().expect("avg");
        assert!((avg - 40.0).abs() < 1e-9, "avg={avg}");
        // 取走后窗口清空
        assert!(shared.take_latency_avg_ms().is_none());
    }

    #[test]
    fn backoff_sequence_1_2_4_8_then_cap_10() {
        let secs: Vec<u64> = (1..=8).map(|a| backoff_delay(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 10, 10, 10, 10]);
        // 0 也按第 1 次处理，别退化成 0 秒空转
        assert_eq!(backoff_delay(0).as_secs(), 1);
    }

    #[test]
    fn status_and_send_slot() {
        let shared = Shared::new();
        assert!(!shared.send(vec![1, 2, 3]), "没连接时发送应被丢弃");
        assert_eq!(
            shared.status(),
            ConnStatus::Reconnecting { attempt: 0 }
        );

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        shared.set_out_tx(Some(tx));
        assert!(shared.send(vec![7]));
        assert_eq!(rx.recv().expect("payload"), vec![7]);

        shared.set_status(ConnStatus::Connected);
        assert_eq!(shared.status(), ConnStatus::Connected);
        shared.set_status(ConnStatus::Reconnecting { attempt: 3 });
        assert_eq!(shared.status(), ConnStatus::Reconnecting { attempt: 3 });
        shared.set_status(ConnStatus::Terminal {
            message: String::from("认证失败：密码错误"),
        });
        assert!(matches!(shared.status(), ConnStatus::Terminal { .. }));

        shared.set_out_tx(None);
        assert!(!shared.send(vec![9]));
        shared.bump_reconnects();
        shared.bump_reconnects();
        assert_eq!(shared.reconnect_count(), 2);
    }

    #[test]
    fn full_frame_grid_corrects_downscaled_canvas() {
        // 服务端降分辨率：ServerHello 仍说 1920x1080，但实际帧网格 25x14 = 1600x896
        let shared = Shared::new();
        shared.set_remote_size(1920, 1080);
        let grid = Grid {
            block_size: 64,
            blocks_x: 25,
            blocks_y: 14,
        };
        assert!(shared.reconcile_canvas_from_grid(grid));
        assert_eq!(shared.remote_size(), Some((1600, 896)));
        // 再来一次同样的帧：已经一致，不该再动
        assert!(!shared.reconcile_canvas_from_grid(grid));

        // 正常 1080p：网格 30x17 向上取整成 1920x1088，比宣布的大 → 不能改
        let shared2 = Shared::new();
        shared2.set_remote_size(1920, 1080);
        let normal = Grid {
            block_size: 64,
            blocks_x: 30,
            blocks_y: 17,
        };
        assert!(!shared2.reconcile_canvas_from_grid(normal));
        assert_eq!(shared2.remote_size(), Some((1920, 1080)));
    }

    #[test]
    fn audio_state_and_counters() {
        let shared = Shared::new();
        assert_eq!(shared.audio_state_name(), "off");
        shared.set_audio_state(audio_state::ON);
        assert_eq!(shared.audio_state_name(), "on");
        shared.set_audio_state(audio_state::ERR);
        assert_eq!(shared.audio_state_name(), "err");
        shared.set_audio_buffered_ms(42);
        assert_eq!(shared.audio_buffered_ms(), 42);
        shared.audio_pkts.fetch_add(3, Ordering::Relaxed);
        shared.audio_drop.fetch_add(1, Ordering::Relaxed);
        assert_eq!(shared.audio_pkts.load(Ordering::Relaxed), 3);
        assert_eq!(shared.audio_drop.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn link_path_and_udp_stats() {
        let shared = Shared::new();
        assert_eq!(shared.link_path(), "tcp");
        assert!(shared.udp_stats().is_none());

        shared.set_link_path("direct_udp");
        assert_eq!(shared.link_path(), "direct_udp");
        shared.set_udp_stats(UdpStats {
            rtt_ms: 12.5,
            loss_permille: 3,
            ..Default::default()
        });
        let stats = shared.udp_stats().expect("udp stats");
        assert!((stats.rtt_ms - 12.5).abs() < 1e-6);
        assert_eq!(stats.loss_permille, 3);

        // 回退 TCP：路径变 tcp，旧的 UDP 统计要清掉
        shared.set_link_path("tcp");
        assert_eq!(shared.link_path(), "tcp");
        assert!(shared.udp_stats().is_none());
    }

    #[test]
    fn audio_rms_window() {
        let shared = Shared::new();
        assert!(shared.take_audio_rms().is_none());
        shared.add_audio_rms(0.1);
        shared.add_audio_rms(0.3);
        let avg = shared.take_audio_rms().expect("rms");
        assert!((avg - 0.2).abs() < 1e-4, "avg={avg}");
        assert!(shared.take_audio_rms().is_none(), "取走后窗口应清空");
    }

    #[test]
    fn display_list_roundtrip() {
        let shared = Shared::new();
        assert!(shared.displays().is_empty());
        shared.set_displays(vec![
            DisplayInfo {
                index: 0,
                width: 1920,
                height: 1080,
                origin_x: 0,
                origin_y: 0,
                primary: true,
            },
            DisplayInfo {
                index: 1,
                width: 1280,
                height: 720,
                origin_x: 1920,
                origin_y: 0,
                primary: false,
            },
        ]);
        let list = shared.displays();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].index, 1);
        assert_eq!((list[1].width, list[1].height), (1280, 720));
        assert!(list[0].primary);
    }

    #[test]
    fn clear_session_state_drops_queued_frames() {
        let shared = Shared::new();
        shared.push_blocks(vec![BlockUpdate {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            data: vec![1u8; 4],
        }]);
        shared.put_video_frame(VideoFrameUpdate {
            width: 4,
            height: 2,
            stride: 16,
            bgra: vec![0u8; 32],
            ts_ms: 1,
            seq: 1,
        });
        shared.set_mode_video();
        shared.mark_block_full_reset();
        shared.request_full();

        shared.clear_session_state();

        assert!(shared.take_blocks().is_empty());
        assert!(shared.take_video_frame().is_none());
        assert!(!shared.is_video_mode());
        assert!(!shared.take_block_full_reset());
        assert!(!shared.take_want_full());
    }
}
