//! 顶层消息：编解码与类型标签。

use crate::grid::Grid;
use crate::input::InputEvent;
use crate::wire::{ProtoError, ProtoResult, Reader, Writer};

pub const MSG_CLIENT_HELLO: u8 = 0x01;
pub const MSG_SERVER_HELLO: u8 = 0x02;
pub const MSG_AUTH_REQUEST: u8 = 0x03;
pub const MSG_AUTH_RESULT: u8 = 0x04;
pub const MSG_VIDEO_FRAME: u8 = 0x10;
pub const MSG_CURSOR_SHAPE: u8 = 0x11;
pub const MSG_CURSOR_POS: u8 = 0x12;
pub const MSG_VIDEO_ACK: u8 = 0x13;
/// M2 视频通道：一个完整访问单元（一帧可解码码流）
pub const MSG_VIDEO_AU: u8 = 0x14;
/// M3 音频通道：一帧 Opus/PCM
pub const MSG_AUDIO_FRAME: u8 = 0x15;
pub const MSG_INPUT_EVENT: u8 = 0x20;
pub const MSG_REQUEST_FULL: u8 = 0x30;
pub const MSG_PING: u8 = 0x40;
pub const MSG_PONG: u8 = 0x41;

/// 单个块的编码数据上限（256×256 BGRA 也才 256 KiB）。
pub const MAX_BLOCK_BLOB: usize = 1024 * 1024;
/// 光标位图上限 256×256 RGBA。
pub const MAX_CURSOR_BYTES: usize = 256 * 256 * 4;
/// 单个视频访问单元上限（4K 关键帧留余量）。
pub const MAX_VIDEO_AU: usize = 8 * 1024 * 1024;
/// 单个音频帧上限（Opus 20ms 通常 <200B；PCM 兜底 48k*2ch*2B*20ms≈3.8KB）。
pub const MAX_AUDIO_FRAME: usize = 64 * 1024;
/// 单帧最多块数。
pub const MAX_BLOCKS_PER_FRAME: usize = 65_535;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub version: u16,
    pub client_id: [u8; 16],
    pub nonce_c: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub version: u16,
    pub width: u32,
    pub height: u32,
    pub rotation: u16,
    pub nonce_s: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthRequest {
    /// HMAC-SHA256(key = password, msg = nonce_c || nonce_s)
    pub mac: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthResult {
    pub ok: bool,
    pub reason: u8,
}

/// 帧内单个变化块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockEntry<'a> {
    /// `idx = y * blocks_x + x`
    pub idx: u32,
    /// 见 `crate::codec_tag`
    pub codec: u8,
    pub data: &'a [u8],
}

/// 一帧增量。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame<'a> {
    pub seq: u32,
    /// 见 `crate::frame_flags`
    pub flags: u8,
    /// 客户端最近一次 ack 的 seq（用于服务端丢弃过期块）
    pub base_seq: u32,
    pub block_size: u16,
    pub blocks_x: u16,
    pub blocks_y: u16,
    pub entries: Vec<BlockEntry<'a>>,
}

impl<'a> VideoFrame<'a> {
    pub fn grid(&self) -> Option<Grid> {
        if self.blocks_x == 0 || self.blocks_y == 0 || self.block_size == 0 {
            return None;
        }
        Some(Grid {
            block_size: self.block_size,
            blocks_x: self.blocks_x,
            blocks_y: self.blocks_y,
        })
    }

    pub fn is_full(&self) -> bool {
        self.flags & crate::frame_flags::FULL != 0
    }
}

/// M2 视频通道的一帧码流（H.264/HEVC 访问单元）。
///
/// 与 `VideoFrame`（块增量）互斥：同一时刻一路连接只有一种在跑。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoAu<'a> {
    /// host 采集完成时刻（毫秒，跨端只用差值）
    pub ts_ms: u32,
    /// 与 VideoFrame 同一 seq 语义，客户端 ack 用
    pub seq: u32,
    /// `crate::codec_tag::H264` 或 `HEVC`
    pub codec: u8,
    /// bit0 = keyframe（IDR）
    pub flags: u8,
    pub data: &'a [u8],
}

impl<'a> VideoAu<'a> {
    pub fn is_key(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// M3 音频通道：一帧编码后的音频（默认 Opus 20ms）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    /// 采集时刻（同机用 GetTickCount64，跨机只作参考）
    pub ts_ms: u32,
    pub seq: u32,
    /// bit0 = Opus；bit1 = 裸 PCM s16le（调试用）
    pub flags: u8,
    pub sample_rate: u16,
    pub channels: u8,
    pub data: &'a [u8],
}

impl<'a> AudioFrame<'a> {
    pub fn is_opus(&self) -> bool {
        self.flags & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPos {
    pub x: i16,
    pub y: i16,
    pub visible: bool,
}

impl CursorPos {
    pub fn hidden() -> Self {
        Self {
            x: 0,
            y: 0,
            visible: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorShape<'a> {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: i16,
    pub hotspot_y: i16,
    /// 长度 = 4 * width * height（BGRA），空表示"无光标位图"。
    pub rgba: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameAck {
    pub seq: u32,
    pub recv_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
    pub ts_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pong {
    pub ts_ms: u64,
}

/// 所有可能的上层消息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    AuthRequest(AuthRequest),
    AuthResult(AuthResult),
    VideoFrame(VideoFrame<'a>),
    VideoAu(VideoAu<'a>),
    AudioFrame(AudioFrame<'a>),
    CursorShape(CursorShape<'a>),
    CursorPos(CursorPos),
    VideoAck(FrameAck),
    Input(InputEvent),
    RequestFull,
    Ping(Ping),
    Pong(Pong),
}

impl<'a> Message<'a> {
    pub fn msg_type(&self) -> u8 {
        match self {
            Message::ClientHello(_) => MSG_CLIENT_HELLO,
            Message::ServerHello(_) => MSG_SERVER_HELLO,
            Message::AuthRequest(_) => MSG_AUTH_REQUEST,
            Message::AuthResult(_) => MSG_AUTH_RESULT,
            Message::VideoFrame(_) => MSG_VIDEO_FRAME,
            Message::VideoAu(_) => MSG_VIDEO_AU,
            Message::AudioFrame(_) => MSG_AUDIO_FRAME,
            Message::CursorShape(_) => MSG_CURSOR_SHAPE,
            Message::CursorPos(_) => MSG_CURSOR_POS,
            Message::VideoAck(_) => MSG_VIDEO_ACK,
            Message::Input(_) => MSG_INPUT_EVENT,
            Message::RequestFull => MSG_REQUEST_FULL,
            Message::Ping(_) => MSG_PING,
            Message::Pong(_) => MSG_PONG,
        }
    }

    /// 编码成 payload（首字节为 type）。
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(256);
        w.u8(self.msg_type());
        match self {
            Message::ClientHello(hello) => {
                w.u16(hello.version);
                w.fixed(&hello.client_id);
                w.fixed(&hello.nonce_c);
            }
            Message::ServerHello(hello) => {
                w.u16(hello.version);
                w.u32(hello.width);
                w.u32(hello.height);
                w.u16(hello.rotation);
                w.fixed(&hello.nonce_s);
            }
            Message::AuthRequest(auth) => {
                w.fixed(&auth.mac);
            }
            Message::AuthResult(res) => {
                w.bool(res.ok);
                w.u8(res.reason);
            }
            Message::VideoFrame(frame) => {
                let count = frame.entries.len().min(MAX_BLOCKS_PER_FRAME);
                w.u32(frame.seq);
                w.u8(frame.flags);
                w.u32(frame.base_seq);
                w.u16(frame.block_size);
                w.u16(frame.blocks_x);
                w.u16(frame.blocks_y);
                w.u16(count as u16);
                for entry in frame.entries.iter().take(count) {
                    w.u32(entry.idx);
                    w.u8(entry.codec);
                    w.blob(entry.data);
                }
            }
            Message::VideoAu(au) => {
                w.u32(au.ts_ms);
                w.u32(au.seq);
                w.u8(au.codec);
                w.u8(au.flags);
                w.blob(au.data);
            }
            Message::AudioFrame(af) => {
                w.u32(af.ts_ms);
                w.u32(af.seq);
                w.u8(af.flags);
                w.u16(af.sample_rate);
                w.u8(af.channels);
                w.blob(af.data);
            }
            Message::CursorShape(shape) => {
                w.u16(shape.width);
                w.u16(shape.height);
                w.i16(shape.hotspot_x);
                w.i16(shape.hotspot_y);
                w.blob(shape.rgba);
            }
            Message::CursorPos(pos) => {
                w.i16(pos.x);
                w.i16(pos.y);
                w.bool(pos.visible);
            }
            Message::VideoAck(ack) => {
                w.u32(ack.seq);
                w.u64(ack.recv_bytes);
            }
            Message::Input(ev) => ev.encode(&mut w),
            Message::RequestFull => {}
            Message::Ping(p) => w.u64(p.ts_ms),
            Message::Pong(p) => w.u64(p.ts_ms),
        }
        w.into_vec()
    }

    /// 解码 payload（首字节为 type）。
    pub fn decode(payload: &'a [u8]) -> ProtoResult<Message<'a>> {
        let mut r = Reader::new(payload);
        let ty = r.u8()?;
        let msg = match ty {
            MSG_CLIENT_HELLO => Message::ClientHello(ClientHello {
                version: r.u16()?,
                client_id: r.fixed()?,
                nonce_c: r.fixed()?,
            }),
            MSG_SERVER_HELLO => Message::ServerHello(ServerHello {
                version: r.u16()?,
                width: r.u32()?,
                height: r.u32()?,
                rotation: r.u16()?,
                nonce_s: r.fixed()?,
            }),
            MSG_AUTH_REQUEST => Message::AuthRequest(AuthRequest { mac: r.fixed()? }),
            MSG_AUTH_RESULT => Message::AuthResult(AuthResult {
                ok: r.bool()?,
                reason: r.u8()?,
            }),
            MSG_VIDEO_FRAME => {
                let seq = r.u32()?;
                let flags = r.u8()?;
                let base_seq = r.u32()?;
                let block_size = r.u16()?;
                let blocks_x = r.u16()?;
                let blocks_y = r.u16()?;
                let count = usize::from(r.u16()?);
                if count > MAX_BLOCKS_PER_FRAME {
                    return Err(ProtoError::BadValue);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let idx = r.u32()?;
                    let codec = r.u8()?;
                    let data = r.blob(MAX_BLOCK_BLOB)?;
                    entries.push(BlockEntry { idx, codec, data });
                }
                Message::VideoFrame(VideoFrame {
                    seq,
                    flags,
                    base_seq,
                    block_size,
                    blocks_x,
                    blocks_y,
                    entries,
                })
            }
            MSG_VIDEO_AU => Message::VideoAu(VideoAu {
                ts_ms: r.u32()?,
                seq: r.u32()?,
                codec: r.u8()?,
                flags: r.u8()?,
                data: r.blob(MAX_VIDEO_AU)?,
            }),
            MSG_AUDIO_FRAME => Message::AudioFrame(AudioFrame {
                ts_ms: r.u32()?,
                seq: r.u32()?,
                flags: r.u8()?,
                sample_rate: r.u16()?,
                channels: r.u8()?,
                data: r.blob(MAX_AUDIO_FRAME)?,
            }),
            MSG_CURSOR_SHAPE => {
                let width = r.u16()?;
                let height = r.u16()?;
                let hotspot_x = r.i16()?;
                let hotspot_y = r.i16()?;
                let rgba = r.blob(MAX_CURSOR_BYTES)?;
                let expect = 4usize
                    .checked_mul(usize::from(width))
                    .and_then(|v| v.checked_mul(usize::from(height)))
                    .ok_or(ProtoError::BadValue)?;
                if expect != rgba.len() {
                    return Err(ProtoError::BadValue);
                }
                Message::CursorShape(CursorShape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    rgba,
                })
            }
            MSG_CURSOR_POS => Message::CursorPos(CursorPos {
                x: r.i16()?,
                y: r.i16()?,
                visible: r.bool()?,
            }),
            MSG_VIDEO_ACK => Message::VideoAck(FrameAck {
                seq: r.u32()?,
                recv_bytes: r.u64()?,
            }),
            MSG_INPUT_EVENT => Message::Input(InputEvent::decode(&mut r)?),
            MSG_REQUEST_FULL => Message::RequestFull,
            MSG_PING => Message::Ping(Ping { ts_ms: r.u64()? }),
            MSG_PONG => Message::Pong(Pong { ts_ms: r.u64()? }),
            other => return Err(ProtoError::UnknownMessage(other)),
        };
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_tag;
    use crate::frame_flags;
    use crate::input::mouse_button;

    #[test]
    fn roundtrip_hello_and_auth() {
        let hello = Message::ClientHello(ClientHello {
            version: 1,
            client_id: [7u8; 16],
            nonce_c: [9u8; 16],
        });
        let buf = hello.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), hello);

        let srv = Message::ServerHello(ServerHello {
            version: 1,
            width: 1920,
            height: 1080,
            rotation: 0,
            nonce_s: [3u8; 16],
        });
        let buf = srv.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), srv);

        let auth = Message::AuthRequest(AuthRequest { mac: [1u8; 32] });
        let buf = auth.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), auth);

        let res = Message::AuthResult(AuthResult {
            ok: false,
            reason: crate::auth_reason::BAD_PASSWORD,
        });
        let buf = res.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), res);
    }

    #[test]
    fn roundtrip_video_frame() {
        let data0 = [0xAAu8; 12];
        let data1 = [0xBBu8; 3];
        let frame = Message::VideoFrame(VideoFrame {
            seq: 42,
            flags: frame_flags::FULL | frame_flags::KEY,
            base_seq: 41,
            block_size: 64,
            blocks_x: 30,
            blocks_y: 17,
            entries: vec![
                BlockEntry {
                    idx: 0,
                    codec: codec_tag::LOSSLESS,
                    data: &data0,
                },
                BlockEntry {
                    idx: 509,
                    codec: codec_tag::JPEG,
                    data: &data1,
                },
            ],
        });
        let buf = frame.encode();
        let decoded = Message::decode(&buf).expect("decode");
        assert_eq!(decoded, frame);
        match decoded {
            Message::VideoFrame(f) => {
                assert!(f.is_full());
                let grid = f.grid().expect("grid");
                assert_eq!(grid.count(), 510);
                assert_eq!(f.entries.len(), 2);
                assert_eq!(f.entries[1].data, &data1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn roundtrip_input_and_control() {
        let ev = Message::Input(InputEvent::MouseButton {
            button: mouse_button::LEFT,
            down: true,
            x: 1920,
            y: 1080,
        });
        let buf = ev.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), ev);

        let ack = Message::VideoAck(FrameAck {
            seq: 5,
            recv_bytes: 1 << 33,
        });
        let buf = ack.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), ack);

        let req = Message::RequestFull;
        let buf = req.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), req);

        let ping = Message::Ping(Ping { ts_ms: 1_234 });
        let buf = ping.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), ping);
    }

    #[test]
    fn roundtrip_video_au() {
        let data = [0x42u8; 64];
        let au = Message::VideoAu(VideoAu {
            ts_ms: 1_234_567,
            seq: 9,
            codec: crate::codec_tag::HEVC,
            flags: 1,
            data: &data,
        });
        let buf = au.encode();
        let decoded = Message::decode(&buf).expect("decode");
        assert_eq!(decoded, au);
        match decoded {
            Message::VideoAu(a) => {
                assert!(a.is_key());
                assert_eq!(a.codec, crate::codec_tag::HEVC);
                assert_eq!(a.data.len(), 64);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn roundtrip_audio_frame() {
        let data = [0x11u8; 40];
        let af = Message::AudioFrame(AudioFrame {
            ts_ms: 999,
            seq: 3,
            flags: 1,
            sample_rate: 48_000,
            channels: 2,
            data: &data,
        });
        let buf = af.encode();
        let decoded = Message::decode(&buf).expect("decode");
        assert_eq!(decoded, af);
        match decoded {
            Message::AudioFrame(a) => {
                assert!(a.is_opus());
                assert_eq!(a.sample_rate, 48_000);
                assert_eq!(a.channels, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn cursor_shape_length_checked() {
        let rgba = [0u8; 4 * 2 * 2];
        let shape = Message::CursorShape(CursorShape {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 0,
            rgba: &rgba,
        });
        let buf = shape.encode();
        assert_eq!(Message::decode(&buf).expect("decode"), shape);

        // 手工造一个长度不匹配的
        let mut w = Writer::new();
        w.u8(MSG_CURSOR_SHAPE);
        w.u16(4);
        w.u16(4);
        w.i16(0);
        w.i16(0);
        w.blob(&[0u8; 8]);
        let bad = w.into_vec();
        assert!(Message::decode(&bad).is_err());
    }

    #[test]
    fn unknown_type_rejected() {
        assert!(Message::decode(&[0x7f]).is_err());
        // 空 payload
        assert!(Message::decode(&[]).is_err());
    }
}
