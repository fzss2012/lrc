//! lrc 线格式：消息编码/解码、块网格几何、输入事件。
//!
//! 全部手写小端二进制，零第三方依赖，方便被控端与主控端共用同一份实现。
//! 设计见 `docs/DESIGN.md`。

pub mod grid;
pub mod input;
pub mod message;
pub mod wire;

pub use grid::{Grid, Rect};
pub use input::InputEvent;
pub use message::{
    AuthRequest, AuthResult, BlockEntry, ClientHello, CursorPos, CursorShape, FrameAck, Message,
    Ping, Pong, ServerHello, VideoAu, VideoFrame, MSG_AUTH_REQUEST, MSG_AUTH_RESULT,
    MSG_CLIENT_HELLO, MSG_CURSOR_POS, MSG_CURSOR_SHAPE, MSG_INPUT_EVENT, MSG_PING, MSG_PONG,
    MSG_REQUEST_FULL, MSG_SERVER_HELLO, MSG_VIDEO_ACK, MSG_VIDEO_AU, MSG_VIDEO_FRAME,
};
pub use wire::{ProtoError, Reader, Writer};

/// 协议版本。握手双方都必须一致。
pub const PROTO_VERSION: u16 = 1;

/// 单条消息 payload 的硬上限（防对端乱发导致内存爆炸）。
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// 块编码标签。
pub mod codec_tag {
    /// 无损：zstd + 左预测
    pub const LOSSLESS: u8 = 0;
    /// 有损：JPEG
    pub const JPEG: u8 = 1;
    /// 原样 BGRA
    pub const RAW: u8 = 2;
    /// 视频码流：H.264（整帧，独立 VideoAu 消息）
    pub const H264: u8 = 3;
    /// 视频码流：HEVC / H.265
    pub const HEVC: u8 = 4;
}

/// VideoFrame 的 flags 位。
pub mod frame_flags {
    /// 整帧（解码端可丢弃旧画面直接重建）
    pub const FULL: u8 = 1 << 0;
    /// 关键帧
    pub const KEY: u8 = 1 << 1;
}

/// AuthResult.reason
pub mod auth_reason {
    pub const OK: u8 = 0;
    pub const BAD_PASSWORD: u8 = 1;
    pub const VERSION_MISMATCH: u8 = 2;
    pub const RATE_LIMITED: u8 = 3;
}
