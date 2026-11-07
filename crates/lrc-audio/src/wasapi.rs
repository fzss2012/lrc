//! WASAPI 回环采集 + 渲染播放（共享模式）。
//!
//! 采集：默认**渲染**设备的 loopback（AUDCLNT_STREAMFLAGS_LOOPBACK），拿到的就是
//! "系统正在播放的声音"。设备没在放音时 WASAPI 不产生数据包，read() 会返回 0 ——
//! 正好等于"静音不占带宽"。
//!
//! 播放：默认渲染设备 + 40ms 设备缓冲 + 60ms 抖动缓冲，缓冲满时丢最旧的
//! （音频宁丢不卡）。
//!
//! 设备格式（通常是 48kHz 立体声 float32）由 WASAPI 混音格式决定，这里做
//! float32/int16 → s16 的转换；采样率不是 48kHz 或没有渲染端点就报错，
//! 让调用方干净地关掉音频通道。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::{AudioError, AudioFormat, AudioResult};

const WAVE_FORMAT_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// 设备混音格式（我们只关心采样表示，采样率必须是 48kHz）。
#[derive(Clone, Copy, Debug)]
struct DeviceFormat {
    channels: u16,
    sample_rate: u32,
    bits: u16,
    float: bool,
}

impl DeviceFormat {
    fn bytes_per_sample(&self) -> usize {
        usize::from(self.bits / 8)
    }

    fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * self.bytes_per_sample()
    }
}

fn com_init() {
    // 多线程套间；已经初始化过（S_FALSE / RPC_E_CHANGED_MODE）都当成功。
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        log::warn!("audio: CoInitializeEx 失败 hr={hr:?}（继续尝试）");
    }
}

unsafe fn default_render_client() -> AudioResult<IAudioClient> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&windows::core::IUnknown>, CLSCTX_ALL) }
            .map_err(|e| AudioError::Device(format!("IMMDeviceEnumerator: {e}")))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .map_err(|e| AudioError::Device(format!("没有默认渲染设备: {e}")))?;
    unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| AudioError::Device(format!("激活 IAudioClient: {e}")))
}

unsafe fn read_device_format(p: *const WAVEFORMATEX) -> AudioResult<DeviceFormat> {
    if p.is_null() {
        return Err(AudioError::Device("设备混音格式为空".to_string()));
    }
    let wf = unsafe { &*p };
    let mut float = wf.wFormatTag == WAVE_FORMAT_FLOAT_TAG;
    if wf.wFormatTag == WAVE_FORMAT_EXTENSIBLE_TAG && wf.cbSize >= 22 {
        // WAVEFORMATEXTENSIBLE.SubFormat 的低 32 位就是 format tag
        let ext = unsafe { &*(p as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE) };
        // data1: 3 = IEEE float, 1 = PCM
        float = ext.SubFormat.data1 == 3;
    }
    let fmt = DeviceFormat {
        channels: wf.nChannels,
        sample_rate: wf.nSamplesPerSec,
        bits: wf.wBitsPerSample,
        float,
    };
    log::info!(
        "audio: 设备混音格式 {}Hz/{}ch/{}{}bit",
        fmt.sample_rate,
        fmt.channels,
        if fmt.float { "float" } else { "pcm" },
        fmt.bits
    );
    if fmt.sample_rate != 48_000 {
        return Err(AudioError::Unsupported(
            "设备混音采样率不是 48kHz（M3 不做重采样）",
        ));
    }
    if fmt.channels == 0 || fmt.channels > 2 {
        return Err(AudioError::Unsupported("设备声道数只支持 1/2"));
    }
    if !fmt.float && (fmt.bits != 16 || wf.wFormatTag != WAVE_FORMAT_PCM_TAG) {
        return Err(AudioError::Unsupported("设备位深只支持 16bit 或 float32"));
    }
    Ok(fmt)
}

// ---------------------------------------------------------------- 采集

/// 系统声音回环采集（WASAPI loopback，默认渲染设备）。
pub struct LoopbackCapturer {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    dev: DeviceFormat,
    out_format: AudioFormat,
    block_bytes: usize,
    /// 已转换、还没交出去的 s16le 字节
    pending: Vec<u8>,
}

impl LoopbackCapturer {
    /// 设备不可用/无声卡时返回 Err(AudioError::Device)，调用方可据此关闭音频通道。
    pub fn new(format: AudioFormat) -> AudioResult<Self> {
        format.ensure_supported()?;
        com_init();
        let client = unsafe { default_render_client()? };
        let mix_ptr = unsafe { client.GetMixFormat() }
            .map_err(|e| AudioError::Device(format!("GetMixFormat: {e}")))?;
        let dev = unsafe { read_device_format(mix_ptr) };
        let dev = match dev {
            Ok(d) => d,
            Err(e) => {
                unsafe { CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void)) };
                return Err(e);
            }
        };
        // 200ms 设备缓冲（loopback 给大一点，避免采集端被调度抖动拖垮）
        let init = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                2_000_000,
                0,
                mix_ptr,
                None,
            )
        };
        unsafe { CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void)) };
        init.map_err(|e| AudioError::Device(format!("Initialize(loopback): {e}")))?;

        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| AudioError::Device(format!("IAudioCaptureClient: {e}")))?;
        unsafe { client.Start() }.map_err(|e| AudioError::Device(format!("采集 Start: {e}")))?;
        log::info!(
            "audio: 回环采集就绪（{}Hz/{}ch → {}Hz/{}ch）",
            dev.sample_rate,
            dev.channels,
            format.sample_rate,
            format.channels
        );
        Ok(Self {
            client,
            capture,
            dev,
            out_format: format,
            block_bytes: format.bytes_per_20ms(),
            pending: Vec::with_capacity(format.bytes_per_20ms() * 4),
        })
    }

    /// 阻塞读一块 s16le 交错 PCM（20ms）。返回实际字节数；
    /// 系统没在放音时 WASAPI 不产包，可能返回 0（此时一个字节都不该发）。
    pub fn read(&mut self, buf: &mut [u8]) -> AudioResult<usize> {
        let want = self.block_bytes.min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        let deadline = Instant::now() + Duration::from_millis(200);
        let out_ch = usize::from(self.out_format.channels);
        while self.pending.len() < want {
            let mut got_any = false;
            loop {
                let next = unsafe { self.capture.GetNextPacketSize() }
                    .map_err(|e| AudioError::Device(format!("GetNextPacketSize: {e}")))?;
                if next == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .map_err(|e| AudioError::Device(format!("GetBuffer: {e}")))?;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                if silent || data.is_null() || frames == 0 {
                    let zeros = frames as usize * out_ch * 2;
                    self.pending.resize(self.pending.len() + zeros, 0);
                } else {
                    let byte_len = frames as usize * self.dev.frame_bytes();
                    let src = unsafe { std::slice::from_raw_parts(data, byte_len) };
                    convert_capture(src, self.dev, out_ch, &mut self.pending);
                }
                unsafe { self.capture.ReleaseBuffer(frames) }
                    .map_err(|e| AudioError::Device(format!("ReleaseBuffer: {e}")))?;
                got_any = true;
            }
            if self.pending.len() >= want || Instant::now() >= deadline {
                break;
            }
            if !got_any {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let n = want.min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Drop for LoopbackCapturer {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// 设备格式的采集数据 → 48kHz s16le（声道 1/2，超出取前两路）。
fn convert_capture(src: &[u8], dev: DeviceFormat, out_channels: usize, out: &mut Vec<u8>) {
    let bps = dev.bytes_per_sample();
    let frame = dev.frame_bytes();
    if frame == 0 || src.len() < frame {
        return;
    }
    let frames = src.len() / frame;
    out.reserve(frames * out_channels * 2);
    for f in 0..frames {
        let base = f * frame;
        for c in 0..out_channels {
            let ch = if dev.channels <= 1 {
                0
            } else {
                c.min(usize::from(dev.channels) - 1)
            };
            let s = base + ch * bps;
            let v = if dev.float {
                if s + 4 > src.len() {
                    break;
                }
                let bits = u32::from_le_bytes([src[s], src[s + 1], src[s + 2], src[s + 3]]);
                f32::from_bits(bits).clamp(-1.0, 1.0)
            } else {
                if s + 2 > src.len() {
                    break;
                }
                f32::from(i16::from_le_bytes([src[s], src[s + 1]])) / 32768.0
            };
            let sample = (v * 32767.0).clamp(-32768.0, 32767.0) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
}

// ---------------------------------------------------------------- 播放

/// 播放（WASAPI 渲染）。
pub struct Player {
    client: IAudioClient,
    render: IAudioRenderClient,
    dev: DeviceFormat,
    out_format: AudioFormat,
    buffer_frames: u32,
    /// 抖动缓冲（s16le 48k stereo），满则丢最旧
    jitter: Vec<u8>,
    jitter_cap: usize,
}

impl Player {
    pub fn new(format: AudioFormat) -> AudioResult<Self> {
        format.ensure_supported()?;
        com_init();
        let client = unsafe { default_render_client()? };
        let mix_ptr = unsafe { client.GetMixFormat() }
            .map_err(|e| AudioError::Device(format!("GetMixFormat: {e}")))?;
        let dev = unsafe { read_device_format(mix_ptr) };
        let dev = match dev {
            Ok(d) => d,
            Err(e) => {
                unsafe { CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void)) };
                return Err(e);
            }
        };
        // 40ms 设备缓冲：加上抖动缓冲，稳态总水位落在 30-60ms
        let init = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                400_000,
                0,
                mix_ptr,
                None,
            )
        };
        unsafe { CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void)) };
        init.map_err(|e| AudioError::Device(format!("Initialize(render): {e}")))?;
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| AudioError::Device(format!("GetBufferSize: {e}")))?;
        let render: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|e| AudioError::Device(format!("IAudioRenderClient: {e}")))?;
        unsafe { client.Start() }.map_err(|e| AudioError::Device(format!("播放 Start: {e}")))?;
        let jitter_cap = format.bytes_per_sec() * 60 / 1000;
        log::info!(
            "audio: 播放就绪（设备 {}Hz/{}ch/{}{}bit，设备缓冲 {} 帧）",
            dev.sample_rate,
            dev.channels,
            if dev.float { "float" } else { "pcm" },
            dev.bits,
            buffer_frames
        );
        Ok(Self {
            client,
            render,
            dev,
            out_format: format,
            buffer_frames,
            jitter: Vec::with_capacity(jitter_cap),
            jitter_cap,
        })
    }

    /// 非阻塞写入 PCM（20ms 一块）；内部缓冲满时丢最旧的数据。
    pub fn write(&mut self, pcm: &[u8]) -> AudioResult<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.jitter.extend_from_slice(pcm);
        if self.jitter.len() > self.jitter_cap {
            // 宁丢不卡：丢掉最旧的多余字节（按帧对齐）
            let frame = self.out_format.block_align().max(1);
            let mut excess = self.jitter.len() - self.jitter_cap;
            excess -= excess % frame;
            if excess > 0 {
                self.jitter.drain(..excess);
                log::debug!("audio: 抖动缓冲溢出，丢最旧 {excess} 字节");
            }
        }
        self.pump()
    }

    /// 把抖动缓冲尽量喂进设备。
    fn pump(&mut self) -> AudioResult<()> {
        let frame_bytes = self.out_format.block_align();
        loop {
            let padding = unsafe { self.client.GetCurrentPadding() }
                .map_err(|e| AudioError::Device(format!("GetCurrentPadding: {e}")))?;
            let free = self.buffer_frames.saturating_sub(padding) as usize;
            let avail = self.jitter.len() / frame_bytes;
            if free == 0 || avail == 0 {
                return Ok(());
            }
            let frames = free.min(avail);
            let data = unsafe { self.render.GetBuffer(frames as u32) }
                .map_err(|e| AudioError::Device(format!("render GetBuffer: {e}")))?;
            if data.is_null() {
                return Ok(());
            }
            let bytes = frames * frame_bytes;
            unsafe { convert_playback(&self.jitter[..bytes], self.out_format, self.dev, data) };
            self.jitter.drain(..bytes);
            unsafe { self.render.ReleaseBuffer(frames as u32, 0) }
                .map_err(|e| AudioError::Device(format!("render ReleaseBuffer: {e}")))?;
        }
    }

    /// 当前待播音频的毫秒数（抖动缓冲 + 设备缓冲水位）。
    pub fn buffered_ms(&self) -> u32 {
        let frame_bytes = self.out_format.block_align();
        let jitter_frames = (self.jitter.len() / frame_bytes) as u32;
        let padding = unsafe { self.client.GetCurrentPadding() }.unwrap_or(0);
        (jitter_frames + padding) * 1000 / self.out_format.sample_rate
    }

    /// 设备缓冲能装多少毫秒（自测/日志用）。
    pub fn device_buffer_ms(&self) -> u32 {
        self.buffer_frames * 1000 / self.out_format.sample_rate
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// s16le 48k stereo（我们自己的口径）→ 设备格式。
unsafe fn convert_playback(src: &[u8], src_format: AudioFormat, dev: DeviceFormat, dst: *mut u8) {
    let src_ch = usize::from(src_format.channels);
    let src_frame = src_format.block_align();
    let frames = src.len() / src_frame;
    let dev_bps = dev.bytes_per_sample();
    for f in 0..frames {
        for dc in 0..usize::from(dev.channels) {
            let sc = if src_ch <= 1 { 0 } else { dc.min(src_ch - 1) };
            let s = f * src_frame + sc * 2;
            if s + 2 > src.len() {
                continue;
            }
            let v = i16::from_le_bytes([src[s], src[s + 1]]);
            let p = f * dev.frame_bytes() + dc * dev_bps;
            if dev.float {
                let fv = f32::from(v) / 32768.0;
                let bytes = fv.to_le_bytes();
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(p), 4) };
            } else {
                let bytes = v.to_le_bytes();
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(p), 2) };
            }
        }
    }
}
