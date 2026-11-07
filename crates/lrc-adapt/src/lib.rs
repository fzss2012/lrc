//! 自适应控制器（M2）：根据链路实测反馈，决定 模式 / 帧率 / 块质量 / 视频码率。
//!
//! 这是一个**纯逻辑**库：输入每秒统计，输出下一阶段的参数目标。不做 IO、不起线程，
//! 方便用模拟链路做单元测试（"给定 2 Mbps 链路，10 秒内收敛且不抖"这种）。
//!
//! 由 wire-codec 实现。契约可扩展，但已有字段语义不要改。

/// 当前使用的传输模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 块增量（文字/办公最优）
    Blocks,
    /// 整帧视频（NVENC/HEVC 或 H.264）
    Video,
}

/// 一秒的链路反馈（由 host 填写）。
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkStats {
    /// 这一秒实际发出的线上字节
    pub bytes_sent: u64,
    /// 未确认字节（流控水位）
    pub inflight_bytes: u64,
    /// ack 往返（毫秒）
    pub rtt_ms: u32,
    /// 这一秒因为流控丢掉的帧
    pub dropped_frames: u32,
    /// 变化块比例 0.0..=1.0（内容活跃度）
    pub changed_ratio: f32,
    /// 这一秒的帧率
    pub fps: f32,
}

impl LinkStats {
    /// 这一秒的实际码率（bit/s）
    pub fn kbps(&self) -> f64 {
        self.bytes_sent as f64 * 8.0 / 1000.0
    }
}

/// 控制器输出：下一阶段的目标参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub mode: Mode,
    /// 目标帧率
    pub fps: u8,
    /// 块模式 JPEG 质量
    pub block_quality: u8,
    /// 视频模式目标码率（bit/s）
    pub video_bitrate_bps: u32,
    /// 分辨率缩放（1.0 = 原生，0.83 ≈ 1600x900，0.67 = 1280x720）
    pub resolution_scale: f32,
}

impl Target {
    pub fn default_blocks() -> Self {
        Self {
            mode: Mode::Blocks,
            fps: 30,
            block_quality: 85,
            video_bitrate_bps: 4_000_000,
            resolution_scale: 1.0,
        }
    }
}

/// 控制器配置。
#[derive(Clone, Copy, Debug)]
pub struct ControllerConfig {
    /// 链路预算（bit/s），例如 VPS 出口 5 Mbps → 4_000_000
    pub link_budget_bps: u32,
    /// 进视频模式的变化块比例阈值
    pub video_enter_ratio: f32,
    /// 回块模式的变化块比例阈值
    pub video_exit_ratio: f32,
    /// 是否允许视频模式
    pub video_enabled: bool,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            link_budget_bps: 4_000_000,
            video_enter_ratio: 0.45,
            video_exit_ratio: 0.10,
            video_enabled: true,
        }
    }
}

/// 自适应控制器。
///
/// 约定：`on_tick` 每秒被调用一次（dt_ms 通常 1000），返回下一阶段目标。
pub struct Controller {
    _priv: (),
}

impl Controller {
    pub fn new(_cfg: ControllerConfig) -> Self {
        todo!("wire-codec 实现")
    }

    pub fn config(&self) -> ControllerConfig {
        todo!("wire-codec 实现")
    }

    /// 喂一秒统计，拿到下一阶段目标。要求：有滞回，不许抖。
    pub fn on_tick(&mut self, _stats: &LinkStats, _dt_ms: u32) -> Target {
        todo!("wire-codec 实现：帧率 → 质量 → 分辨率 → 模式的降级顺序，带滞回")
    }
}
