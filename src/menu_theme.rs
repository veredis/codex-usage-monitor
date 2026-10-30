//! Dark or light context menus that match the widget.
//!
//! Windows has no documented switch for dark Win32 menus. uxtheme's
//! `SetPreferredAppMode` (ordinal 135) and `FlushMenuThemes` (ordinal 136)
//! have been stable since Windows 10 1903 (build 18362) and are what Windows
//! Terminal, Notepad++ and other native apps use; menus then keep their
//! native drawing, keyboard handling, accessibility and Windows 11 rounded
//! frame. They are resolved at runtime on those builds only; anywhere else, or
//! if they are missing, menus keep the light default. Windows high-contrast
//! mode still wins: uxtheme ignores the app mode while it is on.
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use windows::core::{w, PCSTR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

/// `PreferredAppMode::ForceDark` / `ForceLight`.
const FORCE_DARK: i32 = 2;
const FORCE_LIGHT: i32 = 3;
const FIRST_SUPPORTED_BUILD: u32 = 18362;

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();

static APPLIED_MODE: AtomicI32 = AtomicI32::new(0);

/// Use dark or light menus from now on; cheap when unchanged.
pub fn apply(dark: bool) {
    let mode = if dark { FORCE_DARK } else { FORCE_LIGHT };
    if APPLIED_MODE.swap(mode, Ordering::Relaxed) == mode {
        return;
    }
    if let Some((set_preferred_app_mode, flush_menu_themes)) = entry_points() {
        unsafe {
            set_preferred_app_mode(mode);
            flush_menu_themes();
        }
    }
}

fn entry_points() -> Option<(SetPreferredAppMode, FlushMenuThemes)> {
    static ENTRY_POINTS: OnceLock<Option<(SetPreferredAppMode, FlushMenuThemes)>> = OnceLock::new();
    *ENTRY_POINTS.get_or_init(|| unsafe {
        if windows_build()? < FIRST_SUPPORTED_BUILD {
            return None;
        }
        let module = LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        let ordinal = |number: usize| PCSTR(number as *const u8);
        let set = GetProcAddress(module, ordinal(135))?;
        let flush = GetProcAddress(module, ordinal(136))?;
        Some((
            std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(set),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(flush),
        ))
    })
}

/// The real Windows build, which version APIs hide without a manifest entry.
fn windows_build() -> Option<u32> {
    let mut buffer = [0u16; 16];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let length = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length]).parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_real_windows_build_and_finds_the_menu_theme_entry_points() {
        let build = windows_build().expect("CurrentBuildNumber");
        assert!(build >= 10240, "{build}");
        if build >= FIRST_SUPPORTED_BUILD {
            assert!(entry_points().is_some());
        }
    }

    /// Opens a real native menu (checked item, separator, disabled item,
    /// submenu arrow) in each mode, captures it from the screen to
    /// `target/render-fixtures/menu-*.bmp`, and closes it. Briefly visible.
    #[test]
    #[ignore = "shows a real menu on screen for visual review"]
    fn render_menu_fixtures() {
        use windows::Win32::Foundation::*;
        use windows::Win32::UI::WindowsAndMessaging::*;

        static CAPTURE: std::sync::Mutex<Option<(RECT, Vec<u32>)>> = std::sync::Mutex::new(None);
        unsafe extern "system" fn capture(_: HWND, _: u32, _: usize, _: u32) {
            if let Ok(menu) = FindWindowW(w!("#32768"), None) {
                let mut bounds = RECT::default();
                if GetWindowRect(menu, &mut bounds).is_ok() {
                    let (width, height) = (
                        bounds.right - bounds.left + 16,
                        bounds.bottom - bounds.top + 16,
                    );
                    let pixels = crate::native_interop::capture_screen(
                        bounds.left - 8,
                        bounds.top - 8,
                        width,
                        height,
                    );
                    *CAPTURE.lock().unwrap() = Some((
                        RECT {
                            right: width,
                            bottom: height,
                            ..bounds
                        },
                        pixels,
                    ));
                }
            }
            let _ = EndMenu();
        }

        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("render-fixtures");
        std::fs::create_dir_all(&directory).unwrap();
        unsafe {
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("menu-theme-test"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            for (name, dark) in [("menu-dark", true), ("menu-light", false)] {
                apply(dark);
                let submenu = CreatePopupMenu().unwrap();
                let _ = AppendMenuW(submenu, MF_CHECKED, 2, w!("Continuous rounded"));
                let menu = CreatePopupMenu().unwrap();
                let _ = AppendMenuW(menu, MF_CHECKED, 1, w!("Show provider icons"));
                let _ = AppendMenuW(menu, MENU_ITEM_FLAGS(0), 3, w!("Reset appearance"));
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                let _ = AppendMenuW(
                    menu,
                    MF_GRAYED,
                    4,
                    w!("Codex Usage 1.9.1-veredis.7 (Build 14)"),
                );
                let _ = AppendMenuW(menu, MF_POPUP, submenu.0 as usize, w!("Bars"));
                SetTimer(owner, 1, 400, Some(capture));
                let _ =
                    TrackPopupMenu(menu, TPM_NONOTIFY | TPM_RETURNCMD, 400, 300, 0, owner, None);
                let _ = KillTimer(owner, 1);
                let _ = DestroyMenu(menu);
                let (size, pixels) = CAPTURE.lock().unwrap().take().expect("menu captured");
                crate::native_interop::write_bmp(
                    &directory.join(format!("{name}.bmp")),
                    size.right,
                    size.bottom,
                    &pixels,
                );
            }
            let _ = DestroyWindow(owner);
        }
    }
}
