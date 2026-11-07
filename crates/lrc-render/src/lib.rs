//! 主控端呈现：D3D11 swapchain + 动图纹理逐块更新 + 本光标合成。
//!
//! 契约见 `docs/DESIGN.md` §1。**接口签名是共享契约**，改动先跟 Lead 对齐。
//!
//! M1 实现取向：backbuffer 尺寸 = 被控端分辨率，每帧把「完整桌面纹理」CopyResource 到
//! backbuffer 再由 DWM 缩放到窗口。这样不需要编译 HLSL（也就不需要 d3dcompiler/fxc feature），
//! 且块更新只走 UpdateSubresource + D3D11_BOX，没有额外全屏 blit。

use std::fmt;

use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

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

/// windows 错误 → `0x887A0005 (msg)`
fn hr_text(e: &windows::core::Error) -> String {
    format!("0x{:08X} ({e})", e.code().0 as u32)
}

/// 光标位图（BGRA，长度 = 4*w*h）。
#[derive(Clone, Debug, Default)]
pub struct CursorBitmap {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bgra: Vec<u8>,
}

fn create_device() -> RenderResult<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device as *mut _),
            None,
            Some(&mut context as *mut _),
        )
    }
    .map_err(|e| RenderError::Init(format!("D3D11CreateDevice: {}", hr_text(&e))))?;
    let device = device.ok_or_else(|| RenderError::Init("D3D11CreateDevice 未返回 device".into()))?;
    let context =
        context.ok_or_else(|| RenderError::Init("D3D11CreateDevice 未返回 context".into()))?;
    Ok((device, context))
}

fn create_desktop_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> RenderResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(1),
        Height: h.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex as *mut _)) }
        .map_err(|e| RenderError::Init(format!("CreateTexture2D(desktop): {}", hr_text(&e))))?;
    tex.ok_or_else(|| RenderError::Init("CreateTexture2D(desktop) 返回空".into()))
}

/// 呈现器：把解码后的块拷进纹理，`present()` 提交一帧。
pub struct Presenter {
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    /// bitblt 模型的 backbuffer；ResizeBuffers 前必须释放（置 None）
    backbuffer: Option<ID3D11Texture2D>,
    /// 完整桌面纹理，局部块更新都打在这上面，present 时整块拷到 backbuffer
    desktop: ID3D11Texture2D,
    width: u32,
    height: u32,
    cursor: Option<CursorBitmap>,
    cursor_visible: bool,
    /// 越界块裁剪时用的中转缓冲
    scratch: Vec<u8>,
}

impl Presenter {
    /// `hwnd` 为主控端窗口句柄（isize，避免把 windows 类型泄进 API）。
    pub fn new(hwnd: isize, width: u32, height: u32) -> RenderResult<Self> {
        if hwnd == 0 {
            return Err(RenderError::Init("hwnd 为 0".into()));
        }
        let width = width.max(1);
        let height = height.max(1);
        let (device, context) = create_device()?;

        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| RenderError::Init(format!("CreateDXGIFactory1<IDXGIFactory2>: {}", hr_text(&e))))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swapchain = unsafe {
            factory.CreateSwapChainForHwnd(
                &device,
                HWND(hwnd as *mut core::ffi::c_void),
                &desc,
                None,
                None::<&IDXGIOutput>,
            )
        }
        .map_err(|e| RenderError::Init(format!("CreateSwapChainForHwnd: {}", hr_text(&e))))?;

        let backbuffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }
            .map_err(|e| RenderError::Init(format!("GetBuffer(0): {}", hr_text(&e))))?;
        let desktop = create_desktop_texture(&device, width, height)?;

        log::debug!("render: 初始化完成 {width}x{height}");

        Ok(Self {
            hwnd: HWND(hwnd as *mut core::ffi::c_void),
            device,
            context,
            swapchain,
            backbuffer: Some(backbuffer),
            desktop,
            width,
            height,
            cursor: None,
            cursor_visible: true,
            scratch: Vec::new(),
        })
    }

    /// 屏幕尺寸变化（被控端分辨率/显示器切换）。
    pub fn resize(&mut self, width: u32, height: u32) -> RenderResult<()> {
        if width == 0 || height == 0 {
            // 窗口最小化时客户区可能是 0，忽略这次 resize
            return Ok(());
        }
        if width == self.width && height == self.height {
            return Ok(());
        }
        // ResizeBuffers 要求先释放所有 backbuffer 引用
        self.backbuffer = None;
        unsafe {
            self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|e| RenderError::Init(format!("ResizeBuffers({width}x{height}): {}", hr_text(&e))))?;
        let backbuffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }
            .map_err(|e| RenderError::Init(format!("GetBuffer(0) after resize: {}", hr_text(&e))))?;
        self.backbuffer = Some(backbuffer);
        self.desktop = create_desktop_texture(&self.device, width, height)?;
        self.width = width;
        self.height = height;
        self.scratch.clear();
        log::debug!("render: resize → {width}x{height}");
        Ok(())
    }

    /// 用一整块 BGRA 覆盖（整帧/清屏）。
    pub fn update_full(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
        stride: usize,
    ) -> RenderResult<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let row = width as usize * 4;
        if stride < row || bgra.len() < stride.saturating_mul(height as usize) {
            return Err(RenderError::Init(format!(
                "update_full 数据不足: {width}x{height} stride={stride} len={}",
                bgra.len()
            )));
        }
        self.resize(width, height)?;
        let depth = stride.saturating_mul(height as usize).min(u32::MAX as usize) as u32;
        unsafe {
            self.context.UpdateSubresource(
                &self.desktop,
                0,
                None,
                bgra.as_ptr() as *const core::ffi::c_void,
                stride as u32,
                depth,
            )
        };
        Ok(())
    }

    /// 更新屏幕上一块矩形（坐标为采集源像素坐标）。
    pub fn update_block(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        bgra: &[u8],
        stride: usize,
    ) -> RenderResult<()> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        let row = w as usize * 4;
        if stride < row || bgra.len() < stride.saturating_mul(h as usize) {
            return Err(RenderError::Init(format!(
                "update_block 数据不足: {w}x{h} stride={stride} len={}",
                bgra.len()
            )));
        }
        // 裁到屏幕内
        let tw = self.width.min(i32::MAX as u32) as i32;
        let th = self.height.min(i32::MAX as u32) as i32;
        let left = x.clamp(0, tw);
        let top = y.clamp(0, th);
        let right = x.saturating_add(w as i32).clamp(0, tw);
        let bottom = y.saturating_add(h as i32).clamp(0, th);
        if right <= left || bottom <= top {
            return Ok(());
        }
        let (cw, ch) = ((right - left) as u32, (bottom - top) as u32);
        let src_x = (left - x).max(0) as usize;
        let src_y = (top - y).max(0) as usize;

        let dst_box = D3D11_BOX {
            left: left as u32,
            top: top as u32,
            front: 0,
            right: right as u32,
            bottom: bottom as u32,
            back: 1,
        };

        if src_x == 0 && src_y == 0 && cw == w && ch == h {
            // 常见路径：完全不裁剪，直接从调用方的数据上传
            unsafe {
                self.context.UpdateSubresource(
                    &self.desktop,
                    0,
                    Some(&dst_box),
                    bgra.as_ptr() as *const core::ffi::c_void,
                    stride as u32,
                    0,
                )
            };
        } else {
            // 被裁剪：先把需要的那部分收到 scratch 里再上传
            let cw_row = cw as usize * 4;
            let need = cw_row * ch as usize;
            self.scratch.clear();
            self.scratch.resize(need, 0);
            for yy in 0..ch as usize {
                let s = (src_y + yy) * stride + src_x * 4;
                let d = yy * cw_row;
                let (Some(src), Some(dst)) = (bgra.get(s..s + cw_row), self.scratch.get_mut(d..d + cw_row))
                else {
                    return Err(RenderError::Init("update_block 越界".into()));
                };
                dst.copy_from_slice(src);
            }
            unsafe {
                self.context.UpdateSubresource(
                    &self.desktop,
                    0,
                    Some(&dst_box),
                    self.scratch.as_ptr() as *const core::ffi::c_void,
                    cw_row as u32,
                    0,
                )
            };
        }
        Ok(())
    }

    /// 光标位图变化时更新（`None` 表示隐藏）。
    pub fn set_cursor_shape(&mut self, cursor: Option<&CursorBitmap>) -> RenderResult<()> {
        // M1：光标由主控端本地窗口绘制（WM_SETCURSOR / SetCursor），不叠进 D3D 纹理。
        // 这里只缓存，保证不 panic、不丢状态，M1.5 再换成 D3D 叠加。
        self.cursor = cursor.cloned();
        self.cursor_visible = self
            .cursor
            .as_ref()
            .map(|c| !c.bgra.is_empty() && c.width > 0 && c.height > 0)
            .unwrap_or(false);
        Ok(())
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    /// 提交一帧到屏幕。
    pub fn present(&mut self) -> RenderResult<()> {
        let Some(backbuffer) = self.backbuffer.as_ref() else {
            return Err(RenderError::Present("backbuffer 不存在（resize 后未恢复）".into()));
        };
        unsafe { self.context.CopyResource(backbuffer, &self.desktop) };
        // Present(1, 0)：跟垂直同步，避免撕裂
        let hr = unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            return Err(RenderError::Present(format!("Present: {}", hr_text(&hr.into()))));
        }
        Ok(())
    }

    /// 主控端窗口客户区尺寸（用于把鼠标坐标映射回被控端坐标）。
    pub fn client_size(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rc) }.is_ok() {
            let w = (rc.right - rc.left).max(0) as u32;
            let h = (rc.bottom - rc.top).max(0) as u32;
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
        (self.width, self.height)
    }

    /// 当前桌面纹理尺寸（= 被控端分辨率）。
    pub fn source_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}
