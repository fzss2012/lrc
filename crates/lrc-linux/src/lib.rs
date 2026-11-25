//! Linux（X11）版的 lrc 被控端/控制端。
//!
//! Windows 端的 `lrc-host`/`lrc-client` 依赖 DXGI/WGC/D3D11/Win32，无法直接在 Linux 上跑；
//! 这个 crate 只重写**平台相关**的三小块（采集、输入注入、窗口呈现），
//! 协议、块编码、传输（TCP/UDP/relay）、视频通道、自适应全部复用已有 crate。
//!
//! 运行环境假设：**X11 会话**（Wayland 不在本期范围）。
//!
//! 分工（按文件，避免并行冲突）：
//! - `capture.rs` / `input.rs` / `bin/x11-probe.rs`  → windows-gfx
//! - `bin/lrc-host-linux.rs`                        → host-app
//! - `bin/lrc-client-linux.rs`                      → client-app

pub mod capture;
pub mod cursor;
pub mod display;
pub mod input;

pub use capture::{Frame, X11Capturer};
pub use cursor::{CursorPoll, CursorShape, CursorState, CursorWatcher, CursorWatcherStats};
pub use display::{monitors, pick_monitor, DisplayInfo, MonitorInfo, Region};
pub use input::{send_key, send_mouse_button, send_mouse_move, send_wheel, set_pointer_origin};
