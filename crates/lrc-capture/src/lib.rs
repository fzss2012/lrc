//! Windows 屏幕采集（DXGI Desktop Duplication，GDI 兜底）。
//!
//! 契约见 `docs/DESIGN.md` §4。**接口签名是三方共享契约**，改动先跟 Lead 对齐。

use std::fmt;

pub use lrc_proto::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    /// 0/90/180/270，仅桌面朝向，暂不做旋转补偿
    pub rotation: u16,
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

/// 枚举可用输出（序号即 `Capturer::new` 的 `output_index`）。
pub fn list_outputs() -> CaptureResult<Vec<ScreenInfo>> {
    todo!("T1 实现：IDXGIFactory1 → EnumAdapters1 → EnumOutputs → GetDesc")
}

/// 一个采集器绑定一路输出。
pub struct Capturer {
    _priv: (),
}

impl Capturer {
    /// `timeout_ms`：`grab()` 内部等待新帧的上限；0 表示非阻塞。
    pub fn new(_output_index: usize, _timeout_ms: u32) -> CaptureResult<Self> {
        todo!("T1 实现：D3D11 设备 + IDXGIOutput1::DuplicateOutput + staging 纹理")
    }

    pub fn info(&self) -> ScreenInfo {
        todo!("T1 实现")
    }

    /// 取下一帧。没有新帧时返回 `Ok(None)`（**这是"零变化零发送"的源头**）。
    pub fn grab(&mut self) -> CaptureResult<Option<Frame>> {
        todo!("T1 实现：AcquireNextFrame → GetFrameDirtyRects/MoveRects → 拷到 bgra")
    }
}

/// 块哈希（BGRA，按行做 xxh3 再混合）。未变块不参与编码。
pub fn hash_block(_src: &[u8], _stride: usize, _w: u32, _h: u32) -> u64 {
    todo!("T1 实现：xxh3_64 逐行哈希与已有字节比较，全同返回 0 语义由调用方处理")
}

/// 当前单调毫秒时钟（供 `Frame.ts_ms` 与统计用）。
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
