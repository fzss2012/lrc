//! WASAPI 回环采集 + 渲染播放（共享模式）。
//!
//! 采集：默认**渲染**设备的 loopback（AUDCLNT_STREAMFLAGS_LOOPBACK），拿到的就是
//! "系统正在播放的声音"。设备没在放音时 WASAPI 不产生数据包，read() 会返回 0 ——
//! 正好等于"静音不占带宽"。
//!
//! 播放：默认渲染设备 + 40ms 设备缓冲 + 60ms 抖动缓冲，缓冲满时丢最旧的
//! （音频宁丢不卡）。
//!
//! WASAPI 共享模式只接受设备的**混音格式**（本机是 44100Hz/f32/2ch），所以这里
//! 做两件事：采样格式转换（f32/int16）+ 线性插值重采样（44.1k ⇄ 48k，M3 够用）。
//! 对外始终是契约里的 48kHz/16bit，设备采样率不影响调用方。

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
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// 设备混音格式。
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
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        log::warn!("audio: CoInitializeEx hr={hr:?}（继续尝试）");
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
        let ext = unsafe { &*(p as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE) };
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
    if fmt.sample_rate == 0 || fmt.channels == 0 || fmt.channels > 2 {
        return Err(AudioError::Device(format!(
            "设备格式不支持（{}ch {}Hz）",
            fmt.channels, fmt.sample_rate
        )));
    }
    if !fmt.float && fmt.bits != 16 {
        return Err(AudioError::Device(format!("设备位深 {}bit 不支持", fmt.bits)));
    }
    Ok(fmt)
}

// ---------------------------------------------------------------- 重采样

/// 线性插值重采样器（源率 → 目标率，交错 N 声道 f32）。
///
/// M3 只求"能听、不断音"，不做高质量 SRC；44.1k⇄48k 的误差远小于 Opus 有损本身。
struct Resampler {
    /// 每个输出采样前进多少源帧
    step: f64,
    channels: usize,
    /// 上一块最后一帧
    last: Vec<f32>,
    /// 当前块内起始位置（相对 last 的帧偏移，可为 0..step+1）
    pos: f64,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            step: f64::from(src_rate) / f64::from(dst_rate),
            channels,
            last: vec![0.0; channels],
            pos: 0.0,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let n = input.len() / ch;
        if n == 0 {
            return;
        }
        let mut pos = self.pos;
        loop {
            let i0 = pos.floor() as usize;
            if i0 + 1 > n {
                break;
            }
            let frac = (pos - i0 as f64) as f32;
            // 逻辑序列：索引 0 = 上一块最后一帧，索引 k>=1 = input[k-1]
            let a: &[f32] = if i0 == 0 {
                &self.last[..]
            } else {
                &input[(i0 - 1) * ch..i0 * ch]
            };
            let b: &[f32] = &input[i0 * ch..(i0 + 1) * ch];
            for c in 0..ch {
                out.push(a[c] + (b[c] - a[c]) * frac);
            }
            pos += self.step;
        }
        self.last
            .copy_from_slice(&input[(n - 1) * self.channels..n * self.channels]);
        self.pos = pos - n as f64;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
    }
}

// ---------------------------------------------------------------- 采集

/// 系统声音回环采集（WASAPI loopback，默认渲染设备）。
pub struct LoopbackCapturer {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    dev: DeviceFormat,
    resampler: Option<Resampler>,
    out_format: AudioFormat,
    block_bytes: usize,
    /// 已转换、还没交出去的 s16le 字节
    pending: Vec<u8>,
    f32_buf: Vec<f32>,
    resampled: Vec<f32>,
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
        let resampler = if dev.sample_rate == format.sample_rate {
            None
        } else {
            log::info!(
                "audio: 采集重采样 {}Hz -> {}Hz",
                dev.sample_rate,
                format.sample_rate
            );
            Some(Resampler::new(
                dev.sample_rate,
                format.sample_rate,
                usize::from(format.channels),
            ))
        };
        log::info!(
            "audio: 回环采集就绪（设备 {}Hz/{}ch → {}Hz/{}ch）",
            dev.sample_rate,
            dev.channels,
            format.sample_rate,
            format.channels
        );
        Ok(Self {
            client,
            capture,
            dev,
            resampler,
            out_format: format,
            block_bytes: format.bytes_per_20ms(),
            pending: Vec::with_capacity(format.bytes_per_20ms() * 4),
            f32_buf: Vec::new(),
            resampled: Vec::new(),
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
                    self.convert(src);
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
        if n == 0 {
            return Ok(0);
        }
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        // 静音门限：整块幅度 <= 1 LSB 就当没有声音，不产出（线上一个字节都不发）。
        // WASAPI 在没人放音时也会持续送静音包，不拦住的话会白烧 32kbps。
        let silent = buf[..n]
            .chunks_exact(2)
            .all(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() <= 1);
        if silent {
            return Ok(0);
        }
        Ok(n)
    }

    /// 设备格式 → f32（取前两声道）→ 重采样 → s16le 追加到 pending。
    fn convert(&mut self, src: &[u8]) {
        let dev = self.dev;
        let bps = dev.bytes_per_sample();
        let frame = dev.frame_bytes();
        if frame == 0 || src.len() < frame {
            return;
        }
        let out_ch = usize::from(self.out_format.channels);
        let frames = src.len() / frame;
        self.f32_buf.clear();
        self.f32_buf.reserve(frames * out_ch);
        for f in 0..frames {
            let base = f * frame;
            for c in 0..out_ch {
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
                self.f32_buf.push(v);
            }
        }
        let samples = if let Some(rs) = self.resampler.as_mut() {
            self.resampled.clear();
            rs.process(&self.f32_buf, &mut self.resampled);
            &self.resampled
        } else {
            &self.f32_buf
        };
        self.pending.reserve(samples.len() * 2);
        for &v in samples {
            let s = (v * 32767.0).clamp(-32768.0, 32767.0) as i16;
            self.pending.extend_from_slice(&s.to_le_bytes());
        }
    }
}

impl Drop for LoopbackCapturer {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
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
    jitter: Vec<u8>,
    jitter_cap: usize,
    resampler: Option<Resampler>,
    f32_buf: Vec<f32>,
    resampled: Vec<f32>,
    device_bytes: Vec<u8>,
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
        let init =
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 400_000, 0, mix_ptr, None) };
        unsafe { CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void)) };
        init.map_err(|e| AudioError::Device(format!("Initialize(render): {e}")))?;
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| AudioError::Device(format!("GetBufferSize: {e}")))?;
        let render: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|e| AudioError::Device(format!("IAudioRenderClient: {e}")))?;
        unsafe { client.Start() }.map_err(|e| AudioError::Device(format!("播放 Start: {e}")))?;
        let jitter_cap = format.bytes_per_sec() * 60 / 1000;
        let resampler = if dev.sample_rate == format.sample_rate {
            None
        } else {
            log::info!(
                "audio: 播放重采样 {}Hz -> {}Hz",
                format.sample_rate,
                dev.sample_rate
            );
            Some(Resampler::new(
                format.sample_rate,
                dev.sample_rate,
                usize::from(format.channels),
            ))
        };
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
            resampler,
            f32_buf: Vec::new(),
            resampled: Vec::new(),
            device_bytes: Vec::new(),
        })
    }

    /// 非阻塞写入 PCM（20ms 一块）；内部缓冲满时丢最旧的数据。
    pub fn write(&mut self, pcm: &[u8]) -> AudioResult<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.jitter.extend_from_slice(pcm);
        if self.jitter.len() > self.jitter_cap {
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

    /// 把抖动缓冲尽量喂进设备（必要时重采样 + 转设备格式）。
    fn pump(&mut self) -> AudioResult<()> {
        let src_frame = self.out_format.block_align();
        loop {
            let padding = unsafe { self.client.GetCurrentPadding() }
                .map_err(|e| AudioError::Device(format!("GetCurrentPadding: {e}")))?;
            let free = self.buffer_frames.saturating_sub(padding) as usize;
            if free == 0 || self.jitter.len() < src_frame {
                return Ok(());
            }
            // 取一块源 PCM（最多 free 帧对应的量），转 f32 → 重采样 → 设备格式
            let src_ch = usize::from(self.out_format.channels);
            let take_frames = free.min(self.jitter.len() / src_frame);
            let take_bytes = take_frames * src_frame;
            self.f32_buf.clear();
            self.f32_buf.reserve(take_frames * src_ch);
            for s in self.jitter[..take_bytes].chunks_exact(2) {
                self.f32_buf
                    .push(f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0);
            }
            let samples = if let Some(rs) = self.resampler.as_mut() {
                self.resampled.clear();
                rs.process(&self.f32_buf, &mut self.resampled);
                &self.resampled
            } else {
                &self.f32_buf
            };
            let out_frames = samples.len() / src_ch;
            if out_frames == 0 {
                return Ok(());
            }
            // 设备格式
            let dev = self.dev;
            let dev_frame = dev.frame_bytes();
            self.device_bytes.clear();
            self.device_bytes.resize(out_frames * dev_frame, 0);
            encode_device(samples, src_ch, dev, &mut self.device_bytes);
            let data = unsafe { self.render.GetBuffer(out_frames as u32) }
                .map_err(|e| AudioError::Device(format!("render GetBuffer: {e}")))?;
            if data.is_null() {
                return Ok(());
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.device_bytes.as_ptr(),
                    data,
                    self.device_bytes.len(),
                )
            };
            unsafe { self.render.ReleaseBuffer(out_frames as u32, 0) }
                .map_err(|e| AudioError::Device(format!("render ReleaseBuffer: {e}")))?;
            self.jitter.drain(..take_bytes);
        }
    }

    /// 当前待播音频的毫秒数（抖动缓冲 + 设备缓冲水位，都按 48kHz 口径算）。
    pub fn buffered_ms(&self) -> u32 {
        let frame_bytes = self.out_format.block_align();
        let jitter_frames = (self.jitter.len() / frame_bytes) as u32;
        let jitter_ms = jitter_frames * 1000 / self.out_format.sample_rate;
        let padding = unsafe { self.client.GetCurrentPadding() }.unwrap_or(0);
        // padding 是设备帧数，按设备采样率折算
        let dev_ms = padding * 1000 / self.dev.sample_rate.max(1);
        jitter_ms + dev_ms
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

/// 交错 f32（N 声道）→ 设备格式字节。
fn encode_device(samples: &[f32], src_ch: usize, dev: DeviceFormat, out: &mut [u8]) {
    let dev_ch = usize::from(dev.channels);
    let bps = dev.bytes_per_sample();
    let frame_bytes = dev.frame_bytes();
    let frames = out.len() / frame_bytes.max(1);
    for f in 0..frames {
        for dc in 0..dev_ch {
            let sc = if src_ch <= 1 { 0 } else { dc.min(src_ch - 1) };
            let i = f * src_ch + sc;
            let v = samples.get(i).copied().unwrap_or(0.0);
            let p = f * frame_bytes + dc * bps;
            if dev.float {
                let bytes = v.clamp(-1.0, 1.0).to_le_bytes();
                out[p..p + 4].copy_from_slice(&bytes);
            } else {
                let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
                out[p..p + 2].copy_from_slice(&s.to_le_bytes());
            }
        }
    }
}
