//! Text rendering into a per-pixel-alpha ARGB surface.
//!
//! Pipeline: GDI draws each text layer **white on black** into a 32-bpp
//! top-down DIB, and [`accumulate_layer`] blends that mask, scaled by the
//! layer's opacity, into a premultiplied ARGB frame that
//! `UpdateLayeredWindow` expects. Two layers crossfade by simple addition.
//!
//! The mask is read as **luminance**, and the text is rasterised with
//! ClearType: its subpixel hinting snaps stems to the pixel grid the way
//! the taskbar's own text is drawn, and folding R/G/B back into one
//! coverage value keeps the coloured fringes off a per-pixel-alpha
//! surface. Plain grayscale antialiasing (what this used to use) leaves
//! the same glyph about one pixel softer and fatter.
//!
//! The blending is a pure function; the GDI half is a thin resource-owning
//! host.

use crate::settings::Settings;
use crate::win32::*;

fn scale(c: u8, a: u8) -> u8 {
    ((u32::from(c) * u32::from(a) + 127) / 255) as u8
}

/// Accumulate one coverage-mask layer into the premultiplied ARGB frame.
///
/// `mask` and `frame` are BGRA, row-major, top-down; the mask's red
/// channel carries coverage. `alpha_q` is the layer opacity (0-255).
/// Additive with saturation: two crossfading layers simply sum. Pure:
/// no GDI, no globals.
pub fn accumulate_layer(
    mask: &[u8],
    frame: &mut [u8],
    color: (u8, u8, u8),
    max_alpha: u8,
    alpha_q: u8,
) {
    let (mp, _) = mask.as_chunks::<4>();
    let (fp, _) = frame.as_chunks_mut::<4>();
    for (m, f) in mp.iter().zip(fp.iter_mut()) {
        // Luminance (0.299 R + 0.587 G + 0.114 B) of a white-on-black mask
        // pixel: with ClearType the three channels differ per subpixel, and
        // averaging them back is what turns the coloured rasterisation into
        // a usable grayscale coverage value.
        let cov =
            ((u32::from(m[2]) * 77 + u32::from(m[1]) * 150 + u32::from(m[0]) * 29) >> 8) as u8;
        let a = scale(scale(max_alpha, cov), alpha_q);
        f[0] = f[0].saturating_add(scale(color.2, a));
        f[1] = f[1].saturating_add(scale(color.1, a));
        f[2] = f[2].saturating_add(scale(color.0, a));
        f[3] = f[3].saturating_add(a);
    }
}

/// Write `face` into a `LOGFONTW` name field: UTF-16, NUL-terminated,
/// at most 31 units plus the terminator (the field is `[u16; 32]`).
/// Pure — the truncation contract is unit-tested.
fn set_face_name(lf: &mut LOGFONTW, face: &str) {
    lf.lfFaceName = [0; 32];
    for (slot, unit) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *slot = unit;
    }
}

/// The configured face at its weight and point size, for the given DPI.
///
/// The charset and pitch still come from the system message font, so a
/// non-Chinese locale keeps its own mapping when GDI substitutes a face;
/// only the face, weight and size come from `settings.json`.
unsafe fn create_font(dpi: u32, style: &Settings) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = core::mem::zeroed();
        ncm.cbSize = core::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        let ok = SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            &mut ncm as *mut _ as *mut core::ffi::c_void,
            0,
        );
        let mut lf = if ok != 0 {
            ncm.lfMessageFont
        } else {
            core::mem::zeroed()
        };
        // Negative height = character height; points → pixels at this DPI.
        lf.lfHeight = -MulDiv(style.size, dpi as i32, 72);
        lf.lfWeight = style.weight;
        // ClearType rasterisation, folded back to grayscale coverage by
        // accumulate_layer: sharper stems, no coloured fringes on screen.
        lf.lfQuality = CLEARTYPE_QUALITY;
        set_face_name(&mut lf, &style.font);
        CreateFontIndirectW(&lf)
    }
}

/// An offscreen DIB + memory DC + font, owned and cleaned up on drop.
pub struct Surface {
    pub width: i32,
    pub height: i32,
    hdc: HDC,
    hbmp: HBITMAP,
    bits: *mut u8,
    frame: Vec<u8>, // premultiplied accumulation buffer (w*h*4)
    font: HFONT,
    /// Text colour and opacity, from `settings.json`.
    color: (u8, u8, u8),
    alpha: u8,
    old_bmp: HGDIOBJ,
    old_font: HGDIOBJ,
}

impl Surface {
    /// Create a surface of `width` × `height` px at `dpi`, drawing with
    /// the typeface and colour described by `style`; `light_theme` resolves
    /// a `color: auto` against the taskbar's light/dark mode.
    pub fn new(
        width: i32,
        height: i32,
        dpi: u32,
        style: &Settings,
        light_theme: bool,
    ) -> Option<Surface> {
        if width <= 0 || height <= 0 {
            return None;
        }
        unsafe {
            let hdc = CreateCompatibleDC(core::ptr::null_mut());
            if hdc.is_null() {
                return None;
            }
            let header = BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbmp = CreateDIBSection(
                hdc,
                &header,
                DIB_RGB_COLORS,
                &mut bits,
                core::ptr::null_mut(),
                0,
            );
            if hbmp.is_null() || bits.is_null() {
                DeleteDC(hdc);
                return None;
            }
            let old_bmp = SelectObject(hdc, hbmp);
            let font = create_font(dpi, style);
            let old_font = SelectObject(hdc, font);
            Some(Surface {
                width,
                height,
                hdc,
                hbmp,
                bits: bits as *mut u8,
                frame: vec![0; (width as usize) * (height as usize) * 4],
                font,
                color: style.rgb(light_theme),
                alpha: style.alpha(),
                old_bmp,
                old_font,
            })
        }
    }

    /// Measure the rendered width of `text` (px).
    pub fn measure(&self, text: &str) -> i32 {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: self.height,
        };
        unsafe {
            DrawTextW(
                self.hdc,
                wide.as_mut_ptr(),
                wide.len() as i32,
                &mut rc,
                DT_SINGLELINE | DT_CALCRECT | DT_NOPREFIX,
            );
        }
        rc.right.max(0)
    }

    /// Draw text layers `(text, x, alpha)`, producing the final ARGB
    /// pixels in [`bits`](Self::bits_mut). Each layer is rasterized as a
    /// coverage mask into the DIB, then accumulated into the frame scaled
    /// by its alpha — one primitive serves both the marquee's fade-reset
    /// (single layer, alpha envelope) and the quote crossfade (two
    /// layers). The frame is copied back into the DIB for ULW.
    pub fn draw(&mut self, layers: &[(&str, i32, f32)]) {
        self.frame.fill(0);
        for &(text, x, alpha) in layers {
            if text.is_empty() || alpha <= 0.0 {
                continue;
            }
            unsafe {
                let mut rc = RECT {
                    left: 0,
                    top: 0,
                    right: self.width,
                    bottom: self.height,
                };
                FillRect(self.hdc, &mut rc, GetStockObject(BLACK_BRUSH));
                SetBkColor(self.hdc, rgb(0, 0, 0));
                SetTextColor(self.hdc, rgb(255, 255, 255));
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                let mut target = RECT {
                    left: x,
                    top: 0,
                    right: x + self.width,
                    bottom: self.height,
                };
                // DT_NOCLIP is essential: DrawTextW by default clips to the
                // layout rect, and this rect slides with the text origin —
                // the tail would never render past (x + width). With
                // DT_NOCLIP, GDI still clips at the DIB surface, which is
                // exactly the marquee's window edge.
                DrawTextW(
                    self.hdc,
                    wide.as_mut_ptr(),
                    wide.len() as i32,
                    &mut target,
                    DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_NOCLIP,
                );
            }
            let alpha_q = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
            let len = (self.width as usize) * (self.height as usize) * 4;
            // `bits` is a raw pointer field — reading it holds no borrow,
            // so the mask slice can coexist with `&mut self.frame`.
            let mask = unsafe { core::slice::from_raw_parts(self.bits, len) };
            accumulate_layer(mask, &mut self.frame, self.color, self.alpha, alpha_q);
        }
        // Copy the composed frame back into the DIB for ULW. Raw-pointer
        // copy: `bits_mut()` would take `&mut self` and clash with the
        // `&self.frame` borrow.
        unsafe {
            core::ptr::copy_nonoverlapping(self.frame.as_ptr(), self.bits, self.frame.len());
        }
    }

    /// The DIB pixel buffer, `w*h*4` bytes, BGRA top-down. Test-only:
    /// production code writes through the raw `bits` pointer (borrow-free).
    #[cfg(test)]
    pub fn bits_mut(&mut self) -> &mut [u8] {
        let len = (self.width as usize) * (self.height as usize) * 4;
        unsafe { core::slice::from_raw_parts_mut(self.bits, len) }
    }

    pub fn hdc(&self) -> HDC {
        self.hdc
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old_font);
            SelectObject(self.hdc, self.old_bmp);
            DeleteObject(self.font);
            DeleteObject(self.hbmp);
            DeleteDC(self.hdc);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure half)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    /// The shipped defaults, spelled out so the blend arithmetic reads at
    /// each call site.
    const COLOR: (u8, u8, u8) = (30, 30, 30);
    const MAX_ALPHA: u8 = 217;

    fn px(b: &[u8]) -> [u8; 4] {
        [b[0], b[1], b[2], b[3]]
    }

    #[test]
    fn background_is_fully_transparent() {
        let mask = [0u8, 0, 0, 0xFF, 0, 0, 0, 0xFF];
        let mut frame = [0u8; 8];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        assert_eq!(frame, [0u8; 8]);
    }

    #[test]
    fn solid_pixel_gets_full_color_and_capped_alpha() {
        // White solid: coverage 255. Premultiplied: rgb = color * alpha,
        // so 30 * 217/255 = 26 — the *displayed* color stays 30/255 at
        // 85% opacity, which is exactly what premultiplication means.
        let mask = [255u8, 255, 255, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        let expected = [
            scale(COLOR.2, MAX_ALPHA),
            scale(COLOR.1, MAX_ALPHA),
            scale(COLOR.0, MAX_ALPHA),
            MAX_ALPHA,
        ];
        assert_eq!(px(&frame), expected);
        assert_eq!(expected[0], 26); // 30 * 217 / 255, rounded
    }

    #[test]
    fn half_coverage_blends_by_coverage() {
        let mask = [128u8, 128, 128, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        let a = scale(217, 128);
        assert_eq!(px(&frame), [scale(30, a), scale(30, a), scale(30, a), a]);
    }

    #[test]
    fn full_opacity_max_alpha_is_255() {
        let mask = [255u8, 255, 255, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, (10, 20, 30), 255, 255);
        assert_eq!(px(&frame), [30, 20, 10, 255]);
    }

    #[test]
    fn layer_alpha_scales_opacity() {
        let mask = [255u8, 255, 255, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 128);
        let a = scale(MAX_ALPHA, 128);
        assert_eq!(px(&frame), [scale(30, a), scale(30, a), scale(30, a), a]);
    }

    #[test]
    fn zero_alpha_layer_is_invisible() {
        let mask = [255u8, 255, 255, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 0);
        assert_eq!(frame, [0u8; 4]);
    }

    #[test]
    fn two_layers_accumulate_with_saturation() {
        let mask = [255u8, 255, 255, 0];
        let mut frame = [0u8; 4];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        let once = px(&frame);
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        let twice = px(&frame);
        assert!(
            twice[3] > once[3],
            "a second layer adds alpha ({} -> {})",
            once[3],
            twice[3]
        );
    }

    #[test]
    fn trailing_partial_chunk_is_untouched() {
        let mask = [255u8, 255, 255, 0, 9, 9];
        let mut frame = [0u8, 0, 0, 0, 7, 7];
        accumulate_layer(&mask, &mut frame, COLOR, MAX_ALPHA, 255);
        assert_eq!(&frame[4..], &[7, 7]);
    }

    #[test]
    fn face_name_is_utf16_and_nul_terminated() {
        let mut lf: LOGFONTW = unsafe { core::mem::zeroed() };
        set_face_name(&mut lf, settings::DEFAULT_FACE);
        let units: Vec<u16> = lf
            .lfFaceName
            .iter()
            .copied()
            .take_while(|&u| u != 0)
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), settings::DEFAULT_FACE);
        assert_eq!(
            lf.lfFaceName[settings::DEFAULT_FACE.len()],
            0,
            "terminated right after the face"
        );
    }

    #[test]
    fn face_name_truncates_to_the_field() {
        let mut lf: LOGFONTW = unsafe { core::mem::zeroed() };
        set_face_name(&mut lf, &"X".repeat(40));
        assert_eq!(lf.lfFaceName[..31], [u16::from(b'X'); 31]);
        assert_eq!(lf.lfFaceName[31], 0, "the 32nd unit is reserved for NUL");
    }

    /// The mask is white-on-black rasterisation, but ClearType puts
    /// different coverage in R, G and B; the blend must read luminance, not
    /// one channel. A pure-red pixel carries 0.299 of full coverage.
    #[test]
    fn coverage_is_luminance_not_a_single_channel() {
        let mut frame = [0u8; 4];
        accumulate_layer(&[0, 0, 255, 0], &mut frame, (0, 0, 0), 255, 255);
        let a = frame[3];
        assert_eq!(
            a, 76,
            "red-only coverage must weigh 0.299 of full (got {a})"
        );
        let mut frame = [0u8; 4];
        accumulate_layer(&[255, 255, 255, 0], &mut frame, (0, 0, 0), 255, 255);
        assert_eq!(frame[3], 255, "white is full coverage");
    }

    #[test]
    fn scale_rounds_correctly() {
        assert_eq!(scale(255, 255), 255);
        assert_eq!(scale(0, 255), 0);
        assert_eq!(scale(255, 0), 0);
        // 30*217/255 = 25.53 → 26
        assert_eq!(scale(30, 217), 26);
    }

    /// Regression: DrawTextW without DT_NOCLIP clips to the layout rect,
    /// which slides with the text origin during the marquee — the tail of
    /// a long quote never rendered (user-visible as ~29 CJK chars max).
    /// A long text drawn at a negative origin must still put ink at the
    /// strip's right edge, where the tail arrives during the slide.
    #[test]
    fn draw_renders_tail_when_origin_is_negative() {
        let mut s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        // ~120 ASCII chars ≈ 800 px wide at 13 pt — far wider than 500.
        let text = "The quick brown fox jumps over the lazy dog and keeps \
                    running far beyond the edge of the strip ABCDEFGHIJKL \
                    0123456789 abcdefghijklmnopqrstuvwxyz";
        assert!(s.measure(text) > 700, "test text must overflow the strip");
        s.draw(&[(text, -300, 1.0)]);
        // Any ink (alpha > 0) in the rightmost 100 px column band.
        let (w, h) = (s.width as usize, s.height as usize);
        let buf = s.bits_mut();
        let mut ink = 0;
        for y in 0..h {
            for xx in (w - 100)..w {
                if buf[(y * w + xx) * 4 + 3] > 0 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 0, "tail must render at the right edge (DT_NOCLIP)");
    }

    /// The user-reported quote must overflow the 470 px resting width so
    /// the marquee engages at all. Beyond that behavioral check, the
    /// full-width property is asserted *relatively* to an equal-count
    /// ASCII baseline (CJK ≈ 2× half-width; the ratio must exceed 3/2),
    /// so a collapsed or half-width CJK measurement fails without the
    /// test hard-coding any one font's pixel metrics.
    #[test]
    fn measure_cjk_long_quote_overflows_strip() {
        let s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        let q = "翻山越岭只为一场遇见，全力以赴只为一次改变。答案就在眼泪里，\
                 也凝结在汗水中、感官和心灵。";
        let ascii = "abcdefghijklmnopqrstuvwxyz0123456789abcdefgh";
        assert_eq!(q.chars().count(), ascii.chars().count(), "equal baselines");
        let (cjk, base) = (s.measure(q), s.measure(ascii));
        eprintln!("CJK {cjk} px vs ASCII {base} px (44 chars each)");
        assert!(
            cjk * 2 > base * 3,
            "CJK must measure well beyond the ASCII baseline ({cjk} vs {base})"
        );
        assert!(
            cjk > 470,
            "44-char CJK quote must overflow 470 px (got {cjk})"
        );
    }

    /// The symmetric half: with the origin at rest the head is visible.
    #[test]
    fn draw_renders_head_at_rest() {
        let mut s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        s.draw(&[("金句", 15, 1.0)]);
        let (w, h) = (s.width as usize, s.height as usize);
        let buf = s.bits_mut();
        let mut ink = 0;
        for y in 0..h {
            for xx in 0..100 {
                if buf[(y * w + xx) * 4 + 3] > 0 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 0, "head must render at the left edge");
    }

    /// Two layers at different positions and opacities must both leave ink.
    #[test]
    fn draw_blends_two_layers() {
        let mut s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        s.draw(&[("金句", 15, 0.5), ("金句", 200, 0.5)]);
        let (w, h) = (s.width as usize, s.height as usize);
        let buf = s.bits_mut();
        let mut left_ink = 0;
        let mut right_ink = 0;
        for y in 0..h {
            for xx in 0..100 {
                if buf[(y * w + xx) * 4 + 3] > 0 {
                    left_ink += 1;
                }
            }
            for xx in 200..300 {
                if buf[(y * w + xx) * 4 + 3] > 0 {
                    right_ink += 1;
                }
            }
        }
        assert!(left_ink > 0, "first layer must leave ink at x=15");
        assert!(right_ink > 0, "second layer must leave ink at x=200");
    }

    /// A layer with zero alpha is a complete no-op.
    #[test]
    fn draw_zero_alpha_leaves_no_ink() {
        let mut s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        s.draw(&[("金句", 15, 0.0)]);
        let buf = s.bits_mut();
        assert!(
            buf.iter().all(|&b| b == 0),
            "zero-alpha layer must leave the frame untouched"
        );
    }

    /// Layer opacity modulates the accumulated ink end to end: with the
    /// opaque default, a half-alpha layer leaves roughly half the ink.
    #[test]
    fn draw_half_alpha_scales_ink() {
        let mut s = Surface::new(500, 36, 96, &Settings::default(), true).expect("GDI surface");
        s.draw(&[("金句", 15, 0.5)]);
        let buf = s.bits_mut();
        let max_a = buf.iter().skip(3).step_by(4).copied().max().unwrap_or(0);
        assert!(
            (118..=136).contains(&max_a),
            "half-opacity layer must halve the ink alpha (got {max_a}; full opacity is 255)"
        );
    }
}
