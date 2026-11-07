//! 纯 D3D11 回读基准（与 duplication 无关）：判断 8MB GPU→CPU 拷贝本身有多慢。
//!
//! 用法：cargo run --release -p lrc-capture --bin readback-bench -- [轮数]

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIAdapter, DXGI_ERROR_NOT_FOUND,
};

fn stage_desc(w: u32, h: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    }
}

fn default_desc(w: u32, h: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_DEFAULT,
        CPUAccessFlags: 0,
        ..stage_desc(w, h)
    }
}

fn one_round(
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    dst: &ID3D11Texture2D,
    w: u32,
    h: u32,
) -> (u64, u64, u64) {
    let t0 = Instant::now();
    unsafe { context.CopyResource(dst, src) };
    let copy_us = t0.elapsed().as_micros() as u64;

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let t1 = Instant::now();
    let map_res = unsafe { context.Map(dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped as *mut _)) };
    let map_us = t1.elapsed().as_micros() as u64;
    if map_res.is_err() {
        return (copy_us, map_us, 0);
    }
    let t2 = Instant::now();
    let pitch = mapped.RowPitch as usize;
    let mut sum = 0u64;
    if !mapped.pData.is_null() {
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h as usize) };
        // 触摸每一行，确保真的读了系统内存
        for y in 0..h as usize {
            sum = sum.wrapping_add(u64::from(src[y * pitch])).wrapping_add(u64::from(src[y * pitch + w as usize * 4 - 1]));
        }
    }
    let touch_us = t2.elapsed().as_micros() as u64;
    unsafe { context.Unmap(dst, 0) };
    (copy_us, map_us, touch_us)
}

pub fn run_bench(rounds: u32) -> Result<(), Box<dyn std::error::Error>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| format!("CreateDXGIFactory1: 0x{:08X}", e.code().0 as u32))?;
    let adapter: IDXGIAdapter = match unsafe { factory.EnumAdapters1(0) } {
        Ok(a) => a.cast()?,
        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => return Err("没有 adapter".into()),
        Err(e) => return Err(format!("EnumAdapters1: 0x{:08X}", e.code().0 as u32).into()),
    };

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
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
            None,
            Some(&mut context as *mut _),
        )
    }
    .map_err(|e| format!("D3D11CreateDevice: 0x{:08X}", e.code().0 as u32))?;
    let device = device.ok_or("没有 device")?;
    let context = context.ok_or("没有 context")?;

    for (w, h) in [(1920u32, 1080u32), (256, 256), (64, 64)] {
        let mut src: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&default_desc(w, h), None, Some(&mut src as *mut _)) }
            .map_err(|e| format!("CreateTexture2D(src {w}x{h}): 0x{:08X}", e.code().0 as u32))?;
        let src = src.ok_or("src 为空")?;
        let mut dst: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&stage_desc(w, h), None, Some(&mut dst as *mut _)) }
            .map_err(|e| format!("CreateTexture2D(staging {w}x{h}): 0x{:08X}", e.code().0 as u32))?;
        let dst = dst.ok_or("dst 为空")?;

        // 预热
        let _ = one_round(&context, &src, &dst, w, h);
        let mut best = u64::MAX;
        let mut worst = 0u64;
        let mut sum_map = 0u64;
        let mut sum_copy = 0u64;
        for _ in 0..rounds {
            let (c, m, _) = one_round(&context, &src, &dst, w, h);
            sum_copy += c;
            sum_map += m;
            best = best.min(c + m);
            worst = worst.max(c + m);
        }
        let n = rounds.max(1) as f64;
        println!(
            "  {w}x{h} ({}KB): copy {:.2}ms  map {:.2}ms  total best {:.2}ms worst {:.2}ms",
            (w as u64 * h as u64 * 4) / 1024,
            sum_copy as f64 / n / 1000.0,
            sum_map as f64 / n / 1000.0,
            best as f64 / 1000.0,
            worst as f64 / 1000.0
        );
    }
    Ok(())
}

fn main() {
    let rounds: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    println!("[readback-bench] 纯 D3D11 default→staging 回读基准（{rounds} 轮/尺寸）");
    if let Err(e) = run_bench(rounds) {
        eprintln!("[readback-bench] 失败: {e}");
        std::process::exit(1);
    }
}
