//! D3D11 呈现自测：真开一个窗口 → 建 Presenter → 整帧 + 逐块更新 + Present。
//!
//! 用法：cargo run --release -p lrc-render --bin render-probe -- [--seconds 3] [--size WxH]

use std::time::{Duration, Instant};

use lrc_render::{CursorBitmap, Presenter};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, PostQuitMessage,
    RegisterClassW, ShowWindow, TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

const CLASS_NAME: windows::core::PCWSTR = w!("lrc_render_probe");

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const WM_DESTROY: u32 = 0x0002;
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_window(w: i32, h: i32) -> Result<HWND, Box<dyn std::error::Error>> {
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let wc = WNDCLASSW {
        style: Default::default(),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: Default::default(),
        hCursor: Default::default(),
        hbrBackground: Default::default(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        return Err("RegisterClassW 失败".into());
    }
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            w!("lrc render probe"),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            w + 16,
            h + 39,
            None,
            None,
            Some(hinstance),
            None,
        )
    }?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }
    Ok(hwnd)
}

fn fill_gradient(buf: &mut [u8], w: u32, h: u32, stride: usize) {
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * stride + x * 4;
            if i + 3 < buf.len() {
                buf[i] = (x * 255 / w.max(1) as usize) as u8; // B
                buf[i + 1] = (y * 255 / h.max(1) as usize) as u8; // G
                buf[i + 2] = 128; // R
                buf[i + 3] = 255; // A
            }
        }
    }
}

fn pump() -> bool {
    let mut msg = MSG::default();
    loop {
        let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
        if !got.as_bool() {
            return true;
        }
        if msg.message == 0x0012 {
            // WM_QUIT
            return false;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() {
    let mut seconds = 3.0f64;
    let mut rw = 640u32;
    let mut rh = 360u32;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => {
                seconds = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(3.0);
            }
            "--size" => {
                if let Some(s) = it.next() {
                    let mut parts = s.split(['x', 'X']);
                    rw = parts.next().and_then(|p| p.parse().ok()).unwrap_or(640);
                    rh = parts.next().and_then(|p| p.parse().ok()).unwrap_or(360);
                }
            }
            _ => {}
        }
    }

    if let Err(e) = run(rw, rh, seconds) {
        eprintln!("[render-probe] 失败: {e}");
        std::process::exit(1);
    }
}

fn run(width: u32, height: u32, seconds: f64) -> Result<(), Box<dyn std::error::Error>> {
    let hwnd = create_window(800, 450)?;
    println!("[render-probe] 窗口 hwnd={:?}", hwnd);

    let mut presenter = Presenter::new(hwnd.0 as isize, width, height)?;
    println!(
        "[render-probe] Presenter 建立 source={:?} client={:?}",
        presenter.source_size(),
        presenter.client_size()
    );

    // 整帧
    let stride = width as usize * 4;
    let mut full = vec![0u8; stride * height as usize];
    fill_gradient(&mut full, width, height, stride);
    let t0 = Instant::now();
    presenter.update_full(width, height, &full, stride)?;
    println!("[render-probe] update_full {}x{} 用时 {}us", width, height, t0.elapsed().as_micros());

    // 一句话光标 no-op
    let cursor = CursorBitmap {
        width: 2,
        height: 2,
        hotspot_x: 0,
        hotspot_y: 0,
        bgra: vec![255; 16],
    };
    presenter.set_cursor_shape(Some(&cursor))?;
    presenter.set_cursor_visible(true);

    let block = 64u32;
    let mut block_buf = vec![0u8; block as usize * 4 * block as usize];
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut frames = 0u64;
    let mut t_update = 0u64;
    let mut t_present = 0u64;
    let mut x = 0i64;

    while Instant::now() < deadline {
        if !pump() {
            break;
        }
        // 一个来回移动的 64x64 方块（模拟块更新）
        let bx = ((x * 7) % (width.saturating_sub(block).max(1) as i64)) as i32;
        for p in block_buf.chunks_exact_mut(4) {
            p[0] = 30;
            p[1] = 200;
            p[2] = 255;
            p[3] = 255;
        }
        let t1 = Instant::now();
        presenter.update_block(bx, 100, block, block, &block_buf, block as usize * 4)?;
        t_update += t1.elapsed().as_micros() as u64;

        let t2 = Instant::now();
        presenter.present()?;
        t_present += t2.elapsed().as_micros() as u64;
        frames += 1;
        x += 1;
        std::thread::sleep(Duration::from_millis(16));
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    let n = frames.max(1) as f64;
    println!(
        "[render-probe] present {} 帧 ({}x{}), update_block {:.0}us/帧, present {:.0}us/帧",
        frames,
        width,
        height,
        t_update as f64 / n,
        t_present as f64 / n
    );
    Ok(())
}
