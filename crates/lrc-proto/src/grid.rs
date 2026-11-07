//! 屏幕块网格与矩形。

/// 像素矩形（左上角 + 尺寸）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h as i32)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            Rect::default()
        } else {
            Rect::new(x, y, (right - x) as u32, (bottom - y) as u32)
        }
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(x, y, (right - x) as u32, (bottom - y) as u32)
    }
}

/// 块网格：把 W×H 屏幕按 `block_size` 切开，最右/最下一列块可能不满。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub block_size: u16,
    pub blocks_x: u16,
    pub blocks_y: u16,
}

impl Grid {
    /// `block_size` 为 0 或屏幕为 0 时返回 `None`。
    pub fn new(block_size: u16, screen_w: u32, screen_h: u32) -> Option<Self> {
        if block_size == 0 || screen_w == 0 || screen_h == 0 {
            return None;
        }
        let bs = u32::from(block_size);
        let bx = (screen_w + bs - 1) / bs;
        let by = (screen_h + bs - 1) / bs;
        Some(Self {
            block_size,
            blocks_x: u16::try_from(bx).ok()?,
            blocks_y: u16::try_from(by).ok()?,
        })
    }

    pub fn count(&self) -> u32 {
        u32::from(self.blocks_x) * u32::from(self.blocks_y)
    }

    /// 块下标 → 屏幕像素矩形（已按屏幕边界裁剪）。
    pub fn block_rect(&self, idx: u32, screen_w: u32, screen_h: u32) -> Option<Rect> {
        if idx >= self.count() {
            return None;
        }
        let bx = u32::from(self.blocks_x);
        let bs = u32::from(self.block_size);
        let (gx, gy) = (idx % bx, idx / bx);
        let x = gx * bs;
        let y = gy * bs;
        let w = bs.min(screen_w.saturating_sub(x));
        let h = bs.min(screen_h.saturating_sub(y));
        if w == 0 || h == 0 {
            return None;
        }
        Some(Rect::new(x as i32, y as i32, w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_geometry_1080p() {
        let g = Grid::new(64, 1920, 1080).expect("grid");
        assert_eq!(g.blocks_x, 30);
        assert_eq!(g.blocks_y, 17);
        assert_eq!(g.count(), 510);

        let r = g.block_rect(0, 1920, 1080).expect("block 0");
        assert_eq!(r, Rect::new(0, 0, 64, 64));

        // 最后一列块，宽 1920 - 29*64 = 64... 恰好整除；改用 1000 宽验证裁剪
        let g2 = Grid::new(64, 1000, 300).expect("grid2");
        let last_col = g2.blocks_x - 1;
        let r = g2.block_rect(u32::from(last_col), 1000, 300).expect("last col");
        assert_eq!(r.x, i32::from(last_col) * 64);
        assert_eq!(r.w, 1000 - u32::from(last_col) * 64);

        let last_row = g2.blocks_y - 1;
        let r = g2
            .block_rect(u32::from(last_row) * u32::from(g2.blocks_x), 1000, 300)
            .expect("last row");
        assert_eq!(r.y, i32::from(last_row) * 64);
        assert_eq!(r.h, 300 - u32::from(last_row) * 64);
    }

    #[test]
    fn rect_ops() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert!(a.contains(0, 0));
        assert!(!a.contains(10, 0));
        assert_eq!(Rect::new(0, 0, 0, 0).area(), 0);
    }
}
