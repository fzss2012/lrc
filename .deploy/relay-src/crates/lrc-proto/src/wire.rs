//! 读写原语。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    /// 数据不足
    Truncated,
    /// 未知的消息类型
    UnknownMessage(u8),
    /// 字段越界/非法
    BadValue,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "proto: 数据被截断"),
            ProtoError::UnknownMessage(t) => write!(f, "proto: 未知消息类型 0x{t:02x}"),
            ProtoError::BadValue => write!(f, "proto: 字段非法"),
        }
    }
}

impl std::error::Error for ProtoError {}

pub type ProtoResult<T> = Result<T, ProtoError>;

/// 顺序写入器。
#[derive(Default, Debug)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn fixed<const N: usize>(&mut self, v: &[u8; N]) {
        self.buf.extend_from_slice(v);
    }

    /// 写入 `u32` 小端长度前缀 + 内容。
    pub fn blob(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.bytes(v);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 就地回填某处 u32（用于先占位后写长度）。`at` 必须已写入 4 字节。
    pub fn patch_u32(&mut self, at: usize, v: u32) {
        if let Some(slot) = self.buf.get_mut(at..at + 4) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// 顺序读取器，越界一律返回 `ProtoError::Truncated`。
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> ProtoResult<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(ProtoError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> ProtoResult<u8> {
        let b = self.take(1)?;
        Ok(b[0])
    }

    pub fn u16(&mut self) -> ProtoResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> ProtoResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> ProtoResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn i16(&mut self) -> ProtoResult<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn i32(&mut self) -> ProtoResult<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn bool(&mut self) -> ProtoResult<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn fixed<const N: usize>(&mut self) -> ProtoResult<[u8; N]> {
        let b = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(b);
        Ok(out)
    }

    /// 读 `u32` 长度前缀 + 内容，并校验上限。
    pub fn blob(&mut self, max: usize) -> ProtoResult<&'a [u8]> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(ProtoError::BadValue);
        }
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_primitives() {
        let mut w = Writer::new();
        w.u8(7);
        w.u16(0x1234);
        w.u32(0xdead_beef);
        w.i32(-5);
        w.blob(b"hello");
        let v = w.into_vec();

        let mut r = Reader::new(&v);
        assert_eq!(r.u8().expect("u8"), 7);
        assert_eq!(r.u16().expect("u16"), 0x1234);
        assert_eq!(r.u32().expect("u32"), 0xdead_beef);
        assert_eq!(r.i32().expect("i32"), -5);
        assert_eq!(r.blob(16).expect("blob"), b"hello");
        assert!(r.is_empty());
    }

    #[test]
    fn truncated_is_error() {
        let mut r = Reader::new(&[1, 2]);
        assert!(r.u32().is_err());
    }

    #[test]
    fn blob_over_max_rejected() {
        let mut w = Writer::new();
        w.blob(&[0u8; 32]);
        let v = w.into_vec();
        let mut r = Reader::new(&v);
        assert!(r.blob(8).is_err());
    }
}
