//! Tray icon and the recording bubble, on one Win32 thread with one message pump.
//!
//! The bubble is a native layered window, not a webview. The one property it must have is that it
//! **never takes focus** — the caret has to stay in the application the user is dictating into, or
//! the text lands nowhere. That means `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT`
//! and `SW_SHOWNOACTIVATE`, asserted natively; no toolkit exposes this reliably.
//!
//! Commands arrive as window messages rather than thread messages: modal loops (an open tray menu)
//! silently drop thread messages, but window messages survive them.

use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, EndPaint, FillRect,
    InvalidateRect, SelectObject, SetWindowRgn, HBRUSH, HGDIOBJ, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadCursorW, PostMessageW, RegisterClassW,
    KillTimer, RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TrackPopupMenu, TranslateMessage, HICON, HWND_TOPMOST, IDC_ARROW, MF_STRING, MSG,
    SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const MSG_RECORDING: u32 = WM_APP + 1;
const MSG_TRANSCRIBING: u32 = WM_APP + 2;
const MSG_HIDE: u32 = WM_APP + 3;
const MSG_TRAY: u32 = WM_APP + 10;

const CMD_OPEN_CONFIG: usize = 1;
const CMD_EXIT: usize = 2;

// Small and minimal, in the Ash Lumen spirit: monochrome, hierarchy through luminance.
const BUBBLE_W: i32 = 78;
const BUBBLE_H: i32 = 30;
const MARGIN: i32 = 22;

const BARS: usize = 5;
const ANIM_TIMER: usize = 1;
const FRAME_MS: u32 = 45;

/// 0 = hidden, 1 = recording, 2 = transcribing. Read by the paint handler.
static STATE: AtomicU8 = AtomicU8::new(0);
/// Animation frame, advanced by the window timer.
static FRAME: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static UI_HWND: AtomicIsize = AtomicIsize::new(0);
static CONFIG_PATH_W: OnceLock<Vec<u16>> = OnceLock::new();
static TIP: OnceLock<Vec<u16>> = OnceLock::new();

fn colorref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// Handle owned by the main loop. All methods are fire-and-forget.
pub struct Ui {
    hwnd: isize,
}

impl Ui {
    /// Spawn the UI thread: tray icon, hidden bubble, message pump.
    pub fn start(hotkey: &str, config_path: &std::path::Path) -> Result<Self> {
        let tip: Vec<u16> = format!("dictate — hold {hotkey}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = TIP.set(tip);
        let path_w: Vec<u16> = config_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = CONFIG_PATH_W.set(path_w);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize>>();

        std::thread::Builder::new()
            .name("dictate-ui".into())
            .spawn(move || ui_thread(ready_tx))
            .context("spawning UI thread")?;

        let hwnd = ready_rx.recv().context("UI thread died during startup")??;
        Ok(Self { hwnd })
    }

    pub fn recording(&self) {
        self.post(MSG_RECORDING);
    }

    pub fn transcribing(&self) {
        self.post(MSG_TRANSCRIBING);
    }

    pub fn hide(&self) {
        self.post(MSG_HIDE);
    }

    fn post(&self, msg: u32) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                msg,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn ui_thread(ready: std::sync::mpsc::Sender<Result<isize>>) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    UI_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

    if let Err(e) = add_tray_icon(hwnd) {
        let _ = ready.send(Err(e));
        return;
    }

    let _ = ready.send(Ok(hwnd.0 as isize));

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn create_window() -> Result<HWND> {
    unsafe {
        // Explorer restarts broadcast this; the tray icon must be re-added or it vanishes.
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::SeqCst,
        );

        let instance = GetModuleHandleW(None).context("module handle")?;
        let class = w!("DictateBubble");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        // NOACTIVATE + TOOLWINDOW + TRANSPARENT: never takes focus, never in Alt+Tab,
        // clicks pass straight through it.
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED
                | WS_EX_TRANSPARENT,
            class,
            w!("dictate"),
            WS_POPUP,
            0,
            0,
            BUBBLE_W,
            BUBBLE_H,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .context("creating bubble window")?;

        // Uniform translucency plus rounded corners.
        windows::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(
            hwnd,
            COLORREF(0),
            235,
            windows::Win32::UI::WindowsAndMessaging::LWA_ALPHA,
        )
        .context("layered attributes")?;
        let rgn = CreateRoundRectRgn(0, 0, BUBBLE_W, BUBBLE_H, BUBBLE_H, BUBBLE_H);
        SetWindowRgn(hwnd, Some(rgn), true);

        Ok(hwnd)
    }
}

/// A plain filled circle, built in memory: no icon file, no build-script resource compiler.
fn make_icon() -> HICON {
    const S: usize = 32;
    let mut argb = [0u8; S * S * 4];
    let (cx, cy, r) = (15.5f32, 15.5f32, 12.0f32);
    for y in 0..S {
        for x in 0..S {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            // Soft edge over one pixel so it does not look jagged in the tray.
            let a = ((r - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            let i = (y * S + x) * 4;
            // Premultiplied BGRA, light gray.
            let v = (224u16 * a as u16 / 255) as u8;
            argb[i] = v;
            argb[i + 1] = v;
            argb[i + 2] = v;
            argb[i + 3] = a;
        }
    }
    let mask = [0u8; S * S / 8];
    unsafe {
        CreateIcon(
            None,
            S as i32,
            S as i32,
            1,
            32,
            mask.as_ptr(),
            argb.as_ptr(),
        )
        .unwrap_or_default()
    }
}

fn add_tray_icon(hwnd: HWND) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: MSG_TRAY,
        hIcon: make_icon(),
        ..Default::default()
    };
    if let Some(tip) = TIP.get() {
        let n = tip.len().min(data.szTip.len());
        data.szTip[..n].copy_from_slice(&tip[..n]);
    }
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &data)
            .ok()
            .context("adding tray icon")?;
    }
    Ok(())
}

fn remove_tray_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// Bottom-center of the work area, above the taskbar.
fn place(hwnd: HWND) {
    unsafe {
        let mut area = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut area as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let x = area.left + (area.right - area.left - BUBBLE_W) / 2;
        let y = area.bottom - BUBBLE_H - MARGIN;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE);
    }
}

fn show_state(hwnd: HWND, state: u8) {
    STATE.store(state, Ordering::SeqCst);
    unsafe {
        if state == 0 {
            let _ = KillTimer(Some(hwnd), ANIM_TIMER);
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            FRAME.store(0, Ordering::SeqCst);
            place(hwnd);
            // SW_SHOWNOACTIVATE is the whole point: visible, but focus stays where it was.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetTimer(Some(hwnd), ANIM_TIMER, FRAME_MS, None);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// A tiny equalizer. Monochrome by design: no hue, just luminance, the whole point of the
/// Ash Lumen look. Bars react while recording; they settle into a slow shared breath while
/// transcribing, so the two states read differently without a label.
fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        // Fill the whole client area with the pill colour. The rounded region clips the corners.
        let full = RECT {
            left: 0,
            top: 0,
            right: BUBBLE_W,
            bottom: BUBBLE_H,
        };
        let bg = CreateSolidBrush(colorref(24, 24, 24));
        FillRect(hdc, &full, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        let state = STATE.load(Ordering::SeqCst);
        let frame = FRAME.load(Ordering::SeqCst) as f32;

        let bar_w = 3i32;
        let gap = 5i32;
        let total = BARS as i32 * bar_w + (BARS as i32 - 1) * gap;
        let x0 = (BUBBLE_W - total) / 2;
        let cy = BUBBLE_H / 2;
        let max_h = 14.0f32;
        let min_h = 3.0f32;

        // Bars are driven by the real microphone level, so the row is flat in silence and only
        // comes alive when there is something to hear -- no canned animation. sqrt makes quiet
        // speech visible; the gain maps typical speech peaks into the full height.
        let level = (crate::audio::input_level() * 6.0).sqrt().clamp(0.0, 1.0);

        // A gentle centre-tall profile so the flat state still reads as a waveform rather than a
        // ruler, and each bar leads the next slightly so motion travels across the row.
        const PROFILE: [f32; BARS] = [0.55, 0.8, 1.0, 0.8, 0.55];

        for i in 0..BARS {
            let (amp, lum) = if state == 1 {
                // Per-bar wobble whose size scales with level: zero level, zero motion.
                let wobble = (frame * 0.6 + i as f32 * 1.3).sin() * 0.18 * level;
                let a = (level * PROFILE[i] + wobble).clamp(0.0, 1.0);
                (a, 210.0)
            } else {
                // Transcribing: no live audio, so hold a flat dim row rather than inventing motion.
                (0.12, 150.0)
            };
            let h = (min_h + amp * (max_h - min_h)).round().max(min_h) as i32;
            let x = x0 + i as i32 * (bar_w + gap);
            let c = lum as u8;
            let brush = CreateSolidBrush(colorref(c, c, c));
            // Rounded caps on each bar so it reads soft, not blocky.
            let old = SelectObject(hdc, HGDIOBJ(brush.0));
            let pen_old = SelectObject(
                hdc,
                HGDIOBJ(windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::NULL_PEN,
                )
                .0),
            );
            let _ = windows::Win32::Graphics::Gdi::RoundRect(
                hdc,
                x,
                cy - h / 2,
                x + bar_w,
                cy + h / 2,
                bar_w,
                bar_w,
            );
            SelectObject(hdc, pen_old);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }

        let _ = EndPaint(hwnd, &ps);
    }
}

fn tray_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_CONFIG, w!("open config"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("exit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required for the menu to dismiss on outside-click; user-initiated, so this
        // one focus grab is fine.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            CMD_OPEN_CONFIG => {
                if let Some(path) = CONFIG_PATH_W.get() {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        w!("notepad.exe"),
                        PCWSTR(path.as_ptr()),
                        PCWSTR::null(),
                        windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
                    );
                }
            }
            CMD_EXIT => {
                remove_tray_icon(hwnd);
                // No state to flush: config is read-only at runtime and the model is
                // process-lifetime. A hard exit is honest here.
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        MSG_RECORDING => {
            show_state(hwnd, 1);
            LRESULT(0)
        }
        MSG_TRANSCRIBING => {
            show_state(hwnd, 2);
            LRESULT(0)
        }
        MSG_HIDE => {
            show_state(hwnd, 0);
            LRESULT(0)
        }
        MSG_TRAY => {
            let event = lp.0 as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            FRAME.fetch_add(1, Ordering::SeqCst);
            // Do not erase: the paint handler repaints the whole client area, so erasing first
            // only adds flicker.
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_COMMAND => LRESULT(0),
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            LRESULT(0)
        }
        _ => {
            if msg == TASKBAR_CREATED.load(Ordering::SeqCst) && msg != 0 {
                let _ = add_tray_icon(hwnd);
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}
