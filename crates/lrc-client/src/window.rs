//! 主控端窗口：Win32 窗口类 + 消息循环 + 呈现 + 输入转发。
//!
//! 线程模型：本模块全部代码跑在窗口线程（主线程）。接收线程只往
//! `Shared::pending` 里塞解好的脏块；8ms 定时器触发本线程 drain → `Presenter::update_block`
//! → `present()`（D3D11 设备非线程安全，呈现必须在这条线程）。

use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use lrc_proto::input::mouse_button;
use lrc_proto::{FrameAck, InputEvent, Message, Ping, Rect};
use lrc_render::Presenter;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromWindow, ScreenToClient, SetBkMode, SetTextColor, StretchDIBits,
    UpdateWindow, ValidateRect, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLOR_WINDOW, DIB_RGB_COLORS,
    HBRUSH, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, RGBQUAD, SRCCOPY,
    TRANSPARENT, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW,
    PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWL_STYLE,
    GWLP_USERDATA, HICON, IDC_ARROW, MSG, SetCursor, SetWindowTextW, SW_SHOWNORMAL,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WM_CREATE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SIZE,
    WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::cli::Config;
use crate::mapping;
use crate::net;
use crate::state::{BlockUpdate, ConnStatus, CursorShapeOwned, Shared};

/// 呈现定时器：8 ms 拉一次脏块队列（≈125 Hz 上限）。
const TIMER_PRESENT: usize = 1;
/// 统计/ack 定时器：1 s。
const TIMER_TICK: usize = 2;
/// 脏块推送间隔。
const PRESENT_INTERVAL_MS: u32 = 8;
/// 统计/ack 间隔。
const TICK_INTERVAL_MS: u32 = 1000;

fn window_class() -> PCWSTR {
    windows::core::w!("lrc_client_window")
}

/// 创建窗口、启动接收线程、跑消息循环，直到窗口销毁。
pub fn run(cfg: &Config) -> anyhow::Result<()> {
    let shared = Arc::new(Shared::new());
    let mut app = Box::new(AppState::new(shared.clone(), cfg));
    let app_ptr: *mut AppState = &mut *app;

    let hwnd = match create_window(app_ptr, cfg) {
        Ok(hwnd) => hwnd,
        Err(e) => {
            drop(app);
            return Err(e);
        }
    };

    // 接收线程：连接 + 握手 + 收流解码；断线自动重连，不需要窗口线程参与。
    if let Err(e) = net::spawn(cfg, shared.clone()) {
        log::error!("接收线程启动失败: {e}");
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    log::info!("窗口已就绪（hwnd={hwnd:?}），连接 {} ...", cfg.connect);

    unsafe {
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == -1 {
                let err = GetLastError();
                drop(app);
                return Err(anyhow::anyhow!("GetMessageW 失败: {}", err.0));
            }
            if ret.0 == 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    drop(app);
    Ok(())
}

fn create_window(app_ptr: *mut AppState, cfg: &Config) -> anyhow::Result<HWND> {
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|e| anyhow::anyhow!("GetModuleHandleW 失败: {e}"))?;
    let hinstance = HINSTANCE(module.0);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| anyhow::anyhow!("LoadCursorW 失败: {e}"))?;

    let class_name = window_class();
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: HICON::default(),
        hCursor: cursor,
        // 系统色背景（COLOR_WINDOW + 1 是 Win32 约定："使用系统色画背景"）
        hbrBackground: HBRUSH((COLOR_WINDOW.0 as isize + 1) as *mut core::ffi::c_void),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: class_name,
    };
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        let err = unsafe { GetLastError() };
        return Err(anyhow::anyhow!("RegisterClassW 失败: Win32 错误 {}", err.0));
    }

    // 让"客户区"正好是 1280x720（窗口边框/标题栏额外占的那圈不算）。
    let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: cfg.width as i32,
        bottom: cfg.height as i32,
    };
    unsafe { AdjustWindowRectEx(&mut frame, style, false, WINDOW_EX_STYLE(0)) }
        .map_err(|e| anyhow::anyhow!("AdjustWindowRectEx 失败: {e}"))?;

    let title = encode_wide(&cfg.title);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            PCWSTR::from_raw(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            frame.right - frame.left,
            frame.bottom - frame.top,
            None,
            None,
            Some(hinstance),
            Some(app_ptr as *const core::ffi::c_void),
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW 失败: {e}"))?;

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = UpdateWindow(hwnd);
    }
    Ok(hwnd)
}

fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 呈现面：优先 D3D11；初始化失败（例如 renderer 未实现/驱动异常）时回退 GDI。
enum Surface {
    None,
    Gpu(Presenter),
    Gdi(SoftCanvas),
}

/// GDI 回退用的整屏 BGRA 缓冲。
struct SoftCanvas {
    w: u32,
    h: u32,
    bgra: Vec<u8>,
}

impl SoftCanvas {
    fn new(w: u32, h: u32) -> Self {
        let len = (w as usize)
            .checked_mul(4)
            .and_then(|stride| stride.checked_mul(h as usize))
            .unwrap_or(0);
        Self {
            w,
            h,
            bgra: vec![0u8; len],
        }
    }

    /// 把一块紧排 BGRA（行步长 `src_stride`）贴到 (x, y)，自动裁到画布内。
    fn blit_rect(&mut self, x: i32, y: i32, w: u32, h: u32, data: &[u8], src_stride: usize) {
        if x < 0 || y < 0 || w == 0 || h == 0 || self.w == 0 || src_stride == 0 {
            return;
        }
        let Some(dst_stride) = (self.w as usize).checked_mul(4) else {
            return;
        };
        let bx = x as usize;
        let by = y as usize;
        if bx >= self.w as usize || by >= self.h as usize {
            return;
        }
        let copy_w = (w as usize).min(self.w as usize - bx);
        let copy_h = (h as usize).min(self.h as usize - by);
        for row in 0..copy_h {
            let Some(src_off) = row.checked_mul(src_stride) else {
                return;
            };
            let Some(dst_off) = (by + row)
                .checked_mul(dst_stride)
                .and_then(|v| v.checked_add(bx * 4))
            else {
                return;
            };
            let bytes = copy_w * 4;
            let (Some(src_end), Some(dst_end)) =
                (src_off.checked_add(bytes), dst_off.checked_add(bytes))
            else {
                return;
            };
            let (Some(src), Some(dst)) =
                (data.get(src_off..src_end), self.bgra.get_mut(dst_off..dst_end))
            else {
                return;
            };
            dst.copy_from_slice(src);
        }
    }

    /// 往 HDC 上画：黑底 + 拉伸铺满客户区（与 D3D 的 DXGI_SCALING_STRETCH 同口径）。
    unsafe fn paint_hdc(&self, hdc: HDC, client: (u32, u32)) {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: client.0 as i32,
            bottom: client.1 as i32,
        };
        let brush = CreateSolidBrush(COLORREF(0));
        FillRect(hdc, &client_rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let (dest_w, dest_h) = (client.0 as i32, client.1 as i32);
        if dest_w > 0 && dest_h > 0 && self.w > 0 && self.h > 0 && !self.bgra.is_empty() {
            let header = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.w as i32,
                // 负高度 = top-down DIB（我们的 BGRA 就是从上往下存的）
                biHeight: -(self.h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let info = BITMAPINFO {
                bmiHeader: header,
                bmiColors: [RGBQUAD::default()],
            };
            let _ = StretchDIBits(
                hdc,
                0,
                0,
                dest_w,
                dest_h,
                0,
                0,
                self.w as i32,
                self.h as i32,
                Some(self.bgra.as_ptr() as *const core::ffi::c_void),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
}

/// 窗口线程专属状态（`Presenter` 非 Send，必须留在本线程）。
struct AppState {
    hwnd: HWND,
    shared: Arc<Shared>,
    /// 命令行给的基础标题（重连/错误提示在它后面加后缀）
    base_title: String,
    /// 当前已经设到窗口上的标题（避免每 tick 都 SetWindowText）
    title_applied: String,
    /// 窗口内叠加的状态提示（重连中/认证失败）；None = 不显示
    status_text: Option<String>,
    stats_enabled: bool,
    surface: Surface,
    /// 当前呈现面对应的被控端尺寸
    surface_size: Option<(u32, u32)>,
    /// 窗口被遮挡/缩放后需要重新 present 一次
    need_present: bool,
    /// D3D 初始化/呈现已经失败，后续只用 GDI
    gpu_disabled: bool,
    /// 首帧 FULL 后按被控端宽高比调整过一次客户区
    aspect_fixed: bool,
    /// 干净的解码帧（被控端像素，BGRA 行连续）—— 光标合成的底图
    clean: Vec<u8>,
    /// 光标合成用的临时缓冲（只有矩形与光标有交集时才用）
    scratch: Vec<u8>,
    /// 光标本地合成状态
    cursor: CursorRender,
    stats: Stats,
}

struct Stats {
    at: Instant,
    frames: u64,
    bytes: u64,
    /// 累计解码出的视频帧（算 video_fps 用）
    video_frames: u64,
    /// 上一次的管线延迟（这一秒没有视频帧时沿用）
    lat_ms: f64,
}

impl Stats {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            frames: 0,
            bytes: 0,
            video_frames: 0,
            lat_ms: 0.0,
        }
    }
}

impl AppState {
    fn new(shared: Arc<Shared>, cfg: &Config) -> Self {
        Self {
            hwnd: HWND(std::ptr::null_mut()),
            shared,
            base_title: cfg.title.clone(),
            title_applied: cfg.title.clone(),
            status_text: None,
            stats_enabled: cfg.stats,
            surface: Surface::None,
            surface_size: None,
            need_present: false,
            gpu_disabled: false,
            aspect_fixed: false,
            clean: Vec::new(),
            scratch: Vec::new(),
            cursor: CursorRender::default(),
            stats: Stats::new(),
        }
    }

    fn on_create(&mut self, hwnd: HWND) {
        self.hwnd = hwnd;
        let present = unsafe { SetTimer(Some(hwnd), TIMER_PRESENT, PRESENT_INTERVAL_MS, None) };
        if present == 0 {
            log::warn!("SetTimer(呈现) 失败");
        }
        let tick = unsafe { SetTimer(Some(hwnd), TIMER_TICK, TICK_INTERVAL_MS, None) };
        if tick == 0 {
            log::warn!("SetTimer(统计) 失败");
        }
    }

    fn on_destroy(&mut self) {
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_PRESENT) };
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_TICK) };
        self.shared.set_quit();
        self.shared.set_connected(false);
    }

    // ---- 呈现 ----

    fn on_present_timer(&mut self) {
        self.sync_surface();
        self.fit_window_aspect_once();
        // 0) 视频通道：整帧解码结果先上屏（只在视频路径用 update_full）
        self.apply_video_frame();

        // 1) 块通道：解码块先进干净帧（光标不进码流，合成前必须先有"无光标"的底图）
        let blocks = self.shared.take_blocks();
        let mut damage: Vec<Rect> = Vec::with_capacity(blocks.len() + 4);
        if self.shared.take_block_full_reset() {
            // 视频 → 块：整屏重建 clean，顺手把旧光标/视频残影清掉
            self.reset_clean_for_blocks(&mut damage);
        }
        for block in &blocks {
            self.apply_block_to_clean(block, &mut damage);
        }
        // 2) 光标位置/形状/可见性变了：旧包围盒要擦、新包围盒要画
        self.refresh_cursor(&mut damage);
        // 3) 逐矩形合成（干净帧 + 光标）并提交
        self.render(damage);
    }

    /// 收到首帧（flags FULL）后，把客户区调成被控端的宽高比。
    ///
    /// 口径见 DESIGN §4.5：画面是"拉伸铺满"客户区的，把客户区比例对齐后
    /// 常规使用就没有拉伸失真，鼠标也就自然点得准。只做一次。
    fn fit_window_aspect_once(&mut self) {
        if self.aspect_fixed || !self.shared.has_first_full_frame() {
            return;
        }
        let Some(remote) = self.shared.remote_size() else {
            return;
        };
        self.aspect_fixed = true;

        let client = self.client_size();
        if client.0 == 0 || client.1 == 0 {
            return;
        }
        let deviation = mapping::aspect_deviation(client, remote);
        if deviation <= mapping::ASPECT_TOLERANCE {
            log::debug!(
                "客户区 {:?} 与被控端 {:?} 宽高比一致（偏差 {:.2}%），不调整窗口",
                client,
                remote,
                deviation * 100.0
            );
            return;
        }

        // 保持宽度按比例算高，再受显示器工作区限制（减去标题栏/边框占的高度）
        let max_client_h = self
            .work_area_height()
            .map(|work| work.saturating_sub(self.window_chrome_height()))
            .unwrap_or(0);
        let new_h = mapping::client_height_for_remote(client.0, remote, max_client_h);
        if new_h == 0 || new_h == client.1 {
            return;
        }
        log::info!(
            "首帧后自适应：客户区 {:?} → 宽 {} 高 {new_h}（被控端 {remote:?}，宽高比偏差 {:.2}%）",
            client,
            client.0,
            deviation * 100.0
        );
        self.resize_client(client.0, new_h);
    }

    /// 客户区高度里有多少是窗口边框/标题栏（用于算工作区能容下多高的客户区）。
    fn window_chrome_height(&self) -> u32 {
        let (_, client_h) = self.client_size();
        let mut outer = RECT::default();
        if unsafe { GetWindowRect(self.hwnd, &mut outer) }.is_err() {
            return 0;
        }
        let outer_h = (outer.bottom - outer.top).max(0) as u32;
        outer_h.saturating_sub(client_h)
    }

    /// 窗口所在显示器的工作区高度（物理像素）。
    fn work_area_height(&self) -> Option<u32> {
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return None;
        }
        Some((info.rcWork.bottom - info.rcWork.top).max(0) as u32)
    }

    /// 按客户区尺寸设置窗口大小（保持位置）。
    fn resize_client(&mut self, client_w: u32, client_h: u32) {
        let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) } as u32);
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: client_w as i32,
            bottom: client_h as i32,
        };
        if unsafe { AdjustWindowRectEx(&mut frame, style, false, WINDOW_EX_STYLE(0)) }.is_err() {
            return;
        }
        let outer_w = frame.right - frame.left;
        let outer_h = frame.bottom - frame.top;
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                outer_w,
                outer_h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        self.need_present = true;
    }

    /// 被控端尺寸变化时建立/调整呈现面，并请求一帧整屏。
    fn sync_surface(&mut self) {
        let Some((w, h)) = self.shared.remote_size() else {
            return;
        };
        if self.surface_size == Some((w, h)) {
            return;
        }
        log::info!("被控端画面尺寸 {w}x{h}，建立呈现面");

        let mut resized_ok = false;
        let resize_result = match &mut self.surface {
            Surface::Gpu(presenter) => Some(presenter.resize(w, h)),
            _ => None,
        };
        match resize_result {
            Some(Ok(())) => resized_ok = true,
            Some(Err(e)) => {
                log::warn!("Presenter::resize 失败，改用 GDI: {e}");
                self.gpu_disabled = true;
                self.surface = Surface::None;
            }
            None => {}
        }

        if !resized_ok {
            match self.try_new_presenter(w, h) {
                Some(presenter) => self.surface = Surface::Gpu(presenter),
                None => {
                    self.gpu_disabled = true;
                    self.surface = Surface::Gdi(SoftCanvas::new(w, h));
                }
            }
        }

        self.surface_size = Some((w, h));
        // 干净帧缓冲按新尺寸重建（旧内容作废）；光标包围盒也作废，下个 tick 重画。
        // 末尾多留一行 padding：Presenter::update_block 要求 len >= stride*h，
        // 而零拷贝提交的窗口切片最后一行的后半段会越过逻辑宽高。
        let stride = (w as usize).saturating_mul(4);
        let len = stride.saturating_mul(h as usize + 1);
        self.clean.clear();
        self.clean.resize(len, 0);
        self.scratch.clear();
        self.cursor.reset();
        self.need_present = true;
        // 旧画面作废（或呈现面重建），向被控端要一帧整屏。
        self.shared.request_full();
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn try_new_presenter(&self, w: u32, h: u32) -> Option<Presenter> {
        if self.gpu_disabled {
            return None;
        }
        let hwnd = self.hwnd.0 as isize;
        // 渲染器初始化可能 panic（D3D 设备丢失、驱动异常、或 renderer 尚未实现）。
        // 远控客户端不该因为呈现后端失败就整机 panic，就地回退 GDI。
        let created = catch_unwind(AssertUnwindSafe(move || Presenter::new(hwnd, w, h)));
        match created {
            Ok(Ok(presenter)) => Some(presenter),
            Ok(Err(e)) => {
                log::warn!("D3D 呈现初始化失败: {e}（回退 GDI 绘制）");
                None
            }
            Err(_) => {
                log::warn!("D3D 呈现初始化 panic（回退 GDI 绘制）");
                None
            }
        }
    }

    /// 视频通道：把最新解出的一帧整帧上屏，并同步光标合成用的 clean。
    ///
    /// 视频路径**只用 `update_full`**（块路径用 `update_block`）；之后强制光标重绘，
    /// 让它叠到新帧上。
    fn apply_video_frame(&mut self) {
        let Some(frame) = self.shared.take_video_frame() else {
            return;
        };
        let (width, height, stride) = (frame.width, frame.height, frame.stride);
        if width == 0 || height == 0 {
            return;
        }
        let Some(clean_stride) = (width as usize).checked_mul(4) else {
            return;
        };
        if stride < clean_stride {
            log::warn!("视频帧 stride 非法: {stride} < {clean_stride}");
            return;
        }
        // 尺寸变了（被控端分辨率切换）：重建呈现面 + clean
        self.ensure_surface_for(width, height);

        // 整帧上屏：视频路径只用 update_full
        let mut degraded = false;
        match &mut self.surface {
            Surface::Gpu(presenter) => {
                let updated = catch_unwind(AssertUnwindSafe(|| {
                    presenter.update_full(width, height, &frame.bgra, stride)
                }));
                match updated {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("update_full 失败: {e}");
                        degraded = true;
                    }
                    Err(_) => {
                        log::warn!("update_full panic");
                        degraded = true;
                    }
                }
            }
            Surface::Gdi(canvas) => {
                canvas.blit_rect(0, 0, width, height, &frame.bgra, stride);
                let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
            }
            Surface::None => {}
        }
        if degraded {
            self.degrade_to_gdi();
        }

        // 更新干净帧（光标合成底图），否则光标一移动就会擦出视频帧之前的内容。
        // 行步长一致时直接把解码缓冲挪进 clean（省一次全屏拷贝），末尾补一行 padding。
        let rows = height as usize;
        let needed = clean_stride.saturating_mul(rows + 1);
        if stride == clean_stride {
            let mut bgra = frame.bgra;
            bgra.resize(needed, 0);
            self.clean = bgra;
        } else {
            if self.clean.len() < needed {
                self.clean.clear();
                self.clean.resize(needed, 0);
            }
            for row in 0..rows {
                let (Some(src_off), Some(dst_off)) =
                    (row.checked_mul(stride), row.checked_mul(clean_stride))
                else {
                    break;
                };
                let (Some(src_end), Some(dst_end)) =
                    (src_off.checked_add(clean_stride), dst_off.checked_add(clean_stride))
                else {
                    break;
                };
                let (Some(src), Some(dst)) = (
                    frame.bgra.get(src_off..src_end),
                    self.clean.get_mut(dst_off..dst_end),
                ) else {
                    break;
                };
                dst.copy_from_slice(src);
            }
        }

        // update_full 换掉了整张纹理：强制下一次光标合成把光标画到新帧上
        self.cursor.drawn = None;
        self.cursor.version = 0;
        self.need_present = true;
        log::trace!(
            "视频帧上屏 seq={} ts={} {}x{} stride={}",
            frame.seq,
            frame.ts_ms,
            width,
            height,
            stride
        );
    }

    /// 视频帧尺寸和当前呈现面不一致时重建（分辨率切换）。
    fn ensure_surface_for(&mut self, width: u32, height: u32) {
        if self.surface_size == Some((width, height)) {
            return;
        }
        log::info!("呈现面尺寸切换为 {width}x{height}");
        self.surface_size = Some((width, height));
        let stride = (width as usize).saturating_mul(4);
        self.clean.clear();
        self.clean.resize(
            stride.saturating_mul(height as usize + 1),
            0,
        );
        self.scratch.clear();
        self.cursor.reset();
        self.need_present = true;
        // Presenter 会在 update_full / resize 里自己 resize；GDI 回退的画布要换新的
        if let Surface::Gdi(canvas) = &self.surface {
            if (canvas.w, canvas.h) != (width, height) {
                self.surface = Surface::Gdi(SoftCanvas::new(width, height));
            }
        }
    }

    /// 从视频模式切回块模式：整屏重建 clean（清掉视频帧残留），并让光标重画。
    fn reset_clean_for_blocks(&mut self, damage: &mut Vec<Rect>) {
        let Some((width, height)) = self.surface_size else {
            return;
        };
        for byte in &mut self.clean {
            *byte = 0;
        }
        // 旧光标包围盒作废：refresh_cursor 会把它重画到重建后的画面上
        self.cursor.drawn = None;
        self.cursor.version = 0;
        if let Some(rect) = clipped_rect_pixels(
            0,
            0,
            width as i32,
            height as i32,
            (width, height),
        ) {
            damage.push(rect);
        }
        self.need_present = true;
        log::info!("切回块模式：整屏重建干净帧（{width}x{height}）");
    }

    /// 把一个解码块拷进干净帧，并把它的矩形记入本次要重绘的区域。
    fn apply_block_to_clean(&mut self, block: &BlockUpdate, damage: &mut Vec<Rect>) {
        let Some((screen_w, screen_h)) = self.surface_size else {
            return;
        };
        if block.x < 0 || block.y < 0 || block.w == 0 || block.h == 0 {
            return;
        }
        let Some(src_stride) = (block.w as usize).checked_mul(4) else {
            return;
        };
        let Some(dst_stride) = (screen_w as usize).checked_mul(4) else {
            return;
        };
        let bx = block.x as usize;
        let by = block.y as usize;
        if bx >= screen_w as usize || by >= screen_h as usize {
            return;
        }
        let copy_w = (block.w as usize).min(screen_w as usize - bx);
        let copy_h = (block.h as usize).min(screen_h as usize - by);
        for row in 0..copy_h {
            let Some(src_off) = row.checked_mul(src_stride) else {
                return;
            };
            let Some(dst_off) = (by + row)
                .checked_mul(dst_stride)
                .and_then(|v| v.checked_add(bx * 4))
            else {
                return;
            };
            let bytes = copy_w * 4;
            let (Some(src_end), Some(dst_end)) =
                (src_off.checked_add(bytes), dst_off.checked_add(bytes))
            else {
                return;
            };
            let (Some(src), Some(dst)) = (
                block.data.get(src_off..src_end),
                self.clean.get_mut(dst_off..dst_end),
            ) else {
                return;
            };
            dst.copy_from_slice(src);
        }
        if let Some(rect) = clipped_rect_pixels(
            block.x,
            block.y,
            copy_w as i32,
            copy_h as i32,
            (screen_w, screen_h),
        ) {
            damage.push(rect);
        }
    }

    /// 光标状态变了：把旧包围盒擦回干净帧，并把新包围盒纳入重绘。
    fn refresh_cursor(&mut self, damage: &mut Vec<Rect>) {
        let (snapshot, version) = self.shared.cursor_snapshot();
        if version == self.cursor.version {
            return;
        }
        self.cursor.version = version;

        let Some(screen) = self.surface_size else {
            // 呈现面还没建好：等 sync_surface 建好后会重置版本号再重画
            self.cursor.drawn = None;
            return;
        };
        if let Some(old) = self.cursor.drawn.take() {
            damage.push(old);
        }
        self.cursor.shape = snapshot.shape;
        self.cursor.pos = (snapshot.x, snapshot.y);
        self.cursor.visible = snapshot.visible;
        let bbox = self.cursor.bbox(screen);
        self.cursor.drawn = bbox;
        if let Some(rect) = bbox {
            damage.push(rect);
        }
        log::trace!(
            "光标 pos={:?} visible={} size={:?} bbox={bbox:?}",
            self.cursor.pos,
            self.cursor.visible,
            self.cursor.shape.as_ref().map(|s| (s.width, s.height))
        );
    }

    /// 把 damage 矩形逐块合成（干净帧 + 光标）并提交。
    ///
    /// 没有光标覆盖的矩形直接引用干净帧（零拷贝）；有覆盖的才拷进 scratch 混一次。
    fn render(&mut self, damage: Vec<Rect>) {
        if damage.is_empty() && !self.need_present {
            return;
        }
        self.need_present = false;

        let Some((screen_w, screen_h)) = self.surface_size else {
            return;
        };
        let Some(screen_stride) = (screen_w as usize).checked_mul(4) else {
            return;
        };
        if self.clean.len() < screen_stride.saturating_mul(screen_h as usize) {
            return;
        }

        let hwnd = self.hwnd;
        // 分字段借用：surface 要 &mut，其余是只读底图 / 光标状态 / 临时缓冲
        let clean = &self.clean[..];
        let cursor = &self.cursor;
        let scratch = &mut self.scratch;

        let mut degrade = false;
        match &mut self.surface {
            Surface::Gpu(presenter) => {
                let update = catch_unwind(AssertUnwindSafe(|| {
                    for rect in &damage {
                        let Some((data, stride)) =
                            compose_rect(clean, scratch, cursor, screen_stride, *rect)
                        else {
                            log::warn!("脏矩形无法合成: {rect:?}");
                            continue;
                        };
                        if let Err(e) =
                            presenter.update_block(rect.x, rect.y, rect.w, rect.h, data, stride)
                        {
                            log::warn!("update_block 失败 {rect:?}: {e}");
                        }
                    }
                    presenter.present()
                }));
                match update {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("present 失败: {e}");
                        degrade = true;
                    }
                    Err(_) => {
                        log::warn!("present panic");
                        degrade = true;
                    }
                }
            }
            Surface::Gdi(canvas) => {
                for rect in &damage {
                    let Some((data, stride)) =
                        compose_rect(clean, scratch, cursor, screen_stride, *rect)
                    else {
                        continue;
                    };
                    canvas.blit_rect(rect.x, rect.y, rect.w, rect.h, data, stride);
                }
                let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            }
            Surface::None => {
                if !damage.is_empty() {
                    log::debug!("呈现面未就绪，丢弃 {} 个脏矩形", damage.len());
                }
            }
        }

        if degrade {
            self.degrade_to_gdi();
        }
    }

    fn degrade_to_gdi(&mut self) {
        let Some((w, h)) = self.surface_size else {
            self.surface = Surface::None;
            return;
        };
        self.gpu_disabled = true;
        self.surface = Surface::Gdi(SoftCanvas::new(w, h));
        self.shared.request_full();
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn on_paint(&mut self, hwnd: HWND) {
        let client = self.client_size();
        let status = self.status_text.clone();
        match &mut self.surface {
            Surface::Gpu(_) => {
                // D3D 自己画；这里只要校验区域，另外借下一次定时器重画一帧。
                // 断线时画面保持最后一帧，连接状态看标题栏。
                self.need_present = true;
                let _ = unsafe { ValidateRect(Some(hwnd), None) };
            }
            Surface::Gdi(canvas) => unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    canvas.paint_hdc(hdc, client);
                    if let Some(text) = status.as_deref() {
                        draw_status_text(hdc, client, text);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
            },
            Surface::None => unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    fill_black(hdc, client);
                    if let Some(text) = status.as_deref() {
                        draw_status_text(hdc, client, text);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
            },
        }
    }

    fn on_size(&mut self) {
        self.need_present = true;
        if !matches!(self.surface, Surface::Gpu(_)) {
            let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
        }
    }

    // ---- 每秒：连接状态 + ack + stats ----

    /// 把 Shared 里的连接状态同步到窗口标题/内嵌提示。
    ///
    /// - 断线重连：标题 `lrc viewer — 重连中(第 N 次)`，画面保持最后一帧；
    /// - 认证失败：标题显示原因，窗口内叠加一行提示（不再弹 MessageBox 阻塞）。
    fn sync_status(&mut self) {
        let status = self.shared.status();
        let (desired, overlay) = match &status {
            ConnStatus::Terminal { message } => (
                format!("{} — {message}", self.base_title),
                Some(message.clone()),
            ),
            ConnStatus::Reconnecting { attempt } => (
                format!("{} — 重连中(第 {attempt} 次)", self.base_title),
                Some(format!("连接断开，重连中（第 {attempt} 次）…")),
            ),
            ConnStatus::Connected => (self.base_title.clone(), None),
        };
        if desired == self.title_applied {
            return;
        }
        self.title_applied = desired.clone();
        self.status_text = overlay;
        let wide = encode_wide(&desired);
        let _ = unsafe { SetWindowTextW(self.hwnd, PCWSTR::from_raw(wide.as_ptr())) };
        // GDI/无画面时能把提示画出来；D3D 模式靠标题栏（画面保持最后一帧）
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
        log::info!("窗口状态：{desired}");
    }

    fn on_tick(&mut self) {
        self.sync_status();
        self.print_stats();
        if self.shared.is_connected() {
            let ack = FrameAck {
                seq: self.shared.last_seq.load(Ordering::Relaxed),
                recv_bytes: self.shared.recv_bytes.load(Ordering::Relaxed),
            };
            self.shared.send(Message::VideoAck(ack).encode());
            // RTT：每秒一个 Ping，Pong 回来算最近一次往返（loopback 亚毫秒级）
            let ts = self.shared.begin_ping();
            self.shared.send(Message::Ping(Ping { ts_ms: ts }).encode());
        }
        if self.shared.take_want_full() {
            log::debug!("请求整帧");
            self.shared.send(Message::RequestFull.encode());
        }
    }

    fn print_stats(&mut self) {
        let now = Instant::now();
        let frames = self.shared.frame_count.load(Ordering::Relaxed);
        let bytes = self.shared.recv_bytes.load(Ordering::Relaxed);
        let blocks = self.shared.decoded_blocks.load(Ordering::Relaxed);
        let video_frames = self.shared.video_frames.load(Ordering::Relaxed);
        let mode = if self.shared.is_video_mode() {
            "video"
        } else {
            "blocks"
        };
        let dt = now.duration_since(self.stats.at).as_secs_f64();
        let dframes = frames.saturating_sub(self.stats.frames);
        let dbytes = bytes.saturating_sub(self.stats.bytes);
        let dvideo = video_frames.saturating_sub(self.stats.video_frames);
        // 最近一秒的管线延迟平均；这一秒没有视频帧就沿用上一次，避免抖成 0
        let lat_ms = match self.shared.take_latency_avg_ms() {
            Some(avg) => avg,
            None => self.stats.lat_ms,
        };
        self.stats = Stats {
            at: now,
            frames,
            bytes,
            video_frames,
            lat_ms,
        };
        if !self.stats_enabled || dt <= 0.0 {
            return;
        }
        let fps = dframes as f64 / dt;
        let kbps = dbytes as f64 * 8.0 / 1000.0 / dt;
        let video_fps = dvideo as f64 / dt;
        let rtt_ms = self.shared.rtt_micros() as f64 / 1000.0;
        // 每秒一行，便于重定向到文件后逐行看。
        // lat_ms 只在视频模式下、且两端用同一时钟源（GetTickCount64）时才是真延迟。
        println!(
            "fps={fps:.0} recv_kbps={kbps:.1} decoded_blocks={blocks} rtt_ms={rtt_ms:.3} \
mode={mode} video_fps={video_fps:.1} lat_ms={lat_ms:.1} reconnects={}",
            self.shared.reconnect_count()
        );
        let _ = std::io::stdout().flush();
    }

    /// 是否已经拿到远端光标位图（拿到后就不再画本地系统光标）。
    fn remote_cursor_active(&self) -> bool {
        self.cursor.shape.is_some()
    }

    // ---- 输入转发 ----

    fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rect) }.is_err() {
            return (0, 0);
        }
        let w = (rect.right - rect.left).max(0) as u32;
        let h = (rect.bottom - rect.top).max(0) as u32;
        (w, h)
    }

    fn remote_point(&self, cx: i32, cy: i32) -> Option<(i32, i32)> {
        let remote = self.shared.remote_size()?;
        Some(mapping::client_to_remote(
            cx,
            cy,
            self.client_size(),
            remote,
        ))
    }

    fn send_input(&self, event: InputEvent) {
        if !self.shared.is_connected() {
            log::debug!("未连接，输入事件仅本地记录: {event:?}");
            return;
        }
        log::debug!("input -> {event:?}");
        self.shared.send(Message::Input(event).encode());
    }

    fn on_mouse_move(&mut self, lparam: LPARAM) {
        let (cx, cy) = mapping::lparam_xy(lparam.0);
        // 抓了鼠标后 client 坐标可能是负的/超界的（拖出窗口），这里记录原始值便于排查
        log::debug!("WM_MOUSEMOVE client=({cx},{cy})");
        match self.remote_point(cx, cy) {
            Some((x, y)) => self.send_input(InputEvent::MouseMove { x, y }),
            // 还没收到 ServerHello（不知道被控端分辨率）时映射不了。
            None => log::debug!("WM_MOUSEMOVE 无法映射：被控端尺寸未知"),
        }
    }

    fn on_mouse_button(&mut self, button: u8, down: bool, lparam: LPARAM) {
        let (cx, cy) = mapping::lparam_xy(lparam.0);
        // 远控里"按住拖动到窗口外"太常见：按下时抓鼠标，松开时释放，
        // 否则 WndProc 收不到客户区外的 WM_MOUSEMOVE，拖动会断。
        if down {
            unsafe { SetCapture(self.hwnd) };
        } else {
            let _ = unsafe { ReleaseCapture() };
        }
        log::debug!("WM_*BUTTON client=({cx},{cy}) button={button} down={down} capture={down}");
        match self.remote_point(cx, cy) {
            Some((x, y)) => self.send_input(InputEvent::MouseButton { button, down, x, y }),
            None => log::debug!("WM_*BUTTON 无法映射：被控端尺寸未知"),
        }
    }

    fn on_wheel(&mut self, wparam: WPARAM, lparam: LPARAM, horizontal: bool) {
        let notches = mapping::wheel_notches(mapping::hiword_signed(wparam.0));
        if notches == 0 {
            return;
        }
        // WM_MOUSEWHEEL 的 lParam 是屏幕坐标，先转成客户区坐标
        let (sx, sy) = mapping::lparam_xy(lparam.0);
        let mut point = POINT { x: sx, y: sy };
        let _ = unsafe { ScreenToClient(self.hwnd, &mut point) };
        let Some((x, y)) = self.remote_point(point.x, point.y) else {
            log::debug!(
                "WM_*WHEEL notches={notches} screen=({sx},{sy}) client=({},{})，被控端尺寸未知",
                point.x,
                point.y
            );
            return;
        };
        let (dx, dy) = if horizontal {
            (notches, 0)
        } else {
            (0, notches)
        };
        self.send_input(InputEvent::Wheel { dx, dy, x, y });
    }

    fn on_key(&mut self, lparam: LPARAM, down: bool) {
        let event = mapping::key_from_lparam(lparam.0, down);
        self.send_input(event);
    }


}

/// 光标本地合成状态（只在窗口线程用）。
#[derive(Default)]
struct CursorRender {
    shape: Option<Arc<CursorShapeOwned>>,
    pos: (i32, i32),
    visible: bool,
    /// 已应用的 Shared 光标版本号（变了才重绘）
    version: u64,
    /// 当前已经画进呈现面的光标包围盒（被控端像素，已裁到屏幕内）
    drawn: Option<Rect>,
}

impl CursorRender {
    /// 呈现面重建后调用：光标需要重画到新面上。
    fn reset(&mut self) {
        self.drawn = None;
        self.version = 0;
    }

    /// 光标在**被控端屏幕**上占的矩形（按热点定位，裁到屏幕内）。
    ///
    /// 位图按被控端像素 1:1 画进 remote 分辨率的纹理，Presenter 把整张纹理
    /// 拉伸到客户区时，光标就和画面同比例缩放（等效于屏幕上按 client/remote 缩放），
    /// 不会出现"光标比画面大/小"的失真。
    fn bbox(&self, screen: (u32, u32)) -> Option<Rect> {
        if !self.visible {
            return None;
        }
        let shape = self.shape.as_ref()?;
        if shape.width == 0 || shape.height == 0 {
            return None;
        }
        let needed = (shape.width as usize)
            .checked_mul(shape.height as usize)
            .and_then(|n| n.checked_mul(4))?;
        if shape.bgra.len() < needed {
            return None;
        }
        let left = self.pos.0.checked_sub(shape.hotspot_x)?;
        let top = self.pos.1.checked_sub(shape.hotspot_y)?;
        clipped_rect_pixels(left, top, shape.width as i32, shape.height as i32, screen)
    }

    /// 把光标像素按 alpha 混合进 `dst`（`rect` 内的紧排 BGRA，行步长 `dst_stride`）。
    fn blend_into(&self, rect: &Rect, dst: &mut [u8], dst_stride: usize) {
        if !self.visible {
            return;
        }
        let Some(shape) = self.shape.as_ref() else {
            return;
        };
        let (Some(left), Some(top)) = (
            self.pos.0.checked_sub(shape.hotspot_x),
            self.pos.1.checked_sub(shape.hotspot_y),
        ) else {
            return;
        };
        let width = shape.width as i32;
        let height = shape.height as i32;
        if width <= 0 || height <= 0 {
            return;
        }
        let rect_right = rect.x.saturating_add(rect.w as i32);
        let rect_bottom = rect.y.saturating_add(rect.h as i32);
        for py in 0..height {
            let sy = top + py;
            if sy < rect.y || sy >= rect_bottom {
                continue;
            }
            for px in 0..width {
                let sx = left + px;
                if sx < rect.x || sx >= rect_right {
                    continue;
                }
                let Some(src_off) =
                    ((py as usize) * (width as usize) + px as usize).checked_mul(4)
                else {
                    continue;
                };
                let Some(src) = shape.bgra.get(src_off..src_off + 4) else {
                    continue;
                };
                let alpha = u32::from(src[3]);
                if alpha == 0 {
                    continue;
                }
                let dx = (sx - rect.x) as usize;
                let dy = (sy - rect.y) as usize;
                let Some(dst_off) = dy
                    .checked_mul(dst_stride)
                    .and_then(|v| v.checked_add(dx.checked_mul(4)?))
                else {
                    continue;
                };
                let Some(px_dst) = dst.get_mut(dst_off..dst_off + 4) else {
                    continue;
                };
                if alpha == 255 {
                    px_dst.copy_from_slice(src);
                } else {
                    // 直通 alpha 混合；输出 alpha 记 255（纹理不做 alpha 混合）
                    let inv = 255 - alpha;
                    for channel in 0..3 {
                        let fg = u32::from(src[channel]) * alpha;
                        let bg = u32::from(px_dst[channel]) * inv;
                        px_dst[channel] = ((fg + bg + 127) / 255) as u8;
                    }
                    px_dst[3] = 255;
                }
            }
        }
    }
}

/// 把矩形裁到屏幕内（`screen` = 被控端宽高）。
fn clipped_rect_pixels(x: i32, y: i32, w: i32, h: i32, screen: (u32, u32)) -> Option<Rect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let left = x.max(0);
    let top = y.max(0);
    let right = x.checked_add(w)?.min(screen.0 as i32);
    let bottom = y.checked_add(h)?.min(screen.1 as i32);
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        left,
        top,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// 取某个脏矩形的提交数据：无光标覆盖时直接返回干净帧切片（零拷贝），
/// 有覆盖时把干净帧 + 光标合成进 `scratch` 再返回。
fn compose_rect<'a>(
    clean: &'a [u8],
    scratch: &'a mut Vec<u8>,
    cursor: &CursorRender,
    screen_stride: usize,
    rect: Rect,
) -> Option<(&'a [u8], usize)> {
    if rect.w == 0 || rect.h == 0 || screen_stride == 0 {
        return None;
    }
    let row = (rect.w as usize).checked_mul(4)?;
    let cursor_hits = cursor
        .drawn
        .map(|bbox| !bbox.intersect(&rect).is_empty())
        .unwrap_or(false);
    let x = usize::try_from(rect.x).ok()?;
    let y = usize::try_from(rect.y).ok()?;
    if !cursor_hits {
        let offset = y
            .checked_mul(screen_stride)?
            .checked_add(x.checked_mul(4)?)?;
        // Presenter 要求 bgra.len() >= stride * h（哪怕最后一行只用 row 字节），
        // clean 末尾预留了一行 padding，所以这里可以整段给到 stride*h。
        let len = (rect.h as usize).checked_mul(screen_stride)?;
        let end = offset.checked_add(len)?;
        return Some((clean.get(offset..end)?, screen_stride));
    }
    let len = row.checked_mul(rect.h as usize)?;
    scratch.clear();
    scratch.resize(len, 0);
    for line in 0..rect.h as usize {
        let src_off = (y + line)
            .checked_mul(screen_stride)?
            .checked_add(x.checked_mul(4)?)?;
        let dst_off = line.checked_mul(row)?;
        let src_end = src_off.checked_add(row)?;
        let dst_end = dst_off.checked_add(row)?;
        let src = clean.get(src_off..src_end)?;
        let dst = scratch.get_mut(dst_off..dst_end)?;
        dst.copy_from_slice(src);
    }
    cursor.blend_into(&rect, scratch, row);
    Some((scratch.as_slice(), row))
}

/// 没有画面时的黑底。
unsafe fn fill_black(hdc: HDC, client: (u32, u32)) {
    let rect = RECT {
        left: 0,
        top: 0,
        right: client.0 as i32,
        bottom: client.1 as i32,
    };
    let brush = CreateSolidBrush(COLORREF(0));
    FillRect(hdc, &rect, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

/// 在客户区底部叠一条状态提示带（重连中 / 认证失败）。
///
/// D3D 模式下画面由 swapchain 覆盖，这条提示看不到（靠标题栏），
/// 无画面或 GDI 回退时能直接看到。
unsafe fn draw_status_text(hdc: HDC, client: (u32, u32), text: &str) {
    let bottom = client.1 as i32;
    let band_h = 34;
    let band = RECT {
        left: 0,
        top: (bottom - band_h).max(0),
        right: client.0 as i32,
        bottom,
    };
    let brush = CreateSolidBrush(COLORREF(0x0020_2020));
    FillRect(hdc, &band, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));

    let mut rect = RECT {
        left: 12,
        top: band.top,
        right: (band.right - 12).max(0),
        bottom,
    };
    let mut wide = encode_wide(text);
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(0x00D0_D0D0));
    let _ = DrawTextW(
        hdc,
        &mut wide[..],
        &mut rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
}

/// 取窗口关联的 AppState（WM_NCCREATE 时存进 GWLP_USERDATA）。
unsafe fn app_state<'a>(hwnd: HWND) -> Option<&'a mut AppState> {
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
    raw.as_mut()
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = (lparam.0 as *const CREATESTRUCTW).as_ref();
            if let Some(create) = create {
                let params = create.lpCreateParams as *mut AppState;
                if !params.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, params as isize);
                }
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_CREATE => {
            if let Some(app) = app_state(hwnd) {
                app.on_create(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            if let Some(app) = app_state(hwnd) {
                app.on_destroy();
            }
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }

    let Some(app) = app_state(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_PAINT => app.on_paint(hwnd),
        WM_SIZE => app.on_size(),
        WM_TIMER => match wparam.0 {
            TIMER_PRESENT => app.on_present_timer(),
            TIMER_TICK => app.on_tick(),
            _ => {}
        },
        // 画面/黑底都是自己画的，别让系统再刷一遍背景（D3D 下会闪）。
        WM_ERASEBKGND => return LRESULT(1),
        // 已经有远端光标位图时隐藏本地系统箭头（光标由我们自己画进画面，避免"双鼠标"）。
        WM_SETCURSOR => {
            if app.remote_cursor_active() {
                let _ = unsafe { SetCursor(None) };
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => app.on_key(lparam, true),
        WM_KEYUP | WM_SYSKEYUP => app.on_key(lparam, false),
        WM_MOUSEMOVE => app.on_mouse_move(lparam),
        WM_LBUTTONDOWN => app.on_mouse_button(mouse_button::LEFT, true, lparam),
        WM_LBUTTONUP => app.on_mouse_button(mouse_button::LEFT, false, lparam),
        WM_RBUTTONDOWN => app.on_mouse_button(mouse_button::RIGHT, true, lparam),
        WM_RBUTTONUP => app.on_mouse_button(mouse_button::RIGHT, false, lparam),
        WM_MBUTTONDOWN => app.on_mouse_button(mouse_button::MIDDLE, true, lparam),
        WM_MBUTTONUP => app.on_mouse_button(mouse_button::MIDDLE, false, lparam),
        WM_MOUSEWHEEL => app.on_wheel(wparam, lparam, false),
        WM_MOUSEHWHEEL => app.on_wheel(wparam, lparam, true),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect::new(x, y, w, h)
    }

    /// 2x2 光标：左上红、右上绿、左下蓝、右下全透明（BGRA）。
    fn shape_2x2() -> CursorShapeOwned {
        let mut bgra = vec![0u8; 16];
        bgra[0..4].copy_from_slice(&[0, 0, 255, 255]);
        bgra[4..8].copy_from_slice(&[0, 255, 0, 255]);
        bgra[8..12].copy_from_slice(&[255, 0, 0, 255]);
        bgra[12..16].copy_from_slice(&[0, 0, 0, 0]);
        CursorShapeOwned {
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra,
        }
    }

    fn cursor_at(x: i32, y: i32, hotspot: (i32, i32), visible: bool) -> CursorRender {
        let mut shape = shape_2x2();
        shape.hotspot_x = hotspot.0;
        shape.hotspot_y = hotspot.1;
        CursorRender {
            shape: Some(Arc::new(shape)),
            pos: (x, y),
            visible,
            version: 1,
            drawn: None,
        }
    }

    #[test]
    fn soft_canvas_blit_rect_clips_to_bounds() {
        let mut canvas = SoftCanvas::new(4, 2);
        // 4x2 的块放在 x=2：只该吃到右边 2 列
        let data = vec![7u8; 4 * 4 * 2];
        canvas.blit_rect(2, 0, 4, 2, &data, 4 * 4);
        assert_eq!(canvas.bgra.len(), 4 * 4 * 2);
        assert_eq!(&canvas.bgra[8..16], &[7u8; 8]);
        assert_eq!(&canvas.bgra[0..8], &[0u8; 8]);
        assert_eq!(&canvas.bgra[24..32], &[7u8; 8]);
    }

    #[test]
    fn soft_canvas_ignores_out_of_range_blits() {
        let mut canvas = SoftCanvas::new(4, 4);
        canvas.blit_rect(-2, 0, 4, 4, &[1u8; 64], 16);
        assert!(canvas.bgra.iter().all(|b| *b == 0));
        canvas.blit_rect(10, 10, 4, 4, &[1u8; 64], 16);
        assert!(canvas.bgra.iter().all(|b| *b == 0));
    }

    #[test]
    fn soft_canvas_handles_short_input() {
        let mut canvas = SoftCanvas::new(64, 64);
        // 数据远不够 64x64：不能 panic、也不能越界写
        canvas.blit_rect(0, 0, 64, 64, &[9u8; 8], 64 * 4);
        assert!(canvas.bgra.iter().all(|b| *b == 0));
    }

    #[test]
    fn cursor_bbox_uses_hotspot_and_clips() {
        let screen = (1920u32, 1080u32);
        assert_eq!(cursor_at(10, 20, (0, 0), true).bbox(screen), Some(rect(10, 20, 2, 2)));
        assert_eq!(cursor_at(10, 20, (1, 1), true).bbox(screen), Some(rect(9, 19, 2, 2)));
        // 贴右下角：裁成 1x1
        assert_eq!(
            cursor_at(1919, 1079, (0, 0), true).bbox(screen),
            Some(rect(1919, 1079, 1, 1))
        );
        // 完全在屏幕外 / 不可见 → 不画
        assert_eq!(cursor_at(5000, 5000, (0, 0), true).bbox(screen), None);
        assert_eq!(cursor_at(10, 20, (0, 0), false).bbox(screen), None);
    }

    #[test]
    fn cursor_blend_respects_alpha() {
        let cursor = cursor_at(0, 0, (0, 0), true);
        // 白底 2x2
        let mut dst = vec![255u8; 2 * 2 * 4];
        cursor.blend_into(&rect(0, 0, 2, 2), &mut dst, 8);
        assert_eq!(&dst[0..4], &[0, 0, 255, 255], "左上应变红");
        assert_eq!(&dst[4..8], &[0, 255, 0, 255], "右上应变绿");
        assert_eq!(&dst[8..12], &[255, 0, 0, 255], "左下应变蓝");
        assert_eq!(&dst[12..16], &[255, 255, 255, 255], "全透明像素保持底色");
    }

    #[test]
    fn cursor_blend_semi_transparent() {
        let mut shape = shape_2x2();
        // 让左上像素 alpha=128（红色），叠在白底上应得到 ~(255,127,127)
        shape.bgra[0..4].copy_from_slice(&[0, 0, 255, 128]);
        let cursor = CursorRender {
            shape: Some(Arc::new(shape)),
            pos: (0, 0),
            visible: true,
            version: 1,
            drawn: None,
        };
        let mut dst = vec![255u8; 2 * 2 * 4];
        cursor.blend_into(&rect(0, 0, 2, 2), &mut dst, 8);
        // 半透明红叠在白底上：R≈255，G/B≈127
        let (b, g, r) = (dst[0], dst[1], dst[2]);
        assert!((125..=130).contains(&b), "b={b}");
        assert!((125..=130).contains(&g), "g={g}");
        assert!(r >= 250, "r={r}");
    }

    #[test]
    fn compose_rect_without_cursor_is_zero_copy() {
        // 4x4 屏幕，每行 16B；干净帧填 0x11
        let clean = vec![0x11u8; 4 * 4 * 4];
        let mut scratch = Vec::new();
        let cursor = cursor_at(0, 0, (0, 0), false);
        let (len, stride, all_clean) = {
            // data 借着 scratch 的生命周期，先算完再放掉
            let (data, stride) = compose_rect(&clean, &mut scratch, &cursor, 16, rect(1, 1, 2, 2))
                .expect("compose");
            (data.len(), stride, data.iter().all(|b| *b == 0x11))
        };
        assert_eq!(stride, 16, "无光标时直接用干净帧的 stride");
        assert_eq!(len, 2 * 16, "两行 * stride（Presenter 要求 len >= stride*h）");
        assert!(scratch.is_empty(), "不该动用 scratch");
        assert!(all_clean);
    }

    #[test]
    fn compose_rect_with_cursor_blends() {
        // 干净帧全 0，光标画在 (0,0)
        let clean = vec![0u8; 4 * 4 * 4];
        let mut scratch = Vec::new();
        let mut cursor = cursor_at(0, 0, (0, 0), true);
        cursor.drawn = Some(rect(0, 0, 2, 2));
        let (data, stride) = compose_rect(&clean, &mut scratch, &cursor, 16, rect(0, 0, 2, 2))
            .expect("compose");
        assert_eq!(stride, 8, "有光标时用紧排 stride");
        assert_eq!(&data[0..4], &[0, 0, 255, 255]);
        assert_eq!(&data[4..8], &[0, 255, 0, 255]);
        assert_eq!(&data[8..12], &[255, 0, 0, 255]);
        // alpha=0 的像素不写，保留干净帧原值（这里干净帧是 0）
        assert_eq!(&data[12..16], &[0, 0, 0, 0], "全透明像素保留底色");
    }

    #[test]
    fn compose_rect_cursor_elsewhere_is_direct() {
        // 与真实 clean 一致：末尾多一行 padding
        let clean = vec![0u8; 4 * 4 * 4 + 16];
        let mut scratch = Vec::new();
        let mut cursor = cursor_at(0, 0, (0, 0), true);
        cursor.drawn = Some(rect(0, 0, 2, 2));
        // 光标在 (0,0)，别处的脏矩形应该零拷贝
        let (_, stride) =
            compose_rect(&clean, &mut scratch, &cursor, 16, rect(2, 2, 2, 2)).expect("compose");
        assert_eq!(stride, 16);
        assert!(scratch.is_empty());
    }

    #[test]
    fn clipped_rect_pixels_clips_to_screen() {
        assert_eq!(clipped_rect_pixels(0, 0, 4, 4, (4, 4)), Some(rect(0, 0, 4, 4)));
        assert_eq!(
            clipped_rect_pixels(3, 3, 4, 4, (4, 4)),
            Some(rect(3, 3, 1, 1))
        );
        assert_eq!(clipped_rect_pixels(-2, -2, 4, 4, (4, 4)), Some(rect(0, 0, 2, 2)));
        assert_eq!(clipped_rect_pixels(10, 10, 4, 4, (4, 4)), None);
        assert_eq!(clipped_rect_pixels(0, 0, 0, 0, (4, 4)), None);
    }
}
