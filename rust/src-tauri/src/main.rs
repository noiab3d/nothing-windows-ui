//! Nothing Windows UI — unofficial system tray app for Nothing earbuds.
//!
//! Lives in the tray; left-click the icon to open the panel. Started with --tray
//! (as Windows autostart does) it stays hidden until the icon is clicked.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ear;

use std::collections::HashSet;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

use ear::{Change, Ear, ST_BT_OFF, ST_CONNECTED, ST_CONNECTING, ST_DISCONNECTED, ST_UNPAIRED};
use serde_json::{json, Value};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WebviewWindow, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use windows::core::w;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SetWindowPos, SystemParametersInfoW,
    GA_ROOTOWNER, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNORMAL,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const APP_NAME: &str = "Nothing Windows UI";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Gap to the screen edge / taskbar, like the Windows 11 quick settings panel.
const FLYOUT_MARGIN: f64 = 12.0;
const TRAY_ID: &str = "main";

const SLIDE_IN: Duration = Duration::from_millis(220);
const SLIDE_OUT: Duration = Duration::from_millis(160);

const ANC_LABELS: [(&str, &str); 6] = [
    ("off", "anc_label_off"),
    ("transparency", "anc_label_tr"),
    ("nc_adaptive", "anc_label_adaptive"),
    ("nc_low", "anc_label_low"),
    ("nc_mid", "anc_label_mid"),
    ("nc_high", "anc_label_high"),
];

// --- autostart --------------------------------------------------------------
fn autostart_command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    format!("\"{exe}\" --tray")
}

fn get_autostart() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(APP_NAME))
        .is_ok_and(|v| v == autostart_command())
}

fn set_autostart(enabled: bool) {
    if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        let _ = if enabled { k.set_value(APP_NAME, &autostart_command()) } else { k.delete_value(APP_NAME) };
    }
}

// --- tray icon drawing (dot-matrix) -----------------------------------------
const DIGITS: [[&str; 5]; 10] = [
    ["111", "101", "101", "101", "111"],
    ["010", "110", "010", "010", "111"],
    ["111", "001", "111", "100", "111"],
    ["111", "001", "111", "001", "111"],
    ["101", "101", "111", "001", "001"],
    ["111", "100", "111", "001", "111"],
    ["111", "100", "111", "101", "111"],
    ["111", "001", "010", "010", "010"],
    ["111", "101", "111", "101", "111"],
    ["111", "101", "111", "001", "111"],
];
const RED: [u8; 4] = [215, 25, 33, 255];

fn taskbar_is_light() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("SystemUsesLightTheme"))
        .is_ok_and(|v| v == 1)
}

/// Anti-aliased filled circle.
fn dot(px: &mut [u8], cx: f32, cy: f32, r: f32, c: [u8; 4]) {
    for y in 0..64usize {
        for x in 0..64usize {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let cover = (r + 0.5 - d).clamp(0.0, 1.0);
            if cover > 0.0 {
                let i = (y * 64 + x) * 4;
                px[i..i + 3].copy_from_slice(&c[..3]);
                px[i + 3] = px[i + 3].max((c[3] as f32 * cover) as u8);
            }
        }
    }
}

/// 64px icon: battery % as dot-matrix digits, or a dotted ring when disconnected.
fn make_icon(level: Option<u8>, charging: bool) -> Image<'static> {
    let v = if taskbar_is_light() { 20 } else { 245 };
    let (fg, dim) = ([v, v, v, 255], [v, v, v, 90]);
    let mut px = vec![0u8; 64 * 64 * 4];
    match level {
        None => {
            for i in 0..12 {
                let a = i as f32 * std::f32::consts::PI / 6.0;
                dot(&mut px, 32.0 + 22.0 * a.cos(), 32.0 + 22.0 * a.sin(), 4.0, dim);
            }
            dot(&mut px, 32.0, 32.0, 7.0, RED);
        }
        Some(level) => {
            let shown = level.min(99) as usize;
            let (tens, ones) = (shown / 10, shown % 10);
            let (cell, ox, oy) = (9.0f32, 1.0f32, 9.0f32);
            for (col, digit) in [(0, (tens > 0).then_some(tens)), (1, Some(ones))] {
                let Some(digit) = digit else { continue };
                for (r, row) in DIGITS[digit].iter().enumerate() {
                    for (c, bit) in row.chars().enumerate() {
                        if bit == '1' {
                            let x = ox + (col * 4 + c) as f32 * cell + 4.0;
                            let y = oy + r as f32 * cell + 4.0;
                            dot(&mut px, x, y, 4.2, fg);
                        }
                    }
                }
            }
            if charging || level <= 15 {
                dot(&mut px, 57.5, 5.5, 6.0, RED);
            }
        }
    }
    Image::new_owned(px, 64, 64)
}

// --- app ----------------------------------------------------------------------
struct Ctx {
    ear: Arc<Ear>,
    visible: AtomicBool,
    auto_hidden_at: Mutex<Option<Instant>>,
    last_status: Mutex<&'static str>,
    welcome_pending: AtomicBool,
    notified_low: Mutex<HashSet<&'static str>>,
    menu_key: Mutex<String>,
    slide_id: AtomicU64,
}

fn battery_levels(ear: &Ear) -> Value {
    if ear.status() == ST_CONNECTED { ear.get("battery").unwrap_or(json!({})) } else { json!({}) }
}

fn battery_text(ear: &Ear) -> String {
    let bat = battery_levels(ear);
    let parts: Vec<String> = [("left", "left_s"), ("right", "right_s"), ("single", "battery"), ("case", "case")]
        .iter()
        .filter(|(k, _)| bat.get(*k).is_some())
        .map(|(k, key)| {
            let bolt = if bat[*k]["charging"] == true { "⚡" } else { "" };
            format!("{} {}%{bolt}", ear.t(key), bat[*k]["level"])
        })
        .collect();
    if parts.is_empty() { ear.t("tray_no_data") } else { parts.join(" · ") }
}

fn status_line(ear: &Ear) -> String {
    match ear.status() {
        ST_CONNECTED => battery_text(ear),
        ST_BT_OFF => ear.t("st_bt_off"),
        ST_CONNECTING => ear.t("st_connecting"),
        ST_UNPAIRED => ear.t("st_unpaired"),
        _ => ear.t("tray_earbuds_off"),
    }
}

fn notify(app: &AppHandle, ctx: &Ctx, msg: &str) {
    if ctx.ear.cfg.lock().unwrap().notifications.unwrap_or(true) {
        let _ = app.notification().builder().title(ctx.ear.display_name()).body(msg).show();
    }
}

fn open_bluetooth_settings_page() {
    unsafe {
        ShellExecuteW(None, w!("open"), w!("ms-settings:bluetooth"), None, None, SW_SHOWNORMAL);
    }
}

fn on_change(app: &AppHandle, change: Change) {
    let Some(ctx) = app.try_state::<Arc<Ctx>>() else { return };
    let ear = &ctx.ear;
    if change == Change::Status {
        let st = ear.status();
        let mut last = ctx.last_status.lock().unwrap();
        if st != *last {
            if st == ST_CONNECTED {
                ctx.welcome_pending.store(true, Ordering::Relaxed);
            } else if (st == ST_DISCONNECTED || st == ST_BT_OFF) && *last == ST_CONNECTED {
                notify(app, &ctx, &ear.t(if st == ST_BT_OFF { "n_bt_off" } else { "n_disconnected" }));
                ctx.notified_low.lock().unwrap().clear();
            }
            *last = st;
        }
    }
    let bat = ear.get("battery").unwrap_or(json!({}));
    if change == Change::Battery || change == Change::Status {
        if ear.status() == ST_CONNECTED && bat.as_object().is_some_and(|b| !b.is_empty())
            && ctx.welcome_pending.swap(false, Ordering::Relaxed)
        {
            notify(app, &ctx, &format!("{} · {}", ear.t("n_connected"), battery_text(ear)));
        }
        for (side, key) in [("left", "n_low_l"), ("right", "n_low_r")] {
            let b = &bat[side];
            let level = b["level"].as_u64().unwrap_or(100);
            if b.is_object() && level <= 15 && b["charging"] != true && ctx.notified_low.lock().unwrap().insert(side) {
                notify(app, &ctx, &format!("{}: {level}%", ear.t(key)));
            }
        }
    }
    refresh_tray(app, &ctx);
}

fn refresh_tray(app: &AppHandle, ctx: &Ctx) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let ear = &ctx.ear;
    let bat = battery_levels(ear);
    let buds: Vec<&Value> = ["left", "right", "single"].iter().filter_map(|k| bat.get(*k)).collect();
    let low = buds.iter().filter_map(|b| b["level"].as_u64()).min();
    let charging = buds.iter().any(|b| b["charging"] == true);
    let _ = tray.set_icon(Some(make_icon(low.map(|l| l as u8), charging)));
    let _ = tray.set_tooltip(Some(format!("{}\n{}", ear.display_name(), status_line(ear))));
    let key = format!("{}|{}|{}|{}|{}", status_line(ear), ear.status(), ear.get("anc").unwrap_or(Value::Null), get_autostart(), ear.capabilities()["levels"]);
    let mut last = ctx.menu_key.lock().unwrap();
    if *last != key {
        *last = key;
        if let Ok(menu) = build_menu(app, ear) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn build_menu(app: &AppHandle, ear: &Ear) -> tauri::Result<Menu<tauri::Wry>> {
    let status = ear.status();
    let connected = status == ST_CONNECTED;
    let anc = ear.get("anc").unwrap_or(Value::Null);
    let caps = ear.capabilities();
    let levels = caps["levels"].as_array().cloned().unwrap_or_default();
    let mut noise = SubmenuBuilder::new(app, ear.t("noise"));
    for (mode, key) in ANC_LABELS {
        let available = mode == "off"
            || levels.iter().any(|l| l == mode)
            || (mode == "transparency" && caps["transparency"] == true)
            || (mode == "nc_high" && caps["anc"] == true && levels.is_empty());
        if available {
            let item = CheckMenuItem::with_id(app, format!("anc:{mode}"), ear.t(key), connected, anc == mode, None::<&str>)?;
            noise = noise.item(&item);
        }
    }
    let mut menu = MenuBuilder::new(app)
        .item(&MenuItem::with_id(app, "open", ear.t("tray_open"), true, None::<&str>)?)
        .item(&MenuItem::with_id(app, "status", status_line(ear), false, None::<&str>)?);
    if status == ST_BT_OFF {
        menu = menu.item(&MenuItem::with_id(app, "bt", ear.t("tray_bt"), true, None::<&str>)?);
    }
    if connected {
        menu = menu.item(&MenuItem::with_id(app, "disconnect", ear.t("disconnect"), true, None::<&str>)?);
    }
    if status == ST_DISCONNECTED {
        menu = menu.item(&MenuItem::with_id(app, "connect", ear.t("connect"), true, None::<&str>)?);
    }
    menu.separator()
        .item(&noise.build()?)
        .separator()
        .item(&CheckMenuItem::with_id(app, "autostart", ear.t("autostart"), true, get_autostart(), None::<&str>)?)
        .item(&MenuItem::with_id(app, "quit", ear.t("quit"), true, None::<&str>)?)
        .build()
}

// --- window ---------------------------------------------------------------------
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}

fn hwnd_of(w: &WebviewWindow) -> Option<HWND> {
    w.hwnd().ok().map(|h| HWND(h.0))
}

/// Where the window rests: the bottom-right corner of the work area, like a Windows flyout.
fn corner(w: &WebviewWindow) -> Option<(i32, i32)> {
    let mut work = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    let margin = (FLYOUT_MARGIN * w.scale_factor().unwrap_or(1.0)).round() as i32;
    // Position by the visible area: the outer rect includes an invisible resize border.
    let (size, inner, outer) = (w.inner_size().ok()?, w.inner_position().ok()?, w.outer_position().ok()?);
    Some((
        work.right - margin - size.width as i32 - (inner.x - outer.x),
        work.bottom - margin - size.height as i32 - (inner.y - outer.y),
    ))
}

fn move_to(hwnd: isize, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(HWND(hwnd as *mut _), None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Move the window vertically (it passes behind the taskbar). False if a newer show/hide took over.
fn slide(ctx: &Ctx, hwnd: isize, x: i32, from: i32, to: i32, time: Duration, ease_out: bool) -> bool {
    let me = ctx.slide_id.fetch_add(1, Ordering::Relaxed) + 1;
    let start = Instant::now();
    loop {
        let p = (start.elapsed().as_secs_f64() / time.as_secs_f64()).min(1.0);
        let eased = if ease_out { 1.0 - (1.0 - p).powi(3) } else { p.powi(3) };
        if ctx.slide_id.load(Ordering::Relaxed) != me {
            return false;
        }
        move_to(hwnd, x, from + ((to - from) as f64 * eased).round() as i32);
        if p >= 1.0 {
            return true;
        }
        sleep(Duration::from_millis(6));
    }
}

fn show(app: &AppHandle) {
    let (Some(w), Some(ctx)) = (main_window(app), app.try_state::<Arc<Ctx>>()) else { return };
    let (Some(hwnd), Some((x, y))) = (hwnd_of(&w), corner(&w)) else { return };
    ctx.visible.store(true, Ordering::Relaxed);
    let hwnd = hwnd.0 as isize;
    let off_screen = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    move_to(hwnd, x, off_screen);
    let _ = w.show();
    let _ = w.set_focus();
    let ctx = ctx.inner().clone();
    spawn(move || {
        slide(&ctx, hwnd, x, off_screen, y, SLIDE_IN, true);
    });
}

fn hide(app: &AppHandle) {
    let (Some(w), Some(ctx)) = (main_window(app), app.try_state::<Arc<Ctx>>()) else { return };
    let was_visible = ctx.visible.swap(false, Ordering::Relaxed);
    let (Some(hwnd), Ok(pos)) = (hwnd_of(&w), w.outer_position()) else { return };
    let hwnd = hwnd.0 as isize;
    let ctx = ctx.inner().clone();
    spawn(move || {
        let off_screen = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if !was_visible || slide(&ctx, hwnd, pos.x, pos.y, off_screen, SLIDE_OUT, false) {
            let _ = w.hide();
        }
    });
}

fn toggle(app: &AppHandle) {
    let Some(ctx) = app.try_state::<Arc<Ctx>>() else { return };
    // Clicking the tray icon first steals focus (which auto-hides the panel);
    // don't reopen it straight away in that case.
    let just_hidden = ctx.auto_hidden_at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(500));
    if ctx.visible.load(Ordering::Relaxed) {
        hide(app);
    } else if !just_hidden {
        show(app);
    }
}

/// Hide the panel when another window takes focus (click outside), like Windows flyouts.
fn watch_focus(app: AppHandle) {
    let mut had_focus = false;
    let pid = std::process::id();
    loop {
        sleep(Duration::from_millis(100));
        let (Some(w), Some(ctx)) = (main_window(&app), app.try_state::<Arc<Ctx>>()) else { continue };
        let Some(hwnd) = hwnd_of(&w) else { continue };
        if !ctx.visible.load(Ordering::Relaxed) {
            had_focus = false;
            continue;
        }
        let ours = unsafe {
            let fg = GetForegroundWindow();
            let mut owner = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut owner));
            !fg.is_invalid() && (fg == hwnd || GetAncestor(fg, GA_ROOTOWNER) == hwnd || owner == pid)
        };
        if ours {
            had_focus = true;
        } else if had_focus {
            had_focus = false;
            *ctx.auto_hidden_at.lock().unwrap() = Some(Instant::now());
            hide(&app);
        }
    }
}

// --- commands called from the UI ----------------------------------------------------
#[tauri::command]
fn get_state(ctx: State<'_, Arc<Ctx>>) -> Value {
    let mut snap = ctx.ear.snapshot();
    snap["autostart"] = json!(get_autostart());
    snap
}

// async: actions pause between set and re-read, so keep them off the UI thread.
#[tauri::command(async)]
fn action(ctx: State<'_, Arc<Ctx>>, body: Value) -> Value {
    ctx.ear.action(&body)
}

#[tauri::command]
fn open_bluetooth_settings() {
    open_bluetooth_settings_page();
}

#[tauri::command]
fn set_option(app: AppHandle, ctx: State<'_, Arc<Ctx>>, key: String, value: Value) {
    {
        let mut cfg = ctx.ear.cfg.lock().unwrap();
        match key.as_str() {
            "autostart" => {
                let on = value.as_bool().unwrap_or(false);
                set_autostart(on);
                cfg.autostart = Some(on);
            }
            "notifications" => cfg.notifications = value.as_bool(),
            "theme" if value == "light" || value == "oled" => cfg.theme = value.as_str().map(String::from),
            "language" => cfg.language = value.as_str().map(String::from),
            _ => {}
        }
    }
    ctx.ear.save_config();
    refresh_tray(&app, &ctx);
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    hide(&app);
}

#[tauri::command]
fn quit(app: AppHandle, ctx: State<'_, Arc<Ctx>>) {
    ctx.ear.shutdown();
    app.exit(0);
}

fn build_tray(app: &AppHandle, ear: &Ear) -> tauri::Result<TrayIcon> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(make_icon(None, false))
        .tooltip(APP_NAME)
        .menu(&build_menu(app, ear)?)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            let Some(ctx) = app.try_state::<Arc<Ctx>>() else { return };
            match id {
                "open" => show(app),
                "bt" => open_bluetooth_settings_page(),
                "disconnect" | "connect" => {
                    let ear = ctx.ear.clone();
                    let body = json!({ "action": id });
                    spawn(move || {
                        ear.action(&body);
                    });
                }
                "quit" => {
                    ctx.ear.shutdown();
                    app.exit(0);
                }
                "autostart" => {
                    let on = !get_autostart();
                    set_autostart(on);
                    ctx.ear.cfg.lock().unwrap().autostart = Some(on);
                    ctx.ear.save_config();
                    refresh_tray(app, &ctx);
                }
                _ => {
                    if let Some(mode) = id.strip_prefix("anc:") {
                        let ear = ctx.ear.clone();
                        let body = json!({"action": "anc", "mode": mode});
                        spawn(move || {
                            ear.action(&body);
                        });
                    }
                }
            }
        })
        .build(app)
}

fn main() {
    let start_hidden = std::env::args().any(|a| a == "--tray");
    ear::init_winsock();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show(app)))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![get_state, action, set_option, open_bluetooth_settings, hide_window, quit])
        .on_window_event(|window, event| {
            // Closing the window only hides it; the app keeps running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide(window.app_handle());
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let notify_handle = handle.clone();
            let ear = Ear::new(config_path, Box::new(move |c| on_change(&notify_handle, c)));

            // Keep the autostart entry pointing at this copy of the app.
            if ear.cfg.lock().unwrap().autostart.unwrap_or(true) {
                set_autostart(true);
            }
            app.manage(Arc::new(Ctx {
                ear: ear.clone(),
                visible: AtomicBool::new(false),
                auto_hidden_at: Mutex::new(None),
                last_status: Mutex::new(""),
                welcome_pending: AtomicBool::new(false),
                notified_low: Mutex::new(HashSet::new()),
                menu_key: Mutex::new(String::new()),
                slide_id: AtomicU64::new(0),
            }));
            build_tray(&handle, &ear)?;

            if let Some(w) = main_window(&handle) {
                if let Some(hwnd) = hwnd_of(&w) {
                    let pref = DWMWCP_ROUND;
                    unsafe {
                        let _ = DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_WINDOW_CORNER_PREFERENCE,
                            &pref as *const _ as *const _,
                            size_of::<i32>() as u32,
                        );
                    }
                }
                if ear.cfg.lock().unwrap().theme.as_deref() == Some("oled") {
                    let _ = w.set_background_color(Some(tauri::window::Color(0, 0, 0, 255)));
                }
            }
            spawn(move || ear.run());
            let focus_handle = handle.clone();
            spawn(move || watch_focus(focus_handle));
            if !start_hidden {
                show(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start")
        .run(|_, _| {});
}
