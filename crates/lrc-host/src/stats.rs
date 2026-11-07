//! 每秒统计（--stats）。格式由 Lead 的验收脚本 scripts/verify-e2e.ps1 固定。
//!
//! 一行样例：
//! t=12.0 fps=18.3 blocks=42 sent_frames=220 bytes=1234567 kbps=987.6 idle_frames=15 dropped=0 sec_bytes=12345

/// 一组计数。逐秒一份（sec）和整场一份（total）用同一个结构。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// 真正发出去的 VideoFrame 数
    pub frames: u64,
    /// 这些帧里包含的块数
    pub blocks: u64,
    /// 线上字节数（payload + 4 字节长度前缀，含光标消息）
    pub bytes: u64,
    /// 采集到但没有像素变化的次数（grab 返回 None，或候选块哈希全同）
    pub idle: u64,
    /// 因流控（未确认字节超限）丢掉的帧数
    pub dropped: u64,
    /// 发出去的光标消息数（CursorPos + CursorShape）
    pub cursor_msgs: u64,
    /// 响应 RequestFull 的整帧重发次数（含静止桌面用缓存帧补的）
    pub full_resends: u64,
    /// 采集耗时累加（毫秒），只出现在会话总结里
    pub capture_ms_sum: f64,
}

impl Stats {
    pub fn add(&mut self, other: &Stats) {
        self.frames += other.frames;
        self.blocks += other.blocks;
        self.bytes += other.bytes;
        self.idle += other.idle;
        self.dropped += other.dropped;
        self.cursor_msgs += other.cursor_msgs;
        self.full_resends += other.full_resends;
        self.capture_ms_sum += other.capture_ms_sum;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 一行 --stats。
///
/// - t：会话已运行秒数（1 位小数）
/// - fps：本区间发出帧数 / 区间秒数
/// - blocks：本区间发出的变化块数
/// - sent_frames / bytes / dropped：累计值
/// - kbps：本区间码率（千比特/秒 = bytes * 8 / 1000 / 秒）
/// - idle_frames：本区间"无变化"次数
/// - sec_bytes：本区间发出的字节数（静止时应当为 0，验收"零变化零发送"就看这个）
/// - cursor_msgs / full_resends：累计的光标消息数、响应整帧请求的重发次数
pub fn format_line(total: &Stats, sec: &Stats, session_secs: f64, interval_secs: f64) -> String {
    let interval = if interval_secs > 0.0 { interval_secs } else { 1.0 };
    let t = if session_secs > 0.0 { session_secs } else { 0.0 };
    let fps = sec.frames as f64 / interval;
    let kbps = (sec.bytes as f64 * 8.0 / 1000.0) / interval;
    line(t, fps, sec.blocks, total, kbps, sec.idle, sec.bytes)
}

/// 会话结束时的总结行（同一套字段，fps/kbps 用整场平均值）。
pub fn format_summary(total: &Stats, session_secs: f64) -> String {
    let t = if session_secs > 0.0 { session_secs } else { 1.0 };
    let fps = total.frames as f64 / t;
    let kbps = (total.bytes as f64 * 8.0 / 1000.0) / t;
    line(t, fps, total.blocks, total, kbps, total.idle, total.bytes)
}

fn line(
    t: f64,
    fps: f64,
    blocks: u64,
    total: &Stats,
    kbps: f64,
    idle_frames: u64,
    sec_bytes: u64,
) -> String {
    format!(
        "t={t:.1} fps={fps:.1} blocks={blocks} sent_frames={} bytes={} kbps={kbps:.1} idle_frames={idle_frames} dropped={} sec_bytes={sec_bytes} cursor_msgs={} full_resends={}",
        total.frames, total.bytes, total.dropped, total.cursor_msgs, total.full_resends
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec_sample() -> Stats {
        Stats {
            frames: 30,
            blocks: 120,
            bytes: 100_000,
            idle: 5,
            dropped: 0,
            cursor_msgs: 0,
            full_resends: 0,
            capture_ms_sum: 60.0,
        }
    }

    #[test]
    fn line_matches_lead_format() {
        let mut total = sec_sample();
        total.dropped = 2;
        let sec = sec_sample();
        assert_eq!(
            format_line(&total, &sec, 1.0, 1.0),
            "t=1.0 fps=30.0 blocks=120 sent_frames=30 bytes=100000 kbps=800.0 idle_frames=5 dropped=2 sec_bytes=100000 cursor_msgs=0 full_resends=0"
        );
    }

    #[test]
    fn idle_second_reports_zero_bytes() {
        let total = Stats {
            frames: 220,
            bytes: 1_234_567,
            ..Stats::default()
        };
        let sec = Stats {
            idle: 490,
            ..Stats::default()
        };
        let line = format_line(&total, &sec, 12.0, 1.0);
        assert_eq!(
            line,
            "t=12.0 fps=0.0 blocks=0 sent_frames=220 bytes=1234567 kbps=0.0 idle_frames=490 dropped=0 sec_bytes=0 cursor_msgs=0 full_resends=0"
        );
        // 验收脚本按 kbps= 抓数
        assert!(line.contains("kbps=0.0"));
        assert!(line.contains("sec_bytes=0"));
    }

    #[test]
    fn two_second_window_halves_rate() {
        let total = Stats {
            frames: 30,
            bytes: 1_000,
            ..Stats::default()
        };
        let line = format_line(&total, &total, 2.0, 2.0);
        assert_eq!(
            line,
            "t=2.0 fps=15.0 blocks=0 sent_frames=30 bytes=1000 kbps=4.0 idle_frames=0 dropped=0 sec_bytes=1000 cursor_msgs=0 full_resends=0"
        );
    }

    #[test]
    fn summary_uses_session_average() {
        let total = Stats {
            frames: 100,
            blocks: 500,
            bytes: 10_000,
            idle: 50,
            dropped: 1,
            cursor_msgs: 3,
            full_resends: 1,
            capture_ms_sum: 200.0,
        };
        let line = format_summary(&total, 10.0);
        assert_eq!(
            line,
            "t=10.0 fps=10.0 blocks=500 sent_frames=100 bytes=10000 kbps=8.0 idle_frames=50 dropped=1 sec_bytes=10000 cursor_msgs=3 full_resends=1"
        );
    }

    #[test]
    fn zero_elapsed_is_safe() {
        let s = Stats::default();
        let line = format_line(&s, &s, 0.0, 0.0);
        assert!(line.starts_with("t=0.0 fps=0.0 "), "{line}");
    }

    #[test]
    fn add_and_reset() {
        let mut a = Stats {
            frames: 2,
            bytes: 10,
            capture_ms_sum: 1.5,
            ..Stats::default()
        };
        let b = Stats {
            frames: 3,
            blocks: 7,
            bytes: 5,
            idle: 1,
            dropped: 4,
            cursor_msgs: 6,
            full_resends: 2,
            capture_ms_sum: 0.5,
        };
        a.add(&b);
        assert_eq!(a.frames, 5);
        assert_eq!(a.blocks, 7);
        assert_eq!(a.bytes, 15);
        assert_eq!(a.idle, 1);
        assert_eq!(a.dropped, 4);
        assert_eq!(a.cursor_msgs, 6);
        assert_eq!(a.full_resends, 2);
        assert!((a.capture_ms_sum - 2.0).abs() < 1e-9);
        a.reset();
        assert_eq!(a, Stats::default());
    }
}
