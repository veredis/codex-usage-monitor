//! Hover popup over a quota countdown: the exact local reset date and time,
//! or what `--` / `!--` mean.
//!
//! Behaviour follows benb-atlantis' upstream PR #16 popup: shown only over the
//! hovered text, centred over it, 12 logical pixels outside the taskbar
//! (below it for a top-edge taskbar), clamped to the monitor, never
//! activated, dismissed on leave, drag, menus and hiding. Instead of PR #16's
//! custom window and menu hook, it uses the native tooltip control as upstream
//! main's `quota_tooltip.rs` does, in tracking mode: native look, no focus
//! changes, no extra hooks.
use std::sync::Mutex;

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Logical gap between the taskbar edge and the popup (PR #16).
pub const TASKBAR_GAP: i32 = 12;

struct State {
    window: isize,
    owner: isize,
    text: Vec<u16>,
    visible: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    window: 0,
    owner: 0,
    text: Vec::new(),
    visible: false,
});

// No comctl32 v6 dependency: exclude the v6-only reserved field so the
// standard tooltip control accepts TOOLINFO (as upstream does).
const TOOL_INFO_SIZE: u32 = std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32;

fn tool_info(owner: HWND, text: &mut [u16]) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: TOOL_INFO_SIZE,
        uFlags: TTF_TRACK | TTF_ABSOLUTE,
        hwnd: owner,
        uId: 1,
        lpszText: PWSTR(text.as_mut_ptr()),
        ..Default::default()
    }
}

/// Where the popup goes: centred on `anchor`, outside the taskbar, inside the
/// monitor. All values are physical screen pixels.
pub fn place(anchor: RECT, size: (i32, i32), taskbar: RECT, monitor: RECT, gap: i32) -> (i32, i32) {
    let (width, height) = size;
    let center = (anchor.left + anchor.right) / 2;
    let monitor_middle = (monitor.top + monitor.bottom) / 2;
    let below = taskbar.bottom <= monitor_middle;
    let y = if below {
        taskbar.bottom + gap
    } else {
        taskbar.top - gap - height
    };
    let x = (center - width / 2).clamp(monitor.left, (monitor.right - width).max(monitor.left));
    let y = y.clamp(monitor.top, (monitor.bottom - height).max(monitor.top));
    (x, y)
}

/// Show `text` for the hovered `anchor` (screen coordinates).
pub fn show(owner: HWND, text: &str, anchor: RECT, taskbar: RECT, gap: i32) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        if state.window != 0 && state.owner != owner.0 as isize {
            let _ = DestroyWindow(HWND(state.window as *mut _));
            state.window = 0;
        }
        if state.window == 0 {
            let controls = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES,
            };
            let _ = InitCommonControlsEx(&controls);
            let Ok(window) = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("tooltips_class32"),
                w!(""),
                WS_POPUP | WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                owner,
                None,
                None,
                None,
            ) else {
                return;
            };
            state.window = window.0 as isize;
            state.owner = owner.0 as isize;
            SendMessageW(window, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(360));
            let mut empty = vec![0u16];
            let info = tool_info(owner, &mut empty);
            SendMessageW(
                window,
                TTM_ADDTOOLW,
                WPARAM(0),
                LPARAM(&info as *const _ as isize),
            );
        }
        let window = HWND(state.window as *mut _);
        state.text = text.encode_utf16().chain(Some(0)).collect();
        let info = tool_info(owner, &mut state.text);
        SendMessageW(
            window,
            TTM_UPDATETIPTEXTW,
            WPARAM(0),
            LPARAM(&info as *const _ as isize),
        );
        // TTM_GETBUBBLESIZE needs Common Controls 6, which this app does not
        // load; activate first and measure the tooltip window instead.
        let track_position = |x: i32, y: i32| {
            SendMessageW(
                window,
                TTM_TRACKPOSITION,
                WPARAM(0),
                LPARAM(((y as u32 & 0xFFFF) << 16 | (x as u32 & 0xFFFF)) as isize),
            );
        };
        if !state.visible {
            track_position(anchor.left, anchor.top);
            SendMessageW(
                window,
                TTM_TRACKACTIVATE,
                WPARAM(1),
                LPARAM(&info as *const _ as isize),
            );
            state.visible = true;
        }
        let mut bounds = RECT::default();
        let size = if GetWindowRect(window, &mut bounds).is_ok() {
            (bounds.right - bounds.left, bounds.bottom - bounds.top)
        } else {
            (0, 0)
        };
        let monitor = {
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
        };
        let (x, y) = place(anchor, size, taskbar, monitor, gap);
        track_position(x, y);
    }
}

#[cfg(test)]
fn is_visible() -> bool {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).visible
}

/// Dismiss without destroying the control.
pub fn hide() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.window == 0 || !state.visible {
        return;
    }
    unsafe {
        let mut empty = vec![0u16];
        let info = tool_info(HWND(state.owner as *mut _), &mut empty);
        SendMessageW(
            HWND(state.window as *mut _),
            TTM_TRACKACTIVATE,
            WPARAM(0),
            LPARAM(&info as *const _ as isize),
        );
    }
    state.visible = false;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tracking_tooltip_shows_and_hides_without_activation() {
        unsafe {
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("quota-popup-test"),
                WS_POPUP,
                0,
                0,
                400,
                60,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let before = GetForegroundWindow();
            show(
                owner,
                "Codex 5h resets Thu 10/31/2026 6:59 PM",
                rect(100, 1040, 160, 1056),
                rect(0, 1032, 1920, 1080),
                12,
            );
            assert!(is_visible());
            let popup = HWND(STATE.lock().unwrap().window as *mut _);
            let style = GetWindowLongW(popup, GWL_EXSTYLE) as u32;
            assert!(style & WS_EX_NOACTIVATE.0 != 0);
            assert_eq!(GetForegroundWindow(), before, "focus unchanged");
            hide();
            assert!(!is_visible());
            let _ = DestroyWindow(owner);
        }
    }
}
