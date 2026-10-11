use std::ffi::CString;
use std::path::{Path, PathBuf};

use ab_glyph::{Font as _, FontRef, PxScale, ScaleFont};
use fontconfig::{Fontconfig, Pattern, FC_FAMILY};

const BASE_SIZE: f32 = 10.0;
pub const TEXT_NORMAL: u32 = 0xFFC5C8C6;

fn scale_for(pt: f32, dpi: f32) -> f32 {
    pt * dpi / 72.0 / BASE_SIZE
}

pub struct Font {
    font: FontRef<'static>,
    pt: f32,
    pub scale: f32,
}

impl Font {
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        let tokens: Vec<&str> = spec.split_whitespace().collect();
        let (family, pt) = match tokens.last().and_then(|t| t.parse::<u32>().ok()) {
            Some(size) => {
                if !(1..=100).contains(&size) {
                    return Err(format!("font size out of range (1..=100): {size}"));
                }
                (tokens[..tokens.len() - 1].join(" "), size as f32)
            }
            None => (tokens.join(" "), BASE_SIZE),
        };

        let path = resolve_font(&family)?;
        let font = load_font_file(&path)?;
        Ok(Self {
            font,
            pt,
            scale: scale_for(pt, 96.0),
        })
    }

    pub fn set_dpi(&mut self, dpi: f32) {
        self.scale = scale_for(self.pt, dpi);
    }

    pub fn ascent(&self, size: f32) -> f32 {
        let size = size * self.scale;
        self.font.as_scaled(PxScale { x: size, y: size }).ascent()
    }

    pub fn line_height(&self, size: f32) -> f32 {
        let size = size * self.scale;
        let scaled = self.font.as_scaled(PxScale { x: size, y: size });
        scaled.ascent() - scaled.descent()
    }

    pub fn measure(&self, text: &str, size: f32) -> f32 {
        let size = size * self.scale;
        let scaled = self.font.as_scaled(PxScale { x: size, y: size });
        text.chars()
            .map(|c| scaled.h_advance(self.font.glyph_id(c)))
            .sum()
    }

    pub fn truncate(&self, text: &str, size: f32, max_width: f32) -> String {
        if self.measure(text, size) <= max_width {
            return text.to_string();
        }
        let ellipsis = '…';
        let ellipsis_width = self.measure(&ellipsis.to_string(), size);
        let mut out = String::new();
        for ch in text.chars() {
            if self.measure(&format!("{out}{ch}"), size) + ellipsis_width > max_width {
                break;
            }
            out.push(ch);
        }
        format!("{out}{ellipsis}")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        dst: &mut [u8],
        width: u32,
        height: u32,
        x: i32,
        baseline_y: i32,
        text: &str,
        size: f32,
        color: u32,
    ) {
        let size = size * self.scale;
        let scaled = self.font.as_scaled(PxScale { x: size, y: size });
        let mut cx = x as f32;
        for ch in text.chars() {
            let glyph = scaled.scaled_glyph(ch);
            let advance = scaled.h_advance(glyph.id);
            if let Some(outlined) = self.font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let ox = cx + bounds.min.x;
                let oy = baseline_y as f32 + bounds.min.y;
                outlined.draw(|px, py, coverage| {
                    if coverage <= 0.0 {
                        return;
                    }
                    let px_i = (ox + px as f32) as i32;
                    let py_i = (oy + py as f32) as i32;
                    if px_i < 0 || px_i >= width as i32 || py_i < 0 || py_i >= height as i32 {
                        return;
                    }
                    blend_pixel(dst, width, px_i, py_i, color, coverage);
                });
            }
            cx += advance;
        }
    }
}

fn resolve_font(name: &str) -> Result<PathBuf, String> {
    let fc = Fontconfig::new()
        .ok_or_else(|| "fontconfig is not available on this system".to_string())?;
    let family = if name.is_empty() { "monospace" } else { name };
    let family = CString::new(family).map_err(|_| format!("invalid font name {name:?}"))?;
    let mut pattern = Pattern::new(&fc)
        .map_err(|e| format!("failed to create font pattern: {e}"))?;
    pattern
        .add_string(FC_FAMILY, &family)
        .map_err(|e| format!("failed to set font family: {e}"))?;
    let matched = pattern
        .font_match()
        .map_err(|e| format!("failed to resolve font {name:?}: {e}"))?;
    let path = PathBuf::from(
        matched
            .filename()
            .map_err(|e| format!("no font file for {name:?}: {e}"))?,
    );
    Ok(path)
}

fn load_font_file(path: &Path) -> Result<FontRef<'static>, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("failed to read font {}: {e}", path.display()))?;
    let static_bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    FontRef::try_from_slice(static_bytes)
        .map_err(|e| format!("invalid font file {}: {e}", path.display()))
}

fn blend_pixel(dst: &mut [u8], width: u32, x: i32, y: i32, color: u32, coverage: f32) {
    let i = ((y as usize) * (width as usize) + x as usize) * 4;
    let ca = ((color >> 24) & 0xFF) as f32 / 255.0;
    let sa = coverage.min(1.0) * ca;
    if sa <= 0.0 {
        return;
    }
    let sr = ((color >> 16) & 0xFF) as f32;
    let sg = ((color >> 8) & 0xFF) as f32;
    let sb = (color & 0xFF) as f32;
    let db = dst[i] as f32;
    let dg = dst[i + 1] as f32;
    let dr = dst[i + 2] as f32;
    let da = dst[i + 3] as f32;
    let inv = 1.0 - sa;
    dst[i] = (sb * ca * sa + db * inv) as u8;
    dst[i + 1] = (sg * ca * sa + dg * inv) as u8;
    dst[i + 2] = (sr * ca * sa + dr * inv) as u8;
    dst[i + 3] = (sa * 255.0 + da * inv).min(255.0) as u8;
}

#[allow(clippy::too_many_arguments)]
pub fn fill_rect(dst: &mut [u8], dst_w: u32, dst_h: u32, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let bytes = [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        ((color >> 24) & 0xFF) as u8,
    ];
    let x2 = (x + w).min(dst_w);
    let y2 = (y + h).min(dst_h);
    if x >= x2 || y >= y2 {
        return;
    }
    for py in y..y2 {
        let start = ((py as usize) * (dst_w as usize) + x as usize) * 4;
        let end = start + ((x2 - x) as usize) * 4;
        for chunk in dst[start..end].as_chunks_mut::<4>().0 {
            *chunk = bytes;
        }
    }
}



