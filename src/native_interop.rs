use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, FILETIME, HWND, LPARAM, RECT, SYSTEMTIME};
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_HIGHLIGHT};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::ViewManagement::{UIColorType, UISettings};

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;
pub const TIMER_CREDENTIAL_WATCH: usize = 5;
pub const TIMER_LAYOUT_RECONCILE: usize = 6;
/// One-shot refresh shortly after the system resumes.
pub const TIMER_RESUME_RECOVERY: usize = 7;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

const WINDOWS_TO_UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;

/// `value` in the user's locale and time zone, e.g. `Thu 10/31/2026 6:59 PM`
/// (weekday, short date, time without seconds).
pub fn format_local_datetime(value: SystemTime) -> Option<String> {
    use windows::Win32::Globalization::{
        GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS,
    };
    let local = system_time_to_local(value)?;
    let read = |buffer: &[u16], len: i32| {
        (len > 1).then(|| String::from_utf16_lossy(&buffer[..len as usize - 1]))
    };
    unsafe {
        let mut weekday = [0u16; 32];
        let weekday_format = wide_str("ddd");
        let weekday_len = GetDateFormatEx(
            PCWSTR::null(),
            Default::default(),
            Some(&local),
            PCWSTR::from_raw(weekday_format.as_ptr()),
            Some(&mut weekday),
            PCWSTR::null(),
        );
        let mut date = [0u16; 64];
        let date_len = GetDateFormatEx(
            PCWSTR::null(),
            DATE_SHORTDATE,
            Some(&local),
            PCWSTR::null(),
            Some(&mut date),
            PCWSTR::null(),
        );
        let mut time = [0u16; 64];
        let time_len = GetTimeFormatEx(
            PCWSTR::null(),
            TIME_NOSECONDS,
            Some(&local),
            PCWSTR::null(),
            Some(&mut time),
        );
        let parts: Vec<String> = [
            read(&weekday, weekday_len),
            read(&date, date_len),
            read(&time, time_len),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

pub fn system_time_to_local(value: SystemTime) -> Option<SYSTEMTIME> {
    let unix_seconds = value.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let file_ticks = unix_seconds
        .checked_add(WINDOWS_TO_UNIX_EPOCH_SECONDS)?
        .checked_mul(10_000_000)?;
    let file_time = FILETIME {
        dwLowDateTime: file_ticks as u32,
        dwHighDateTime: (file_ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(local)
}

#[derive(Clone, Copy, Debug)]
pub struct TaskbarWindow {
    pub hwnd: HWND,
    pub rect: RECT,
    /// `Shell_TrayWnd` (the primary taskbar) rather than a secondary one.
    pub is_primary: bool,
}

/// Windows display device name (for example `\\.\DISPLAY2`) of a taskbar's
/// monitor. Stable across taskbar reordering, unlike an enumeration index.
/// Adapted from upstream main `taskbar_monitor_device` (upstream-ray, MIT).
pub fn taskbar_monitor_device(taskbar: &TaskbarWindow) -> Option<String> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };
    let point = windows::Win32::Foundation::POINT {
        x: taskbar.rect.left + (taskbar.rect.right - taskbar.rect.left) / 2,
        y: taskbar.rect.top + (taskbar.rect.bottom - taskbar.rect.top) / 2,
    };
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return None;
        }
        let len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        Some(String::from_utf16_lossy(&info.szDevice[..len])).filter(|name| !name.is_empty())
    }
}

pub fn find_taskbars() -> Vec<TaskbarWindow> {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let taskbars = &mut *(lparam.0 as *mut Vec<TaskbarWindow>);
        let mut class_name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_TrayWnd" || class_name == "Shell_SecondaryTrayWnd" {
                if let Some(rect) = get_taskbar_rect(hwnd).or_else(|| get_window_rect_safe(hwnd)) {
                    taskbars.push(TaskbarWindow {
                        hwnd,
                        rect,
                        is_primary: class_name == "Shell_TrayWnd",
                    });
                }
            }
        }
        BOOL(1)
    }

    let mut taskbars: Vec<TaskbarWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut taskbars as *mut _ as isize));
    }
    taskbars.sort_by_key(|taskbar| {
        (
            taskbar.rect.top,
            taskbar.rect.left,
            taskbar.rect.bottom,
            taskbar.rect.right,
        )
    });
    taskbars
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Return visible direct child rectangles for taskbar collision avoidance.
/// The shell owns these windows; callers use the rectangles only as a
/// conservative geometry hint and must tolerate an empty result.
pub fn visible_child_window_rects(parent: HWND, exclude: Option<HWND>) -> Vec<RECT> {
    struct Collection {
        exclude: Option<HWND>,
        rectangles: Vec<RECT>,
    }

    let rectangles = Vec::new();
    unsafe extern "system" fn collect_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let collection = &mut *(lparam.0 as *mut Collection);
        if collection.exclude != Some(hwnd) && IsWindowVisible(hwnd).as_bool() {
            if let Some(rect) = get_window_rect_safe(hwnd) {
                if rect.right > rect.left && rect.bottom > rect.top {
                    collection.rectangles.push(rect);
                }
            }
        }
        BOOL(1)
    }
    let mut collection = Collection {
        exclude,
        rectangles,
    };
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(collect_proc),
            LPARAM(&mut collection as *mut Collection as isize),
        );
    }
    collection.rectangles
}

/// Union of the visible notification-area windows on a taskbar, including
/// nested descendants (secondary taskbars may expose only a clock). Adapted
/// from upstream PR #25 (diogomrpr).
pub fn taskbar_notification_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    struct Search {
        taskbar: RECT,
        notification: Option<RECT>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut class_name = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class_name);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if let Some(rect) = get_window_rect_safe(hwnd).and_then(|rect| {
                crate::taskbar_layout::notification_candidate_rect(
                    search.taskbar,
                    &class_name,
                    rect,
                )
            }) {
                search.notification = Some(match search.notification {
                    Some(previous) => crate::taskbar_layout::union_rect(previous, rect),
                    None => rect,
                });
            }
        }
        BOOL(1)
    }

    let mut search = Search {
        taskbar: get_window_rect_safe(taskbar_hwnd)?,
        notification: None,
    };
    unsafe {
        // EnumChildWindows visits descendants as well as direct children.
        let _ = EnumChildWindows(
            taskbar_hwnd,
            Some(enum_proc),
            LPARAM(&mut search as *mut Search as isize),
        );
    }
    search.notification
}

/// Width and height of the primary taskbar's notification area, used to size
/// the reservation on taskbars that expose no notification window.
pub fn primary_notification_area() -> Option<(i32, i32)> {
    let primary = find_taskbars()
        .into_iter()
        .find(|taskbar| class_name_of(taskbar.hwnd).as_deref() == Some("Shell_TrayWnd"))?;
    let area = find_child_window(primary.hwnd, "TrayNotifyWnd").and_then(get_window_rect_safe)?;
    let height = primary.rect.bottom - primary.rect.top;
    (area.right > area.left && height > 0).then_some((area.right - area.left, height))
}

fn class_name_of(hwnd: HWND) -> Option<String> {
    let mut class_name = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
    (len > 0).then(|| String::from_utf16_lossy(&class_name[..len as usize]))
}

pub fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd).as_bool() }
}

/// True for the taskbar itself or any of its descendant windows.
pub fn is_taskbar_or_descendant(taskbar: HWND, hwnd: HWND) -> bool {
    hwnd == taskbar || unsafe { IsChild(taskbar, hwnd).as_bool() }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut class_name = [0u16; 64];
        let len = GetClassNameW(taskbar_hwnd, &mut class_name);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_SecondaryTrayWnd" {
                return get_window_rect_safe(taskbar_hwnd);
            }
        }

        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

/// Embed our window as a child of the taskbar
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) {
    unsafe {
        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[allow(dead_code)]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Self { r, g, b }
    }

    pub const fn from_colorref(value: u32) -> Self {
        Self {
            r: (value & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: ((value >> 16) & 0xFF) as u8,
        }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}

/// Get the current Windows UI accent color, with the DWM/highlight colors as
/// fallbacks for older or unavailable Windows Runtime configurations.
pub fn windows_accent_color() -> Color {
    if let Ok(settings) = UISettings::new() {
        if let Ok(accent) = settings.GetColorValue(UIColorType::Accent) {
            return Color::new(accent.R, accent.G, accent.B);
        }
    }

    unsafe {
        let mut colorization = 0u32;
        let mut opaque_blend = BOOL(0);
        if DwmGetColorizationColor(&mut colorization, &mut opaque_blend).is_ok() {
            return Color::from_colorref(colorization);
        }

        Color::from_colorref(GetSysColor(COLOR_HIGHLIGHT))
    }
}

/// Show the native Windows color picker and return the selected color.
pub fn choose_custom_color(hwnd: HWND, initial: Color) -> Option<Color> {
    unsafe {
        let mut custom_colors = [COLORREF(0); 16];
        let mut chooser = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: hwnd,
            rgbResult: COLORREF(initial.to_colorref()),
            lpCustColors: custom_colors.as_mut_ptr(),
            Flags: CC_FULLOPEN | CC_RGBINIT,
            ..Default::default()
        };

        ChooseColorW(&mut chooser)
            .as_bool()
            .then(|| Color::from_colorref(chooser.rgbResult.0))
    }
}

/// Test fixtures: the composed screen pixels (`0xRRGGBB`) of a rectangle.
#[cfg(test)]
pub fn capture_screen(left: i32, top: i32, width: i32, height: i32) -> Vec<u32> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
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
        let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        let old = SelectObject(dc, dib);
        BitBlt(
            dc,
            0,
            0,
            width,
            height,
            screen,
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        )
        .unwrap();
        let _ = GdiFlush();
        let pixels = std::slice::from_raw_parts(bits as *const u32, (width * height) as usize)
            .iter()
            .map(|p| p & 0xFFFFFF)
            .collect();
        SelectObject(dc, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        pixels
    }
}

/// Test fixtures: write `0xRRGGBB` pixels as a 24-bit top-down BMP.
#[cfg(test)]
pub fn write_bmp(path: &std::path::Path, width: i32, height: i32, pixels: &[u32]) {
    let row_bytes = (width * 3 + 3) & !3;
    let image_size = (row_bytes * height) as u32;
    let mut bmp = Vec::new();
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(54 + image_size).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&(-height).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0u8; 24]);
    for row in pixels.chunks(width as usize) {
        for p in row {
            bmp.extend_from_slice(&[*p as u8, (*p >> 8) as u8, (*p >> 16) as u8]);
        }
        bmp.extend(std::iter::repeat_n(0u8, (row_bytes - width * 3) as usize));
    }
    std::fs::write(path, bmp).unwrap();
}
