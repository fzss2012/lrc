//! 接收线程 ↔ 窗口线程共享的状态。
//!
//! 窗口线程（主线程）是唯一碰 D3D/GDI 的地方；接收线程只解码并把"脏块"塞进队列，
//! 由 8ms 定时器在主线程 drain。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// 自绘错误框（握手失败/连接断开）的自定义窗口消息。
pub const WM_APP_FAILURE: u32 = WM_APP + 1;

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
    /// 需要弹框并退出的错误（握手失败、连接断开）
    failure: Mutex<Option<String>>,
    /// 窗口句柄（isize 形式，避免把非 Send 的 HWND 存进来）
    hwnd: AtomicIsize,
    /// 光标状态 + 版本号（版本变了主线程才重绘光标）
    cursor: Mutex<CursorSnapshot>,
    cursor_version: AtomicU64,
    /// 最近一次 Ping 的 (ts_ms, 发出时刻)，只保留一条（1 秒一个，RTT 远小于 1 秒）
    ping_pending: Mutex<Option<(u64, Instant)>>,
    /// 最近一次测得的 RTT（微秒），0 = 还没测到
    rtt_micros: AtomicU64,
    /// 单调时钟原点（Ping 的 ts_ms 用它算；两个线程共用）
    epoch: Instant,
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
            failure: Mutex::new(None),
            hwnd: AtomicIsize::new(0),
            cursor: Mutex::new(CursorSnapshot::default()),
            cursor_version: AtomicU64::new(0),
            ping_pending: Mutex::new(None),
            rtt_micros: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    /// 窗口创建后由主线程写入，接收线程用它 PostMessage。
    pub fn set_hwnd(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::SeqCst);
    }

    /// 从任意线程往窗口线程投递自定义消息。
    pub fn post(&self, msg: u32) {
        let raw = self.hwnd.load(Ordering::SeqCst);
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)) };
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

    /// 记录致命错误并唤醒窗口线程弹框。
    pub fn fail(&self, message: String) {
        {
            let mut slot = lock_or_recover(&self.failure);
            if slot.is_none() {
                *slot = Some(message);
            }
        }
        self.post(WM_APP_FAILURE);
    }

    /// 窗口线程取走待弹框的错误。
    pub fn take_failure(&self) -> Option<String> {
        lock_or_recover(&self.failure).take()
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
    fn failure_is_sticky_until_taken() {
        let shared = Shared::new();
        shared.fail(String::from("认证失败"));
        shared.fail(String::from("连接断开"));
        assert_eq!(shared.take_failure().as_deref(), Some("认证失败"));
        assert!(shared.take_failure().is_none());
    }
}
