//! 主控端呈现：D3D11 swapchain + 动图纹理逐块更新 + 本光标合成。
//!
//! 契约见 `docs/DESIGN.md` §1。**接口签名是共享契约**，改动先跟 Lead 对齐。

use std::fmt;

#[derive(Debug)]
pub enum RenderError {
    Init(String),
    Present(String),
    Unsupported(&'static str),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Init(m) => write!(f, "render: 初始化失败 {m}"),
            RenderError::Present(m) => write!(f, "render: 呈现失败 {m}"),
            RenderError::Unsupported(m) => write!(f, "render: 不支持 {m}"),
        }
    }
}

impl std::error::Error for RenderError {}

pub type RenderResult<T> = Result<T, RenderError>;

/// 光标位图（BGRA，长度 = 4*w*h）。
#[derive(Clone, Debug, Default)]
pub struct CursorBitmap {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bgra: Vec<u8>,
}

/// 呈现器：把解码后的块拷进纹理，`present()` 提交一帧。
pub struct Presenter {
    _priv: (),
}

impl Presenter {
    /// `hwnd` 为主控端窗口句柄（isize，避免把 windows 类型泄进 API）。
    pub fn new(_hwnd: isize, _width: u32, _height: u32) -> RenderResult<Self> {
        todo!("T1 实现：D3D11 设备 + DXGI swapchain(BGRA) + 全屏四边形 / 或动图纹理")
    }

    /// 屏幕尺寸变化（被控端分辨率/显示器切换）。
    pub fn resize(&mut self, _width: u32, _height: u32) -> RenderResult<()> {
        todo!("T1 实现")
    }

    /// 用一整块 BGRA 覆盖（整帧/清屏）。
    pub fn update_full(
        &mut self,
        _width: u32,
        _height: u32,
        _bgra: &[u8],
        _stride: usize,
    ) -> RenderResult<()> {
        todo!("T1 实现")
    }

    /// 更新屏幕上一块矩形（坐标为采集源像素坐标）。
    pub fn update_block(
        &mut self,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
        _bgra: &[u8],
        _stride: usize,
    ) -> RenderResult<()> {
        todo!("T1 实现：Map 动图纹理对应行/区域并拷贝")
    }

    /// 光标位图变化时更新（`None` 表示隐藏）。
    pub fn set_cursor_shape(&mut self, _cursor: Option<&CursorBitmap>) -> RenderResult<()> {
        todo!("T1 实现（可选，M1.5）：D3D11 叠加绘制光标；M1 可先 no-op")
    }

    pub fn set_cursor_visible(&mut self, _visible: bool) {}

    /// 提交一帧到屏幕。
    pub fn present(&mut self) -> RenderResult<()> {
        todo!("T1 实现：swapchain Present(1) 或非阻塞 Present(0) + 统计")
    }

    /// 主控端窗口客户区尺寸（用于把鼠标坐标映射回被控端坐标）。
    pub fn client_size(&self) -> (u32, u32) {
        todo!("T1 实现")
    }
}
