//! Rasterization powered by DirectWrite.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use dwrote::{
    FontCollection, FontFace, FontFallback, FontStretch, FontStyle, FontWeight, GlyphOffset,
    GlyphRunAnalysis, TextAnalysisSource, TextAnalysisSourceMethods, DWRITE_GLYPH_RUN,
};

use winapi::shared::ntdef::{HRESULT, LOCALE_NAME_MAX_LENGTH};
use winapi::shared::winerror::SUCCEEDED;
use winapi::um::dwrite;
use winapi::um::dwrite_2::{
    IDWriteColorGlyphRunEnumerator, IDWriteFactory2, DWRITE_COLOR_GLYPH_RUN,
};
use winapi::um::unknwnbase::IUnknown;
use winapi::um::winnls::GetUserDefaultLocaleName;
use winapi::Interface;

use super::{
    BitmapBuffer, Error, FontDesc, FontKey, GlyphKey, Metrics, RasterizedGlyph, Size, Slant, Style,
    Weight,
};

/// DirectWrite uses 0 for missing glyph symbols.
/// https://docs.microsoft.com/en-us/typography/opentype/spec/recom#glyph-0-the-notdef-glyph
const MISSING_GLYPH_INDEX: u16 = 0;

/// `TranslateColorGlyphRun` returns this when the glyph run has no color layers.
const DWRITE_E_NOCOLOR: HRESULT = 0x8898_500Cu32 as HRESULT;

/// Palette index used by DirectWrite to mean "use the current text (foreground) color".
const FOREGROUND_PALETTE_INDEX: u16 = 0xFFFF;

/// Owns an `IDWriteFactory2`, used to decompose color (emoji) glyph runs into their layers.
///
/// DirectWrite COM objects are only touched from the single rasterization thread, matching how
/// `dwrote`'s own handles are used, so it is sound to move this between threads.
struct ColorFactory(*mut IDWriteFactory2);

unsafe impl Send for ColorFactory {}
unsafe impl Sync for ColorFactory {}

impl Drop for ColorFactory {
    fn drop(&mut self) {
        unsafe {
            (*self.0).Release();
        }
    }
}

impl ColorFactory {
    fn new() -> Option<ColorFactory> {
        unsafe {
            let mut factory: *mut IDWriteFactory2 = ptr::null_mut();
            let hr = dwrite::DWriteCreateFactory(
                dwrite::DWRITE_FACTORY_TYPE_SHARED,
                &IDWriteFactory2::uuidof(),
                &mut factory as *mut *mut IDWriteFactory2 as *mut *mut IUnknown,
            );

            if SUCCEEDED(hr) && !factory.is_null() {
                Some(ColorFactory(factory))
            } else {
                None
            }
        }
    }
}

/// Cached DirectWrite font.
struct Font {
    face: FontFace,
    family_name: String,
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
}

pub struct DirectWriteRasterizer {
    fonts: HashMap<FontKey, Font>,
    keys: HashMap<FontDesc, FontKey>,
    available_fonts: FontCollection,
    fallback_sequence: Option<FontFallback>,
    color_factory: Option<ColorFactory>,
}

impl DirectWriteRasterizer {
    fn rasterize_glyph(
        &self,
        face: &FontFace,
        size: Size,
        character: char,
        glyph_index: u16,
    ) -> Result<RasterizedGlyph, Error> {
        let em_size = size.as_px();

        let glyph_run = DWRITE_GLYPH_RUN {
            fontFace: unsafe { face.as_ptr() },
            fontEmSize: em_size,
            glyphCount: 1,
            glyphIndices: &glyph_index,
            glyphAdvances: &0.0,
            glyphOffsets: &GlyphOffset::default(),
            isSideways: 0,
            bidiLevel: 0,
        };

        let rendering_mode = face.get_recommended_rendering_mode_default_params(
            em_size,
            1.,
            dwrote::DWRITE_MEASURING_MODE_NATURAL,
        );

        // Colored (emoji) glyphs are decomposed into per-color layers and composited into an
        // RGBA bitmap. Non-color glyphs fall through to the greyscale ClearType path below.
        if let Some(color_factory) = &self.color_factory {
            if let Some(glyph) = unsafe {
                rasterize_color_glyph(color_factory.0, &glyph_run, rendering_mode, character)
            } {
                return Ok(glyph);
            }
        }

        let glyph_analysis = GlyphRunAnalysis::create(
            &glyph_run,
            1.,
            None,
            rendering_mode,
            dwrote::DWRITE_MEASURING_MODE_NATURAL,
            0.0,
            0.0,
        )?;

        let bounds =
            glyph_analysis.get_alpha_texture_bounds(dwrote::DWRITE_TEXTURE_CLEARTYPE_3x1)?;

        let buffer = BitmapBuffer::Rgb(
            glyph_analysis.create_alpha_texture(dwrote::DWRITE_TEXTURE_CLEARTYPE_3x1, bounds)?,
        );

        Ok(RasterizedGlyph {
            character,
            width: bounds.right - bounds.left,
            height: bounds.bottom - bounds.top,
            top: -bounds.top,
            left: bounds.left,
            advance: (0, 0),
            buffer,
        })
    }

    fn get_loaded_font(&self, font_key: FontKey) -> Result<&Font, Error> {
        self.fonts.get(&font_key).ok_or(Error::UnknownFontKey)
    }

    fn get_glyph_index(&self, face: &FontFace, character: char) -> u16 {
        face.get_glyph_indices(&[character as u32]).first().copied().unwrap_or(MISSING_GLYPH_INDEX)
    }

    fn get_fallback_font(&self, loaded_font: &Font, character: char) -> Option<dwrote::Font> {
        let fallback = self.fallback_sequence.as_ref()?;

        let mut buffer = [0u16; 2];
        character.encode_utf16(&mut buffer);

        let length = character.len_utf16() as u32;
        let utf16_codepoints = &buffer[..length as usize];

        let locale = get_current_locale();

        let text_analysis_source_data = TextAnalysisSourceData { locale: &locale, length };
        let text_analysis_source = TextAnalysisSource::from_text(
            Box::new(text_analysis_source_data),
            Cow::Borrowed(utf16_codepoints),
        );

        let fallback_result = fallback.map_characters(
            &text_analysis_source,
            0,
            length,
            &self.available_fonts,
            Some(&loaded_font.family_name),
            loaded_font.weight,
            loaded_font.style,
            loaded_font.stretch,
        );

        fallback_result.mapped_font
    }
}

impl crate::Rasterize for DirectWriteRasterizer {
    fn new() -> Result<DirectWriteRasterizer, Error> {
        Ok(DirectWriteRasterizer {
            fonts: HashMap::new(),
            keys: HashMap::new(),
            available_fonts: FontCollection::system(),
            fallback_sequence: FontFallback::get_system_fallback(),
            color_factory: ColorFactory::new(),
        })
    }

    fn metrics(&self, key: FontKey, size: Size) -> Result<Metrics, Error> {
        let face = &self.get_loaded_font(key)?.face;
        let vmetrics = face.metrics().metrics0();

        let scale = size.as_px() / f32::from(vmetrics.designUnitsPerEm);

        let underline_position = f32::from(vmetrics.underlinePosition) * scale;
        let underline_thickness = f32::from(vmetrics.underlineThickness) * scale;

        let strikeout_position = f32::from(vmetrics.strikethroughPosition) * scale;
        let strikeout_thickness = f32::from(vmetrics.strikethroughThickness) * scale;

        let ascent = f32::from(vmetrics.ascent) * scale;
        let descent = -f32::from(vmetrics.descent) * scale;
        let line_gap = f32::from(vmetrics.lineGap) * scale;

        let line_height = f64::from(ascent - descent + line_gap);

        // Since all monospace characters have the same width, we use `!` for horizontal metrics.
        let character = '!';
        let glyph_index = self.get_glyph_index(face, character);

        let glyph_metrics = face.get_design_glyph_metrics(&[glyph_index], false);
        let hmetrics = glyph_metrics.first().ok_or(Error::MetricsNotFound)?;

        let average_advance = f64::from(hmetrics.advanceWidth) * f64::from(scale);

        Ok(Metrics {
            descent,
            average_advance,
            line_height,
            underline_position,
            underline_thickness,
            strikeout_position,
            strikeout_thickness,
        })
    }

    fn load_font(&mut self, desc: &FontDesc, _size: Size) -> Result<FontKey, Error> {
        // Fast path if face is already loaded.
        if let Some(key) = self.keys.get(desc) {
            return Ok(*key);
        }

        let family = self
            .available_fonts
            .get_font_family_by_name(&desc.name)
            .ok_or_else(|| Error::FontNotFound(desc.clone()))?;

        let font = match desc.style {
            Style::Description { weight, slant } => {
                // This searches for the "best" font - should mean we don't have to worry about
                // fallbacks if our exact desired weight/style isn't available.
                Ok(family.get_first_matching_font(weight.into(), FontStretch::Normal, slant.into()))
            },
            Style::Specific(ref style) => {
                let mut idx = 0;
                let count = family.get_font_count();

                loop {
                    if idx == count {
                        break Err(Error::FontNotFound(desc.clone()));
                    }

                    let font = family.get_font(idx);

                    if font.face_name() == *style {
                        break Ok(font);
                    }

                    idx += 1;
                }
            },
        }?;

        let key = FontKey::next();
        self.keys.insert(desc.clone(), key);
        self.fonts.insert(key, font.into());

        Ok(key)
    }

    fn get_glyph(&mut self, glyph: GlyphKey) -> Result<RasterizedGlyph, Error> {
        let loaded_font = self.get_loaded_font(glyph.font_key)?;

        let loaded_fallback_font;
        let mut font = loaded_font;
        let mut glyph_index = self.get_glyph_index(&loaded_font.face, glyph.character);
        if glyph_index == MISSING_GLYPH_INDEX {
            if let Some(fallback_font) = self.get_fallback_font(loaded_font, glyph.character) {
                loaded_fallback_font = Font::from(fallback_font);
                glyph_index = self.get_glyph_index(&loaded_fallback_font.face, glyph.character);
                font = &loaded_fallback_font;
            }
        }

        let rasterized_glyph =
            self.rasterize_glyph(&font.face, glyph.size, glyph.character, glyph_index)?;

        if glyph_index == MISSING_GLYPH_INDEX {
            Err(Error::MissingGlyph(rasterized_glyph))
        } else {
            Ok(rasterized_glyph)
        }
    }

    fn kerning(&mut self, _left: GlyphKey, _right: GlyphKey) -> (f32, f32) {
        (0., 0.)
    }
}

/// Rasterize a color (emoji) glyph by decomposing it into DirectWrite color layers.
///
/// Returns `None` when the glyph has no color layers (`DWRITE_E_NOCOLOR`) or on any error, so the
/// caller can fall back to the greyscale rendering path. On success it returns an RGBA bitmap with
/// premultiplied alpha, matching the FreeType/CoreText color output.
unsafe fn rasterize_color_glyph(
    factory: *mut IDWriteFactory2,
    glyph_run: &DWRITE_GLYPH_RUN,
    rendering_mode: dwrite::DWRITE_RENDERING_MODE,
    character: char,
) -> Option<RasterizedGlyph> {
    let mut enumerator: *mut IDWriteColorGlyphRunEnumerator = ptr::null_mut();
    let hr = (*factory).TranslateColorGlyphRun(
        0.0,
        0.0,
        glyph_run as *const DWRITE_GLYPH_RUN,
        ptr::null(),
        dwrote::DWRITE_MEASURING_MODE_NATURAL,
        ptr::null(),
        0,
        &mut enumerator,
    );

    // Not a color glyph (or failed): let the caller render it in greyscale.
    if hr == DWRITE_E_NOCOLOR || !SUCCEEDED(hr) || enumerator.is_null() {
        return None;
    }

    struct Layer {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        // ClearType 3x1 coverage, 3 bytes per pixel.
        coverage: Vec<u8>,
        color: [f32; 4],
    }

    let mut layers: Vec<Layer> = Vec::new();

    loop {
        let mut has_run: i32 = 0;
        if !SUCCEEDED((*enumerator).MoveNext(&mut has_run)) || has_run == 0 {
            break;
        }

        let mut color_run: *const DWRITE_COLOR_GLYPH_RUN = ptr::null();
        if !SUCCEEDED((*enumerator).GetCurrentRun(&mut color_run)) || color_run.is_null() {
            break;
        }
        let run = &*color_run;

        let analysis = match GlyphRunAnalysis::create(
            &run.glyphRun,
            1.,
            None,
            rendering_mode,
            dwrote::DWRITE_MEASURING_MODE_NATURAL,
            0.0,
            0.0,
        ) {
            Ok(analysis) => analysis,
            Err(_) => continue,
        };

        let bounds = match analysis.get_alpha_texture_bounds(dwrote::DWRITE_TEXTURE_CLEARTYPE_3x1) {
            Ok(bounds) => bounds,
            Err(_) => continue,
        };
        if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
            continue;
        }

        let coverage =
            match analysis.create_alpha_texture(dwrote::DWRITE_TEXTURE_CLEARTYPE_3x1, bounds) {
                Ok(coverage) => coverage,
                Err(_) => continue,
            };

        // A palette index of 0xFFFF means "use the foreground color". Crossfont doesn't know the
        // cell's text color here, so render such layers white (visible, uncommon for emoji).
        let color = if run.paletteIndex == FOREGROUND_PALETTE_INDEX {
            [1.0, 1.0, 1.0, 1.0]
        } else {
            [run.runColor.r, run.runColor.g, run.runColor.b, run.runColor.a]
        };

        layers.push(Layer {
            left: bounds.left,
            top: bounds.top,
            right: bounds.right,
            bottom: bounds.bottom,
            coverage,
            color,
        });
    }

    (*enumerator).Release();

    if layers.is_empty() {
        return None;
    }

    // Union of all layer bounds is the final glyph bitmap.
    let left = layers.iter().map(|l| l.left).min().unwrap();
    let top = layers.iter().map(|l| l.top).min().unwrap();
    let right = layers.iter().map(|l| l.right).max().unwrap();
    let bottom = layers.iter().map(|l| l.bottom).max().unwrap();
    let width = (right - left) as usize;
    let height = (bottom - top) as usize;
    if width == 0 || height == 0 {
        return None;
    }

    // Premultiplied RGBA, transparent to start with.
    let mut buffer = vec![0u8; width * height * 4];

    // Layers come back in back-to-front paint order; composite source-over.
    for layer in &layers {
        let layer_width = (layer.right - layer.left) as usize;
        let layer_height = (layer.bottom - layer.top) as usize;
        let offset_x = (layer.left - left) as usize;
        let offset_y = (layer.top - top) as usize;
        let [cr, cg, cb, ca] = layer.color;

        for y in 0..layer_height {
            for x in 0..layer_width {
                let cov_base = (y * layer_width + x) * 3;
                let cov = (u32::from(layer.coverage[cov_base])
                    + u32::from(layer.coverage[cov_base + 1])
                    + u32::from(layer.coverage[cov_base + 2])) as f32
                    / (3.0 * 255.0);
                if cov <= 0.0 {
                    continue;
                }

                let src_a = ca * cov;
                let src_r = cr * src_a;
                let src_g = cg * src_a;
                let src_b = cb * src_a;

                let idx = ((offset_y + y) * width + (offset_x + x)) * 4;
                let inv = 1.0 - src_a;
                let dst_r = f32::from(buffer[idx]) / 255.0;
                let dst_g = f32::from(buffer[idx + 1]) / 255.0;
                let dst_b = f32::from(buffer[idx + 2]) / 255.0;
                let dst_a = f32::from(buffer[idx + 3]) / 255.0;

                buffer[idx] = ((src_r + dst_r * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
                buffer[idx + 1] = ((src_g + dst_g * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
                buffer[idx + 2] = ((src_b + dst_b * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
                buffer[idx + 3] = ((src_a + dst_a * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    Some(RasterizedGlyph {
        character,
        width: right - left,
        height: bottom - top,
        top: -top,
        left,
        advance: (0, 0),
        buffer: BitmapBuffer::Rgba(buffer),
    })
}

impl From<dwrote::Font> for Font {
    fn from(font: dwrote::Font) -> Font {
        Font {
            face: font.create_font_face(),
            family_name: font.family_name(),
            weight: font.weight(),
            style: font.style(),
            stretch: font.stretch(),
        }
    }
}

impl From<Weight> for FontWeight {
    fn from(weight: Weight) -> FontWeight {
        match weight {
            Weight::Bold => FontWeight::Bold,
            Weight::Normal => FontWeight::Regular,
        }
    }
}

impl From<Slant> for FontStyle {
    fn from(slant: Slant) -> FontStyle {
        match slant {
            Slant::Oblique => FontStyle::Oblique,
            Slant::Italic => FontStyle::Italic,
            Slant::Normal => FontStyle::Normal,
        }
    }
}

fn get_current_locale() -> String {
    let mut buffer = vec![0u16; LOCALE_NAME_MAX_LENGTH];
    let len =
        unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) as usize };

    // `len` includes null byte, which we don't need in Rust.
    OsString::from_wide(&buffer[..len - 1]).into_string().expect("Locale not valid unicode")
}

/// Font fallback information for dwrote's TextAnalysisSource.
struct TextAnalysisSourceData<'a> {
    locale: &'a str,
    length: u32,
}

impl TextAnalysisSourceMethods for TextAnalysisSourceData<'_> {
    fn get_locale_name(&self, _text_position: u32) -> (Cow<str>, u32) {
        (Cow::Borrowed(self.locale), self.length)
    }

    fn get_paragraph_reading_direction(&self) -> dwrite::DWRITE_READING_DIRECTION {
        dwrite::DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
    }
}

impl From<HRESULT> for Error {
    fn from(hresult: HRESULT) -> Self {
        let message = format!("a DirectWrite rendering error occurred: {:X}", hresult);
        Error::PlatformError(message)
    }
}
