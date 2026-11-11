//! JinJuApp — a taskbar-side quote ticker for Windows.
//!
//! Pure-Win32 rewrite of the original PyQt application: no bootloader, no
//! packer, no framework — a native binary that renders one line of text
//! through a per-pixel-alpha layered window. All decision logic lives in
//! pure, unit-tested modules (`quotes`, `geometry`, `layout`, `render`);
//! this file is the thin host that wires them to the message loop.

#![windows_subsystem = "windows"]

mod geometry;
mod layout;
mod quotes;
mod render;
mod settings;
mod startup;
mod tray;
mod win32;

use geometry::Rect;
use layout::Scroll;
use quotes::{Bag, Rng, Source};
use render::Surface;
use settings::Settings;
use win32::*;

// ---------------------------------------------------------------------------
// Tunables (logical px @ 96 DPI unless noted)
// ---------------------------------------------------------------------------

const WINDOW_CLASS: &str = "JinJuApp";
const MUTEX_NAME: &str = "JinJuApp-SingleInstance";
const WINDOW_W: i32 = 500;
const WINDOW_H: i32 = 45;
const PLACE_MARGIN: i32 = 10;
const QUOTE_INTERVAL_MS: u32 = 3 * 60 * 1000; // every 3 minutes, as before
const QUOTE_TIMER: usize = 1;
const SCROLL_TIMER: usize = 2;
const SCROLL_TICK_MS: u32 = 15;
/// Scroll speed in logical px per second @ 96 DPI (scaled to the DPI at
/// which the surface renders).
const SCROLL_SPEED_LOGICAL: f32 = 45.0;
const Z_ORDER_TIMER: usize = 3;
/// Quote-change crossfade duration: the outgoing line fades out where it
/// stands while the incoming line fades in at rest.
const TRANSITION_MS: u64 = 400;
/// The strip shares the taskbar band with explorer, which re-asserts its
/// own topmost status whenever the taskbar is activated — including when
/// a click on the (click-through) strip lands on the bar beneath it, which
/// is what makes the text visibly blink at a 500 ms cadence. Re-assert at
/// this cadence so any cover is bounded to ~2 frames and reads as nothing.
const Z_ORDER_INTERVAL_MS: u32 = 100;

/// Message id the shell broadcasts when the taskbar is (re)created.
static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// The strip's window handle, for the WinEvent callback — the event only
/// names the window it fired on (the taskbar), never our own.
static STRIP_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Cached `Shell_TrayWnd` handle; 0 = not resolved yet. Cleared on the
/// shell's `TaskbarCreated` broadcast so an explorer restart re-resolves.
static TASKBAR_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

// ---------------------------------------------------------------------------
// Command line (pure, unit-tested)
// ---------------------------------------------------------------------------

/// `--quiet`: started by autostart — degrade silently instead of popping
/// a message box in the user's face at logon.
#[derive(Debug, Default, PartialEq, Eq)]
struct Cli {
    quiet: bool,
}

fn parse_cli(args: &[String]) -> Cli {
    Cli {
        quiet: args.iter().any(|a| a == "--quiet" || a == "-q"),
    }
}

// ---------------------------------------------------------------------------
// Application state (owned by the window via GWLP_USERDATA)
// ---------------------------------------------------------------------------

/// State of an in-flight quote crossfade.
struct Transition {
    /// The outgoing quote, fading out where it stands.
    prev_text: String,
    /// Its position and opacity at the quote change, captured before the
    /// scroller resets — the fade starts from exactly what was on screen,
    /// so a change landing mid-marquee-fade can't pop the old line back
    /// to full opacity.
    prev_x: i32,
    prev_alpha: f32,
    /// GetTickCount64 at the quote change.
    start: u64,
}

struct App {
    hwnd: HWND,
    dpi: u32,
    win_w: i32,
    win_h: i32,
    surface: Surface,
    scroll: Scroll,
    text: String,
    quotes: Source,
    bag: Bag,
    rng: Rng,
    last_tick_ms: u64,
    last_presented_x: i32,
    last_presented_alpha: i32,
    /// In-flight quote crossfade; `None` = no transition.
    transition: Option<Transition>,
    /// Where the strip was last placed, so re-anchoring (which runs on
    /// every quote change) only moves the window when the target changed.
    placed: Option<(i32, i32)>,
    /// Typeface settings from `settings.json`, reused when the DPI changes.
    style: Settings,
    /// Windows light-mode flag, resolved into `color: auto` and refreshed
    /// when the taskbar theme changes.
    light: bool,
    exe_path: String,
}

impl App {
    fn new(
        hwnd: HWND,
        dpi: u32,
        quotes: Source,
        style: Settings,
        light: bool,
        exe_path: String,
    ) -> Option<App> {
        let win_w = scale_pt(WINDOW_W, dpi);
        let win_h = scale_pt(WINDOW_H, dpi);
        let surface = Surface::new(win_w, win_h, dpi, &style, light)?;
        let bag = Bag::new(quotes.lines().len());
        let speed_px_per_s = SCROLL_SPEED_LOGICAL * dpi as f32 / 96.0;
        Some(App {
            hwnd,
            dpi,
            win_w,
            win_h,
            surface,
            scroll: Scroll::new(win_w, 0, speed_px_per_s),
            text: String::new(),
            quotes,
            bag,
            rng: Rng::from_entropy(),
            last_tick_ms: 0,
            last_presented_x: i32::MIN,
            last_presented_alpha: -1,
            transition: None,
            placed: None,
            style,
            light,
            exe_path,
        })
    }

    /// Pick the next quote, adapt the scroller, present.
    fn next_quote(&mut self) {
        let i = self.bag.next(&mut self.rng);
        let new_text = self.quotes.lines()[i].clone();
        // Crossfade: keep the outgoing line at its current spot for one
        // TRANSITION_MS envelope while the new line fades in at rest.
        // Position AND opacity must be captured before refresh() resets
        // the scroller — otherwise a change landing mid-marquee-fade
        // would pop the old line back to full opacity.
        if !self.text.is_empty() && self.text != new_text {
            self.transition = Some(Transition {
                prev_text: std::mem::replace(&mut self.text, new_text),
                prev_x: self.scroll.x(),
                prev_alpha: self.scroll.alpha(),
                start: unsafe { GetTickCount64() },
            });
        } else {
            self.text = new_text;
        }
        self.refresh();
    }

    /// Re-measure the current text and (re)start or stop the scroll timer.
    fn refresh(&mut self) {
        let text_w = self.surface.measure(&self.text);
        self.scroll.set_text(text_w);
        unsafe {
            // The scroll timer also drives an in-flight quote crossfade.
            if self.scroll.scrolling() || self.transition.is_some() {
                SetTimer(
                    self.hwnd,
                    SCROLL_TIMER,
                    SCROLL_TICK_MS,
                    core::ptr::null_mut(),
                );
            } else {
                KillTimer(self.hwnd, SCROLL_TIMER);
            }
            self.last_tick_ms = GetTickCount64();
        }
        self.present();
    }

    /// Composite the text at the current scroll offset onto the screen.
    fn present(&mut self) {
        let x = self.scroll.x();
        let alpha = self.scroll.alpha();
        // Crossfade envelope: the outgoing layer runs prev_alpha → 0, the
        // incoming layer 0 → alpha, over TRANSITION_MS.
        let mut t = 1.0_f32;
        if let Some(tr) = &self.transition {
            let now = unsafe { GetTickCount64() };
            t = crossfade_alpha(now - tr.start);
        }
        if t >= 1.0 {
            self.transition = None;
        }
        self.last_presented_x = x;
        self.last_presented_alpha = (alpha * t * 255.0).round() as i32;
        match &self.transition {
            Some(tr) => self.surface.draw(&[
                (tr.prev_text.as_str(), tr.prev_x, tr.prev_alpha * (1.0 - t)),
                (self.text.as_str(), x, alpha * t),
            ]),
            None => self.surface.draw(&[(self.text.as_str(), x, alpha * t)]),
        }
        unsafe {
            // Frame pacing: submit on the compositor's boundary instead of
            // mid-scan, so marquee frames don't land at arbitrary offsets
            // within the DWM refresh. Best-effort — on failure we simply
            // keep the previous pacing.
            let _ = DwmFlush();
            let size = POINT {
                x: self.win_w,
                y: self.win_h,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: 0, // AC_SRC_OVER
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA,
            };
            UpdateLayeredWindow(
                self.hwnd,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                &size,
                self.surface.hdc(),
                &src,
                0,
                &blend,
                ULW_ALPHA,
            );
        }
    }

    /// Advance the marquee by the time since the last tick.
    ///
    /// The 15 ms cadence (~64 Hz, near display refresh) keeps the per-frame
    /// step under 1 px, so rounding produces a steady 0/1 px pattern instead
    /// of the 2/3 px alternation that reads as stutter at 20 Hz.
    fn tick_scroll(&mut self) {
        let now = unsafe { GetTickCount64() };
        // Clamp: after a sleep/suspend the delta is meaningless.
        let dt = (now - self.last_tick_ms).min(250) as f32;
        self.last_tick_ms = now;
        self.scroll.tick(dt);
        let alpha_q = (self.scroll.alpha() * 255.0).round() as i32;
        if self.transition.is_some()
            || self.scroll.x() != self.last_presented_x
            || alpha_q != self.last_presented_alpha
        {
            self.present();
        }
        // Nothing left to animate: stop the 64 Hz cadence.
        if !self.scroll.scrolling() && self.transition.is_none() {
            unsafe { KillTimer(self.hwnd, SCROLL_TIMER) };
        }
    }

    /// Move the strip to its configured corner of the usable desktop.
    ///
    /// Anchored to the work area, so the taskbar — whatever edge it is
    /// docked to — can never be covered. `settings.json`'s `position`
    /// picks the horizontal anchor: 0 = bottom-left (v1 parity),
    /// 1 = bottom-right, ending at the notification area's left edge.
    fn reposition(&mut self) {
        let Some((monitor, work)) = desktop_geometry() else {
            return;
        };
        let r = geometry::place(
            &work,
            &monitor,
            notification_area(),
            traffic_monitor_window(),
            self.style.position == 1,
            self.win_w,
            self.win_h,
            PLACE_MARGIN,
        );
        if self.placed == Some((r.left, r.top)) {
            return; // already there; the tray or DPI has not moved since
        }
        self.placed = Some((r.left, r.top));
        unsafe {
            MoveWindow(self.hwnd, r.left, r.top, self.win_w, self.win_h, 0);
        }
    }

    /// Re-resolve `color: auto` against the taskbar's light/dark mode.
    ///
    /// Called on WM_SETTINGCHANGE, which is also how Windows broadcasts a
    /// theme switch. Only a real change rebuilds the surface.
    fn follow_theme(&mut self) {
        let light = system_uses_light_theme();
        if light == self.light {
            return;
        }
        self.light = light;
        if let Some(surface) = Surface::new(self.win_w, self.win_h, self.dpi, &self.style, light) {
            self.surface = surface;
            self.refresh();
        }
    }

    /// DPI changed (window moved between monitors): rebuild at new size.
    fn on_dpi_changed(&mut self, dpi: u32) {
        let Some(surface) = Surface::new(
            scale_pt(WINDOW_W, dpi),
            scale_pt(WINDOW_H, dpi),
            dpi,
            &self.style,
            self.light,
        ) else {
            return;
        };
        self.surface = surface;
        self.dpi = dpi;
        self.win_w = scale_pt(WINDOW_W, dpi);
        self.win_h = scale_pt(WINDOW_H, dpi);
        // The scroller caches view width and speed in px — both are
        // DPI-dependent, so rebuild it (refresh() re-measures the text).
        self.scroll = Scroll::new(self.win_w, 0, SCROLL_SPEED_LOGICAL * dpi as f32 / 96.0);
        // An in-flight crossfade's cached geometry belongs to the old DPI.
        self.transition = None;
        // Re-anchor rather than trusting the suggested rect: the strip's
        // position is derived from the work area, not from where Windows
        // thinks this window belonged before the scale change.
        self.reposition();
        self.refresh();
    }

    fn quotes_file_path(&self) -> String {
        let dir = std::path::Path::new(&self.exe_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        dir.join(quotes::FILE_NAME).to_string_lossy().into_owned()
    }
}

/// Crossfade envelope: 0 → 1 linearly over TRANSITION_MS, clamped.
fn crossfade_alpha(elapsed_ms: u64) -> f32 {
    (elapsed_ms as f32 / TRANSITION_MS as f32).min(1.0)
}

/// Logical px @ 96 DPI → physical px at `dpi`.
fn scale_pt(v: i32, dpi: u32) -> i32 {
    ((i64::from(v) * i64::from(dpi) + 95) / 96) as i32
}

/// Windows light-mode flag for the taskbar
/// (`HKCU\...\Themes\Personalize\SystemUsesLightTheme`). Missing or
/// unreadable values mean light, the Windows default.
fn system_uses_light_theme() -> bool {
    const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const VALUE: &str = "SystemUsesLightTheme";
    unsafe {
        let sub = widez(KEY);
        let name = widez(VALUE);
        let mut hkey: HKEY = core::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return true;
        }
        let mut data = [0u8; 4];
        let mut size: u32 = data.len() as u32;
        let mut ty: u32 = 0;
        let ok = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            core::ptr::null_mut(),
            &mut ty,
            data.as_mut_ptr(),
            &mut size,
        );
        RegCloseKey(hkey);
        if ok != 0 || ty != REG_DWORD || size < 4 {
            return true;
        }
        u32::from_le_bytes(data) != 0
    }
}

/// The TrafficMonitor taskbar window, when that app is running.
///
/// TrafficMonitor parents its strip to the taskbar and titles it
/// `TrafficMonitorTaskbarWindow`, so it is a child of `Shell_TrayWnd`
/// rather than a top-level window. It is only an obstacle: the gold strip
/// steps to its left when there is room.
fn traffic_monitor_window() -> Option<Rect> {
    unsafe {
        let tray = FindWindowW(widez("Shell_TrayWnd").as_ptr(), core::ptr::null());
        if tray.is_null() {
            return None;
        }
        let window = FindWindowExW(
            tray,
            core::ptr::null_mut(),
            core::ptr::null(),
            widez("TrafficMonitorTaskbarWindow").as_ptr(),
        );
        if window.is_null() {
            return None;
        }
        let mut r: RECT = core::mem::zeroed();
        if GetWindowRect(window, &mut r) == 0 {
            return None;
        }
        Some(Rect::from(r))
    }
}

/// The shell's notification-area container (`TrayNotifyWnd`) in screen
/// coordinates, when the shell exposes it.
///
/// It is a child of `Shell_TrayWnd`; its left edge is where the clock, IME
/// indicator, tray icons and hidden-icon chevron begin. `None` means the
/// strip simply uses the work area's right edge.
fn notification_area() -> Option<Rect> {
    unsafe {
        let tray = FindWindowW(widez("Shell_TrayWnd").as_ptr(), core::ptr::null());
        if tray.is_null() {
            return None;
        }
        let notify = FindWindowExW(
            tray,
            core::ptr::null_mut(),
            widez("TrayNotifyWnd").as_ptr(),
            core::ptr::null(),
        );
        if notify.is_null() {
            return None;
        }
        let mut r: RECT = core::mem::zeroed();
        if GetWindowRect(notify, &mut r) == 0 {
            return None;
        }
        Some(Rect::from(r))
    }
}

/// Primary monitor rect + its work area (the desktop minus the taskbar).
///
/// The work area is the placement authority: Windows maintains it for every
/// docked edge, so the strip can be anchored away from the bar without
/// classifying the edge or trusting an appbar query.
fn desktop_geometry() -> Option<(Rect, Rect)> {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = core::mem::zeroed();
        mi.cbSize = core::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut mi) == 0 {
            return None;
        }
        Some((Rect::from(mi.rcMonitor), Rect::from(mi.rcWork)))
    }
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Rect {
        Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe fn app_of(hwnd: HWND) -> Option<&'static mut App> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if p.is_null() { None } else { Some(&mut *p) }
    }
}

/// The taskbar raised itself above the strip: put the strip back at once.
///
/// Explorer re-asserts the bar's topmost status whenever it is activated —
/// button clicks, flyouts — which covers the strip sharing its band until
/// the Z_ORDER_TIMER next fires. This hook watches the bar's z-order moves
/// and restores the strip within the same message cycle, so the overlap is
/// under one frame and never reads as a blink. The timer stays as the
/// safety net for covers this hook does not see.
///
/// Runs on the UI thread: the hook is OUTOFCONTEXT, delivered while the
/// thread pumps messages. Legitimate overlays (menus, popups) are not
/// fought here — only the taskbar's own z-order moves are.
unsafe extern "system" fn on_taskbar_reordered(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    use std::sync::atomic::Ordering;
    unsafe {
        if event != EVENT_OBJECT_REORDER || id_object != OBJID_WINDOW || id_child != CHILDID_SELF {
            return;
        }
        let mut taskbar = TASKBAR_HWND.load(Ordering::Relaxed) as HWND;
        if taskbar.is_null() {
            taskbar = FindWindowW(widez("Shell_TrayWnd").as_ptr(), core::ptr::null());
            if taskbar.is_null() {
                return; // explorer is down; nothing to watch yet
            }
            TASKBAR_HWND.store(taskbar as isize, Ordering::Relaxed);
        }
        if hwnd != taskbar {
            return;
        }
        let strip = STRIP_HWND.load(Ordering::Relaxed) as HWND;
        if strip.is_null() {
            return;
        }
        // Demoted? Walk the windows stacked above the strip and look for the
        // bar; anything else up there (menus, popups) is left alone.
        let mut above = GetWindow(strip, GW_HWNDPREV);
        for _ in 0..64 {
            if above.is_null() {
                return;
            }
            if above == taskbar {
                SetWindowPos(
                    strip,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                return;
            }
            above = GetWindow(above, GW_HWNDPREV);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: UINT, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                // Take ownership of the App box passed through CreateWindowExW.
                let cs = lp as *const CREATESTRUCTW;
                let app = (*cs).lpCreateParams as *mut App;
                if !app.is_null() {
                    (*app).hwnd = hwnd;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_TIMER => {
                let Some(app) = app_of(hwnd) else { return 0 };
                match wp {
                    QUOTE_TIMER => {
                        // Gentle recovery: if hidden (e.g. Win+D), re-show at
                        // the natural quote-change moment — never fight the
                        // user at 1 Hz like the original watchdog did.
                        if IsWindowVisible(hwnd) == 0 {
                            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                        // Follow the notification area: tray icons (and so
                        // the chevron) move as apps hide and show them.
                        app.reposition();
                        app.next_quote();
                    }
                    SCROLL_TIMER => app.tick_scroll(),
                    Z_ORDER_TIMER => {
                        // NOACTIVATE: never steal focus. No SHOWWINDOW: never
                        // fight Win+D (visibility is owned by the quote tick).
                        SetWindowPos(
                            hwnd,
                            HWND_TOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                    _ => {}
                }
                0
            }
            WM_DPICHANGED => {
                let Some(app) = app_of(hwnd) else { return 0 };
                let dpi = ((wp >> 16) & 0xFFFF) as u32;
                app.on_dpi_changed(dpi);
                0
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                let Some(app) = app_of(hwnd) else { return 0 };
                // Theme switches arrive here too; this is what keeps the
                // text readable when the taskbar flips to dark mode.
                app.follow_theme();
                app.reposition();
                0
            }
            WM_DESTROY => {
                tray::remove(hwnd);
                STRIP_HWND.store(0, std::sync::atomic::Ordering::Relaxed);
                // Drop the App box now, before the window handle dies.
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                0
            }
            _ => {
                if TASKBAR_CREATED.get() == Some(&msg) {
                    // The cached taskbar handle belongs to the old explorer.
                    TASKBAR_HWND.store(0, std::sync::atomic::Ordering::Relaxed);
                    // Explorer restarted: the tray icon is gone, re-add it — unless
                    // the user hid it via settings.json.
                    if app_of(hwnd).is_some_and(|a| a.style.tray_icon == 1) {
                        tray::add(hwnd);
                    }
                    return 0;
                }
                if msg == tray::WM_TRAY {
                    let Some(app) = app_of(hwnd) else { return 0 };
                    let event = lp as u32;
                    if event == WM_RBUTTONUP {
                        let on = startup::is_enabled(&app.exe_path);
                        match tray::show_menu(hwnd, on) {
                            tray::CMD_OPEN_QUOTES => {
                                tray::open_quotes_file(&app.quotes_file_path());
                            }
                            tray::CMD_TOGGLE_STARTUP => {
                                let result = if on {
                                    startup::disable()
                                } else {
                                    startup::enable(&app.exe_path)
                                };
                                if let Err(e) = result {
                                    alert(hwnd, &format!("切换开机自启动失败：{e}"));
                                }
                            }
                            tray::CMD_EXIT => {
                                DestroyWindow(hwnd);
                            }
                            _ => {}
                        }
                    } else if event == WM_LBUTTONDBLCLK {
                        tray::open_quotes_file(&app.quotes_file_path());
                    }
                    return 0;
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
    }
}

fn alert(hwnd: HWND, text: &str) {
    unsafe {
        let msg: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let caption: Vec<u16> = "JinJuApp".encode_utf16().chain([0]).collect();
        MessageBoxW(
            hwnd,
            msg.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONWARNING | MB_SETFOREGROUND,
        );
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn widez(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn exe_path() -> String {
    unsafe {
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameW(core::ptr::null_mut(), buf.as_mut_ptr(), 1024) as usize;
        String::from_utf16_lossy(&buf[..len])
    }
}

fn primary_dpi() -> u32 {
    unsafe {
        let hdc = GetDC(core::ptr::null_mut());
        let dpi = if hdc.is_null() {
            96
        } else {
            GetDeviceCaps(hdc, LOGPIXELSX).max(96) as u32
        };
        ReleaseDC(core::ptr::null_mut(), hdc);
        dpi
    }
}

fn main() {
    let cli = parse_cli(&std::env::args().skip(1).collect::<Vec<_>>());

    // Per-monitor v2: crisp text at any display scale (best-effort on
    // older systems; the call simply fails and we stay system-default).
    unsafe {
        SetProcessDpiAwarenessContext(dpi_awareness_per_monitor_v2());
    }

    // Single instance.
    unsafe {
        let name = widez(MUTEX_NAME);
        let mutex = CreateMutexW(core::ptr::null_mut(), 0, name.as_ptr());
        if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            return; // already running — silently defer to the first instance
        }
        let _ = mutex; // held until process exit (raw pointer, no drop)
    }

    let exe = exe_path();
    let exe_dir = std::path::Path::new(&exe)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let quotes = quotes::load(&exe_dir);
    let (style, style_note) = settings::load(&exe_dir);
    if !cli.quiet {
        // One dialog for every startup problem: the user fixes the files in
        // one pass instead of restarting once per message box.
        let mut problems: Vec<String> = Vec::new();
        if let Some(reason) = quotes.note() {
            problems.push(format!(
                "{reason}\n\n本次运行使用内置金句。修正该文件后重启应用即可。\n文件格式：[{{\"quote\": \"…\"}}, …]"
            ));
        }
        if let Some(reason) = style_note {
            problems.push(reason);
        }
        if !problems.is_empty() {
            alert(core::ptr::null_mut(), &problems.join("\n\n"));
        }
    }

    unsafe {
        let hinst = GetModuleHandleW(core::ptr::null_mut());
        let class_name = widez(WINDOW_CLASS);

        let wc = WNDCLASSEXW {
            cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(hinst, IMAGE_ICON_APP),
            hCursor: LoadCursorW(core::ptr::null_mut(), IDC_ARROW),
            hbrBackground: core::ptr::null_mut(),
            lpszMenuName: core::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: LoadIconW(hinst, IMAGE_ICON_APP),
        };
        if RegisterClassExW(&wc) == 0 {
            alert(core::ptr::null_mut(), "窗口类注册失败");
            return;
        }

        TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(widez("TaskbarCreated").as_ptr()));

        let dpi = primary_dpi();
        let light = system_uses_light_theme();
        let app = match App::new(
            core::ptr::null_mut(),
            dpi,
            quotes,
            style,
            light,
            exe.clone(),
        ) {
            Some(a) => a,
            None => {
                alert(core::ptr::null_mut(), "初始化渲染表面失败");
                return;
            }
        };
        let app = Box::into_raw(Box::new(app));

        let ex_style =
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let hwnd = CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            widez("JinJuApp").as_ptr(),
            WS_POPUP,
            // Position is finalized by reposition(); pass CW_USEDEFAULT.
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale_pt(WINDOW_W, dpi),
            scale_pt(WINDOW_H, dpi),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            hinst,
            app as *mut core::ffi::c_void,
        );
        if hwnd.is_null() {
            drop(Box::from_raw(app));
            alert(core::ptr::null_mut(), "窗口创建失败");
            return;
        }

        {
            let app = &mut *app;
            app.reposition();
            app.next_quote();
            SetTimer(hwnd, QUOTE_TIMER, QUOTE_INTERVAL_MS, core::ptr::null_mut());
            SetTimer(
                hwnd,
                Z_ORDER_TIMER,
                Z_ORDER_INTERVAL_MS,
                core::ptr::null_mut(),
            );
        }

        // Instant z-order recovery whenever the taskbar covers the strip:
        // see `on_taskbar_reordered`.
        STRIP_HWND.store(hwnd as isize, std::sync::atomic::Ordering::Relaxed);
        SetWinEventHook(
            EVENT_OBJECT_REORDER,
            EVENT_OBJECT_REORDER,
            core::ptr::null_mut(),
            on_taskbar_reordered,
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );

        // The tray icon is opt-out: `settings.json`'s `tray_icon: 0`
        // keeps it out of the notification area. Without the icon the
        // app can only be exited via Task Manager.
        if (*app).style.tray_icon == 1 && !tray::add(hwnd) {
            alert(
                hwnd,
                "无法创建托盘图标：请通过任务管理器结束 JinJuApp.exe 退出。",
            );
        }
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg: MSG = core::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, core::ptr::null_mut(), 0, 0);
            if r <= 0 {
                // 0 = WM_QUIT, -1 = error: either way, stop.
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure halves)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_interactive() {
        assert_eq!(parse_cli(&[]), Cli { quiet: false });
    }

    #[test]
    fn cli_accepts_quiet_and_short_form() {
        assert_eq!(parse_cli(&["--quiet".to_string()]), Cli { quiet: true });
        assert_eq!(parse_cli(&["-q".to_string()]), Cli { quiet: true });
    }

    #[test]
    fn cli_ignores_other_arguments() {
        assert_eq!(
            parse_cli(&["--foo".to_string(), "bar".to_string()]),
            Cli { quiet: false }
        );
    }

    #[test]
    fn crossfade_alpha_ramps_linearly_and_clamps() {
        assert_eq!(crossfade_alpha(0), 0.0);
        assert_eq!(crossfade_alpha(TRANSITION_MS / 2), 0.5);
        assert_eq!(crossfade_alpha(TRANSITION_MS), 1.0);
        assert_eq!(crossfade_alpha(u64::MAX), 1.0);
    }

    #[test]
    fn scale_pt_scales_by_dpi() {
        assert_eq!(scale_pt(500, 96), 500);
        assert_eq!(scale_pt(500, 192), 1000);
        assert_eq!(scale_pt(500, 144), 750);
        assert_eq!(scale_pt(45, 96), 45);
    }

    #[test]
    fn scale_pt_rounds_up_at_half() {
        // 45 * 1.25 = 56.25 → 57 (96→120 dpi)
        assert_eq!(scale_pt(45, 120), 57);
    }
}
