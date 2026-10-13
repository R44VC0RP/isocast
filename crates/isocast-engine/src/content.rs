//! Content items (scenes as code, 5.5a): text and images an item shows
//! without a source, made from settings when the scenes are resolved (on
//! the scene thread, never per frame). Each picture is kept by a key made
//! from everything that decides it (settings, the text shown, the box it
//! wraps in, an image file's time), so items showing the same content
//! share it and a change draws only what it changes. The compositor makes
//! a texture of each when it adopts the scenes.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use isocast_protocol::{Content, ContentKind, ImageSource, SceneItem, TextSource};

use crate::data::DataStore;
use crate::layout::{ContentLeaf, ContentSizes};
use crate::picture::{self, Picture};
use crate::text;

/// Pictures of content, and what goes into them, kept between resolves
/// while they are used.
#[derive(Default)]
pub(crate) struct Contents {
    /// Counts resolves, to forget what the last one didn't use.
    round: u64,
    /// Pictures drawn, by key (`None`: it couldn't be drawn).
    pictures: HashMap<u64, (Option<Arc<Picture>>, u64)>,
    /// Image files decoded, by file, size and modification time.
    images: HashMap<u64, (Option<Arc<Picture>>, u64)>,
    /// Text read from files: its file's modification time and text.
    files: HashMap<PathBuf, (Option<SystemTime>, Option<String>, u64)>,
    /// Text measured, by its settings and text (and the width it wraps
    /// at).
    sizes: HashMap<u64, ((u32, u32), u64)>,
    /// Text content laid out in this resolve: its settings and its text.
    texts: Vec<(TextSource, String)>,
    /// The data keys the content of this resolve shows.
    keys: HashSet<Arc<str>>,
    /// Problems already warned about.
    warned: HashSet<String>,
    /// New problems, for warnings.
    warnings: Vec<String>,
    /// Time spent drawing pictures in this resolve.
    raster: std::time::Duration,
}

/// What a resolve made: the time it spent drawing, and the pictures the
/// scenes show.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Drawn {
    pub raster_ns: u64,
    pub pictures: u32,
    pub bytes: u64,
}

fn key_of(parts: impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

/// The settings as JSON, to key what they decide.
fn settings_key(settings: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(settings).unwrap_or_default()
}

impl Contents {
    /// Start a resolve.
    pub(crate) fn begin(&mut self) {
        self.round += 1;
        self.texts.clear();
        self.keys.clear();
        self.raster = std::time::Duration::ZERO;
    }

    /// End a resolve: forget what it didn't use. Returns what it drew.
    pub(crate) fn end(&mut self) -> Drawn {
        let round = self.round;
        self.pictures.retain(|_, (_, used)| *used == round);
        self.images.retain(|_, (_, used)| *used == round);
        self.files.retain(|_, (_, _, used)| *used == round);
        self.sizes.retain(|_, (_, used)| *used == round);
        let (pictures, bytes) = self
            .pictures
            .values()
            .filter_map(|(picture, _)| picture.as_ref())
            .fold((0, 0), |(count, bytes), picture| {
                (count + 1, bytes + picture.rgba.len() as u64)
            });
        Drawn {
            raster_ns: self.raster.as_nanos() as u64,
            pictures,
            bytes,
        }
    }

    /// The data keys the last resolve's content shows.
    pub(crate) fn keys(&self) -> &HashSet<Arc<str>> {
        &self.keys
    }

    /// Whether a file whose text content shows has changed since it was
    /// read.
    pub(crate) fn files_changed(&self) -> bool {
        self.files
            .iter()
            .any(|(path, (modified, _, _))| modified_time(path) != *modified)
    }

    /// Whether any content shows a file's text (to look at it now and
    /// then).
    pub(crate) fn reads_files(&self) -> bool {
        !self.files.is_empty()
    }

    /// Problems found since this was last asked, to warn about once.
    pub(crate) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn warn(&mut self, message: String) {
        if self.warned.insert(message.clone()) {
            self.warnings.push(message);
        }
    }

    /// The text `settings` show now: their text, their file's, or their
    /// data key's value.
    fn text(&mut self, settings: &TextSource, data: &DataStore) -> String {
        if let Some(text) = &settings.text {
            return text.clone();
        }
        if let Some(key) = &settings.data {
            self.keys.insert(key.as_str().into());
            return data.text(key);
        }
        let Some(file) = &settings.file else {
            return String::new();
        };
        let path = crate::record::expand_home(file);
        let round = self.round;
        let modified = modified_time(&path);
        let read = match self.files.get_mut(&path) {
            Some((was, text, used)) if *was == modified => {
                *used = round;
                text.clone()
            }
            _ => {
                let text = std::fs::read_to_string(&path)
                    .map(|text| text.trim_end().to_string())
                    .ok();
                self.files
                    .insert(path.clone(), (modified, text.clone(), round));
                text
            }
        };
        match read {
            Some(text) => text,
            None => {
                self.warn(format!(
                    "text content: could not read {}; it shows no text until it can",
                    path.display()
                ));
                String::new()
            }
        }
    }

    /// The size `text` takes as `settings` say.
    fn text_size(&mut self, settings: &TextSource, text: &str) -> (u32, u32) {
        let key = key_of(("size", settings_key(settings), text));
        let round = self.round;
        if let Some((size, used)) = self.sizes.get_mut(&key) {
            *used = round;
            return *size;
        }
        let size = text::size(settings, text);
        self.sizes.insert(key, (size, round));
        size
    }

    fn least_width(&mut self, settings: &TextSource, text: &str) -> u32 {
        let key = key_of(("least", settings_key(settings), text));
        let round = self.round;
        if let Some((size, used)) = self.sizes.get_mut(&key) {
            *used = round;
            return size.0;
        }
        let width = text::least_width(settings, text);
        self.sizes.insert(key, ((width, 0), round));
        width
    }

    /// An image file decoded (its first frame), as `image` says, and the
    /// key it is kept by (its file, size and modification time).
    fn image(&mut self, image: &ImageSource) -> (u64, Option<Arc<Picture>>) {
        let path = crate::record::expand_home(&image.path);
        let key = key_of((
            "image",
            &path,
            image.width,
            image.height,
            modified_time(&path).map(|time| {
                time.duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            }),
        ));
        let round = self.round;
        if let Some((picture, used)) = self.images.get_mut(&key) {
            *used = round;
            return (key, picture.clone());
        }
        let began = Instant::now();
        let loaded = picture::load(&path, image.width.zip(image.height));
        self.raster += began.elapsed();
        let picture = match loaded {
            Ok(mut frames) if !frames.is_empty() => Some(Arc::new(frames.swap_remove(0).0)),
            Ok(_) => None,
            Err(err) => {
                self.warn(format!(
                    "image content: could not load {}: {err:#}; the item shows only its style",
                    path.display()
                ));
                None
            }
        };
        self.images.insert(key, (picture.clone(), round));
        (key, picture)
    }

    /// The picture `item`'s content shows, laid out (every item has its
    /// rect), and the key it is kept by; `None` if it has no content or no
    /// size. Text placed by layout is drawn the size of its box, wrapped to
    /// fit it, so it is never stretched; text placed by a rect is drawn as
    /// a text source draws it and fitted into the rect like one.
    pub(crate) fn picture(
        &mut self,
        item: &SceneItem,
        data: &DataStore,
    ) -> Option<(u64, Option<Arc<Picture>>)> {
        let content = item.content.as_ref()?;
        let rect = item.rect.filter(|rect| rect.width > 0 && rect.height > 0)?;
        let round = self.round;
        match &content.kind {
            ContentKind::Text(settings) => {
                let text = self.text(settings, data);
                let mut drawn = settings.clone();
                drawn.text = Some(text);
                drawn.file = None;
                drawn.data = None;
                if item.layout.is_some() && settings.width.zip(settings.height).is_none() {
                    let text = drawn.text.as_deref().unwrap_or_default();
                    if settings.wrap_width.is_none() {
                        let own = self.text_size(&drawn, text);
                        if own.0 > rect.width {
                            drawn.wrap_width = Some(wrap_width(&drawn, rect.width as f32));
                        }
                    }
                    drawn.width = Some(rect.width);
                    drawn.height = Some(rect.height);
                }
                let key = key_of(("text", settings_key(&drawn)));
                if let Some((picture, used)) = self.pictures.get_mut(&key) {
                    *used = round;
                    return Some((key, picture.clone()));
                }
                let began = Instant::now();
                let picture = text::render(&drawn, drawn.text.as_deref().unwrap_or_default());
                self.raster += began.elapsed();
                let picture = Some(Arc::new(picture));
                self.pictures.insert(key, (picture.clone(), round));
                Some((key, picture))
            }
            ContentKind::Image(image) => {
                let (key, picture) = self.image(image);
                self.pictures.insert(key, (picture.clone(), round));
                Some((key, picture))
            }
        }
    }
}

/// The width text wraps at to fit a box `width` wide, inside the room its
/// settings keep around it.
fn wrap_width(settings: &TextSource, width: f32) -> u32 {
    (width - text::surround(settings).0).floor().max(1.0) as u32
}

fn modified_time(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Content's sizes for layout, with the data its text shows.
pub(crate) struct Sizer<'a> {
    contents: &'a mut Contents,
    data: &'a DataStore,
}

impl<'a> Sizer<'a> {
    pub(crate) fn new(contents: &'a mut Contents, data: &'a DataStore) -> Self {
        Self { contents, data }
    }
}

impl ContentSizes for Sizer<'_> {
    fn leaf(&mut self, content: &Content) -> ContentLeaf {
        match &content.kind {
            ContentKind::Image(image) => ContentLeaf::Picture(
                self.contents
                    .image(image)
                    .1
                    .map(|picture| (picture.width, picture.height)),
            ),
            ContentKind::Text(settings) => {
                let text = self.contents.text(settings, self.data);
                let mut measured = settings.clone();
                measured.text = None;
                measured.file = None;
                measured.data = None;
                let index = self.contents.texts.len() as u32;
                self.contents.texts.push((measured, text));
                ContentLeaf::Text(index)
            }
        }
    }

    fn text_size(&mut self, text: u32, width: Option<f32>) -> (f32, f32) {
        let Some((settings, shown)) = self.contents.texts.get(text as usize).cloned() else {
            return (0.0, 0.0);
        };
        let wraps = settings.wrap_width.is_none() && settings.width.zip(settings.height).is_none();
        let (w, h) = match width {
            Some(width) if wraps => {
                let mut wrapped = settings;
                wrapped.wrap_width = Some(wrap_width(&wrapped, width));
                let (_, height) = self.contents.text_size(&wrapped, &shown);
                // It takes the box's width, as a block of text does.
                (width.max(0.0) as u32, height)
            }
            _ => self.contents.text_size(&settings, &shown),
        };
        (w as f32, h as f32)
    }

    fn text_least_width(&mut self, text: u32) -> f32 {
        let Some((settings, shown)) = self.contents.texts.get(text as usize).cloned() else {
            return 0.0;
        };
        if settings.wrap_width.is_some() || settings.width.zip(settings.height).is_some() {
            return self.contents.text_size(&settings, &shown).0 as f32;
        }
        self.contents.least_width(&settings, &shown) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::{NodeIds, Sample, compile};
    use crate::layout::Layouter;
    use isocast_protocol::{Setup, SourceKind};

    fn setup(scenes: &str) -> Setup {
        let setup = Setup::from_toml(&format!(
            r##"
version = 1
name = "t"
[canvas]
width = 1920
height = 1080
fps = 60
[[sources]]
id = "title-source"
kind = "text"
text = "Building Isocast"
size = 48
{scenes}
[recording]
folder = "/tmp"
[recording.program]
encoder = {{ codec = "hevc", quality = 70 }}
"##
        ))
        .unwrap();
        let issues = setup.validate();
        assert!(issues.is_empty(), "{issues:?}");
        setup
    }

    /// `setup`'s scenes laid out and compiled with their content.
    fn compiled(setup: &Setup, data: &DataStore) -> crate::compiled::CompiledScenes {
        let mut contents = Contents::default();
        contents.begin();
        let mut scenes = setup.scenes.clone();
        let mut ids = NodeIds::default();
        Layouter::default().resolve(
            &mut scenes,
            (1920, 1080),
            &|_| None,
            &mut Sizer::new(&mut contents, data),
            &mut ids,
            &mut Vec::new(),
        );
        compile(
            scenes.into(),
            (1920, 1080),
            &mut ids,
            None,
            &[],
            &mut |item| contents.picture(item, data),
        )
    }

    #[test]
    fn text_by_rect_draws_as_its_source_does() {
        let setup = setup(
            r##"
[[scenes]]
id = "s"
items = [
  { id = "a", content = { kind = "text", text = "Building Isocast", size = 48 }, rect = { x = 0, y = 0, width = 400, height = 60 } },
  { id = "b", content = { kind = "text", text = "Building Isocast", size = 48 }, rect = { x = 0, y = 100, width = 400, height = 60 } },
]
"##,
        );
        let compiled = compiled(&setup, &DataStore::default());
        // Both items show one picture.
        assert_eq!(compiled.contents.len(), 1);
        let samples: Vec<Sample> = compiled.draws.iter().map(|d| d.sample).collect();
        assert_eq!(samples, [Sample::Content(0), Sample::Content(0)]);
        let SourceKind::Text(source) = &setup.sources[0].kind else {
            unreachable!()
        };
        let expected = text::render(source, "Building Isocast");
        let picture = compiled.contents[0].picture.as_ref().unwrap();
        assert_eq!(
            (picture.width, picture.height),
            (expected.width, expected.height)
        );
        assert!(picture.rgba == expected.rgba);
    }

    #[test]
    fn laid_out_text_wraps_to_its_box_and_shows_data() {
        let setup = setup(
            r##"
[[scenes]]
id = "s"
layout = { display = "flex", direction = "column", padding = 100 }
items = [
  { id = "short", content = { kind = "text", data = "title", size = 48 }, layout = {} },
  { id = "long", content = { kind = "text", text = "one two three four five six seven eight nine ten eleven twelve", size = 48 }, layout = { width = 300 } },
]
"##,
        );
        let data = DataStore::new();
        data.set(
            [("title".to_string(), serde_json::json!("Hi"))].into(),
            None,
        )
        .unwrap();
        let compiled = compiled(&setup, &data);
        let short = &compiled.nodes[0];
        let long = &compiled.nodes[1];
        // A block's width is the room it has: the canvas inside its padding.
        assert_eq!(short.size[0], 1720.0);
        // Wrapped at 300: several lines.
        assert_eq!(long.size[0], 300.0);
        assert!(long.size[1] > 48.0 * 1.2 * 3.0, "{:?}", long.size);
        // Each picture is its box's size, so text is never stretched.
        for (node, content) in [(short, 0), (long, 1)] {
            let picture = compiled.contents[content].picture.as_ref().unwrap();
            assert_eq!(
                [picture.width as f32, picture.height as f32],
                node.size,
                "content {content}"
            );
        }
        // A new value is another picture; the same value the same.
        let before = compiled.contents[0].key;
        data.set(
            [("title".to_string(), serde_json::json!("Hello"))].into(),
            None,
        )
        .unwrap();
        assert_ne!(
            super::tests::compiled(&setup, &data).contents[0].key,
            before
        );
    }
}
