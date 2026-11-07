//! 主控端窗口：Win32 窗口类 + 消息循环 + 呈现 + 输入转发。
//!
//! 线程模型：本模块全部代码跑在窗口线程（主线程）。接收线程只往
//! `Shared::pending` 里塞解好的脏块；8ms 定时器触发本线程 drain → `Presenter::update_block`
//! → `present()`（D3D11 设备非线程安全，呈现必须在这条线程）。

use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use lrc_proto::input::mouse_button;
use lrc_proto::{FrameAck, InputEvent, Message};
use lrc_render::Presenter;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromWindow, ScreenToClient, StretchDIBits, UpdateWindow, ValidateRect, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLOR_WINDOW, DIB_RGB_COLORS, HBRUSH, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, RGBQUAD, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW,
    MessageBoxW, PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWL_STYLE,
    GWLP_USERDATA, HICON, IDC_ARROW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MSG, SW_SHOWNORMAL,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SIZE,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::cli::Config;
use crate::mapping;
use crate::net;
use crate::state::{BlockUpdate, Shared, WM_APP_FAILURE};

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
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let mut app = Box::new(AppState::new(shared.clone(), out_tx, cfg));
    let app_ptr: *mut AppState = &mut *app;

    let hwnd = match create_window(app_ptr, cfg) {
        Ok(hwnd) => hwnd,
        Err(e) => {
            drop(app);
            return Err(e);
        }
    };

    // 接收线程：连接 + 握手 + 收流解码。hwnd 必须先写进 Shared，线程才能 PostMessage。
    shared.set_hwnd(hwnd.0 as isize);
    let net_tx = app.out_tx.clone();
    if let Err(e) = net::spawn(cfg, shared.clone(), net_tx, out_rx) {
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

    fn blit(&mut self, block: &BlockUpdate) {
        if block.x < 0 || block.y < 0 || block.w == 0 || block.h == 0 || self.w == 0 {
            return;
        }
        let Some(src_stride) = (block.w as usize).checked_mul(4) else {
            return;
        };
        let Some(dst_stride) = (self.w as usize).checked_mul(4) else {
            return;
        };
        let bx = block.x as usize;
        let by = block.y as usize;
        if bx >= self.w as usize || by >= self.h as usize {
            return;
        }
        let copy_w = (block.w as usize).min(self.w as usize - bx);
        let copy_h = (block.h as usize).min(self.h as usize - by);
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
            let (Some(src_end), Some(dst_end)) = (src_off.checked_add(bytes), dst_off.checked_add(bytes))
            else {
                return;
            };
            if src_end > block.data.len() || dst_end > self.bgra.len() {
                return;
            }
            self.bgra[dst_off..dst_end].copy_from_slice(&block.data[src_off..src_end]);
        }
    }

    /// WM_PAINT：黑底 + 拉伸铺满客户区（与 D3D 的 `DXGI_SCALING_STRETCH` 口径一致）。
    unsafe fn paint(&self, hwnd: HWND, client: (u32, u32)) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
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
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 窗口线程专属状态（`Presenter` 非 Send，必须留在本线程）。
struct AppState {
    hwnd: HWND,
    shared: Arc<Shared>,
    out_tx: Sender<Vec<u8>>,
    title: String,
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
    stats: Stats,
}

struct Stats {
    at: Instant,
    frames: u64,
    bytes: u64,
}

impl Stats {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            frames: 0,
            bytes: 0,
        }
    }
}

impl AppState {
    fn new(shared: Arc<Shared>, out_tx: Sender<Vec<u8>>, cfg: &Config) -> Self {
        Self {
            hwnd: HWND(std::ptr::null_mut()),
            shared,
            out_tx,
            title: cfg.title.clone(),
            stats_enabled: cfg.stats,
            surface: Surface::None,
            surface_size: None,
            need_present: false,
            gpu_disabled: false,
            aspect_fixed: false,
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
        let blocks = self.shared.take_blocks();
        self.render(blocks);
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

    /// 把脏块推给呈现面并提交一帧。
    fn render(&mut self, blocks: Vec<BlockUpdate>) {
        if blocks.is_empty() && !self.need_present {
            return;
        }
        self.need_present = false;

        let mut degrade = false;
        match &mut self.surface {
            Surface::Gpu(presenter) => {
                let drawn = catch_unwind(AssertUnwindSafe(|| {
                    for block in &blocks {
                        let stride = (block.w as usize).saturating_mul(4);
                        if let Err(e) =
                            presenter.update_block(block.x, block.y, block.w, block.h, &block.data, stride)
                        {
                            log::warn!(
                                "update_block 失败 ({},{},{:?}): {e}",
                                block.x,
                                block.y,
                                (block.w, block.h)
                            );
                        }
                    }
                    presenter.present()
                }));
                match drawn {
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
                for block in &blocks {
                    canvas.blit(block);
                }
                let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
            }
            Surface::None => {
                if !blocks.is_empty() {
                    log::debug!("呈现面未就绪，丢弃 {} 个脏块", blocks.len());
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
        match &mut self.surface {
            Surface::Gpu(_) => {
                // D3D 自己画；这里只要校验区域，另外借下一次定时器重画一帧。
                self.need_present = true;
                let _ = unsafe { ValidateRect(Some(hwnd), None) };
            }
            Surface::Gdi(canvas) => unsafe { canvas.paint(hwnd, client) },
            Surface::None => paint_black(hwnd, client),
        }
    }

    fn on_size(&mut self) {
        self.need_present = true;
        if !matches!(self.surface, Surface::Gpu(_)) {
            let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
        }
    }

    // ---- 每秒：ack + stats ----

    fn on_tick(&mut self) {
        self.print_stats();
        if self.shared.is_connected() {
            let ack = FrameAck {
                seq: self.shared.last_seq.load(Ordering::Relaxed),
                recv_bytes: self.shared.recv_bytes.load(Ordering::Relaxed),
            };
            let _ = self.out_tx.send(Message::VideoAck(ack).encode());
        }
        if self.shared.take_want_full() {
            log::debug!("请求整帧");
            let _ = self.out_tx.send(Message::RequestFull.encode());
        }
    }

    fn print_stats(&mut self) {
        let now = Instant::now();
        let frames = self.shared.frame_count.load(Ordering::Relaxed);
        let bytes = self.shared.recv_bytes.load(Ordering::Relaxed);
        let blocks = self.shared.decoded_blocks.load(Ordering::Relaxed);
        let dt = now.duration_since(self.stats.at).as_secs_f64();
        let dframes = frames.saturating_sub(self.stats.frames);
        let dbytes = bytes.saturating_sub(self.stats.bytes);
        self.stats = Stats {
            at: now,
            frames,
            bytes,
        };
        if !self.stats_enabled || dt <= 0.0 {
            return;
        }
        let fps = dframes as f64 / dt;
        let kbps = dbytes as f64 * 8.0 / 1000.0 / dt;
        // 每秒一行，便于重定向到文件后逐行看
        println!("fps={fps:.0} recv_kbps={kbps:.1} decoded_blocks={blocks}");
        let _ = std::io::stdout().flush();
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
        let _ = self.out_tx.send(Message::Input(event).encode());
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

    // ---- 致命错误：弹框后退出 ----

    fn on_failure(&mut self) {
        let Some(message) = self.shared.take_failure() else {
            return;
        };
        log::error!("{message}");
        let text = encode_wide(&message);
        let title = encode_wide(&self.title);
        unsafe {
            MessageBoxW(
                Some(self.hwnd),
                PCWSTR::from_raw(text.as_ptr()),
                PCWSTR::from_raw(title.as_ptr()),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// 没有画面时的黑底。
fn paint_black(hwnd: HWND, client: (u32, u32)) {
    let mut ps = PAINTSTRUCT::default();
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let rect = RECT {
            left: 0,
            top: 0,
            right: client.0 as i32,
            bottom: client.1 as i32,
        };
        let brush = CreateSolidBrush(COLORREF(0));
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = EndPaint(hwnd, &ps);
    }
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
        WM_APP_FAILURE => app.on_failure(),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_canvas_blit_clips_to_bounds() {
        let mut canvas = SoftCanvas::new(4, 2);
        let block = BlockUpdate {
            x: 2,
            y: 0,
            w: 4,
            h: 2,
            data: vec![7u8; 4 * 4 * 2],
        };
        canvas.blit(&block);
        // 只填了最右边 2 列
        assert_eq!(canvas.bgra.len(), 4 * 4 * 2);
        assert_eq!(&canvas.bgra[8..16], &[7u8; 8]);
        assert_eq!(&canvas.bgra[0..8], &[0u8; 8]);
        assert_eq!(&canvas.bgra[24..32], &[7u8; 8]);
    }

    #[test]
    fn soft_canvas_ignores_out_of_range_blocks() {
        let mut canvas = SoftCanvas::new(4, 4);
        let block = BlockUpdate {
            x: -2,
            y: 0,
            w: 4,
            h: 4,
            data: vec![1u8; 4 * 4 * 4],
        };
        canvas.blit(&block);
        assert!(canvas.bgra.iter().all(|b| *b == 0));

        let block = BlockUpdate {
            x: 10,
            y: 10,
            w: 4,
            h: 4,
            data: vec![1u8; 4 * 4 * 4],
        };
        canvas.blit(&block);
        assert!(canvas.bgra.iter().all(|b| *b == 0));
    }

    #[test]
    fn soft_canvas_handles_short_input() {
        let mut canvas = SoftCanvas::new(64, 64);
        let block = BlockUpdate {
            x: 0,
            y: 0,
            w: 64,
            h: 64,
            data: vec![9u8; 8], // 数据不够，不能 panic
        };
        canvas.blit(&block);
        assert!(canvas.bgra.iter().all(|b| *b == 0));
    }
}
