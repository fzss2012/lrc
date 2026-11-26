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
    GetCursorPos, GetDlgItem, GetMessageW, GetWindowLongPtrW, LoadCursorW,
    LoadIconW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowTextW, SetTimer, ShowWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY,
    GWLP_USERDATA, HMENU,
    IDC_ARROW, IDI_APPLICATION, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_SHOWNORMAL,
    TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_SETFONT, WM_TIMER, WM_USER, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE,
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
/// 远程视口：目标地址输入框
const ID_VIEW_ADDR: usize = 110;
/// 远程视口：打开/关闭
const ID_BTN_VIEW: usize = 207;

const ID_TRAY_SHOW: usize = 300;
const ID_TRAY_SETTINGS: usize = 301;
const ID_TRAY_AUTOSTART: usize = 302;
const ID_TRAY_EXIT: usize = 303;

struct App {
    ipc_addr: SocketAddr,
    config_path: PathBuf,
    tray: bool,
    icon_added: bool,
    /// 远程视口进程（lrc-client）。None = 没打开。
    viewer: std::cell::RefCell<Option<std::process::Child>>,
}

fn window_class() -> PCWSTR {
    windows::core::w!("lrc_panel_window_v3")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run(ipc_addr: SocketAddr, config_path: PathBuf, tray: bool) -> Result<(), String> {
    let app = Box::new(App {
        ipc_addr,
        config_path,
        tray,
        icon_added: false,
        viewer: std::cell::RefCell::new(None),
    });
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
            520,
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

            // 状态区（本机被控）
            create_child(hwnd, "READONLY", "", 12, 12, 600, 230, ID_STATUS);

            // ---- 本机：让别人连进来 ----
            create_child(hwnd, "STATIC", "本机允许被远程控制", 12, 250, 600, 20, 998);
            create_child(hwnd, "BUTTON", "暂停服务", 12, 272, 140, 36, ID_BTN_TOGGLE);
            create_child(hwnd, "BUTTON", "重启服务", 162, 272, 120, 36, ID_BTN_RESTART);
            create_child(hwnd, "BUTTON", "允许远程接入", 292, 272, 140, 36, ID_BTN_TUNNEL);
            create_child(hwnd, "BUTTON", "设置", 442, 272, 100, 36, ID_BTN_SETTINGS);

            // ---- 远程视口：连到别的电脑看画面 ----
            create_child(hwnd, "STATIC", "远程控制其他电脑", 12, 322, 600, 20, 997);
            create_child(hwnd, "INPUT", "", 12, 344, 380, 30, ID_VIEW_ADDR);
            create_child(hwnd, "BUTTON", "打开远程画面", 402, 344, 140, 30, ID_BTN_VIEW);

            create_child(
                hwnd,
                "STATIC",
                "关闭窗口会缩到托盘，服务继续在后台运行",
                12,
                386,
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
            sync_viewer_button(hwnd);
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
                ID_BTN_VIEW => toggle_viewer(hwnd),
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


/// 读取输入框里的地址。
fn viewer_addr(hwnd: HWND) -> String {
    // 用 WM_GETTEXT 读：GetWindowTextW 在跨进程/消息过滤场景下可能读不到控件内容
    // （实测在面板自身进程里也会返回 0，而 WM_GETTEXT 稳定）。
    const WM_GETTEXT: u32 = 0x000D;
    let mut buf = [0u16; 256];
    unsafe {
        let edit = GetDlgItem(Some(hwnd), ID_VIEW_ADDR as i32).unwrap_or_default();
        if edit.0.is_null() {
            log::warn!("找不到地址输入框控件");
            return String::new();
        }
        let len = SendMessageW(edit, 0x000E, Some(WPARAM(0)), Some(LPARAM(0)));
        let n = SendMessageW(
            edit,
            WM_GETTEXT,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        );
        log::info!(
            "addr read: hwnd={edit:?} textlen={} got={}",
            len.0,
            n.0
        );
        let n = n.0.max(0) as usize;
        String::from_utf16_lossy(&buf[..n.min(buf.len())])
            .trim()
            .to_string()
    }
}

/// 视口进程是否还活着（并在活着时回收句柄）。
fn viewer_running(hwnd: HWND) -> bool {
    let app = unsafe { app_of(hwnd) };
    let mut slot = app.viewer.borrow_mut();
    match slot.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                *slot = None;
                false
            }
        },
        None => false,
    }
}

fn sync_viewer_button(hwnd: HWND) {
    let running = viewer_running(hwnd);
    set_button_text(hwnd, ID_BTN_VIEW, if running { "关闭远程画面" } else { "打开远程画面" });
}

/// 打开/关闭远程视口。
///
/// 视口是独立的 lrc-client 窗口（它自己有完整的渲染/输入转发），
/// 面板只负责把它拉起来和关掉——这样两者互不影响，视口崩溃也不会拖垮服务。
fn toggle_viewer(hwnd: HWND) {
    if viewer_running(hwnd) {
        let app = unsafe { app_of(hwnd) };
        if let Some(mut child) = app.viewer.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
            log::info!("已关闭远程画面");
        }
        sync_viewer_button(hwnd);
        return;
    }

    let addr = viewer_addr(hwnd);
    log::info!("打开远程画面：addr=<{addr}>");
    if addr.is_empty() {
        show_message(
            hwnd,
            "请先填写要连接的对方地址，例如 192.168.1.10:7805",
        );
        return;
    }

    // 客户端和执行面板放在同一目录（产物是一起发布的）
    let client = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("lrc-client.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("lrc-client.exe"));
    if !client.is_file() {
        show_message(
            hwnd,
            &format!("找不到远程画面程序：\n{}\n\n请确认 lrc-client.exe 与本程序在同一目录。", client.display()),
        );
        return;
    }

    // 密码走环境变量，不出现在命令行（命令行对同机其它用户可见）
    let password = unsafe { app_of(hwnd) }
        .config_path
        .parent()
        .and_then(|_| read_config_password())
        .unwrap_or_default();

    let mut cmd = std::process::Command::new(&client);
    cmd.arg("--connect").arg(&addr);
    if !password.is_empty() {
        cmd.env("LRC_PASSWORD", &password);
    }
    match cmd.spawn() {
        Ok(child) => {
            log::info!("已打开远程画面：{addr}（pid={}）", child.id());
            unsafe { app_of(hwnd) }.viewer.borrow_mut().replace(child);
        }
        Err(e) => show_message(hwnd, &format!("打开远程画面失败：{e}")),
    }
    sync_viewer_button(hwnd);
}

/// 从配置文件里取 password（视口要和本机服务用同一个密码）。
fn read_config_password() -> Option<String> {
    let text = std::fs::read_to_string(lrc_supervisor::config_file::config_path()).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "password" {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 在状态区显示一条提示（不弹模态框——模态框会卡住整个消息循环，
/// 用户不点掉之前面板完全不响应，实测很难受）。
fn show_message(hwnd: HWND, text: &str) {
    log::warn!("{text}");
    let mut msg = String::from(text);
    if !msg.ends_with('\n') {
        msg.push('\n');
    }
    let w = wide(&msg);
    unsafe {
        let child = GetDlgItem(Some(hwnd), ID_STATUS as i32).unwrap_or_default();
        let _ = SetWindowTextW(child, PCWSTR::from_raw(w.as_ptr()));
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
        // 状态区：只读多行 EDIT。
        // 注意必须区分"只读展示"和"可输入"两种 EDIT——
        // 之前统一按只读多行建，导致地址输入框用户打不进字（实测）。
        "READONLY" => {
            let es = (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | WS_VSCROLL.0 as i32) as u32;
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(es)
        }
        // 输入框：单行可编辑
        "INPUT" => {
            let es = (ES_AUTOHSCROLL | WS_TABSTOP.0 as i32) as u32;
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(es)
        }
        _ => WS_BORDER,
    };
    // 我们自己的类别名 -> 真正的 Win32 窗口类
    let win_class = match class {
        "READONLY" | "INPUT" => "EDIT",
        other => other,
    };
    let class_w = wide(win_class);
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
        if class != "READONLY" {
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
    // 这个开关控制的是"别人能不能连进来"（反向隧道），
    // 和下面"远程控制其他电脑"（我去连别人）是两回事，文案必须区分开。
    set_button_text(
        hwnd,
        ID_BTN_TUNNEL,
        if tunnel_running { "停止远程接入" } else { "允许远程接入" },
    );

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
    let busy = get("host.busy") == "true";
    let tunnel_running = get("tunnel.running") == "true";
    let tunnel_healthy = get("tunnel.healthy") == "true";

    // 主状态一句话。busy = 有客户端正连着（单流 host 的正常状态）
    let headline = if !enabled {
        "服务已暂停"
    } else if busy {
        "正在被远程控制"
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
    } else if busy {
        "使用中"
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
            "service_enabled={enabled}\naccept_clients={enabled}\nhost.healthy={healthy}\nhost.busy=false\nhost.running={healthy}\nhost.probe_ms=12\nhost.restarts=0\nsupervisor.pid=42\nsupervisor.uptime_s=200\ntunnel.enabled=true\ntunnel.running=true\ntunnel.healthy=true\ntunnel.remote=yanfa2:17800\ntunnel.reconnects=0\nhost.exe_freshness=match\nhost.last_error=-\ntunnel.last_error=-\nhost.exe=C:\\x.exe\nhost.exe_built=2026-01-01T00:00\n"
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
    fn render_busy_is_not_treated_as_failure() {
        // host 单流：有客户端连上时探测必失败，但那是"正在服务"不是故障。
        // 如果面板把它显示成"正在恢复"，用户会以为服务坏了。
        let raw = sample(true, true).replace("host.busy=false", "host.busy=true");
        let out = render(&raw);
        assert!(out.contains("正在被远程控制"), "{out}");
        assert!(!out.contains("正在恢复"), "{out}");
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
