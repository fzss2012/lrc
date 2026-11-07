//! 每秒统计（--stats）。

/// 一段时间内的计数。字段含义与统计行一一对应。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// 真正发出去的 VideoFrame 数
    pub frames: u64,
    /// 这些帧里包含的块数（blocks_changed）
    pub blocks: u64,
    /// 线上的字节数（payload + 4 字节长度前缀，含光标消息）
    pub bytes: u64,
    /// 采集到但没有像素变化的次数（Ok(None) 或零变化块）
    pub idle: u64,
    /// 因流控（未确认字节超限）丢掉的帧数
    pub dropped: u64,
    /// 采集耗时累加（毫秒），除以 frames 得 capture_ms_avg
    pub capture_ms_sum: f64,
}

impl Stats {
    pub fn add(&mut self, other: &Stats) {
        self.frames += other.frames;
        self.blocks += other.blocks;
        self.bytes += other.bytes;
        self.idle += other.idle;
        self.dropped += other.dropped;
        self.capture_ms_sum += other.capture_ms_sum;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 一行统计：fps= capture_ms_avg= blocks_changed= bytes_sent= kbps= idle_frames= dropped=
    ///
    /// - fps：本区间内发出的帧数 / 秒数
    /// - capture_ms_avg：本区间内每次采集的平均耗时（毫秒）
    /// - kbps：千比特/秒（bytes * 8 / 1000 / 秒数）
    pub fn format_line(&self, elapsed_secs: f64) -> String {
        let secs = if elapsed_secs > 0.0 { elapsed_secs } else { 1.0 };
        let fps = self.frames as f64 / secs;
        let capture_ms_avg = if self.frames == 0 {
            0.0
        } else {
            self.capture_ms_sum / self.frames as f64
        };
        let kbps = (self.bytes as f64 * 8.0 / 1000.0) / secs;
        format!(
            "fps={fps:.1} capture_ms_avg={capture_ms_avg:.2} blocks_changed={} bytes_sent={} kbps={kbps:.1} idle_frames={} dropped={}",
            self.blocks, self.bytes, self.idle, self.dropped
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_line_is_all_zero() {
        let s = Stats::default();
        assert_eq!(
            s.format_line(1.0),
            "fps=0.0 capture_ms_avg=0.00 blocks_changed=0 bytes_sent=0 kbps=0.0 idle_frames=0 dropped=0"
        );
    }

    #[test]
    fn busy_line_numbers() {
        let mut s = Stats::default();
        s.frames = 30;
        s.blocks = 120;
        s.bytes = 100_000;
        s.idle = 5;
        s.dropped = 2;
        s.capture_ms_sum = 60.0;
        assert_eq!(
            s.format_line(1.0),
            "fps=30.0 capture_ms_avg=2.00 blocks_changed=120 bytes_sent=100000 kbps=800.0 idle_frames=5 dropped=2"
        );
    }

    #[test]
    fn two_second_window_halves_rates() {
        let mut s = Stats::default();
        s.frames = 30;
        s.bytes = 1_000;
        s.capture_ms_sum = 30.0;
        assert_eq!(
            s.format_line(2.0),
            "fps=15.0 capture_ms_avg=1.00 blocks_changed=0 bytes_sent=1000 kbps=4.0 idle_frames=0 dropped=0"
        );
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
            capture_ms_sum: 0.5,
        };
        a.add(&b);
        assert_eq!(a.frames, 5);
        assert_eq!(a.blocks, 7);
        assert_eq!(a.bytes, 15);
        assert_eq!(a.idle, 1);
        assert_eq!(a.dropped, 4);
        assert!((a.capture_ms_sum - 2.0).abs() < 1e-9);
        a.reset();
        assert_eq!(a, Stats::default());
    }

    #[test]
    fn zero_elapsed_uses_one_second() {
        let s = Stats {
            frames: 7,
            ..Stats::default()
        };
        assert!(s.format_line(0.0).starts_with("fps=7.0 "));
    }
}
