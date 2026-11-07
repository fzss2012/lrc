//! 最小 Ogg 页切分（只用于给 ffmpeg 的 Ogg 输出分帧，不做 CRC/生成）。

/// 页头长度。
pub(crate) const PAGE_HEADER: usize = 27;

/// 缓冲区里是否已经有一整页；有就返回总长度。
pub(crate) fn page_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < PAGE_HEADER {
        return None;
    }
    if &buf[0..4] != b"OggS" {
        return None;
    }
    let nsegs = buf[26] as usize;
    if buf.len() < PAGE_HEADER + nsegs {
        return None;
    }
    let payload: usize = buf[PAGE_HEADER..PAGE_HEADER + nsegs]
        .iter()
        .map(|s| *s as usize)
        .sum();
    let total = PAGE_HEADER + nsegs + payload;
    if buf.len() < total {
        None
    } else {
        Some(total)
    }
}

/// 是不是音频数据页（BOS 位是 OpusHead 页，第二页是 OpusTags）。
pub(crate) fn is_data_page(page: &[u8]) -> bool {
    if page.len() <= PAGE_HEADER {
        return false;
    }
    page[5] & 0x02 == 0 // header_type bit1 = BOS
}

/// 如果流里混进了垃圾字节，找到下一个 OggS 的位置（用于重同步）。
pub(crate) fn find_next_page(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let mut i = 1usize;
    while i + 4 <= buf.len() {
        if &buf[i..i + 4] == b"OggS" {
            return Some(i);
        }
        i += 1;
    }
    None
}
