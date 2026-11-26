//! 面板窗口 + 托盘。
//!
//! 面向普通用户：只有一个主开关（启动/暂停服务），一个重启，一个远程连接开关，
//! 一个设置。所有技术细节（PID、路径、退出码）只在出错时作为诊断信息出现。
//! 关窗缩到托盘；托盘右键：显示窗口 / 设置 / 开机自启 / 退出。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use lrc_supervisor::{autostart, ipc};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT, SYSTEM_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetDlgItem, GetMessageW, GetWindowLongPtrW, LoadCursorW, LoadIconW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowTextW, SetTimer, ShowWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GWLP_USERDATA, HMENU,
    IDC_ARROW, IDI_APPLICATION, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_SHOWNORMAL,
    TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_SETFONT, WM_TIMER, WM_USER, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    WS_VSCROLL,
};

const REFRESH_MS: u32 = 1000;
const WM_TRAY: u32 = WM_USER + 1;

const ID_STATUS: usize = 100;
// 主开关：一个按钮同时承担"启动 / 暂停"
const ID_BTN_TOGGLE: usize = 200;
const ID_BTN_RESTART: usize = 202;
const ID_BTN_TUNNEL: usize = 205;
const ID_BTN_SETTINGS: usize = 206;

const ID_TRAY_SHOW: usize = 300;
const ID_TRAY_SETTINGS: usize = 301;
const ID_TRAY_AUTOSTART: usize = 302;
const ID_TRAY_EXIT: usize = 303;

struct App {
    ipc_addr: SocketAddr,
    config_path: PathBuf,
    tray: bool,
    icon_added: bool,
}

fn window_class() -> PCWSTR {
    windows::core::w!("lrc_panel_window_v3")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run(ipc_addr: SocketAddr, config_path: PathBuf, tray: bool) -> Result<(), String> {
    let app = Box::new(App { ipc_addr, config_path, tray, icon_added: false });
    let app_ptr: *mut App = Box::into_raw(app);

    let module =
        unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| format!("取模块句柄失败: {e}"))?;
    let hinstance = HINSTANCE(module.0);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|e| format!("加载光标失败: {e}"))?;

    let class_name = window_class();
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() },
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
            (windows::Win32::Graphics::Gdi::COLOR_WINDOW.0 as isize + 1) as *mut core::ffi::c_void,
        ),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: class_name,
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        unsafe { drop(Box::from_raw(app_ptr)) };
        return Err(String::from("RegisterClassW 失败"));
    }

    let title = wide("远程控制服务");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            class_name,
            PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            470,
            None,
            None,
            Some(hinstance),
            Some(app_ptr as *const core::ffi::c_void),
        )
    }
    .map_err(|e| format!("CreateWindowExW 失败: {e}"))?;

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetTimer(Some(hwnd), 1, REFRESH_MS, None);
    }

    unsafe {
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == -1 || ret.0 == 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let app = &*(app_ptr as *const App);
        if app.icon_added {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        drop(Box::from_raw(app_ptr));
    }
    Ok(())
}

unsafe fn app_of(hwnd: HWND) -> &'static App {
    &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App)
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
            let app = &mut *(cs.lpCreateParams as *mut App);

            create_child(hwnd, "EDIT", "", 12, 12, 600, 270, ID_STATUS);
            create_child(hwnd, "BUTTON", "暂停服务", 12, 296, 140, 36, ID_BTN_TOGGLE);
            create_child(hwnd, "BUTTON", "重启服务", 162, 296, 120, 36, ID_BTN_RESTART);
            create_child(hwnd, "BUTTON", "连接远程", 292, 296, 140, 36, ID_BTN_TUNNEL);
            create_child(hwnd, "BUTTON", "设置", 442, 296, 100, 36, ID_BTN_SETTINGS);
            create_child(
                hwnd,
                "STATIC",
                "关闭窗口会缩到托盘，服务继续在后台运行",
                12,
                344,
                600,
                24,
                999,
            );

            if app.tray {
                add_tray_icon(hwnd);
                app.icon_added = true;
            }
            LRESULT(0)
        }
        WM_TIMER => {
            refresh(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as usize;
            match id {
                ID_BTN_TOGGLE => {
                    // 真正的主开关：暂停会停掉所有子进程并禁止自动重启
                    let enabled = service_enabled(hwnd);
                    let _ = send(hwnd, if enabled { "accept off" } else { "accept on" });
                    refresh(hwnd);
                }
                ID_BTN_RESTART => {
                    let _ = send(hwnd, "restart-host");
                    refresh(hwnd);
                }
                ID_BTN_TUNNEL => {
                    let cur = send(hwnd, "status").unwrap_or_default();
                    let running = cur
                        .lines()
                        .find(|l| l.starts_with("tunnel.running="))
                        .map(|l| l.contains("true"))
                        .unwrap_or(false);
                    let cmd = if running { "stop-tunnel" } else { "start-tunnel" };
                    let _ = send(hwnd, cmd);
                    refresh(hwnd);
                }
                ID_BTN_SETTINGS => open_settings(hwnd),
                ID_TRAY_SHOW => {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
                ID_TRAY_SETTINGS => open_settings(hwnd),
                ID_TRAY_AUTOSTART => toggle_autostart(hwnd),
                ID_TRAY_EXIT => {
                    let _ = send(hwnd, "shutdown");
                    unsafe { PostQuitMessage(0) };
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY => {
            match wparam.0 as u32 & 0xFFFF {
                WM_RBUTTONUP | WM_LBUTTONUP => unsafe { show_tray_menu(hwnd) },
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let app = app_of(hwnd);
            if app.tray {
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); };
            } else {
                let _ = send(hwnd, "shutdown");
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    let tip = wide("远程控制服务");
    for (dst, src) in nid.szTip.iter_mut().zip(tip.iter()) {
        *dst = *src;
    }
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let show = wide("显示窗口");
    let settings = wide("设置");
    let auto = match autostart::query() {
        autostart::Autostart::Enabled { .. } => wide("开机自动启动：已开启（点击关闭）"),
        _ => wide("开机自动启动：已关闭（点击开启）"),
    };
    let exit = wide("退出（同时停止服务）");
    let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_SHOW, PCWSTR::from_raw(show.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_SETTINGS, PCWSTR::from_raw(settings.as_ptr()));
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_CHECKED,
        ID_TRAY_AUTOSTART,
        PCWSTR::from_raw(auto.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_EXIT, PCWSTR::from_raw(exit.as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(HMENU(menu.0));
}

fn toggle_autostart(hwnd: HWND) {
    let enabled = matches!(autostart::query(), autostart::Autostart::Enabled { .. });
    let r = if enabled {
        autostart::disable().map(|_| "已取消开机自动启动")
    } else {
        let exe = std::env::current_exe().unwrap_or_default();
        let cfg = unsafe { app_of(hwnd) }.config_path.clone();
        autostart::enable(&exe, &cfg).map(|_| "已开启开机自动启动")
    };
    if let Err(e) = r {
        log::warn!("切换自启失败: {e}");
    }
}

fn open_settings(hwnd: HWND) {
    let p = unsafe { app_of(hwnd) }.config_path.clone();
    let _ = std::process::Command::new("notepad").arg(&p).spawn();
}

fn create_child(
    parent: HWND,
    class: &str,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
) -> HWND {
    let extra = match class {
        "EDIT" => {
            let es = (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | WS_VSCROLL.0 as i32) as u32;
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(es)
        }
        _ => WS_BORDER,
    };
    let class_w = wide(class);
    let text_w = wide(text);
    unsafe {
        let hwnd = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(class_w.as_ptr()),
            PCWSTR::from_raw(text_w.as_ptr()),
            WS_CHILD | WS_VISIBLE | extra,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as *mut core::ffi::c_void)),
            None,
            None,
        )
        .unwrap_or_default();
        if class != "EDIT" {
            let mut font = GetStockObject(DEFAULT_GUI_FONT);
            if font.is_invalid() {
                font = GetStockObject(SYSTEM_FONT);
            }
            let _ = SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
        hwnd
    }
}

fn send(hwnd: HWND, cmd: &str) -> Result<String, String> {
    let addr = unsafe { app_of(hwnd) }.ipc_addr;
    ipc::call(addr, cmd, Duration::from_secs(6))
}

fn service_enabled(hwnd: HWND) -> bool {
    let raw = send(hwnd, "status").unwrap_or_default();
    raw.lines()
        .find(|l| l.starts_with("service_enabled="))
        .map(|l| l.contains("true"))
        .unwrap_or_else(|| {
            raw.lines()
                .find(|l| l.starts_with("accept_clients="))
                .map(|l| l.contains("true"))
                .unwrap_or(true)
        })
}

fn set_button_text(hwnd: HWND, id: usize, text: &str) {
    let w = wide(text);
    unsafe {
        let btn = GetDlgItem(Some(hwnd), id as i32).unwrap_or_default();
        let _ = SetWindowTextW(btn, PCWSTR::from_raw(w.as_ptr()));
    }
}

fn refresh(hwnd: HWND) {
    let raw = match send(hwnd, "status") {
        Ok(t) => t,
        Err(_) => String::new(),
    };
    let enabled = service_enabled(hwnd);
    set_button_text(hwnd, ID_BTN_TOGGLE, if enabled { "暂停服务" } else { "启动服务" });

    let tunnel_running = raw
        .lines()
        .find(|l| l.starts_with("tunnel.running="))
        .map(|l| l.contains("true"))
        .unwrap_or(false);
    set_button_text(hwnd, ID_BTN_TUNNEL, if tunnel_running { "断开远程" } else { "连接远程" });

    let text = if raw.is_empty() {
        String::from("无法连接后台服务。\r\n\r\n请确认程序正在运行，或检查设置。")
    } else {
        render(&raw)
    };
    let w = wide(&text);
    unsafe {
        let child = GetDlgItem(Some(hwnd), ID_STATUS as i32).unwrap_or_default();
        let _ = SetWindowTextW(child, PCWSTR::from_raw(w.as_ptr()));
    }
}

/// 面向普通用户的状态文案：先说结论，再给细节。
fn render(raw: &str) -> String {
    let mut map = std::collections::HashMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let get = |k: &str| map.get(k).cloned().unwrap_or_else(|| String::from("-"));

    let enabled = match get("service_enabled").as_str() {
        "-" => get("accept_clients") == "true",
        v => v == "true",
    };
    let healthy = get("host.healthy") == "true";
    let tunnel_running = get("tunnel.running") == "true";
    let tunnel_healthy = get("tunnel.healthy") == "true";

    // 主状态一句话
    let headline = if !enabled {
        "服务已暂停"
    } else if healthy {
        "服务运行中"
    } else {
        "服务正在恢复…"
    };

    let secs = get("supervisor.uptime_s").parse::<u64>().unwrap_or(0);
    let uptime = if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(headline.to_string());
    lines.push(format!("已运行        {uptime}"));
    lines.push(String::new());

    // 画面服务
    let stream = if !enabled {
        "已暂停"
    } else if healthy {
        "正常"
    } else {
        "正在恢复"
    };
    lines.push(format!("画面服务      {stream}"));
    // 远程连接
    let remote: String = if !enabled {
        String::from("已暂停")
    } else if tunnel_running && tunnel_healthy {
        format!("已连接（{}）", get("tunnel.remote"))
    } else if tunnel_running {
        String::from("正在连接…")
    } else {
        String::from("未连接")
    };
    lines.push(format!("远程连接      {remote}"));

    // 自动修复次数只在异常时显示，避免噪音
    let restarts = get("host.restarts").parse::<u64>().unwrap_or(0);
    if restarts > 0 {
        lines.push(format!("自动修复      {restarts} 次"));
    }
    let reconnects = get("tunnel.reconnects").parse::<u64>().unwrap_or(0);
    if reconnects > 0 {
        lines.push(format!("远程重连      {reconnects} 次"));
    }

    // 版本状态只在需要用户行动时显示
    if get("host.exe_freshness") == "stale" {
        lines.push(String::new());
        lines.push("提示：程序已更新，请点「重启服务」生效".to_string());
    }

    // 错误信息：技术细节收在这里。
    // 注意：服务暂停时 host/隧道本来就是停的，探测必然失败——
    // 那些错误是"暂停"的正常结果，不该报给用户看，否则会以为出故障了。
    let err = if enabled { get("host.last_error") } else { String::from("-") };
    let terr = if enabled { get("tunnel.last_error") } else { String::from("-") };
    if (err != "-" && !err.is_empty()) || (terr != "-" && !terr.is_empty()) {
        lines.push(String::new());
        lines.push("详细信息：".to_string());
        if err != "-" && !err.is_empty() {
            lines.push(format!("  画面服务：{err}"));
        }
        if terr != "-" && !terr.is_empty() {
            lines.push(format!("  远程连接：{terr}"));
        }
    }

    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(enabled: bool, healthy: bool) -> String {
        format!(
            "service_enabled={enabled}\naccept_clients={enabled}\nhost.healthy={healthy}\nhost.running={healthy}\nhost.probe_ms=12\nhost.restarts=0\nsupervisor.pid=42\nsupervisor.uptime_s=200\ntunnel.enabled=true\ntunnel.running=true\ntunnel.healthy=true\ntunnel.remote=yanfa2:17800\ntunnel.reconnects=0\nhost.exe_freshness=match\nhost.last_error=-\ntunnel.last_error=-\nhost.exe=C:\\x.exe\nhost.exe_built=2026-01-01T00:00\n"
        )
    }

    #[test]
    fn render_running_uses_plain_words() {
        let out = render(&sample(true, true));
        assert!(out.contains("服务运行中"), "{out}");
        assert!(out.contains("已连接（yanfa2:17800）"), "{out}");
        assert!(out.contains("3 分 20 秒"), "{out}");
        assert!(!out.contains("PID"), "{out}");
    }

    #[test]
    fn render_paused_is_clear() {
        let out = render(&sample(false, false));
        assert!(out.contains("服务已暂停"), "{out}");
        assert!(out.contains("已暂停"), "{out}");
    }

    #[test]
    fn render_stale_hints_restart() {
        let raw = sample(true, true).replace("host.exe_freshness=match", "host.exe_freshness=stale");
        let out = render(&raw);
        assert!(out.contains("重启服务"), "{out}");
    }

    #[test]
    fn render_hides_expected_errors_while_paused() {
        // 暂停时隧道关闭，探测必然超时；这是正常现象，不能当错误报给用户
        let raw = sample(false, false)
            .replace("tunnel.last_error=-", "tunnel.last_error=连接失败: connection timed out");
        let out = render(&raw);
        assert!(!out.contains("详细信息"), "暂停时不该显示错误详情: {out}");
        assert!(!out.contains("connection timed out"), "{out}");
    }

    #[test]
    fn render_shows_tunnel_error_while_running() {
        let raw = sample(true, true)
            .replace("tunnel.last_error=-", "tunnel.last_error=远端未监听");
        let out = render(&raw);
        assert!(out.contains("远端未监听"), "运行中出错必须显示: {out}");
    }

    #[test]
    fn render_uses_crlf() {
        let out = render(&sample(true, true));
        assert!(out.contains("\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn render_shows_error_details_only_when_present() {
        let out = render(&sample(true, true));
        assert!(!out.contains("详细信息"), "{out}");
        let raw = sample(true, false).replace("host.last_error=-", "host.last_error=连接失败");
        let out = render(&raw);
        assert!(out.contains("详细信息"), "{out}");
        assert!(out.contains("连接失败"), "{out}");
    }
}
