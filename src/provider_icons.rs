//! Provider marks drawn beside each provider's quota column.
//!
//! The marks and generator come from upstream (`src/icons/providers`, Lobe
//! Icons, MIT; Antigravity from SoKeiKei's PR #30). Smoothing follows
//! benb-atlantis' PR #16: each mark is a single colour plus alpha, so its
//! 64 px coverage is area-averaged into the exact physical size (fractional
//! DPI included) and blended over the real background, avoiding GDI's aliased
//! icon scaling and dark halos. Masks are cached per provider and size. PR #16
//! smoothed only the OpenAI mark; here every mark uses the same path, with
//! `DrawIconEx` as the fallback.
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::Color;

/// Logical mark size and the gap to its bars.
pub const SIZE: i32 = 16;
pub const RIGHT_MARGIN: i32 = 5;

const SOURCE_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
    Antigravity,
}

impl Provider {
    fn png(self) -> &'static [u8] {
        match self {
            Self::Claude => include_bytes!("icons/providers/claude.png"),
            Self::Codex => include_bytes!("icons/providers/openai.png"),
            Self::Antigravity => include_bytes!("icons/providers/antigravity.png"),
        }
    }

    /// The artwork's own colour (see `scripts/generate-provider-icons.ps1`).
    pub fn brand_color(self) -> Color {
        Color::from_hex(match self {
            Self::Claude => "#D97757",
            Self::Codex => "#10A37F",
            Self::Antigravity => "#4285F4",
        })
    }

    fn index(self) -> usize {
        self as usize
    }
}

fn icon(provider: Provider) -> HICON {
    // Embedded resources and native handles live for the process lifetime.
    static HANDLES: [OnceLock<isize>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let handle = *HANDLES[provider.index()].get_or_init(|| unsafe {
        CreateIconFromResourceEx(provider.png(), true, 0x00030000, 64, 64, LR_DEFAULTCOLOR)
            .map(|icon| icon.0 as isize)
            .unwrap_or(0)
    });
    HICON(handle as *mut _)
}

/// The mark's 64 px alpha coverage, read once from its icon bitmap.
fn source_alpha(provider: Provider) -> Option<&'static [u8]> {
    static ALPHA: [OnceLock<Option<Vec<u8>>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    ALPHA[provider.index()]
        .get_or_init(|| unsafe {
            let mut info = ICONINFO::default();
            GetIconInfo(icon(provider), &mut info).ok()?;
            let dc = CreateCompatibleDC(HDC::default());
            let mut bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: SOURCE_SIZE as i32,
                    biHeight: -(SOURCE_SIZE as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u32; SOURCE_SIZE * SOURCE_SIZE];
            let copied = if dc.is_invalid() || info.hbmColor.is_invalid() {
                0
            } else {
                GetDIBits(
                    dc,
                    info.hbmColor,
                    0,
                    SOURCE_SIZE as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut bitmap_info,
                    DIB_RGB_COLORS,
                )
            };
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
            let _ = DeleteDC(dc);
            if copied != SOURCE_SIZE as i32 || pixels.iter().all(|p| p >> 24 == 0) {
                return None;
            }
            Some(pixels.into_iter().map(|p| (p >> 24) as u8).collect())
        })
        .as_deref()
}

/// Average coverage over each destination pixel, including fractional source
/// pixels at non-integral DPI sizes.
fn resample_alpha(source: &[u8], source_size: usize, size: usize) -> Vec<u8> {
    let ratio = source_size as f64 / size as f64;
    let mut mask = vec![0; size * size];
    for y in 0..size {
        let top = y as f64 * ratio;
        let bottom = (y + 1) as f64 * ratio;
        for x in 0..size {
            let left = x as f64 * ratio;
            let right = (x + 1) as f64 * ratio;
            let mut alpha = 0.0;
            for sy in top.floor() as usize..(bottom.ceil() as usize).min(source_size) {
                let dy = bottom.min((sy + 1) as f64) - top.max(sy as f64);
                for sx in left.floor() as usize..(right.ceil() as usize).min(source_size) {
                    let dx = right.min((sx + 1) as f64) - left.max(sx as f64);
                    alpha += source[sy * source_size + sx] as f64 * dx * dy;
                }
            }
            mask[y * size + x] = (alpha / (ratio * ratio)).round().clamp(0.0, 255.0) as u8;
        }
    }
    mask
}

/// Blend `color` over existing BGR pixels by coverage; transparent pixels
/// keep the background exactly, so edges have no halo.
fn blend(pixels: &mut [u32], alpha: &[u8], color: Color) {
    for (pixel, &alpha) in pixels.iter_mut().zip(alpha) {
        let alpha = alpha as u32;
        let channel = |shift: u32, foreground: u8| {
            (((*pixel >> shift) & 255u32) * (255 - alpha) + foreground as u32 * alpha + 127) / 255
        };
        *pixel = channel(16, color.r) << 16 | channel(8, color.g) << 8 | channel(0, color.b);
    }
}

fn mask_for(provider: Provider, size: i32) -> Option<Vec<u8>> {
    static MASKS: OnceLock<Mutex<BTreeMap<(Provider, i32), Vec<u8>>>> = OnceLock::new();
    let source = source_alpha(provider)?;
    let mut masks = MASKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Some(
        masks
            .entry((provider, size))
            .or_insert_with(|| resample_alpha(source, SOURCE_SIZE, size as usize))
            .clone(),
    )
}

fn draw_smooth(hdc: HDC, x: i32, y: i32, size: i32, provider: Provider) -> bool {
    let Some(alpha) = mask_for(provider, size) else {
        return false;
    };
    unsafe {
        let dc = CreateCompatibleDC(hdc);
        if dc.is_invalid() {
            return false;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap =
            CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if bitmap.is_invalid() || bits.is_null() {
            let _ = DeleteDC(dc);
            return false;
        }
        let previous = SelectObject(dc, bitmap);
        let mut drawn = false;
        if BitBlt(dc, 0, 0, size, size, hdc, x, y, SRCCOPY).is_ok() {
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
            blend(pixels, &alpha, provider.brand_color());
            drawn = BitBlt(hdc, x, y, size, size, dc, 0, 0, SRCCOPY).is_ok();
        }
        SelectObject(dc, previous);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(dc);
        drawn
    }
}

/// Draw `provider`'s mark at physical `size` pixels.
pub fn draw(hdc: HDC, x: i32, y: i32, size: i32, provider: Provider) {
    if size <= 0 || draw_smooth(hdc, x, y, size, provider) {
        return;
    }
    let handle = icon(provider);
    if !handle.is_invalid() {
        unsafe {
            let _ = DrawIconEx(
                hdc,
                x,
                y,
                handle,
                size,
                size,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mark_decodes_with_transparent_and_covered_pixels() {
        for provider in [Provider::Claude, Provider::Codex, Provider::Antigravity] {
            let alpha = source_alpha(provider).expect("embedded mark decodes");
            assert_eq!(alpha.len(), SOURCE_SIZE * SOURCE_SIZE);
            assert!(
                alpha.iter().any(|&a| a == 0),
                "{provider:?} has transparent gaps"
            );
            assert!(
                alpha.iter().any(|&a| a > 200),
                "{provider:?} has solid coverage"
            );
        }
    }

    #[test]
    fn resampling_keeps_partial_coverage_at_fractional_sizes() {
        let alpha = source_alpha(Provider::Claude).unwrap();
        for size in [16usize, 20, 24, 28, 32] {
            let mask = resample_alpha(alpha, SOURCE_SIZE, size);
            assert_eq!(mask.len(), size * size);
            assert!(
                mask.iter().any(|&a| a > 0 && a < 255),
                "edges at {size}px are antialiased"
            );
            assert!(mask.iter().any(|&a| a == 0), "transparent regions survive");
        }
        // Uniform coverage is preserved exactly.
        let solid = vec![255u8; 64 * 64];
        assert!(resample_alpha(&solid, 64, 20).iter().all(|&a| a == 255));
    }

    #[test]
    fn blending_leaves_background_where_the_mark_is_absent() {
        let mut pixels = vec![0x1C1C1C, 0x1C1C1C, 0x1C1C1C];
        blend(&mut pixels, &[0, 128, 255], Color::from_hex("#D97757"));
        assert_eq!(pixels[0], 0x1C1C1C);
        assert_eq!(pixels[2], 0xD97757);
        assert!(pixels[1] != 0x1C1C1C && pixels[1] != 0xD97757);
    }
}
