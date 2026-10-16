//! Text drawn into pictures, for text sources: shaped and rasterised with
//! cosmic-text, with an optional background, shadow and outline under it.

use parking_lot::Mutex;
use std::f32::consts::TAU;

use cosmic_text::{
    Align, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent,
    Weight, Wrap,
};
use isocast_protocol::{Color, TextAlign, TextSource};

use crate::picture::Picture;

/// Fonts are found once (it takes a moment) and kept.
static FONTS: Mutex<Option<(FontSystem, SwashCache)>> = Mutex::new(None);

/// The size [`render`] draws `text` at, as `settings` say: their fixed
/// size, or the text's own (wrapped at `wrap_width` if they give one).
pub(crate) fn size(settings: &TextSource, text: &str) -> (u32, u32) {
    let mut fonts = FONTS.lock();
    let (font_system, _) = fonts.get_or_insert_with(fonts_found);
    let shaped = shape(font_system, settings, text);
    (shaped.width, shaped.height)
}

/// The narrowest picture `text` fits in as `settings` say (wrapped at
/// every space): its widest word and the room around it.
pub(crate) fn least_width(settings: &TextSource, text: &str) -> u32 {
    let mut fonts = FONTS.lock();
    let (font_system, _) = fonts.get_or_insert_with(fonts_found);
    let mut buffer = buffer(font_system, settings, text, Some((1.0, Wrap::Word)));
    buffer.shape_until_scroll(font_system, false);
    let widest = buffer
        .layout_runs()
        .fold(0.0f32, |widest, run| widest.max(run.line_w));
    (widest.ceil() + surround(settings).0).max(1.0) as u32
}

/// The room `settings` keep around their text, across and down: padding
/// and outline on every side, and the shadow's offset on one.
pub(crate) fn surround(settings: &TextSource) -> (f32, f32) {
    let outline = settings.outline.as_ref().map_or(0.0, |o| o.width.max(0.0));
    let (shadow_x, shadow_y) = settings.shadow.as_ref().map_or((0.0, 0.0), |s| (s.x, s.y));
    let margin = settings.padding as f32 + outline.ceil();
    (2.0 * margin + shadow_x.abs(), 2.0 * margin + shadow_y.abs())
}

fn fonts_found() -> (FontSystem, SwashCache) {
    let mut font_system = FontSystem::new();
    // cosmic-text's default sans serif isn't on Macs.
    font_system.db_mut().set_sans_serif_family("Helvetica Neue");
    (font_system, SwashCache::new())
}

/// Text shaped for drawing, and the picture's size.
struct Shaped {
    buffer: Buffer,
    text_width: f32,
    margin: f32,
    shadow_x: f32,
    fixed: Option<(u32, u32)>,
    width: u32,
    height: u32,
}

/// `text` drawn as `settings` say, at their fixed size or its own.
pub(crate) fn render(settings: &TextSource, text: &str) -> Picture {
    let mut fonts = FONTS.lock();
    let (font_system, cache) = fonts.get_or_insert_with(fonts_found);
    let Shaped {
        mut buffer,
        text_width,
        margin,
        shadow_x,
        fixed,
        width,
        height,
    } = shape(font_system, settings, text);
    // Align lines against the widest one, or across a fixed size's width
    // inside its padding (text wider than that keeps overflowing right).
    if settings.wrap_width.is_none() && settings.align != TextAlign::Left {
        let inner = fixed.map_or(0.0, |_| width as f32 - 2.0 * margin - shadow_x.abs());
        buffer.set_size(Some(text_width.ceil().max(inner)), None);
        buffer.shape_until_scroll(font_system, false);
    }
    let shadow_y = settings.shadow.as_ref().map_or(0.0, |s| s.y);
    let mut canvas = Canvas::new(width, height);
    if let Some(color) = settings.background.as_ref().and_then(Color::rgba) {
        canvas.fill(color);
    }
    draw_text(
        &mut canvas,
        &buffer,
        font_system,
        cache,
        settings,
        (margin + (-shadow_x).max(0.0), margin + (-shadow_y).max(0.0)),
    );
    canvas.into_picture()
}

/// A buffer with `text` set as `settings` say, wrapped at their
/// `wrap_width` or as `wrap` says, not yet shaped.
fn buffer(
    font_system: &mut FontSystem,
    settings: &TextSource,
    text: &str,
    wrap: Option<(f32, Wrap)>,
) -> Buffer {
    let metrics = Metrics::new(settings.size, settings.size * settings.line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    let attrs = Attrs::new()
        .family(Family::Name(&settings.font))
        .weight(Weight(settings.weight))
        .style(if settings.italic {
            Style::Italic
        } else {
            Style::Normal
        });
    let align = match settings.align {
        TextAlign::Left => Align::Left,
        TextAlign::Center => Align::Center,
        TextAlign::Right => Align::Right,
    };
    let wrap = wrap.or(settings
        .wrap_width
        .map(|width| (width as f32, Wrap::WordOrGlyph)));
    match wrap {
        Some((width, wrap)) => {
            buffer.set_wrap(wrap);
            buffer.set_size(Some(width), None);
        }
        None => buffer.set_wrap(Wrap::None),
    }
    buffer.set_text(text, &attrs, Shaping::Advanced, Some(align));
    buffer
}

/// `text` shaped as `settings` say, and the size of its picture.
fn shape(font_system: &mut FontSystem, settings: &TextSource, text: &str) -> Shaped {
    let mut buffer = buffer(font_system, settings, text, None);
    buffer.shape_until_scroll(font_system, false);
    let measure = |buffer: &Buffer| {
        buffer.layout_runs().fold((0.0f32, 0.0f32), |(w, h), run| {
            (w.max(run.line_w), h.max(run.line_top + run.line_height))
        })
    };
    let (mut text_width, text_height) = measure(&buffer);
    if let Some(width) = settings.wrap_width {
        text_width = width as f32;
    }

    let outline = settings.outline.as_ref().map_or(0.0, |o| o.width.max(0.0));
    let (shadow_x, shadow_y) = settings.shadow.as_ref().map_or((0.0, 0.0), |s| (s.x, s.y));
    let margin = settings.padding as f32 + outline.ceil();
    let natural = (
        (text_width.ceil() + 2.0 * margin + shadow_x.abs()).max(1.0) as u32,
        (text_height.ceil() + 2.0 * margin + shadow_y.abs()).max(1.0) as u32,
    );
    let fixed = settings.width.zip(settings.height);
    let (width, height) = fixed.unwrap_or(natural);
    Shaped {
        buffer,
        text_width,
        margin,
        shadow_x,
        fixed,
        width,
        height,
    }
}

/// Draw `buffer`'s text (shaped as `settings` say) on `canvas` at
/// `origin`, with its shadow and outline under it.
fn draw_text(
    canvas: &mut Canvas,
    buffer: &Buffer,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    settings: &TextSource,
    origin: (f32, f32),
) {
    let mut draw = |canvas: &mut Canvas, offset: (f32, f32), color: [f32; 4]| {
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((offset.0, offset.1 + run.line_y), 1.0);
                if let Some(image) = cache.get_image(font_system, physical.cache_key) {
                    let x = physical.x + image.placement.left;
                    let y = physical.y - image.placement.top;
                    canvas.glyph(image, x, y, color);
                }
            }
        }
    };
    if let Some(shadow) = &settings.shadow {
        let color = shadow.color.rgba().unwrap_or_default();
        draw(canvas, (origin.0 + shadow.x, origin.1 + shadow.y), color);
    }
    if let Some(stroke) = settings.outline.as_ref().filter(|o| o.width > 0.0) {
        // The text drawn around a circle (two rings for thick outlines).
        let color = stroke.color.rgba().unwrap_or_default();
        for radius in [stroke.width, stroke.width / 2.0] {
            let steps = (radius * 4.0).clamp(8.0, 32.0) as usize;
            for step in 0..steps {
                let angle = TAU * step as f32 / steps as f32;
                let offset = (
                    origin.0 + radius * angle.cos(),
                    origin.1 + radius * angle.sin(),
                );
                draw(canvas, offset, color);
            }
        }
    }
    let color = settings.color.rgba().unwrap_or([1.0; 4]);
    draw(canvas, origin, color);
}

/// Premultiplied RGBA pixels being drawn on.
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![[0.0; 4]; (width * height) as usize],
        }
    }

    fn fill(&mut self, [r, g, b, a]: [f32; 4]) {
        self.pixels.fill([r * a, g * a, b * a, a]);
    }

    /// Draw a glyph image with its top left at (`x`, `y`), in `color`
    /// (straight alpha) for masks or its own colours for colour glyphs.
    fn glyph(&mut self, image: &cosmic_text::SwashImage, x: i32, y: i32, color: [f32; 4]) {
        let (w, h) = (image.placement.width as i32, image.placement.height as i32);
        for row in 0..h {
            let ty = y + row;
            if ty < 0 || ty >= self.height as i32 {
                continue;
            }
            for col in 0..w {
                let tx = x + col;
                if tx < 0 || tx >= self.width as i32 {
                    continue;
                }
                let i = (row * w + col) as usize;
                let source = match image.content {
                    SwashContent::Mask => {
                        let a = f32::from(image.data[i]) / 255.0 * color[3];
                        [color[0] * a, color[1] * a, color[2] * a, a]
                    }
                    SwashContent::SubpixelMask => {
                        let a = f32::from(image.data[i * 3 + 1]) / 255.0 * color[3];
                        [color[0] * a, color[1] * a, color[2] * a, a]
                    }
                    SwashContent::Color => {
                        let p = &image.data[i * 4..i * 4 + 4];
                        let a = f32::from(p[3]) / 255.0 * color[3];
                        let c = |v: u8| f32::from(v) / 255.0 * a;
                        [c(p[0]), c(p[1]), c(p[2]), a]
                    }
                };
                let pixel = &mut self.pixels[(ty as u32 * self.width + tx as u32) as usize];
                for (d, s) in pixel.iter_mut().zip(source) {
                    *d = s + *d * (1.0 - source[3]);
                }
            }
        }
    }

    /// Straight-alpha RGBA bytes.
    fn into_picture(self) -> Picture {
        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
        for [r, g, b, a] in self.pixels {
            let straight = |v: f32| {
                if a > 0.0 {
                    ((v / a).clamp(0.0, 1.0) * 255.0).round() as u8
                } else {
                    0
                }
            };
            rgba.extend([
                straight(r),
                straight(g),
                straight(b),
                (a.clamp(0.0, 1.0) * 255.0).round() as u8,
            ]);
        }
        Picture {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}
