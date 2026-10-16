use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{Emitter, Manager, PhysicalPosition, WebviewWindow};

mod antigravity_hooks;
mod antigravity_logs;
mod approvals;
mod bus;
mod codex_logs;
mod focus;
mod media_controls;
mod notification_filter;
mod system_monitor;
mod win_notifications;

#[derive(Default)]
struct AppState {
    paused: AtomicBool,
    quiet_mode: AtomicBool,
    allow_during_fullscreen: std::sync::Arc<AtomicBool>,
    focus_session_active: std::sync::Arc<AtomicBool>,
    system_panel_open: std::sync::Arc<AtomicBool>,
    codex_enabled: std::sync::Arc<AtomicBool>,
    hit_area: Mutex<Option<HitArea>>,
    tray: Mutex<Option<TrayIcon>>,
    approvals: std::sync::Arc<approvals::ApprovalRegistry>,
}

#[derive(Clone, Copy, Deserialize)]
struct HitArea {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorOption {
    key: String,
    label: String,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

fn monitor_key(monitor: &tauri::Monitor) -> String {
    let position = monitor.position();
    format!(
        "{}:{}:{}",
        monitor.name().map_or("Display", String::as_str),
        position.x,
        position.y
    )
}

impl HitArea {
    fn contains_with_margin(&self, x: f64, y: f64, margin: f64) -> bool {
        x >= self.x - margin
            && y >= self.y - margin
            && x < self.x + self.width + margin
            && y < self.y + self.height + margin
    }
}

#[tauri::command]
fn update_hit_area(state: tauri::State<'_, AppState>, area: HitArea) {
    if area.x.is_finite()
        && area.y.is_finite()
        && area.width.is_finite()
        && area.height.is_finite()
        && area.width > 0.0
        && area.height > 0.0
    {
        if let Ok(mut hit_area) = state.hit_area.lock() {
            *hit_area = Some(area);
        }
    }
}

#[tauri::command]
fn set_system_panel_open(state: tauri::State<'_, AppState>, open: bool) {
    state.system_panel_open.store(open, Ordering::Relaxed);
}

#[tauri::command]
fn focus_process(pid: u32) -> Result<(), String> {
    focus::focus_process(pid)
}

#[tauri::command]
async fn get_now_playing(allow_browser_media: bool) -> Option<media_controls::NowPlaying> {
    tauri::async_runtime::spawn_blocking(move || media_controls::current(allow_browser_media))
        .await
        .ok()
        .flatten()
}

#[tauri::command]
async fn control_now_playing(action: String, allow_browser_media: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        media_controls::control(&action, allow_browser_media)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn open_whatsapp() -> Result<(), String> {
    Command::new("explorer.exe")
        .arg("whatsapp://")
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn respond_permission(
    state: tauri::State<'_, AppState>,
    event_id: String,
    action: String,
    nonce: String,
) -> Result<(), String> {
    let decision = match action.as_str() {
        "approve" => approvals::ApprovalDecision::Allow,
        "reject" => approvals::ApprovalDecision::Deny,
        _ => return Err("Unknown permission action".into()),
    };
    state.approvals.complete(&event_id, &nonce, decision)
}

#[tauri::command]
fn dismiss_permission(
    state: tauri::State<'_, AppState>,
    event_id: String,
    nonce: String,
) -> Result<(), String> {
    state.approvals.dismiss(&event_id, &nonce)
}

#[tauri::command]
fn get_antigravity_hooks_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    antigravity_hooks::is_enabled(&app)
}

#[tauri::command]
fn set_antigravity_hooks_enabled(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    antigravity_hooks::set_enabled(&app, enabled)
}

#[tauri::command]
fn set_codex_enabled(state: tauri::State<'_, AppState>, enabled: bool) {
    state.codex_enabled.store(enabled, Ordering::Release);
}

fn place_on_work_area(
    window: &WebviewWindow,
    corner: &Corner,
    requested_monitor_key: Option<&str>,
) -> tauri::Result<()> {
    let monitors = window.available_monitors()?;
    let monitor = requested_monitor_key
        .and_then(|key| monitors.iter().find(|monitor| monitor_key(monitor) == key))
        .cloned()
        .or(window.primary_monitor()?);
    let Some(monitor) = monitor else {
        return Ok(());
    };
    let work_area = monitor.work_area();
    let scale = monitor.scale_factor();
    let window_size = window.outer_size()?;
    let margin = (20.0 * scale).round() as i32;
    let (x, y) = position_for(
        (
            work_area.position.x,
            work_area.position.y,
            work_area.size.width,
            work_area.size.height,
        ),
        (window_size.width, window_size.height),
        margin,
        corner,
    );
    window.set_position(PhysicalPosition::new(x, y))
}

fn position_for(
    area: (i32, i32, u32, u32),
    window_size: (u32, u32),
    margin: i32,
    corner: &Corner,
) -> (i32, i32) {
    let (left, top, area_width, area_height) = area;
    let (width, height) = window_size;
    let x = if matches!(corner, Corner::TopLeft | Corner::BottomLeft) {
        left + margin
    } else {
        left + area_width as i32 - width as i32 - margin
    };
    let y = if matches!(corner, Corner::BottomLeft | Corner::BottomRight) {
        top + area_height as i32 - height as i32 - margin
    } else {
        top + margin
    };
    (x, y)
}

#[tauri::command]
fn list_monitors(window: WebviewWindow) -> tauri::Result<Vec<MonitorOption>> {
    let primary_key = window.primary_monitor()?.as_ref().map(monitor_key);
    window.available_monitors().map(|mut monitors| {
        monitors.sort_by_key(|monitor| {
            !primary_key
                .as_ref()
                .is_some_and(|primary| monitor_key(monitor) == *primary)
        });
        monitors
            .into_iter()
            .map(|monitor| {
                let position = monitor.position();
                let size = monitor.size();
                MonitorOption {
                    key: monitor_key(&monitor),
                    label: monitor
                        .name()
                        .cloned()
                        .unwrap_or_else(|| format!("Display {},{}", position.x, position.y)),
                    width: size.width,
                    height: size.height,
                }
            })
            .collect()
    })
}

#[tauri::command]
fn set_placement(
    window: WebviewWindow,
    corner: Corner,
    monitor_key: Option<String>,
) -> tauri::Result<()> {
    place_on_work_area(&window, &corner, monitor_key.as_deref())
}

#[tauri::command]
fn set_auto_start(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut command = Command::new("reg.exe");
        if enabled {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let value = format!("\"{}\" --hidden", executable.display());
            command.args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "DotNotifier",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ]);
            let status = command.status().map_err(|error| error.to_string())?;
            if !status.success() {
                return Err(format!(
                    "Could not enable auto-start (exit code {:?})",
                    status.code()
                ));
            }
        } else {
            command.args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "DotNotifier",
                "/f",
            ]);
            let _ = command.status();
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("Auto-start is available on Windows only".into())
    }
}

fn auto_start_command_matches(registry_output: &str, executable: &str) -> bool {
    let expected = format!("\"{executable}\" --hidden");
    registry_output.lines().any(|line| {
        line.split_once("REG_SZ")
            .is_some_and(|(_, command)| command.trim().eq_ignore_ascii_case(&expected))
    })
}

#[tauri::command]
fn get_auto_start() -> Result<bool, String> {
    #[cfg(windows)]
    {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let output = Command::new("reg.exe")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "DotNotifier",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Ok(false);
        }
        Ok(auto_start_command_matches(
            &String::from_utf8_lossy(&output.stdout),
            &executable.to_string_lossy(),
        ))
    }
    #[cfg(not(windows))]
    {
        Err("Auto-start is available on Windows only".into())
    }
}

#[tauri::command]
fn update_tray_status(
    state: tauri::State<'_, AppState>,
    running: u32,
    waiting: u32,
) -> Result<(), String> {
    let status = format!("{running} running, {waiting} waiting");
    let tray = state
        .tray
        .lock()
        .map_err(|_| "Tray state is unavailable".to_owned())?;
    if let Some(tray) = tray.as_ref() {
        tray.set_tooltip(Some(status))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn set_monitored_apps(app: tauri::AppHandle, mut apps: Vec<String>) -> Result<(), String> {
    if apps.len() > 100 || apps.iter().any(|name| name.len() > 128) {
        return Err("Notification filter is too large".into());
    }
    apps = apps
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    if apps.is_empty() {
        apps.push("WhatsApp".into());
    }
    apps.sort_by_key(|name| name.to_lowercase());
    apps.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let destination = directory.join("notification-filter.json");
    let temporary = directory.join("notification-filter.json.tmp");
    let contents = serde_json::to_vec_pretty(&serde_json::json!({ "monitoredApps": apps }))
        .map_err(|error| error.to_string())?;
    std::fs::write(&temporary, contents).map_err(|error| error.to_string())?;
    std::fs::rename(temporary, destination).map_err(|error| error.to_string())
}

#[tauri::command]
fn check_notification_permission() -> bool {
    win_notifications::check_permission()
}

#[tauri::command]
fn request_notification_permission() -> Result<bool, String> {
    win_notifications::request_permission()
}

fn start_quiet_environment_poll(
    app: tauri::AppHandle,
    focus_session_active: std::sync::Arc<AtomicBool>,
    allow_during_fullscreen: std::sync::Arc<AtomicBool>,
) {
    #[cfg(windows)]
    {
        thread::spawn(move || {
            let focus_session_supported = initialize_focus_runtime();
            let mut previous = None;
            loop {
                let focus_active = focus_session_supported && windows_focus_session_active();
                focus_session_active.store(focus_active, Ordering::Relaxed);
                let quiet =
                    windows_quiet_environment(allow_during_fullscreen.load(Ordering::Relaxed))
                        || focus_active;
                if previous != Some(quiet) {
                    let _ = app.emit("quiet-environment", quiet);
                    previous = Some(quiet);
                }
                thread::sleep(Duration::from_millis(750));
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (app, focus_session_active, allow_during_fullscreen);
    }
}

#[cfg(windows)]
fn initialize_focus_runtime() -> bool {
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    // The monitor owns this dedicated thread, so initializing its WinRT apartment is safe.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok()
}

#[cfg(windows)]
fn windows_focus_session_active() -> bool {
    use windows::UI::Shell::FocusSessionManager;
    FocusSessionManager::IsSupported()
        .ok()
        .filter(|supported| *supported)
        .and_then(|_| FocusSessionManager::GetDefault().ok())
        .and_then(|manager| manager.IsFocusActive().ok())
        .unwrap_or(false)
}

#[tauri::command]
fn is_quiet_environment(allow_during_fullscreen: bool, state: tauri::State<'_, AppState>) -> bool {
    #[cfg(windows)]
    {
        windows_quiet_environment(allow_during_fullscreen)
            || state.focus_session_active.load(Ordering::Relaxed)
    }
    #[cfg(not(windows))]
    {
        let _ = (allow_during_fullscreen, state);
        false
    }
}

#[cfg(windows)]
fn windows_quiet_environment(allow_during_fullscreen: bool) -> bool {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHQueryUserNotificationState(state: *mut u32) -> i32;
    }
    let mut state = 0;
    // HRESULT is zero on success; unsupported states fail open so the overlay remains useful.
    let succeeded = unsafe { SHQueryUserNotificationState(&mut state) } >= 0;
    succeeded
        && (matches!(state, 1 | 4 | 6) || (!allow_during_fullscreen && matches!(state, 2 | 3)))
}

#[tauri::command]
fn set_allow_during_fullscreen(enabled: bool, state: tauri::State<'_, AppState>) {
    state
        .allow_during_fullscreen
        .store(enabled, Ordering::Relaxed);
}

fn start_cursor_poll(window: WebviewWindow) {
    let app = window.app_handle().clone();
    thread::spawn(move || {
        let mut last_ignore = None;
        let mut inside = false;
        loop {
            let current_state = app.state::<AppState>();
            let area = current_state.hit_area.lock().ok().and_then(|area| *area);
            let margin = if inside { 8.0 } else { 4.0 };
            let next_inside = area
                .and_then(|area| {
                    let cursor = global_cursor_position()?;
                    let origin = window.outer_position().ok()?;
                    let scale = window.scale_factor().ok()?;
                    let local_x = (cursor.0 - f64::from(origin.x)) / scale;
                    let local_y = (cursor.1 - f64::from(origin.y)) / scale;
                    Some(area.contains_with_margin(local_x, local_y, margin))
                })
                .unwrap_or(false);
            inside = next_inside;
            let ignore = !next_inside;
            if last_ignore != Some(ignore) {
                if let Err(error) = window.set_ignore_cursor_events(ignore) {
                    eprintln!("Could not update overlay click-through state: {error}");
                }
                last_ignore = Some(ignore);
            }
            thread::sleep(Duration::from_millis(60));
        }
    });
}

#[cfg(windows)]
fn global_cursor_position() -> Option<(f64, f64)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetCursorPos(point: *mut Point) -> i32;
    }

    let mut point = Point { x: 0, y: 0 };
    // GetCursorPos writes the screen-space cursor coordinates into the supplied point.
    let succeeded = unsafe { GetCursorPos(&mut point) } != 0;
    succeeded.then_some((f64::from(point.x), f64::from(point.y)))
}

#[cfg(not(windows))]
fn global_cursor_position() -> Option<(f64, f64)> {
    None
}

fn tray_image_for(theme: tauri::Theme, scale_factor: f64) -> Option<Image<'static>> {
    let size = tray_icon_size(scale_factor);
    let pixels: &[u8] = match (theme, size) {
        (tauri::Theme::Light, 16) => include_bytes!("../icons/tray-light/16x16.rgba"),
        (tauri::Theme::Light, 20) => include_bytes!("../icons/tray-light/20x20.rgba"),
        (tauri::Theme::Light, 24) => include_bytes!("../icons/tray-light/24x24.rgba"),
        (tauri::Theme::Light, _) => include_bytes!("../icons/tray-light/32x32.rgba"),
        (tauri::Theme::Dark, 16) => include_bytes!("../icons/tray-dark/16x16.rgba"),
        (tauri::Theme::Dark, 20) => include_bytes!("../icons/tray-dark/20x20.rgba"),
        (tauri::Theme::Dark, 24) => include_bytes!("../icons/tray-dark/24x24.rgba"),
        (tauri::Theme::Dark, _) => include_bytes!("../icons/tray-dark/32x32.rgba"),
        (_, 16) => include_bytes!("../icons/tray-dark/16x16.rgba"),
        (_, 20) => include_bytes!("../icons/tray-dark/20x20.rgba"),
        (_, 24) => include_bytes!("../icons/tray-dark/24x24.rgba"),
        _ => include_bytes!("../icons/tray-dark/32x32.rgba"),
    };
    Some(Image::new(pixels, size, size))
}

fn tray_icon_size(scale_factor: f64) -> u32 {
    let desired = (16.0 * scale_factor).round() as u32;
    [16u32, 20, 24, 32]
        .into_iter()
        .min_by_key(|size| (*size).abs_diff(desired))
        .unwrap_or(16)
}

fn refresh_tray_icon(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let theme = window.theme().unwrap_or(tauri::Theme::Dark);
    let scale_factor = window.scale_factor().unwrap_or(1.0);
    let state = app.state::<AppState>();
    let Ok(tray) = state.tray.lock() else {
        return;
    };
    if let Some(tray) = tray.as_ref() {
        if let Some(icon) = tray_image_for(theme, scale_factor) {
            if let Err(error) = tray.set_icon(Some(icon)) {
                eprintln!("Could not update tray icon: {error}");
            }
        }
    }
}

fn set_tool_window(window: &WebviewWindow) -> tauri::Result<()> {
    #[cfg(windows)]
    {
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_APPWINDOW: isize = 0x0004_0000;

        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowLongPtrW(window: *mut std::ffi::c_void, index: i32) -> isize;
            fn SetWindowLongPtrW(window: *mut std::ffi::c_void, index: i32, value: isize) -> isize;
        }

        let hwnd = window.hwnd()?;
        let style = unsafe { GetWindowLongPtrW(hwnd.0, GWL_EXSTYLE) };
        let style = (style | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
        unsafe { SetWindowLongPtrW(hwnd.0, GWL_EXSTYLE, style) };
    }
    Ok(())
}

#[cfg(windows)]
fn initialize_single_instance() -> Result<Option<usize>, String> {
    use std::ptr::null_mut;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const EVENT_MODIFY_STATE: u32 = 0x0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(
            attributes: *mut std::ffi::c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut std::ffi::c_void;
        fn CreateMutexW(
            attributes: *mut std::ffi::c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut std::ffi::c_void;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut std::ffi::c_void;
        fn SetEvent(event: *mut std::ffi::c_void) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    let event_name: Vec<u16> = "Local\\DotNotifier.Settings.v1\0".encode_utf16().collect();
    let mutex_name: Vec<u16> = "Local\\DotNotifier.Instance.v1\0".encode_utf16().collect();
    let event = unsafe { CreateEventW(null_mut(), 1, 0, event_name.as_ptr()) };
    if event.is_null() {
        return Err("Could not create the single-instance event".into());
    }
    let mutex = unsafe { CreateMutexW(null_mut(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        unsafe { CloseHandle(event) };
        return Err("Could not create the single-instance mutex".into());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let existing_event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if !existing_event.is_null() {
            unsafe {
                SetEvent(existing_event);
                CloseHandle(existing_event);
            }
        }
        unsafe {
            CloseHandle(event);
            CloseHandle(mutex);
        }
        return Ok(None);
    }

    // Raw handles have no Rust destructor; leaving them open keeps the names alive until exit.
    let _mutex = mutex;
    Ok(Some(event as usize))
}

#[cfg(not(windows))]
fn initialize_single_instance() -> Result<Option<usize>, String> {
    Ok(Some(0))
}

#[cfg(windows)]
fn start_single_instance_listener(app: tauri::AppHandle, event: usize) {
    thread::spawn(move || {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
            fn ResetEvent(event: *mut std::ffi::c_void) -> i32;
        }

        let event = event as *mut std::ffi::c_void;
        loop {
            if unsafe { WaitForSingleObject(event, u32::MAX) } == 0 {
                unsafe { ResetEvent(event) };
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                }
                let _ = app.emit("tray-controls", serde_json::json!({ "openSettings": true }));
            } else {
                break;
            }
        }
    });
}

fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let pause = CheckMenuItem::with_id(
        app,
        "pause",
        "Pause notifications",
        true,
        false,
        None::<&str>,
    )?;
    let quiet = CheckMenuItem::with_id(app, "quiet", "Quiet mode", true, false, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Toggle overlay", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Open settings", true, None::<&str>)?;
    let config = MenuItem::with_id(app, "config", "Open config folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let hint_path = app.path().app_local_data_dir()?.join("tray-pin-hint-seen");
    let pin_hint = if hint_path.exists() {
        None
    } else {
        Some(MenuItem::with_id(
            app,
            "pin_hint",
            "Windows 11: drag this icon from ^ to the taskbar",
            true,
            None::<&str>,
        )?)
    };
    let menu = Menu::new(app)?;
    if let Some(pin_hint) = pin_hint.as_ref() {
        menu.append(pin_hint)?;
        menu.append(&separator)?;
    }
    menu.append_items(&[&show, &pause, &quiet])?;
    menu.append(&separator)?;
    menu.append_items(&[&settings, &config, &quit])?;
    let event_menu = menu.clone();
    let event_pin_hint = pin_hint.clone();

    let icon = app
        .get_webview_window("main")
        .and_then(|window| tray_image_for(window.theme().ok()?, window.scale_factor().ok()?))
        .or_else(|| app.default_window_icon().cloned());
    let mut tray = TrayIconBuilder::new()
        .menu(&menu)
        .on_menu_event(move |app, event| {
            let state = app.state::<AppState>();
            match event.id().as_ref() {
                "pin_hint" => {
                    if let Some(parent) = hint_path.parent() {
                        if std::fs::create_dir_all(parent).is_ok()
                            && std::fs::write(&hint_path, b"seen").is_ok()
                        {
                            if let Some(item) = event_pin_hint.as_ref() {
                                let _ = event_menu.remove(item);
                            }
                        }
                    }
                }
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let is_visible = window.is_visible().unwrap_or(false);
                        if is_visible {
                            let _ = window.hide();
                        } else {
                            let _ = window.show();
                        }
                    }
                }
                "pause" => {
                    let paused = !state.paused.load(Ordering::Relaxed);
                    state.paused.store(paused, Ordering::Relaxed);
                    let _ = pause.set_checked(paused);
                    let _ = app.emit("tray-controls", serde_json::json!({ "paused": paused }));
                }
                "quiet" => {
                    let quiet_mode = !state.quiet_mode.load(Ordering::Relaxed);
                    state.quiet_mode.store(quiet_mode, Ordering::Relaxed);
                    let _ = quiet.set_checked(quiet_mode);
                    let _ = app.emit(
                        "tray-controls",
                        serde_json::json!({ "quietMode": quiet_mode }),
                    );
                }
                "settings" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                    let _ = app.emit("tray-controls", serde_json::json!({ "openSettings": true }));
                }
                "config" => {
                    if let Ok(directory) = app.path().app_config_dir() {
                        if std::fs::create_dir_all(&directory).is_ok() {
                            let _ = Command::new("explorer.exe").arg(directory).spawn();
                        }
                    }
                }
                "quit" => app.exit(0),
                _ => {}
            }
        });
    if let Some(icon) = icon {
        tray = tray.icon(icon);
    }
    let tray = tray.tooltip("0 running, 0 waiting").build(app)?;
    if let Ok(mut stored) = app.state::<AppState>().tray.lock() {
        *stored = Some(tray);
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let instance_event = match initialize_single_instance() {
        Ok(Some(event)) => event,
        Ok(None) => return,
        Err(error) => {
            eprintln!("Could not initialize single-instance mode: {error}");
            return;
        }
    };
    let hidden_start = std::env::args_os().any(|argument| argument == "--hidden");
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            update_hit_area,
            set_system_panel_open,
            focus_process,
            get_now_playing,
            control_now_playing,
            open_whatsapp,
            respond_permission,
            dismiss_permission,
            get_antigravity_hooks_enabled,
            set_antigravity_hooks_enabled,
            set_codex_enabled,
            list_monitors,
            set_placement,
            set_auto_start,
            get_auto_start,
            set_monitored_apps,
            is_quiet_environment,
            set_allow_during_fullscreen,
            check_notification_permission,
            request_notification_permission,
            update_tray_status
        ])
        .setup(move |app| {
            #[cfg(windows)]
            start_single_instance_listener(app.handle().clone(), instance_event);
            #[cfg(not(windows))]
            let _ = instance_event;

            #[cfg(windows)]
            {
                #[link(name = "shell32")]
                unsafe extern "system" {
                    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
                }
                let app_id: Vec<u16> = "com.dotnotifier.app\0".encode_utf16().collect();
                unsafe { SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr()) };
            }
            start_quiet_environment_poll(
                app.handle().clone(),
                app.state::<AppState>().focus_session_active.clone(),
                app.state::<AppState>().allow_during_fullscreen.clone(),
            );
            system_monitor::start(
                app.handle().clone(),
                app.state::<AppState>().system_panel_open.clone(),
            );
            codex_logs::start(
                app.handle().clone(),
                app.state::<AppState>().codex_enabled.clone(),
            );
            antigravity_logs::start(app.handle().clone());
            win_notifications::start(app.handle().clone());
            match app.path().app_local_data_dir() {
                Ok(directory) => {
                    if let Err(error) = bus::start(
                        app.handle().clone(),
                        &directory,
                        app.state::<AppState>().approvals.clone(),
                        app.state::<AppState>().codex_enabled.clone(),
                    ) {
                        eprintln!("Could not start local event bus: {error}");
                    }
                }
                Err(error) => eprintln!("Could not resolve event bus session directory: {error}"),
            }
            if let Some(window) = app.get_webview_window("main") {
                if let Err(error) = set_tool_window(&window) {
                    eprintln!("Could not mark overlay as a tool window: {error}");
                }
                if let Err(error) = place_on_work_area(&window, &Corner::TopRight, None) {
                    eprintln!("Could not place overlay in the work area: {error}");
                }
                if let Err(error) = window.set_ignore_cursor_events(true) {
                    eprintln!("Could not enable overlay click-through: {error}");
                }
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if matches!(
                        event,
                        tauri::WindowEvent::ScaleFactorChanged { .. }
                            | tauri::WindowEvent::ThemeChanged(_)
                    ) {
                        refresh_tray_icon(&app_handle);
                    }
                });
                start_cursor_poll(window.clone());
                if !hidden_start {
                    let _ = window.show();
                }
            }
            setup_tray(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{auto_start_command_matches, position_for, tray_icon_size, Corner, HitArea};

    #[test]
    fn auto_start_rejects_stale_or_unhidden_registry_commands() {
        let executable = r"D:\Dot Notifier\dot-notifier.exe";
        assert!(auto_start_command_matches(
            "DotNotifier    REG_SZ    \"D:\\Dot Notifier\\dot-notifier.exe\" --hidden",
            executable,
        ));
        assert!(!auto_start_command_matches(
            "DotNotifier    REG_SZ    \"C:\\Program Files\\Dot Notifier\\dot-notifier.exe\"",
            executable,
        ));
        assert!(!auto_start_command_matches(
            "DotNotifier    REG_SZ    \"D:\\Dot Notifier\\dot-notifier.exe\"",
            executable,
        ));
    }

    #[test]
    fn tray_icon_size_tracks_common_windows_dpi_scales() {
        assert_eq!(tray_icon_size(1.0), 16);
        assert_eq!(tray_icon_size(1.25), 20);
        assert_eq!(tray_icon_size(1.5), 24);
        assert_eq!(tray_icon_size(2.0), 32);
    }

    #[test]
    fn hit_area_includes_top_left_and_excludes_bottom_right() {
        let area = HitArea {
            x: 10.0,
            y: 20.0,
            width: 40.0,
            height: 30.0,
        };

        assert!(area.contains_with_margin(10.0, 20.0, 0.0));
        assert!(area.contains_with_margin(49.9, 49.9, 0.0));
        assert!(!area.contains_with_margin(50.0, 50.0, 0.0));
        assert!(!area.contains_with_margin(9.9, 20.0, 0.0));
    }

    #[test]
    fn hit_area_margin_supports_distinct_enter_and_leave_hysteresis() {
        let area = HitArea {
            x: 10.0,
            y: 20.0,
            width: 40.0,
            height: 30.0,
        };

        assert!(area.contains_with_margin(6.5, 25.0, 4.0));
        assert!(!area.contains_with_margin(5.5, 25.0, 4.0));
        assert!(area.contains_with_margin(2.5, 25.0, 8.0));
        assert!(!area.contains_with_margin(1.5, 25.0, 8.0));
    }

    #[test]
    fn placement_uses_each_work_area_corner() {
        let area = (100, 200, 1_000, 800);
        let size = (380, 500);
        assert_eq!(position_for(area, size, 20, &Corner::TopLeft), (120, 220));
        assert_eq!(position_for(area, size, 20, &Corner::TopRight), (700, 220));
        assert_eq!(
            position_for(area, size, 20, &Corner::BottomLeft),
            (120, 480)
        );
        assert_eq!(
            position_for(area, size, 20, &Corner::BottomRight),
            (700, 480)
        );
    }
}
