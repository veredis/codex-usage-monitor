//! Hover popup over a quota countdown: the exact local reset date and time,
//! or what `--` / `!--` mean.
//!
//! Placement follows benb-atlantis' upstream PR #16 popup: centred over the
//! hovered text, 12 logical pixels outside the taskbar (below a top taskbar,
//! beside a side taskbar), clamped to the monitor, never activated, dismissed
//! on leave, drag, menus and hiding.
//!
//! Build 13 used the native tooltip control in tracking mode. Without Common
//! Controls 6 that control first showed itself at the tracked point (over the
//! taskbar, under the pointer) and resized in place on every text change
//! before it could be moved; a bubble briefly under the pointer took the
//! mouse from the widget, which dismissed and reopened it. This popup is an
//! owned layered window instead: it is measured first, then positioned, sized
//! and painted in one `UpdateLayeredWindow` call, so no intermediate frame
//! exists; it is click-through, so it never takes the pointer; and it draws
//! its own Windows 11 style dark or light frame with rounded corners.
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Logical gap between the taskbar edge and the popup (PR #16).
pub const TASKBAR_GAP: i32 = 12;
// Logical metrics, close to Windows 11's tooltips and context menus.
const FONT_PX: i32 = 12;
const PADDING_X: i32 = 10;
const PADDING_Y: i32 = 6;
const CORNER_RADIUS: i32 = 6;
const MAX_TEXT_WIDTH: i32 = 360;

/// Popup colours; chosen by the caller from the widget's palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    HighContrastDark,
    HighContrastLight,
}

/// Background, text and border as `0xRRGGBB`.
fn theme_colors(theme: Theme) -> (u32, u32, u32) {
    if let Some(system) = system_high_contrast_colors() {
        return system;
    }
    match theme {
        // Windows 11 dark context menu surface.
        Theme::Dark => (0x2C2C2C, 0xF3F3F3, 0x454545),
        Theme::Light => (0xF9F9F9, 0x1B1B1B, 0xD5D5D5),
        Theme::HighContrastDark => (0x1C1C1C, 0xFFFFFF, 0x8A8A8A),
        Theme::HighContrastLight => (0xFFFFFF, 0x000000, 0x6E6E6E),
    }
}

/// Windows high-contrast mode overrides the palette with the tooltip system
/// colours, as native tooltips do.
fn system_high_contrast_colors() -> Option<(u32, u32, u32)> {
    unsafe {
        let mut contrast = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some(&mut contrast as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        if contrast.dwFlags & HCF_HIGHCONTRASTON == HCF_HIGHCONTRASTON {
            let rgb = |index| {
                let bgr = GetSysColor(index);
                (bgr & 0xFF) << 16 | (bgr & 0xFF00) | (bgr >> 16 & 0xFF)
            };
            Some((
                rgb(COLOR_INFOBK),
                rgb(COLOR_INFOTEXT),
                rgb(COLOR_WINDOWFRAME),
            ))
        } else {
            None
        }
    }
}

fn scale(px: i32, dpi: u32) -> i32 {
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

struct State {
    window: isize,
    visible: bool,
    /// Text, theme and physical bounds on screen, to skip identical updates.
    shown: Option<(Vec<u16>, Theme, RECT)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    window: 0,
    visible: false,
    shown: None,
});

/// Where the popup goes, in physical screen pixels: outside the taskbar,
/// centred on `anchor` along it, inside the monitor.
pub fn place(anchor: RECT, size: (i32, i32), taskbar: RECT, monitor: RECT, gap: i32) -> (i32, i32) {
    let (width, height) = size;
    let max_x = (monitor.right - width).max(monitor.left);
    let max_y = (monitor.bottom - height).max(monitor.top);
    let vertical = taskbar.bottom - taskbar.top > taskbar.right - taskbar.left;
    if vertical {
        let left_edge = taskbar.right <= (monitor.left + monitor.right) / 2;
        let x = if left_edge {
            taskbar.right + gap
        } else {
            taskbar.left - gap - width
        };
        let center = (anchor.top + anchor.bottom) / 2;
        (
            x.clamp(monitor.left, max_x),
            (center - height / 2).clamp(monitor.top, max_y),
        )
    } else {
        let top_edge = taskbar.bottom <= (monitor.top + monitor.bottom) / 2;
        let y = if top_edge {
            taskbar.bottom + gap
        } else {
            taskbar.top - gap - height
        };
        let center = (anchor.left + anchor.right) / 2;
        (
            (center - width / 2).clamp(monitor.left, max_x),
            y.clamp(monitor.top, max_y),
        )
    }
}

/// Coverage of the frame's outer and inner (inside the border) rounded
/// rectangles at pixel (x, y), from 4x4 samples, for antialiased corners.
fn frame_coverage(x: i32, y: i32, size: (i32, i32), radius: f32, border: f32) -> (f32, f32) {
    let (width, height) = (size.0 as f32, size.1 as f32);
    let inside = |sx: f32, sy: f32, inset: f32| {
        let (left, top, right, bottom) = (inset, inset, width - inset, height - inset);
        let r = (radius - inset).max(0.0);
        if sx < left || sx > right || sy < top || sy > bottom {
            return false;
        }
        let cx = sx.clamp(left + r, (right - r).max(left + r));
        let cy = sy.clamp(top + r, (bottom - r).max(top + r));
        (sx - cx).powi(2) + (sy - cy).powi(2) <= r * r
    };
    let (mut outer, mut inner) = (0u32, 0u32);
    for j in 0..4 {
        for i in 0..4 {
            let sx = x as f32 + (i as f32 + 0.5) / 4.0;
            let sy = y as f32 + (j as f32 + 0.5) / 4.0;
            outer += inside(sx, sy, 0.0) as u32;
            inner += inside(sx, sy, border) as u32;
        }
    }
    (outer as f32 / 16.0, inner as f32 / 16.0)
}

unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

const CLASS_NAME: PCWSTR = w!("CodexUsageQuotaPopup");

unsafe fn ensure_window(state: &mut State) -> Option<HWND> {
    let existing = HWND(state.window as *mut _);
    if state.window != 0 && IsWindow(existing).as_bool() {
        return Some(existing);
    }
    let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(popup_proc),
        hInstance: instance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // Fails harmlessly when already registered.
    RegisterClassExW(&class);
    let window = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        CLASS_NAME,
        w!(""),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        instance,
        None,
    )
    .ok()?;
    state.window = window.0 as isize;
    state.visible = false;
    state.shown = None;
    Some(window)
}

unsafe fn monitor_rect(taskbar: RECT) -> RECT {
    let handle = MonitorFromRect(&taskbar, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(handle, &mut info).as_bool() {
        info.rcMonitor
    } else {
        taskbar
    }
}

/// Show `text` for the hovered `anchor` (physical screen coordinates) at
/// `dpi`. Moving to another cell updates text, size and position at once.
pub fn show(text: &str, anchor: RECT, taskbar: RECT, theme: Theme, dpi: u32) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let Some(window) = ensure_window(&mut state) else {
            return;
        };
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
        let font_name = crate::native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            -scale(FONT_PX, dpi),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(dc, font);
        let flags = DT_LEFT | DT_WORDBREAK | DT_NOPREFIX;
        let mut measured = RECT {
            right: scale(MAX_TEXT_WIDTH, dpi),
            ..Default::default()
        };
        DrawTextW(dc, &mut wide, &mut measured, flags | DT_CALCRECT);
        let (pad_x, pad_y) = (scale(PADDING_X, dpi), scale(PADDING_Y, dpi));
        let size = (measured.right + 2 * pad_x, measured.bottom + 2 * pad_y);
        let (x, y) = place(
            anchor,
            size,
            taskbar,
            monitor_rect(taskbar),
            scale(TASKBAR_GAP, dpi),
        );
        let bounds = RECT {
            left: x,
            top: y,
            right: x + size.0,
            bottom: y + size.1,
        };
        let key = (wide.clone(), theme, bounds);
        if !(state.visible && state.shown.as_ref() == Some(&key)) {
            render_and_update(
                window,
                screen,
                dc,
                &mut wide,
                flags,
                theme,
                size,
                (x, y),
                dpi,
            );
            if !state.visible {
                // Content and position are already set; this only shows the
                // window above the taskbar without activating it.
                let _ = SetWindowPos(
                    window,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                state.visible = true;
            }
            state.shown = Some(key);
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(font);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
    }
}

/// Paint the popup into a premultiplied DIB and push text, size and position
/// to the window in one call.
#[allow(clippy::too_many_arguments)]
unsafe fn render_and_update(
    window: HWND,
    screen: HDC,
    dc: HDC,
    text: &mut [u16],
    flags: DRAW_TEXT_FORMAT,
    theme: Theme,
    size: (i32, i32),
    origin: (i32, i32),
    dpi: u32,
) {
    let (width, height) = size;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
        return;
    };
    if bits.is_null() {
        let _ = DeleteObject(dib);
        return;
    }
    let old_bitmap = SelectObject(dc, dib);
    let (background, foreground, border) = theme_colors(theme);
    let colorref = |rgb: u32| COLORREF((rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16 & 0xFF));
    let brush = CreateSolidBrush(colorref(background));
    FillRect(
        dc,
        &RECT {
            right: width,
            bottom: height,
            ..Default::default()
        },
        brush,
    );
    let _ = DeleteObject(brush);
    // Opaque background, so ClearType text stays crisp.
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, colorref(foreground));
    let (pad_x, pad_y) = (scale(PADDING_X, dpi), scale(PADDING_Y, dpi));
    let mut text_rect = RECT {
        left: pad_x,
        top: pad_y,
        right: width - pad_x,
        bottom: height - pad_y,
    };
    DrawTextW(dc, text, &mut text_rect, flags);
    let _ = GdiFlush();

    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    let radius = scale(CORNER_RADIUS, dpi) as f32;
    let channel = |value: u32, shift: u32| (value >> shift & 0xFF) as f32;
    for y in 0..height {
        for x in 0..width {
            let (outer, inner) = frame_coverage(x, y, size, radius, 1.0);
            let pixel = &mut pixels[(y * width + x) as usize];
            let mix = |shift| {
                (channel(*pixel, shift) * inner + channel(border, shift) * (outer - inner)).round()
                    as u32
            };
            *pixel = ((outer * 255.0).round() as u32) << 24 | mix(16) << 16 | mix(8) << 8 | mix(0);
        }
    }

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        window,
        screen,
        Some(&POINT {
            x: origin.0,
            y: origin.1,
        }),
        Some(&SIZE {
            cx: width,
            cy: height,
        }),
        dc,
        Some(&POINT::default()),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    SelectObject(dc, old_bitmap);
    let _ = DeleteObject(dib);
}

#[cfg(test)]
fn is_visible() -> bool {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).visible
}

/// Dismiss without destroying the window.
pub fn hide() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.window == 0 || !state.visible {
        return;
    }
    unsafe {
        let _ = ShowWindow(HWND(state.window as *mut _), SW_HIDE);
    }
    state.visible = false;
    state.shown = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The popup is process-wide; tests that show it take turns.
    static SCREEN: Mutex<()> = Mutex::new(());

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn popup_sits_above_a_bottom_taskbar_centred_on_the_countdown() {
        let monitor = rect(0, 0, 2560, 1080);
        let taskbar = rect(0, 1032, 2560, 1080);
        let anchor = rect(1700, 1040, 1760, 1056);
        let (x, y) = place(anchor, (200, 24), taskbar, monitor, 12);
        assert_eq!((x, y), (1630, 1032 - 12 - 24));
    }

    #[test]
    fn popup_goes_below_a_top_taskbar_and_stays_on_the_monitor() {
        let monitor = rect(-1920, 0, 0, 1080);
        let taskbar = rect(-1920, 0, 0, 48);
        let near_right_edge = rect(-40, 10, -10, 26);
        let (x, y) = place(near_right_edge, (200, 24), taskbar, monitor, 15);
        assert_eq!(y, 48 + 15);
        assert_eq!(x, -200, "clamped to the monitor's right edge");
        let near_left_edge = rect(-1915, 10, -1900, 26);
        assert_eq!(
            place(near_left_edge, (200, 24), taskbar, monitor, 15).0,
            -1920
        );
    }

    #[test]
    fn popup_sits_beside_a_side_taskbar_centred_on_the_countdown() {
        let monitor = rect(0, 0, 1920, 1080);
        let left = rect(0, 0, 60, 1080);
        let anchor = rect(8, 900, 52, 916);
        assert_eq!(
            place(anchor, (200, 40), left, monitor, 12),
            (60 + 12, 908 - 20)
        );
        let right = rect(1860, 0, 1920, 1080);
        let anchor = rect(1868, 1060, 1912, 1076);
        assert_eq!(
            place(anchor, (200, 40), right, monitor, 12),
            (1860 - 12 - 200, 1080 - 40),
            "clamped to the monitor's bottom edge"
        );
    }

    #[test]
    fn frame_has_antialiased_rounded_corners_and_a_border_ring() {
        let size = (120, 30);
        let (outer, _) = frame_coverage(0, 0, size, 6.0, 1.0);
        assert_eq!(outer, 0.0, "the corner pixel is transparent");
        let (outer, inner) = frame_coverage(1, 2, size, 6.0, 1.0);
        assert!(outer > 0.0 && outer < 1.0, "antialiased curve");
        assert!(inner < outer);
        assert_eq!(
            frame_coverage(60, 0, size, 6.0, 1.0),
            (1.0, 0.0),
            "top border"
        );
        assert_eq!(
            frame_coverage(60, 15, size, 6.0, 1.0),
            (1.0, 1.0),
            "interior"
        );
    }

    unsafe fn popup_window() -> HWND {
        HWND(STATE.lock().unwrap().window as *mut _)
    }

    thread_local!(static POSITIONS: RefCell<Vec<(isize, RECT)>> = const { RefCell::new(Vec::new()) });

    // Runs inside show(), so it must not lock STATE; filtered afterwards.
    unsafe extern "system" fn record_positions(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let message = &*(lparam.0 as *const CWPSTRUCT);
        if message.message == WM_WINDOWPOSCHANGED {
            let pos = &*(message.lParam.0 as *const WINDOWPOS);
            if pos.flags & SWP_NOMOVE != SWP_NOMOVE {
                let bounds = rect(pos.x, pos.y, pos.x + pos.cx, pos.y + pos.cy);
                POSITIONS.with(|log| log.borrow_mut().push((message.hwnd.0 as isize, bounds)));
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    #[test]
    fn popup_moves_straight_to_each_cell_without_activation() {
        let _screen = SCREEN.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let monitor = monitor_rect(rect(0, 1032, 1920, 1080));
            let taskbar = rect(
                monitor.left,
                monitor.bottom - 48,
                monitor.right,
                monitor.bottom,
            );
            let cell_a = rect(
                taskbar.right - 420,
                taskbar.top + 8,
                taskbar.right - 380,
                taskbar.top + 24,
            );
            let cell_b = rect(
                taskbar.right - 370,
                taskbar.top + 8,
                taskbar.right - 330,
                taskbar.top + 24,
            );
            let before = GetForegroundWindow();
            let hook = SetWindowsHookExW(
                WH_CALLWNDPROC,
                Some(record_positions),
                None,
                windows::Win32::System::Threading::GetCurrentThreadId(),
            )
            .unwrap();
            let steps = [
                (cell_a, "Codex 5h resets Thu 10/31/2026 6:59 PM"),
                (
                    cell_b,
                    "Claude Code 7d resets Wed 11/5/2026 11:00 AM\n! means this may be out of date (last confirmed Thu 10/30/2026 2:01 PM).",
                ),
                (cell_a, "Codex 5h resets Thu 10/31/2026 6:59 PM"),
            ];
            for (cell, text) in steps {
                show(text, cell, taskbar, Theme::Dark, 96);
                // Identical updates (pointer moves within a cell) are skipped.
                show(text, cell, taskbar, Theme::Dark, 96);
                let mut bounds = RECT::default();
                GetWindowRect(popup_window(), &mut bounds).unwrap();
                assert!(bounds.bottom <= taskbar.top - 12, "outside the taskbar");
                let centre = (bounds.left + bounds.right) / 2;
                assert!(
                    (centre - (cell.left + cell.right) / 2).abs() <= 1,
                    "anchored to the cell"
                );
                let popup = popup_window().0 as isize;
                let moves: Vec<RECT> = POSITIONS
                    .with(|log| std::mem::take(&mut *log.borrow_mut()))
                    .into_iter()
                    .filter_map(|(window, bounds)| (window == popup).then_some(bounds))
                    .collect();
                // UpdateLayeredWindow moves, resizes and repaints in one step
                // without window-position messages; nothing else may move it
                // anywhere but the final place.
                assert!(
                    moves.iter().all(|&step| step == bounds),
                    "intermediate placement {moves:?} before {bounds:?}"
                );
            }
            let _ = UnhookWindowsHookEx(hook);
            assert!(is_visible());
            let popup = popup_window();
            let style = GetWindowLongW(popup, GWL_EXSTYLE) as u32;
            for flag in [
                WS_EX_NOACTIVATE,
                WS_EX_TRANSPARENT,
                WS_EX_LAYERED,
                WS_EX_TOOLWINDOW,
            ] {
                assert!(style & flag.0 != 0, "{flag:?}");
            }
            assert_ne!(GetForegroundWindow(), popup);
            assert_eq!(GetForegroundWindow(), before, "focus unchanged");
            hide();
            assert!(!is_visible());
            assert!(!IsWindowVisible(popup).as_bool());
        }
    }

    /// Shows each theme on screen and captures the composed result to
    /// `target/render-fixtures/popup-*.bmp` for visual review.
    #[test]
    #[ignore]
    fn render_popup_fixtures() {
        let _screen = SCREEN.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("render-fixtures");
        std::fs::create_dir_all(&directory).unwrap();
        unsafe {
            let monitor = monitor_rect(rect(0, 1032, 1920, 1080));
            let taskbar = rect(
                monitor.left,
                monitor.bottom - 48,
                monitor.right,
                monitor.bottom,
            );
            let cell = rect(
                taskbar.right - 420,
                taskbar.top + 8,
                taskbar.right - 380,
                taskbar.top + 24,
            );
            for (name, theme, dpi, text) in [
                ("popup-dark-100", Theme::Dark, 96, "Codex 5h resets Thu 10/31/2026 6:59 PM"),
                (
                    "popup-dark-stale-150",
                    Theme::Dark,
                    144,
                    "Claude Code 7d resets Wed 11/5/2026 11:00 AM\n! means this may be out of date (last confirmed Thu 10/30/2026 2:01 PM).",
                ),
                ("popup-light-125", Theme::Light, 120, "Codex 7d: not reported for this account.\n-- means unavailable, not 0% used or 100% remaining."),
                ("popup-hc-dark-100", Theme::HighContrastDark, 96, "Codex 5h resets Thu 10/31/2026 6:59 PM"),
                ("popup-hc-light-100", Theme::HighContrastLight, 96, "Codex 5h resets Thu 10/31/2026 6:59 PM"),
            ] {
                show(text, cell, taskbar, theme, dpi);
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut bounds = RECT::default();
                GetWindowRect(popup_window(), &mut bounds).unwrap();
                let margin = 6;
                let (left, top) = (bounds.left - margin, bounds.top - margin);
                let width = bounds.right - bounds.left + 2 * margin;
                let height = bounds.bottom - bounds.top + 2 * margin;
                let pixels = crate::native_interop::capture_screen(left, top, width, height);
                crate::native_interop::write_bmp(&directory.join(format!("{name}.bmp")), width, height, &pixels);
            }
            hide();
        }
    }
}
