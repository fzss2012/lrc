//! Windows 屏幕采集（DXGI Desktop Duplication，GDI 兜底）。
//!
//! 契约见 `docs/DESIGN.md` §4。**接口签名是三方共享契约**，改动先跟 Lead 对齐。

use std::fmt;
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};

pub mod cursor;

pub use cursor::{CursorShape, CursorState, CursorWatcher, CursorWatcherStats};
pub use lrc_proto::Rect;

/// dirty 面积超过屏幕的这个百分比就当成整帧。
const FULL_DIRTY_RATIO_PCT: u64 = 60;
/// `AccumulatedFrames` 超过这个值就当成整帧（DXGI 自身的建议阈值）。
const FULL_ACCUMULATED_FRAMES: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    /// 0/90/180/270，仅桌面朝向，暂不做旋转补偿
    pub rotation: u16,
    /// 该输出在**虚拟桌面**中的左上角（物理像素，即 DXGI_OUTPUT_DESC.DesktopCoordinates.left/top）。
    /// 输入注入要用它把"采集画面本地坐标"换算成虚拟桌面绝对坐标。
    pub origin_x: i32,
    pub origin_y: i32,
}

impl ScreenInfo {
    pub fn is_portrait(&self) -> bool {
        self.width < self.height
    }
}

/// DXGI move rect（滚动/窗口拖动时出现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRect {
    pub src: Rect,
    pub dst: Rect,
}

/// 上一帧的 DXGI 原始诊断信息（probe / 调优用，不影响主流程）。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDebug {
    pub accumulated_frames: u32,
    pub total_metadata_bytes: u32,
    pub pointer_shape_bytes: u32,
    pub rects_coalesced: bool,
    pub protected_content: bool,
    /// DXGI_OUTDUPL_DESC.DesktopImageInSystemMemory：TRUE 说明桌面已在系统内存，
    /// MapDesktopSurface 可走零拷贝快路（否则只能用 staging 回读）。
    pub desktop_in_system_memory: bool,
    /// Map 出来的行距（字节）
    pub row_pitch: u32,
    /// 本帧是否走了整帧回读（false = 只回读了脏区并集）
    pub readback_full: bool,
    /// 本帧实际回读的像素数（整帧或脏区并集）
    pub readback_px: u64,
    /// 累计：「有元数据但解析不出 dirty/move 矩形」而被迫整帧的帧数（正常应恒为 0，
    /// 一旦非 0 说明线上某个驱动在丢元数据，需要换 GDI 兜底或放宽策略）
    pub no_rect_frames: u64,
    /// 各阶段耗时（微秒）：查元数据 / CopyResource / Map / 逐行拷贝 / Unmap
    pub meta_us: u64,
    pub copy_us: u64,
    pub map_us: u64,
    pub blit_us: u64,
    pub unmap_us: u64,
}

/// 一帧采集结果，BGRA8 紧凑行（`stride >= width*4`）。
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bgra: Vec<u8>,
    /// 变化矩形（可能为空，也可能覆盖全屏）
    pub dirty: Vec<Rect>,
    pub moves: Vec<MoveRect>,
    /// 是否整帧（首帧、或 dirty 面积过大被判为整帧）
    pub full: bool,
    /// 单调毫秒时间戳（采集完成时刻）
    pub ts_ms: u64,
}

#[derive(Debug)]
pub enum CaptureError {
    /// 初始化失败（找不到输出/设备）
    Init(String),
    /// 采集期错误（可恢复的调用方应重试）
    Runtime(String),
    /// Access Lost，需要重建 Capturer
    Lost,
    /// 平台不支持
    Unsupported(&'static str),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Init(m) => write!(f, "capture: 初始化失败 {m}"),
            CaptureError::Runtime(m) => write!(f, "capture: 运行期错误 {m}"),
            CaptureError::Lost => write!(f, "capture: 访问丢失，需重建"),
            CaptureError::Unsupported(m) => write!(f, "capture: 不支持 {m}"),
        }
    }
}

impl std::error::Error for CaptureError {}

pub type CaptureResult<T> = Result<T, CaptureError>;

/// windows 错误 → `0x887A0027 (msg)`，方便直接贴给 Lead 定位。
fn hr_text(e: &windows::core::Error) -> String {
    format!("0x{:08X} ({e})", e.code().0 as u32)
}

fn rotation_deg(r: DXGI_MODE_ROTATION) -> u16 {
    match r.0 {
        2 => 90,  // DXGI_MODE_ROTATION_ROTATE90
        3 => 180, // DXGI_MODE_ROTATION_ROTATE180
        4 => 270, // DXGI_MODE_ROTATION_ROTATE270
        _ => 0,   // IDENTITY / UNSPECIFIED
    }
}

/// `DXGI_OUTPUT_DESC` → `ScreenInfo`；无桌面面积（未挂载的虚显）返回 `None`。
fn screen_info_from_desc(
    desc: &windows::Win32::Graphics::Dxgi::DXGI_OUTPUT_DESC,
) -> Option<ScreenInfo> {
    let c = desc.DesktopCoordinates;
    let w = (c.right - c.left).max(0) as u32;
    let h = (c.bottom - c.top).max(0) as u32;
    if w == 0 || h == 0 {
        return None;
    }
    Some(ScreenInfo {
        width: w,
        height: h,
        rotation: rotation_deg(desc.Rotation),
        origin_x: c.left,
        origin_y: c.top,
    })
}

/// 枚举顺序即 `Capturer::new` 的 `output_index`：两处共用同一套过滤规则，不能各写一份。
fn enabled_outputs(factory: &IDXGIFactory1) -> CaptureResult<Vec<(IDXGIOutput, ScreenInfo)>> {
    let mut out = Vec::new();
    let mut ai = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => {
                return Err(CaptureError::Init(format!(
                    "EnumAdapters1({ai}): {}",
                    hr_text(&e)
                )))
            }
        };
        let mut oi = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => {
                    return Err(CaptureError::Init(format!(
                        "EnumOutputs(adapter={ai}, output={oi}): {}",
                        hr_text(&e)
                    )))
                }
            };
            let desc = unsafe { output.GetDesc() }
                .map_err(|e| CaptureError::Init(format!("IDXGIOutput::GetDesc: {}", hr_text(&e))))?;
            if let Some(info) = screen_info_from_desc(&desc) {
                out.push((output, info));
            }
            oi += 1;
        }
        ai += 1;
    }
    Ok(out)
}

/// 枚举可用输出（序号即 `Capturer::new` 的 `output_index`）。
pub fn list_outputs() -> CaptureResult<Vec<ScreenInfo>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| CaptureError::Init(format!("CreateDXGIFactory1: {}", hr_text(&e))))?;
    Ok(enabled_outputs(&factory)?
        .into_iter()
        .map(|(_, i)| i)
        .collect())
}

/// 一块 CPU 可读的 staging 纹理，DXGI 回读的终点。
fn create_staging(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> CaptureResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(1),
        Height: h.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex as *mut _)) }
        .map_err(|e| CaptureError::Runtime(format!("CreateTexture2D(staging): {}", hr_text(&e))))?;
    tex.ok_or_else(|| CaptureError::Runtime("CreateTexture2D(staging) 返回空".into()))
}

/// 把 RECT 裁到屏幕内并转成 `lrc_proto::Rect`；空矩形返回 `None`。
fn clamp_rect(l: i32, t: i32, r: i32, b: i32, sw: u32, sh: u32) -> Option<Rect> {
    let max_w = sw.min(i32::MAX as u32) as i32;
    let max_h = sh.min(i32::MAX as u32) as i32;
    let l = l.clamp(0, max_w);
    let t = t.clamp(0, max_h);
    let r = r.clamp(0, max_w);
    let b = b.clamp(0, max_h);
    let w = (r - l).max(0) as u32;
    let h = (b - t).max(0) as u32;
    if w == 0 || h == 0 {
        None
    } else {
        Some(Rect::new(l, t, w, h))
    }
}

/// 回读 staging 的尺寸分桶：向上取到 2 的幂，减少重建次数。
fn bucket_size(v: u32) -> u32 {
    v.max(1).next_power_of_two().min(16384)
}

/// 一个采集器绑定一路输出。
pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    info: ScreenInfo,
    timeout_ms: u32,
    first_frame: bool,
    tex_w: u32,
    tex_h: u32,
    tex_format: DXGI_FORMAT,
    desktop_in_system_memory: bool,
    /// 常驻全量底图（局部回读时只贴变化区，Frame 返回的是它的快照）。
    ///
    /// TODO(M2): 每次 grab 返回 Frame 都要 clone 一次全屏（1080p = 8 MB memcpy，
    /// 30fps 就有 240 MB/s 纯拷贝）。M2 换成双缓冲（mem::replace + 调用方 drop 后归还），
    /// 或给 Frame 增加"只带脏区像素"的增量视图，彻底免掉全屏搬运。
    full_buf: Vec<u8>,
    /// no_rect_frames 的累计值
    no_rect_count: u64,
    /// 脏区回读用的 staging（尺寸 = 脏区并集，按需重建）
    region_tex: Option<ID3D11Texture2D>,
    region_w: u32,
    region_h: u32,
    debug: FrameDebug,
}

impl Capturer {
    /// `timeout_ms`：`grab()` 内部等待新帧的上限；0 表示非阻塞。
    pub fn new(output_index: usize, timeout_ms: u32) -> CaptureResult<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| CaptureError::Init(format!("CreateDXGIFactory1: {}", hr_text(&e))))?;
        let mut outputs = enabled_outputs(&factory)?;
        if outputs.is_empty() {
            return Err(CaptureError::Init("没有可用的显示输出".into()));
        }
        if output_index >= outputs.len() {
            return Err(CaptureError::Init(format!(
                "output_index={output_index} 越界，只有 {} 个输出",
                outputs.len()
            )));
        }
        let (output, info) = outputs.swap_remove(output_index);

        // 1) 找到该 output 所属 adapter（DuplicateOutput 要求 device 与 output 同一 adapter）
        let adapter =
            unsafe { output.GetParent::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>() }.map_err(
                |e| CaptureError::Init(format!("IDXGIOutput::GetParent(IDXGIAdapter): {}", hr_text(&e))),
            )?;

        // 2) 建 device
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature = D3D_FEATURE_LEVEL_11_0;
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device as *mut _),
                Some(&mut feature as *mut _),
                Some(&mut context as *mut _),
            )
        }
        .map_err(|e| CaptureError::Init(format!("D3D11CreateDevice: {}", hr_text(&e))))?;
        let device = device
            .ok_or_else(|| CaptureError::Init("D3D11CreateDevice 未返回 ID3D11Device".into()))?;
        let context = context.ok_or_else(|| {
            CaptureError::Init("D3D11CreateDevice 未返回 ID3D11DeviceContext".into())
        })?;

        // 3) DuplicateOutput
        let output1: IDXGIOutput1 = output
            .cast()
            .map_err(|e| CaptureError::Init(format!("QI IDXGIOutput1: {}", hr_text(&e))))?;
        let dupl = unsafe { output1.DuplicateOutput(&device) }.map_err(|e| {
            CaptureError::Init(format!(
                "DuplicateOutput: {} [0x887A0022=NOT_CURRENTLY_AVAILABLE 已有采集器/会话不支持, 0x887A0004=UNSUPPORTED]",
                hr_text(&e)
            ))
        })?;

        // 4) staging
        let ddesc = unsafe { dupl.GetDesc() };
        let tex_w = ddesc.ModeDesc.Width.max(1);
        let tex_h = ddesc.ModeDesc.Height.max(1);
        let tex_format = if ddesc.ModeDesc.Format == DXGI_FORMAT(0) {
            DXGI_FORMAT_B8G8R8A8_UNORM
        } else {
            ddesc.ModeDesc.Format
        };
        let desktop_in_system_memory = ddesc.DesktopImageInSystemMemory.as_bool();
        let staging = create_staging(&device, tex_w, tex_h, tex_format)?;

        log::debug!(
            "capture: output{output_index} {}x{} origin=({}, {}) rot={} staging={}x{} fmt={}",
            info.width,
            info.height,
            info.origin_x,
            info.origin_y,
            info.rotation,
            tex_w,
            tex_h,
            tex_format.0
        );

        Ok(Self {
            device,
            context,
            dupl,
            staging,
            info,
            timeout_ms,
            first_frame: true,
            tex_w,
            tex_h,
            tex_format,
            desktop_in_system_memory,
            full_buf: vec![0u8; tex_w as usize * 4 * tex_h as usize],
            region_tex: None,
            region_w: 0,
            region_h: 0,
            no_rect_count: 0,
            debug: FrameDebug::default(),
        })
    }

    pub fn info(&self) -> ScreenInfo {
        self.info
    }

    /// 上一帧的 DXGI 诊断信息（probe / 调优用）。
    pub fn last_debug(&self) -> FrameDebug {
        self.debug
    }

    /// 桌面纹理尺寸是否变了（分辨率 / 模式切换）。这是唯一不看元数据的"必须整帧"例外。
    /// 资源为空（纯鼠标更新）时无法判断，按未变处理。
    fn desktop_dims_changed(&self, resource: Option<&IDXGIResource>, is_first: bool) -> bool {
        if is_first {
            return true;
        }
        let Some(res) = resource else {
            return false;
        };
        let Ok(texture) = res.cast::<ID3D11Texture2D>() else {
            return false;
        };
        let mut td = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut td) };
        td.Width != self.tex_w || td.Height != self.tex_h
    }

    /// 取下一帧。没有新帧时返回 `Ok(None)`（**这是"零变化零发送"的源头**）。
    pub fn grab(&mut self) -> CaptureResult<Option<Frame>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            self.dupl
                .AcquireNextFrame(self.timeout_ms, &mut info, &mut resource as *mut _)
        };
        if let Err(e) = acquired {
            let code = e.code();
            if code == DXGI_ERROR_WAIT_TIMEOUT {
                // 「零变化零发送」的关键：超时不是错误。
                return Ok(None);
            }
            if code == DXGI_ERROR_ACCESS_LOST {
                return Err(CaptureError::Lost);
            }
            return Err(CaptureError::Runtime(format!(
                "AcquireNextFrame: {}",
                hr_text(&e)
            )));
        }

        let built = self.build_frame(&info, resource);
        // 无论成功与否都必须 ReleaseFrame，否则下一次 AcquireNextFrame 直接失败。
        match unsafe { self.dupl.ReleaseFrame() } {
            Ok(()) => built,
            Err(e) => {
                log::warn!("capture: ReleaseFrame 失败: {}", hr_text(&e));
                match built {
                    Ok(f) => Ok(f),
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn build_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> CaptureResult<Option<Frame>> {
        let is_first = self.first_frame;
        let sw = self.info.width;
        let sh = self.info.height;
        self.debug = FrameDebug {
            accumulated_frames: info.AccumulatedFrames,
            total_metadata_bytes: info.TotalMetadataBufferSize,
            pointer_shape_bytes: info.PointerShapeBufferSize,
            rects_coalesced: info.RectsCoalesced.as_bool(),
            protected_content: info.ProtectedContentMaskedOut.as_bool(),
            desktop_in_system_memory: self.desktop_in_system_memory,
            ..FrameDebug::default()
        };

        // 分辨率/模式变化是不看元数据的例外：这时哪怕没有 dirty rect 也必须整帧重建。
        let dims_changed = self.desktop_dims_changed(resource.as_ref(), is_first);

        // 【零变化零发送的核心】DXGI 的 TotalMetadataBufferSize 覆盖 dirty/move/pointer 全部可见元数据，
        // 非首帧为 0 就说明「presented 但没有任何可见变化」→ 直接丢，连 CopyResource 都不做。
        if !is_first && info.TotalMetadataBufferSize == 0 && !dims_changed {
            return Ok(None);
        }

        let t_meta = Instant::now();
        let meta_present = info.TotalMetadataBufferSize > 0;
        let (dirty, moves) = if meta_present {
            (
                self.query_dirty_rects(sw, sh, info.TotalMetadataBufferSize)?,
                self.query_move_rects(sw, sh, info.TotalMetadataBufferSize)?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        self.debug.meta_us = t_meta.elapsed().as_micros() as u64;

        // AccumulatedFrames == 0 且没有矩形 ⇒ 这一帧只更新了鼠标/元数据，桌面像素没变。
        if !is_first && info.AccumulatedFrames == 0 && dirty.is_empty() && moves.is_empty() && !dims_changed
        {
            return Ok(None);
        }

        // 「有元数据却解析不出任何矩形」是异常分支：保守走整帧，但要留痕（否则线上会永远发整屏）。
        let no_rects = meta_present
            && !is_first
            && !dims_changed
            && dirty.is_empty()
            && moves.is_empty();
        if no_rects {
            self.no_rect_count = self.no_rect_count.saturating_add(1);
            log::warn!(
                "capture: 元数据 {} 字节但解析出 0 个 dirty/move 矩形（acc={}），保守整帧，累计 {} 次",
                info.TotalMetadataBufferSize,
                info.AccumulatedFrames,
                self.no_rect_count
            );
        }
        self.debug.no_rect_frames = self.no_rect_count;

        let resource = resource
            .ok_or_else(|| CaptureError::Runtime("AcquireNextFrame 未返回桌面资源".into()))?;
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::Runtime(format!("QI ID3D11Texture2D: {}", hr_text(&e))))?;

        // 分辨率/格式可能运行期变化（显示器切换、动态缩放），staging 要跟着重建。
        let mut tdesc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut tdesc) };
        if tdesc.Width != self.tex_w || tdesc.Height != self.tex_h || tdesc.Format != self.tex_format
        {
            log::info!(
                "capture: 桌面纹理变化 {}x{} fmt={} → {}x{} fmt={}",
                self.tex_w,
                self.tex_h,
                self.tex_format.0,
                tdesc.Width,
                tdesc.Height,
                tdesc.Format.0
            );
            self.tex_w = tdesc.Width.max(1);
            self.tex_h = tdesc.Height.max(1);
            self.tex_format = tdesc.Format;
            self.staging = create_staging(&self.device, self.tex_w, self.tex_h, self.tex_format)?;
            self.full_buf = vec![0u8; self.tex_w as usize * 4 * self.tex_h as usize];
            self.region_tex = None;
            self.region_w = 0;
            self.region_h = 0;
        }

        let width = self.tex_w;
        let height = self.tex_h;
        let stride = (width as usize).saturating_mul(4);

        // 判定整帧：首帧 / 分辨率变化 / AccumulatedFrames 过大 / 脏区面积超阈值 / 元数据异常。
        let screen_area = u64::from(width) * u64::from(height);
        let dirty_area: u64 = dirty.iter().map(|r| r.area()).sum();
        let full = is_first
            || dims_changed
            || info.AccumulatedFrames > FULL_ACCUMULATED_FRAMES
            || no_rects
            || (screen_area > 0
                && dirty_area.saturating_mul(100) > screen_area * FULL_DIRTY_RATIO_PCT);

        // 回读区域 = dirty ∪ move.src ∪ move.dst（move 的源和目标都变了，必须一起读。
        // 少读会拿到过期像素，多读只是浪费带宽。）move.src 可能带负坐标，统一裁到屏幕内。
        let screen_rect = Rect::new(0, 0, sw, sh);
        let mut regions: Vec<Rect> = Vec::with_capacity(dirty.len() + moves.len() * 2);
        regions.extend_from_slice(&dirty);
        for m in &moves {
            regions.push(m.src.intersect(&screen_rect));
            regions.push(m.dst.intersect(&screen_rect));
        }
        regions.retain(|r| !r.is_empty());
        let union = regions.iter().fold(Rect::default(), |acc, r| acc.union(r));

        // 整帧要走全尺寸 staging；局部帧只回读脏区并集，省掉大量 PCIe 回读。
        let use_full = full || union.is_empty();
        let union_px = union.area();

        self.debug.readback_full = use_full;
        self.debug.readback_px = if use_full { screen_area } else { union_px };

        if use_full {
            let t_copy = Instant::now();
            unsafe { self.context.CopyResource(&self.staging, &texture) };
            self.debug.copy_us = t_copy.elapsed().as_micros() as u64;

            let t_map = Instant::now();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped as *mut _))
            }
            .map_err(|e| CaptureError::Runtime(format!("Map(staging): {}", hr_text(&e))))?;
            self.debug.map_us = t_map.elapsed().as_micros() as u64;

            let t_blit = Instant::now();
            let blit_result = self.blit_rows_into(mapped, 0, 0, width, height, stride, 0, 0);
            self.debug.blit_us = t_blit.elapsed().as_micros() as u64;

            let t_unmap = Instant::now();
            unsafe { self.context.Unmap(&self.staging, 0) };
            self.debug.unmap_us = t_unmap.elapsed().as_micros() as u64;
            blit_result?;
        } else {
            self.read_region(&texture, &union, &regions, stride)?;
        }

        // Frame 拥有自己的 BGRA：调用方随时可以扔，我们保留一份全量底图做局部增量。
        let bgra = self.full_buf.clone();

        self.first_frame = false;
        Ok(Some(Frame {
            width,
            height,
            stride,
            bgra,
            dirty,
            moves,
            full: use_full,
            ts_ms: now_ms(),
        }))
    }

    /// 把 staging 映射区的一块（sx,sy,w,h）拷进常驻全量底图。
    fn blit_rows_into(
        &mut self,
        mapped: D3D11_MAPPED_SUBRESOURCE,
        sx: u32,
        sy: u32,
        w: u32,
        h: u32,
        stride: usize,
        dx: i32,
        dy: i32,
    ) -> CaptureResult<()> {
        if mapped.pData.is_null() {
            return Err(CaptureError::Runtime("Map 返回空指针".into()));
        }
        if w == 0 || h == 0 {
            return Ok(());
        }
        let dst_x = dx.max(0) as usize;
        let dst_y = dy.max(0) as usize;
        let w = (w as usize).min(stride / 4);
        let pitch = mapped.RowPitch as usize;
        let row_bytes = w * 4;
        if pitch < row_bytes {
            return Err(CaptureError::Runtime(format!(
                "Map RowPitch={pitch} 小于行字节 {row_bytes}"
            )));
        }
        let src_len = pitch.saturating_mul((sy as usize).saturating_add(h as usize));
        // SAFETY: Map 成功且非空；行数由纹理高度保证。
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, src_len) };
        for y in 0..h as usize {
            let s = (sy as usize + y) * pitch + sx as usize * 4;
            let d = (dst_y + y) * stride + dst_x * 4;
            let (Some(s), Some(d)) = (
                src.get(s..s + row_bytes),
                self.full_buf.get_mut(d..d + row_bytes),
            ) else {
                return Err(CaptureError::Runtime("回读拷贝越界".into()));
            };
            d.copy_from_slice(s);
        }
        Ok(())
    }

    /// 局部回读：把 dirty/move 区域从桌面纹理拷进一块「脏区并集」大小的 staging，
    /// 一次 Map 后在 CPU 侧贴回常驻底图。回读量 = 脏区面积，而不是全屏。
    fn read_region(
        &mut self,
        texture: &ID3D11Texture2D,
        union: &Rect,
        regions: &[Rect],
        stride: usize,
    ) -> CaptureResult<()> {
        let uw = union.w;
        let uh = union.h;
        if uw == 0 || uh == 0 {
            return Ok(());
        }
        // 尺寸按 2 的幂分桶，避免脏区大小每帧抖动导致频繁重建 staging（重建 ~100us 且会丢显存块）
        let bw = bucket_size(uw);
        let bh = bucket_size(uh);
        if self.region_tex.is_none() || self.region_w != bw || self.region_h != bh {
            self.region_tex = Some(create_staging(&self.device, bw, bh, self.tex_format)?);
            self.region_w = bw;
            self.region_h = bh;
        }
        let Some(region_tex) = self.region_tex.clone() else {
            return Err(CaptureError::Runtime("region staging 缺失".into()));
        };

        let t_copy = Instant::now();
        for r in regions {
            let clipped = r.intersect(union);
            if clipped.is_empty() {
                continue;
            }
            let x0 = clipped.x.saturating_sub(union.x).max(0) as u32;
            let y0 = clipped.y.saturating_sub(union.y).max(0) as u32;
            let src_box = D3D11_BOX {
                left: clipped.x.max(0) as u32,
                top: clipped.y.max(0) as u32,
                front: 0,
                right: (clipped.x + clipped.w as i32).max(0) as u32,
                bottom: (clipped.y + clipped.h as i32).max(0) as u32,
                back: 1,
            };
            unsafe {
                self.context.CopySubresourceRegion(
                    &region_tex,
                    0,
                    x0,
                    y0,
                    0,
                    texture,
                    0,
                    Some(&src_box),
                )
            };
        }
        self.debug.copy_us = t_copy.elapsed().as_micros() as u64;

        let t_map = Instant::now();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&region_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped as *mut _))
        }
        .map_err(|e| CaptureError::Runtime(format!("Map(region): {}", hr_text(&e))))?;
        self.debug.map_us = t_map.elapsed().as_micros() as u64;
        self.debug.row_pitch = mapped.RowPitch;

        let t_blit = Instant::now();
        let mut result = Ok(());
        for r in regions {
            let clipped = r.intersect(union);
            if clipped.is_empty() {
                continue;
            }
            let sx = clipped.x.saturating_sub(union.x).max(0) as u32;
            let sy = clipped.y.saturating_sub(union.y).max(0) as u32;
            result = self.blit_rows_into(
                mapped,
                sx,
                sy,
                clipped.w,
                clipped.h,
                stride,
                clipped.x,
                clipped.y,
            );
            if result.is_err() {
                break;
            }
        }
        self.debug.blit_us = t_blit.elapsed().as_micros() as u64;

        let t_unmap = Instant::now();
        unsafe { self.context.Unmap(&region_tex, 0) };
        self.debug.unmap_us = t_unmap.elapsed().as_micros() as u64;
        result
    }

    /// `GetFrameDirtyRects`：以 `TotalMetadataBufferSize` 作为容量上界（dirty+move 总和），
    /// 不够再按 required 扩容重试。注意 buffer=0 的探测调用多数驱动不填 required，不能依赖它。
    fn query_dirty_rects(&self, sw: u32, sh: u32, hint: u32) -> CaptureResult<Vec<Rect>> {
        let mut needed: u32 = 0;
        unsafe {
            let _ = self
                .dupl
                .GetFrameDirtyRects(0, std::ptr::null_mut(), &mut needed);
        }
        let mut cap = hint.max(needed);
        let mut attempt = 0u32;
        loop {
            let count = cap as usize / std::mem::size_of::<RECT>();
            if count == 0 {
                return Ok(Vec::new());
            }
            let mut buf: Vec<RECT> = vec![RECT::default(); count];
            let mut required: u32 = 0;
            match unsafe {
                self.dupl
                    .GetFrameDirtyRects(cap, buf.as_mut_ptr(), &mut required)
            } {
                Ok(()) => {
                    let n = (required as usize / std::mem::size_of::<RECT>()).min(buf.len());
                    buf.truncate(n);
                    return Ok(buf
                        .iter()
                        .filter_map(|r| clamp_rect(r.left, r.top, r.right, r.bottom, sw, sh))
                        .collect());
                }
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(CaptureError::Runtime(format!(
                            "GetFrameDirtyRects: {}",
                            hr_text(&e)
                        )));
                    }
                    // 两次调用之间脏区变大（DXGI_ERROR_MORE_DATA）：按新需求扩容重试。
                    cap = required.max(cap.saturating_mul(2)).max(64);
                }
            }
        }
    }

    fn query_move_rects(&self, sw: u32, sh: u32, hint: u32) -> CaptureResult<Vec<MoveRect>> {
        let mut needed: u32 = 0;
        unsafe {
            let _ = self
                .dupl
                .GetFrameMoveRects(0, std::ptr::null_mut(), &mut needed);
        }
        let mut cap = hint.max(needed);
        let mut attempt = 0u32;
        loop {
            let rect_size = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
            let count = cap as usize / rect_size;
            if count == 0 {
                return Ok(Vec::new());
            }
            let mut buf: Vec<DXGI_OUTDUPL_MOVE_RECT> =
                vec![DXGI_OUTDUPL_MOVE_RECT::default(); count];
            let mut required: u32 = 0;
            match unsafe {
                self.dupl
                    .GetFrameMoveRects(cap, buf.as_mut_ptr(), &mut required)
            } {
                Ok(()) => {
                    let n = (required as usize / rect_size).min(buf.len());
                    buf.truncate(n);
                    let mut out = Vec::with_capacity(buf.len());
                    for m in &buf {
                        let d = m.DestinationRect;
                        let Some(dst) = clamp_rect(d.left, d.top, d.right, d.bottom, sw, sh) else {
                            continue;
                        };
                        // 源矩形与目标矩形同尺寸，位置由 SourcePoint 给出
                        let src = Rect::new(m.SourcePoint.x, m.SourcePoint.y, dst.w, dst.h);
                        out.push(MoveRect { src, dst });
                    }
                    return Ok(out);
                }
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(CaptureError::Runtime(format!(
                            "GetFrameMoveRects: {}",
                            hr_text(&e)
                        )));
                    }
                    cap = required
                        .max(cap.saturating_mul(2))
                        .max(rect_size as u32);
                }
            }
        }
    }
}

/// 把当前进程设成 PER_MONITOR_AWARE_V2（物理像素坐标系）。
///
/// **采集和光标坐标都要求物理像素**，被控端启动时必须先调一次；已经 aware 或老系统上失败都无害。
pub fn set_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// 块哈希（BGRA，按行做 xxh3 再混合）。未变块不参与编码。
pub fn hash_block(src: &[u8], stride: usize, w: u32, h: u32) -> u64 {
    use xxhash_rust::xxh3::xxh3_64;

    if w == 0 || h == 0 {
        return 0;
    }
    let row_bytes = (w as usize).saturating_mul(4);
    if stride < row_bytes || src.is_empty() {
        return 0;
    }

    // 起始种子带上行数，避免"不同高但内容相同"的块撞车。
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15 ^ u64::from(h).wrapping_mul(0x0000_0100_0000_01B3);
    for y in 0..h as usize {
        let Some(start) = y.checked_mul(stride) else {
            return 0;
        };
        let Some(end) = start.checked_add(row_bytes) else {
            return 0;
        };
        let Some(row) = src.get(start..end) else {
            return 0;
        };
        acc = acc.rotate_left(7) ^ xxh3_64(row);
    }

    // 末轮混合：块哈希只用于判等，但雪崩好一点没坏处。
    let mut x = acc;
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^= x >> 33;
    x
}

/// 当前单调毫秒时钟（供 `Frame.ts_ms` 与统计用）。
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: usize, h: usize) -> Vec<u8> {
        let stride = w * 4;
        let mut v = vec![0u8; stride * h];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        v
    }

    #[test]
    fn hash_block_is_stable() {
        let buf = pattern(64, 64);
        let h1 = hash_block(&buf, 256, 64, 64);
        assert_eq!(h1, hash_block(&buf, 256, 64, 64));
        assert_ne!(h1, 0);
    }

    #[test]
    fn hash_block_detects_single_pixel_change() {
        let mut buf = pattern(64, 64);
        let before = hash_block(&buf, 256, 64, 64);
        buf[3 * 256 + 10 * 4 + 2] ^= 0x01;
        assert_ne!(before, hash_block(&buf, 256, 64, 64));
    }

    #[test]
    fn hash_block_distinguishes_geometry() {
        let buf = pattern(64, 64);
        let h = hash_block(&buf, 256, 64, 64);
        assert_ne!(h, hash_block(&buf, 256, 64, 32));
        assert_ne!(h, hash_block(&buf, 256, 32, 64));
        // 相同的块内容，但 stride 不同（行尾有 padding）→ 结果一致
        let mut padded = vec![0u8; 320 * 64];
        for y in 0..64 {
            padded[y * 320..y * 320 + 256].copy_from_slice(&buf[y * 256..y * 256 + 256]);
        }
        assert_eq!(h, hash_block(&padded, 320, 64, 64));
    }

    #[test]
    fn hash_block_rejects_bad_input() {
        let buf = pattern(64, 64);
        assert_eq!(hash_block(&[], 0, 64, 64), 0);
        assert_eq!(hash_block(&buf, 4, 64, 64), 0);
        assert_eq!(hash_block(&buf, 256, 0, 64), 0);
        assert_eq!(hash_block(&buf, 256, 64, 0), 0);
        // 数据不足不 panic
        assert_eq!(hash_block(&buf[..100], 256, 64, 64), 0);
    }

    #[test]
    fn rect_clamping() {
        let r = clamp_rect(10, 20, 30, 60, 1920, 1080).expect("rect");
        assert_eq!(r, Rect::new(10, 20, 20, 40));
        // 负坐标裁到 0
        let r = clamp_rect(-5, -5, 10, 10, 100, 100).expect("rect");
        assert_eq!(r, Rect::new(0, 0, 10, 10));
        // 超出屏幕裁掉
        let r = clamp_rect(90, 90, 200, 200, 100, 100).expect("rect");
        assert_eq!(r, Rect::new(90, 90, 10, 10));
        // 完全在屏幕外
        assert!(clamp_rect(-50, -50, -10, -10, 100, 100).is_none());
        assert!(clamp_rect(200, 200, 300, 300, 100, 100).is_none());
    }

    #[test]
    fn rotation_mapping() {
        assert_eq!(rotation_deg(DXGI_MODE_ROTATION(1)), 0);
        assert_eq!(rotation_deg(DXGI_MODE_ROTATION(2)), 90);
        assert_eq!(rotation_deg(DXGI_MODE_ROTATION(3)), 180);
        assert_eq!(rotation_deg(DXGI_MODE_ROTATION(4)), 270);
        assert_eq!(rotation_deg(DXGI_MODE_ROTATION(0)), 0);
    }
}
