//! 输入事件（主控端 → 被控端）。

use crate::wire::{ProtoError, ProtoResult, Reader, Writer};

pub mod input_kind {
    pub const MOUSE_MOVE: u8 = 0;
    pub const MOUSE_BUTTON: u8 = 1;
    pub const WHEEL: u8 = 2;
    pub const KEY: u8 = 3;
    pub const KEY_UNICODE: u8 = 4;
}

pub mod mouse_button {
    pub const LEFT: u8 = 0;
    pub const RIGHT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const X1: u8 = 3;
    pub const X2: u8 = 4;
}

/// 一个输入事件。坐标一律是**被控端屏幕的物理像素**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        down: bool,
        x: i32,
        y: i32,
    },
    Wheel {
        dx: i16,
        dy: i16,
        x: i32,
        y: i32,
    },
    Key {
        scancode: u16,
        down: bool,
        extended: bool,
    },
    KeyUnicode {
        codepoint: u32,
        down: bool,
    },
}

impl InputEvent {
    pub fn encode(&self, w: &mut Writer) {
        match *self {
            InputEvent::MouseMove { x, y } => {
                w.u8(input_kind::MOUSE_MOVE);
                w.i32(x);
                w.i32(y);
            }
            InputEvent::MouseButton {
                button,
                down,
                x,
                y,
            } => {
                w.u8(input_kind::MOUSE_BUTTON);
                w.u8(button);
                w.bool(down);
                w.i32(x);
                w.i32(y);
            }
            InputEvent::Wheel { dx, dy, x, y } => {
                w.u8(input_kind::WHEEL);
                w.i16(dx);
                w.i16(dy);
                w.i32(x);
                w.i32(y);
            }
            InputEvent::Key {
                scancode,
                down,
                extended,
            } => {
                w.u8(input_kind::KEY);
                w.u16(scancode);
                w.bool(down);
                w.bool(extended);
            }
            InputEvent::KeyUnicode { codepoint, down } => {
                w.u8(input_kind::KEY_UNICODE);
                w.u32(codepoint);
                w.bool(down);
            }
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> ProtoResult<Self> {
        let kind = r.u8()?;
        match kind {
            input_kind::MOUSE_MOVE => Ok(InputEvent::MouseMove {
                x: r.i32()?,
                y: r.i32()?,
            }),
            input_kind::MOUSE_BUTTON => Ok(InputEvent::MouseButton {
                button: r.u8()?,
                down: r.bool()?,
                x: r.i32()?,
                y: r.i32()?,
            }),
            input_kind::WHEEL => Ok(InputEvent::Wheel {
                dx: r.i16()?,
                dy: r.i16()?,
                x: r.i32()?,
                y: r.i32()?,
            }),
            input_kind::KEY => Ok(InputEvent::Key {
                scancode: r.u16()?,
                down: r.bool()?,
                extended: r.bool()?,
            }),
            input_kind::KEY_UNICODE => Ok(InputEvent::KeyUnicode {
                codepoint: r.u32()?,
                down: r.bool()?,
            }),
            _ => Err(ProtoError::BadValue),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_variants() {
        let cases = [
            InputEvent::MouseMove { x: -3, y: 1080 },
            InputEvent::MouseButton {
                button: mouse_button::RIGHT,
                down: true,
                x: 5,
                y: 6,
            },
            InputEvent::Wheel { dx: 0, dy: -120, x: 7, y: 8 },
            InputEvent::Key {
                scancode: 0x1e,
                down: true,
                extended: false,
            },
            InputEvent::KeyUnicode {
                codepoint: 0x4e2d,
                down: false,
            },
        ];
        for ev in cases {
            let mut w = Writer::new();
            ev.encode(&mut w);
            let buf = w.into_vec();
            let mut r = Reader::new(&buf);
            assert_eq!(InputEvent::decode(&mut r).expect("decode"), ev);
            assert!(r.is_empty());
        }
    }
}
