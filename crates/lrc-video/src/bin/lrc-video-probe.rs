//! NVENC 视频通道可行性验证（M2 spike）。
//!
//! 目标：证明在本机可以用硬件编码 1080p BGRA → 低延迟 H.264/HEVC，并给出
//! 码率、编码/解码耗时、端到端延迟、以及解码后画面与原始帧的 PSNR。
//!
//! 由 windows-gfx 实现。用法（建议）：
//!   lrc-video-probe --frames 300 --bitrate 4000000 --codec hevc_nvenc
//!                   [--scenario static|typing|scroll|video] [--ffmpeg <path>]

fn main() {
    println!("lrc-video-probe: 待实现");
}
