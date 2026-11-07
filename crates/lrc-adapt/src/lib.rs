//! 自适应控制器（M2）：根据链路实测反馈，决定 模式 / 帧率 / 块质量 / 视频码率。
//!
//! 这是一个**纯逻辑**库：输入每秒统计，输出下一阶段的参数目标。不做 IO、不起线程，
//! 方便用模拟链路做单元测试（"给定 2 Mbps 链路，10 秒内收敛且不抖"这种）。
//!
//! 由 wire-codec 实现。契约可扩展，但已有字段语义不要改。
//!
//! # 控制策略
//!
//! 降级阶梯**严格按 ROADMAP M2 的顺序**（每档都带滞回，默认 3 秒超标才降、10 秒富余才升）：
//!
//! | 档位 | 阶段 | fps | 块质量 | 分辨率 | 视频码率(×预算) |
//! |---:|---|---:|---:|---:|---:|
//! | 0 | 原生 | 30 | 85 | 1.00 | 0.70 |
//! | 1 | ① 降帧率 | 20 | 85 | 1.00 | 0.65 |
//! | 2 | ① 降帧率 | 15 | 85 | 1.00 | 0.60 |
//! | 3 | ① 降帧率 | 10 | 85 | 1.00 | 0.55 |
//! | 4 | ② 抬 QP | 10 | 70 | 1.00 | 0.50 |
//! | 5 | ② 抬 QP | 10 | 55 | 1.00 | 0.45 |
//! | 6 | ③ 降分辨率 | 10 | 55 | 0.83 (1600x900) | 0.40 |
//! | 7 | ③ 降分辨率 | 8 | 50 | 0.83 | 0.35 |
//! | 8 | ③ 降分辨率 | 8 | 45 | 0.67 (1280x720) | 0.30 |
//! | 9 | ④ 近停发 | 2 | 40 | 0.67 | 0.25 |
//!
//! "超预算"的判据按强度排列：**丢帧 > 实际码率超 110% > 未确认字节积压 > RTT 高**。
//! 丢帧是最强信号，见到就立刻降一档并进入冷却期（避免马上又升回去）。
//!
//! 模式决策（块 ⇄ 视频）同样带滞回：连续 3 秒 changed_ratio 超过 video_enter_ratio
//! 才进视频模式；连续 5 秒低于 video_exit_ratio 才回块模式。video_enabled=false 时
//! 永远停在块模式。

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
    /// 这一秒的实际码率（kbit/s；字段名沿用契约）
    pub fn kbps(&self) -> f64 {
        self.bytes_sent as f64 * 8.0 / 1000.0
    }

    /// 这一秒的实际码率（bit/s）
    pub fn bps(&self) -> f64 {
        self.bytes_sent as f64 * 8.0
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

// ---------------------------------------------------------------- 调参常量

/// 连续这么久超预算就降一档（默认 3 秒）。
const DEGRADE_MS: u32 = 3_000;
/// 连续这么久富余才升一档（默认 10 秒）。
const UPGRADE_MS: u32 = 10_000;
/// 丢帧降档后的冷却期：期间不升档，避免"降→升→又丢帧"抖动。
const HOLD_AFTER_DROP_MS: u32 = 5_000;
/// 任意档位变化后的冷却期。
const HOLD_AFTER_CHANGE_MS: u32 = 2_000;
/// 进视频模式要连续超阈值的时长。
const VIDEO_ENTER_MS: u32 = 3_000;
/// 回块模式要连续低于阈值的时长。
const VIDEO_EXIT_MS: u32 = 5_000;

/// 实际码率 > 预算的 110% 算超。
const OVER_RATIO: f64 = 1.10;
/// 实际码率 < 预算的 55% 才算"富余"（避免刚降档就因为没打满又升回去）。
const UNDER_RATIO: f64 = 0.55;
/// RTT 超过这个值视为拥塞。
const RTT_HIGH_MS: u32 = 250;
/// RTT 低于这个值才算链路健康。
const RTT_OK_MS: u32 = 100;
/// 未确认字节超过"预算 × N 秒"视为积压。
const INFLIGHT_HIGH_SECS: f64 = 1.5;
/// 未确认字节低于"预算 × N 秒"才算清空。
const INFLIGHT_LOW_SECS: f64 = 0.25;

/// 一档降级参数。
#[derive(Clone, Copy, Debug)]
struct Level {
    fps: u8,
    block_quality: u8,
    resolution_scale: f32,
    /// 视频模式目标码率 = link_budget_bps × 这个比例
    video_bitrate_frac: f32,
    /// 阶段标签（写日志用）
    stage: &'static str,
}

/// 降级阶梯：索引越大越省带宽（顺序照 ROADMAP M2 的降级顺序）。
const LADDER: [Level; 10] = [
    Level { fps: 30, block_quality: 85, resolution_scale: 1.00, video_bitrate_frac: 0.70, stage: "原生" },
    Level { fps: 20, block_quality: 85, resolution_scale: 1.00, video_bitrate_frac: 0.65, stage: "降帧率" },
    Level { fps: 15, block_quality: 85, resolution_scale: 1.00, video_bitrate_frac: 0.60, stage: "降帧率" },
    Level { fps: 10, block_quality: 85, resolution_scale: 1.00, video_bitrate_frac: 0.55, stage: "降帧率" },
    Level { fps: 10, block_quality: 70, resolution_scale: 1.00, video_bitrate_frac: 0.50, stage: "抬QP" },
    Level { fps: 10, block_quality: 55, resolution_scale: 1.00, video_bitrate_frac: 0.45, stage: "抬QP" },
    Level { fps: 10, block_quality: 55, resolution_scale: 0.83, video_bitrate_frac: 0.40, stage: "降分辨率" },
    Level { fps: 8, block_quality: 50, resolution_scale: 0.83, video_bitrate_frac: 0.35, stage: "降分辨率" },
    Level { fps: 8, block_quality: 45, resolution_scale: 0.67, video_bitrate_frac: 0.30, stage: "降分辨率" },
    Level { fps: 2, block_quality: 40, resolution_scale: 0.67, video_bitrate_frac: 0.25, stage: "近停发" },
];

/// 自适应控制器。
///
/// 约定：on_tick 每秒被调用一次（dt_ms 通常 1000），返回下一阶段目标。
#[derive(Debug)]
pub struct Controller {
    cfg: ControllerConfig,
    /// 当前档位（LADDER 下标）
    level: usize,
    mode: Mode,
    /// 连续超标/富余累计（毫秒）
    over_ms: u32,
    under_ms: u32,
    /// 冷却：这段时间内不升档
    hold_ms: u32,
    /// 视频模式滞回计数
    video_enter_ms: u32,
    video_exit_ms: u32,
}

impl Controller {
    pub fn new(cfg: ControllerConfig) -> Self {
        let mut cfg = cfg;
        // 预算给 0 会让所有除数归零，兜一个保守默认值
        if cfg.link_budget_bps == 0 {
            cfg.link_budget_bps = 4_000_000;
        }
        // 退出阈值必须比进入阈值低，否则模式会来回跳
        if cfg.video_exit_ratio >= cfg.video_enter_ratio {
            cfg.video_exit_ratio = (cfg.video_enter_ratio * 0.25).max(0.0);
        }
        Self {
            cfg,
            level: 0,
            mode: Mode::Blocks,
            over_ms: 0,
            under_ms: 0,
            hold_ms: 0,
            video_enter_ms: 0,
            video_exit_ms: 0,
        }
    }

    pub fn config(&self) -> ControllerConfig {
        self.cfg
    }

    /// 当前降级档位（0 = 最优，越大越省）。
    pub fn level(&self) -> u8 {
        self.level as u8
    }

    /// 当前档位的阶段标签（写日志用）。
    pub fn stage(&self) -> &'static str {
        LADDER[self.level].stage
    }

    /// 当前模式。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 不喂统计时也能拿到"当前目标"（host 启动/重连时用）。
    pub fn target(&self) -> Target {
        self.build_target()
    }

    /// 喂一秒统计，拿到下一阶段目标。要求：有滞回，不许抖。
    pub fn on_tick(&mut self, stats: &LinkStats, dt_ms: u32) -> Target {
        let dt = dt_ms.max(1);
        self.hold_ms = self.hold_ms.saturating_sub(dt);

        self.update_mode(stats, dt);

        let budget = f64::from(self.cfg.link_budget_bps);
        let actual = stats.bps();
        let budget_bytes_per_sec = budget / 8.0;

        // 1) 丢帧是最强信号：立刻降一档，并冷却一段时间不许升
        if stats.dropped_frames > 0 {
            self.degrade_by(1);
            self.over_ms = 0;
            self.under_ms = 0;
            self.hold_ms = HOLD_AFTER_DROP_MS;
            return self.build_target();
        }

        // 2) 其它拥塞信号
        let inflight = stats.inflight_bytes as f64;
        let over = actual > budget * OVER_RATIO
            || inflight > budget_bytes_per_sec * INFLIGHT_HIGH_SECS
            || stats.rtt_ms > RTT_HIGH_MS;
        let under = !over
            && actual < budget * UNDER_RATIO
            && inflight < budget_bytes_per_sec * INFLIGHT_LOW_SECS
            && stats.rtt_ms <= RTT_OK_MS;

        if over {
            self.under_ms = 0;
            self.over_ms = self.over_ms.saturating_add(dt);
            if self.over_ms >= DEGRADE_MS {
                self.over_ms = 0;
                if self.degrade_by(1) {
                    self.hold_ms = HOLD_AFTER_CHANGE_MS;
                }
            }
        } else if under {
            self.over_ms = 0;
            if self.hold_ms == 0 {
                self.under_ms = self.under_ms.saturating_add(dt);
                if self.under_ms >= UPGRADE_MS {
                    self.under_ms = 0;
                    // 升档后不设冷却：10 秒富余门槛本身就是滞回；
                    // 冷却只用来保证"刚降过的档不会马上又爬回去"。
                    self.upgrade_by(1);
                }
            } else {
                // 冷却期内即使富余也不升档
                self.under_ms = 0;
            }
        } else {
            // 中性区：连续计数清零，只让时间自然流逝
            self.over_ms = 0;
            self.under_ms = 0;
        }

        self.build_target()
    }

    // ------------------------------------------------------------ 内部

    fn build_target(&self) -> Target {
        let l = &LADDER[self.level];
        let budget = self.cfg.link_budget_bps;
        let bitrate = (f64::from(budget) * f64::from(l.video_bitrate_frac)) as u32;
        Target {
            mode: if self.cfg.video_enabled {
                self.mode
            } else {
                Mode::Blocks
            },
            fps: l.fps,
            block_quality: l.block_quality,
            video_bitrate_bps: bitrate.min(budget),
            resolution_scale: l.resolution_scale,
        }
    }

    /// 降 n 档，返回是否真的动了。
    fn degrade_by(&mut self, n: usize) -> bool {
        let next = (self.level + n).min(LADDER.len() - 1);
        if next == self.level {
            return false;
        }
        self.level = next;
        log::debug!(
            "adapt: 降档 -> {} ({}, {}fps, q{})",
            self.level,
            self.stage(),
            LADDER[self.level].fps,
            LADDER[self.level].block_quality
        );
        true
    }

    /// 升 n 档，返回是否真的动了。
    fn upgrade_by(&mut self, n: usize) -> bool {
        let next = self.level.saturating_sub(n);
        if next == self.level {
            return false;
        }
        self.level = next;
        log::debug!(
            "adapt: 升档 -> {} ({}, {}fps, q{})",
            self.level,
            self.stage(),
            LADDER[self.level].fps,
            LADDER[self.level].block_quality
        );
        true
    }

    /// 块模式 ⇄ 视频模式，带滞回。
    fn update_mode(&mut self, stats: &LinkStats, dt: u32) {
        if !self.cfg.video_enabled {
            self.mode = Mode::Blocks;
            self.video_enter_ms = 0;
            self.video_exit_ms = 0;
            return;
        }
        match self.mode {
            Mode::Blocks => {
                if stats.changed_ratio >= self.cfg.video_enter_ratio {
                    self.video_enter_ms = self.video_enter_ms.saturating_add(dt);
                    if self.video_enter_ms >= VIDEO_ENTER_MS {
                        self.mode = Mode::Video;
                        self.video_enter_ms = 0;
                        self.video_exit_ms = 0;
                        log::debug!("adapt: 切视频模式（changed_ratio={:.2}）", stats.changed_ratio);
                    }
                } else {
                    self.video_enter_ms = 0;
                }
            }
            Mode::Video => {
                if stats.changed_ratio <= self.cfg.video_exit_ratio {
                    self.video_exit_ms = self.video_exit_ms.saturating_add(dt);
                    if self.video_exit_ms >= VIDEO_EXIT_MS {
                        self.mode = Mode::Blocks;
                        self.video_enter_ms = 0;
                        self.video_exit_ms = 0;
                        log::debug!("adapt: 切回块模式（changed_ratio={:.2}）", stats.changed_ratio);
                    }
                } else {
                    self.video_exit_ms = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ 模拟链路

    /// 模拟链路：给定控制器目标，反推这一秒实际发出多少字节。
    ///
    /// 内容模型（够用即可）：
    /// - 块模式：字节 ∝ fps/30 × 质量系数 × 面积系数（分辨率缩放的平方）
    /// - 视频模式：字节 ≈ 目标码率 / 8
    /// - 可选容量上限：超过就丢 1 帧（模拟流控丢帧）
    struct SimLink {
        /// level 0 / 块模式下每秒发出的字节数（内容越"重"越大）
        base_bytes_per_sec: f64,
        capacity_bytes_per_sec: Option<f64>,
    }

    impl SimLink {
        fn new(base_bytes_per_sec: f64) -> Self {
            Self {
                base_bytes_per_sec,
                capacity_bytes_per_sec: None,
            }
        }

        #[allow(dead_code)]
        fn with_capacity(base_bytes_per_sec: f64, capacity_bytes_per_sec: f64) -> Self {
            Self {
                base_bytes_per_sec,
                capacity_bytes_per_sec: Some(capacity_bytes_per_sec),
            }
        }

        /// 质量 → 字节系数（真实 JPEG 大概就是这个量级）。
        fn quality_factor(q: u8) -> f64 {
            match q {
                85 => 1.0,
                70 => 0.70,
                55 => 0.50,
                50 => 0.45,
                45 => 0.40,
                40 => 0.35,
                other => f64::from(other) / 85.0,
            }
        }

        /// 这一秒实际发出去多少字节、丢了几帧。
        fn step(&self, t: &Target) -> (u64, u32) {
            let bytes = match t.mode {
                Mode::Blocks => {
                    self.base_bytes_per_sec * f64::from(t.fps) / 30.0
                        * Self::quality_factor(t.block_quality)
                        * f64::from(t.resolution_scale).powi(2)
                }
                Mode::Video => f64::from(t.video_bitrate_bps) / 8.0,
            };
            match self.capacity_bytes_per_sec {
                Some(cap) if bytes > cap => (cap as u64, 1),
                _ => (bytes as u64, 0),
            }
        }
    }

    /// 跑 n 秒，返回每秒的 (target, level, bytes_sent)。
    fn drive(
        ctl: &mut Controller,
        link: &SimLink,
        mut target: Target,
        seconds: usize,
        ratio: f32,
    ) -> Vec<(Target, u8, u64)> {
        let mut out = Vec::with_capacity(seconds);
        for _ in 0..seconds {
            let (bytes, dropped) = link.step(&target);
            let stats = LinkStats {
                bytes_sent: bytes,
                inflight_bytes: 0,
                rtt_ms: 20,
                dropped_frames: dropped,
                changed_ratio: ratio,
                fps: f32::from(target.fps),
            };
            target = ctl.on_tick(&stats, 1000);
            out.push((target, ctl.level(), bytes));
        }
        out
    }

    fn cfg_2mbps() -> ControllerConfig {
        ControllerConfig {
            link_budget_bps: 2_000_000,
            ..ControllerConfig::default()
        }
    }

    fn bps_of(bytes: u64) -> f64 {
        bytes as f64 * 8.0
    }

    // ------------------------------------------------------------ 1. 收敛 + 不震荡

    #[test]
    fn converges_to_budget_without_flapping() {
        let mut ctl = Controller::new(cfg_2mbps());
        // level 0 时每秒 375 KB = 3 Mbps，比 2 Mbps 预算多 50%
        let link = SimLink::new(375_000.0);
        let trace = drive(&mut ctl, &link, Target::default_blocks(), 15, 0.02);

        println!("[2Mbps 链路] 每秒决策（level / fps / q / scale / 实际 Mbps）:");
        for (i, (t, lvl, bytes)) in trace.iter().enumerate() {
            println!(
                "  t={:>2}s  level={}  fps={:>2}  q={}  scale={:.2}  实际={:.3} Mbps",
                i + 1,
                lvl,
                t.fps,
                t.block_quality,
                t.resolution_scale,
                bps_of(*bytes) / 1.0e6
            );
        }

        // 1) 10 秒内收敛到预算以内
        let first_ok = trace
            .iter()
            .position(|(_, _, bytes)| bps_of(*bytes) <= 2_000_000.0)
            .expect("应该能在预算内运行");
        assert!(first_ok < 10, "10 秒内没收敛：第 {} 秒才达标", first_ok + 1);

        // 2) 档位单调下降（level 只增不减），且之后不再变化
        let levels: Vec<u8> = trace.iter().map(|(_, lvl, _)| *lvl).collect();
        assert!(
            levels.windows(2).all(|w| w[0] <= w[1]),
            "档位出现了回升（震荡）：{levels:?}"
        );
        let final_level = *levels.last().unwrap_or(&0);
        let stable_from = levels.iter().position(|l| *l == final_level).unwrap_or(0);
        assert!(
            levels[stable_from..].iter().all(|l| *l == final_level),
            "收敛后档位又变了：{levels:?}"
        );

        // 3) 收敛后不再超预算（注意每秒记录的 bytes 是"用上一秒目标"发出去的，
        //    所以跳过收敛点那一秒，从下一秒开始检查）
        let check_from = (stable_from + 1).min(trace.len());
        for (_, _, bytes) in &trace[check_from..] {
            assert!(
                bps_of(*bytes) <= 2_000_000.0,
                "收敛后仍超预算：{:.3} Mbps",
                bps_of(*bytes) / 1.0e6
            );
        }
        assert_eq!(
            final_level, 1,
            "3 Mbps 内容在 2 Mbps 预算下应该只降到第 1 档（20fps）"
        );
    }

    // ------------------------------------------------------------ 2. 恢复

    #[test]
    fn recovers_after_link_improves() {
        let mut ctl = Controller::new(cfg_2mbps());
        // 先给一个重内容（4.8 Mbps），让它降下去
        let heavy = SimLink::new(600_000.0);
        let down = drive(&mut ctl, &heavy, Target::default_blocks(), 20, 0.02);
        let degraded = *down.last().map(|(_, l, _)| l).unwrap_or(&0);
        println!(
            "[恢复] 重内容 20 秒后档位 = {degraded}（{}）",
            ctl.stage()
        );
        assert!(
            degraded >= 3,
            "4.8 Mbps 内容在 2 Mbps 预算下应该至少降到第 3 档，实际 {degraded}"
        );

        // 内容变轻后应该逐步升回去
        let light = SimLink::new(40_000.0);
        let mut target = ctl.target();
        let mut levels = Vec::new();
        for _ in 0..20 {
            let (bytes, dropped) = light.step(&target);
            let stats = LinkStats {
                bytes_sent: bytes,
                inflight_bytes: 0,
                rtt_ms: 20,
                dropped_frames: dropped,
                changed_ratio: 0.02,
                fps: f32::from(target.fps),
            };
            target = ctl.on_tick(&stats, 1000);
            levels.push(ctl.level());
        }
        println!("[恢复] 内容变轻后 20 秒档位序列: {levels:?}");
        let after = *levels.last().unwrap_or(&degraded);
        assert!(
            after + 2 <= degraded,
            "20 秒内应该至少升 2 档（从 {degraded} 到 ≤ {}），实际 {after}",
            degraded.saturating_sub(2)
        );
        assert!(after <= 1, "20 秒后应该回到高档（≤1 档），实际 {after}");
    }

    // ------------------------------------------------------------ 3. 丢帧

    #[test]
    fn dropped_frames_degrade_immediately() {
        let mut ctl = Controller::new(cfg_2mbps());
        // 链路本身没超，但发生丢帧 → 必须立刻降一档
        let stats = LinkStats {
            bytes_sent: 100_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 3,
            changed_ratio: 0.02,
            fps: 30.0,
        };
        let t = ctl.on_tick(&stats, 1000);
        assert_eq!(ctl.level(), 1, "丢帧当秒就该降档");
        assert_eq!(t.fps, 20, "降档后 target 应该立刻变成 20fps");

        // 连续丢帧继续降
        ctl.on_tick(&stats, 1000);
        assert_eq!(ctl.level(), 2);

        // 丢帧后的冷却期内即使很闲也不许升档
        let idle = LinkStats {
            bytes_sent: 10_000,
            inflight_bytes: 0,
            rtt_ms: 10,
            dropped_frames: 0,
            changed_ratio: 0.0,
            fps: 15.0,
        };
        for _ in 0..4 {
            ctl.on_tick(&idle, 1000);
            assert_eq!(ctl.level(), 2, "丢帧后的冷却期内不该升档");
        }
        // 冷却过后再连续 10 秒富余才升
        for _ in 0..9 {
            ctl.on_tick(&idle, 1000);
            assert_eq!(ctl.level(), 2, "富余不满 10 秒不该升档");
        }
        ctl.on_tick(&idle, 1000);
        assert_eq!(ctl.level(), 1, "连续 10 秒富余后升一档");
    }

    // ------------------------------------------------------------ 4. 视频开关

    #[test]
    fn video_disabled_never_switches() {
        let cfg = ControllerConfig {
            video_enabled: false,
            ..ControllerConfig::default()
        };
        let mut ctl = Controller::new(cfg);
        let stats = LinkStats {
            bytes_sent: 100_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.95,
            fps: 30.0,
        };
        for _ in 0..30 {
            let t = ctl.on_tick(&stats, 1000);
            assert_eq!(t.mode, Mode::Blocks, "video_enabled=false 时永远不能切视频模式");
            assert_eq!(ctl.mode(), Mode::Blocks);
        }
    }

    // ------------------------------------------------------------ 5. 模式滞回

    #[test]
    fn video_mode_switch_has_hysteresis() {
        let mut ctl = Controller::new(ControllerConfig::default());
        let busy = LinkStats {
            bytes_sent: 100_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.9,
            fps: 30.0,
        };
        ctl.on_tick(&busy, 1000);
        ctl.on_tick(&busy, 1000);
        assert_eq!(ctl.mode(), Mode::Blocks, "高变化率不足 3 秒不能切视频");
        ctl.on_tick(&busy, 1000);
        assert_eq!(ctl.mode(), Mode::Video, "连续 3 秒高变化率应该切视频");

        let quiet = LinkStats {
            changed_ratio: 0.02,
            ..busy
        };
        for _ in 0..4 {
            ctl.on_tick(&quiet, 1000);
            assert_eq!(ctl.mode(), Mode::Video, "安静不足 5 秒不该回块模式");
        }
        ctl.on_tick(&quiet, 1000);
        assert_eq!(ctl.mode(), Mode::Blocks, "连续 5 秒安静后回块模式");
    }

    // ------------------------------------------------------------ 6. 抖动保护

    #[test]
    fn short_spike_does_not_degrade() {
        let mut ctl = Controller::new(cfg_2mbps());
        let normal = LinkStats {
            bytes_sent: 100_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.02,
            fps: 30.0,
        };
        let spike = LinkStats {
            bytes_sent: 500_000, // 4 Mbps，超预算
            ..normal
        };
        ctl.on_tick(&spike, 1000);
        ctl.on_tick(&spike, 1000);
        assert_eq!(ctl.level(), 0, "超预算不足 3 秒不该降档");
        ctl.on_tick(&normal, 1000);
        assert_eq!(ctl.level(), 0);
        ctl.on_tick(&spike, 1000);
        ctl.on_tick(&spike, 1000);
        ctl.on_tick(&spike, 1000);
        assert_eq!(ctl.level(), 1, "连续 3 秒超预算应该降一档");
    }

    #[test]
    fn short_lull_does_not_upgrade() {
        let mut ctl = Controller::new(cfg_2mbps());
        let idle = LinkStats {
            bytes_sent: 20_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.0,
            fps: 20.0,
        };
        let bad = LinkStats {
            bytes_sent: 400_000,
            ..idle
        };
        for _ in 0..7 {
            ctl.on_tick(&bad, 1000);
        }
        let degraded = ctl.level();
        assert!(degraded >= 1, "先要真的降过档");

        for _ in 0..9 {
            ctl.on_tick(&idle, 1000);
        }
        assert_eq!(ctl.level(), degraded, "富余不足 10 秒不该升档");
        for _ in 0..4 {
            ctl.on_tick(&idle, 1000);
        }
        assert_eq!(ctl.level(), degraded - 1, "连续富余满 10 秒升一档");
    }

    // ------------------------------------------------------------ 7. 降级顺序

    #[test]
    fn degradation_follows_fps_then_quality_then_resolution() {
        let mut ctl = Controller::new(ControllerConfig::default());
        let over = LinkStats {
            bytes_sent: 2_000_000, // 16 Mbps，远超默认 4 Mbps 预算
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.02,
            fps: 30.0,
        };
        let mut targets = Vec::new();
        for _ in 0..40 {
            targets.push(ctl.on_tick(&over, 1000));
        }
        let fps: Vec<u8> = targets.iter().map(|t| t.fps).collect();
        let q: Vec<u8> = targets.iter().map(|t| t.block_quality).collect();
        let scale: Vec<f32> = targets.iter().map(|t| t.resolution_scale).collect();

        println!("[降级顺序] fps={fps:?}");
        println!("[降级顺序] quality={q:?}");
        println!("[降级顺序] scale={scale:?}");

        // ① 降帧率先行：质量到 70 时帧率必须已经降到 10
        let first_fps_drop = fps
            .windows(2)
            .position(|w| w[1] < w[0])
            .expect("帧率应该会降");
        let first_q_drop = q.iter().position(|v| *v < 85).expect("质量应该会降");
        assert!(
            first_fps_drop < first_q_drop,
            "应该先降帧率（第 {first_fps_drop} 秒）再抬 QP（第 {first_q_drop} 秒）"
        );
        assert_eq!(
            fps[first_q_drop - 1],
            10,
            "抬 QP 之前帧率应已降到 10，实际序列 {fps:?}"
        );
        assert_eq!(q[0], 85, "一开始质量是 85");

        // ② 抬 QP 再降分辨率：降分辨率时质量必须已经到 55
        let first_scale_drop = scale.iter().position(|v| *v < 1.0).expect("分辨率应该会降");
        assert!(
            q[..first_scale_drop].iter().all(|v| *v >= 55),
            "降分辨率之前应该先把质量降到 55（现在质量序列 {:?}）",
            q
        );
        assert_eq!(q[first_scale_drop - 1], 55, "降分辨率时质量应已到 55");

        assert!(fps.windows(2).all(|w| w[0] >= w[1]), "帧率不该回升");
        assert!(q.windows(2).all(|w| w[0] >= w[1]), "质量不该回升");
        assert!(scale.windows(2).all(|w| w[0] >= w[1]), "分辨率不该回升");
        assert_eq!(ctl.level(), 9, "持续超标应该降到底档");
        assert_eq!(targets[39].fps, 2, "底档是近停发（2fps）");
    }

    #[test]
    fn video_bitrate_stays_within_budget() {
        let budget = 3_000_000u32;
        let mut ctl = Controller::new(ControllerConfig {
            link_budget_bps: budget,
            ..ControllerConfig::default()
        });
        let over = LinkStats {
            bytes_sent: 2_000_000,
            inflight_bytes: 0,
            rtt_ms: 20,
            dropped_frames: 0,
            changed_ratio: 0.02,
            fps: 30.0,
        };
        let mut last = u32::MAX;
        for _ in 0..40 {
            let t = ctl.on_tick(&over, 1000);
            assert!(t.video_bitrate_bps <= budget, "视频码率不能超过预算");
            assert!(t.video_bitrate_bps <= last, "档位越深视频码率应越低");
            last = t.video_bitrate_bps;
        }
    }

    // ------------------------------------------------------------ 8. 契约细节

    #[test]
    fn default_target_is_sane() {
        let t = Target::default_blocks();
        assert_eq!((t.mode, t.fps, t.block_quality), (Mode::Blocks, 30, 85));
        assert!((t.resolution_scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_budget_and_bad_config_are_clamped() {
        let cfg = ControllerConfig {
            link_budget_bps: 0,
            video_exit_ratio: 0.9,
            video_enter_ratio: 0.5,
            video_enabled: true,
        };
        let ctl = Controller::new(cfg);
        let c = ctl.config();
        assert!(c.link_budget_bps > 0, "预算 0 应兜底成默认值");
        assert!(
            c.video_exit_ratio < c.video_enter_ratio,
            "退出阈值必须低于进入阈值"
        );
        assert!(ctl.target().video_bitrate_bps <= c.link_budget_bps);
    }

    #[test]
    fn level_is_clamped_at_bottom() {
        let mut ctl = Controller::new(cfg_2mbps());
        let drop = LinkStats {
            bytes_sent: 1_000,
            inflight_bytes: 0,
            rtt_ms: 10,
            dropped_frames: 1,
            changed_ratio: 0.0,
            fps: 30.0,
        };
        for _ in 0..40 {
            ctl.on_tick(&drop, 1000);
        }
        assert_eq!(ctl.level(), 9, "连续丢帧应该降到底档且不越界");
        assert_eq!(ctl.target().fps, 2);
    }
}
