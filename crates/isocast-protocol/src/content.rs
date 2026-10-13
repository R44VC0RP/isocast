//! Content items (scenes as code, 5.5a): a picture the engine makes for a
//! scene item from settings, in place of a source, so a scene can hold
//! text and images without a source for each. Its settings are the text
//! and image sources' own, so their checks and schema are shared.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::source::Kind;
use crate::{ImageSource, Issues, TextSource, fields};

/// What an item shows in place of a source or scene: text, or an image,
/// made by the engine from these settings (the text and image sources'
/// own, `kind` saying which). It isn't a source: it has no ISO file and
/// opens nothing; it is drawn again only when its settings, or the data
/// key it shows, change, and items showing the same content share one
/// picture. Text placed by `layout` wraps at the width its box gets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
pub struct Content {
    #[serde(flatten)]
    pub kind: ContentKind,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: fields::UnknownFields,
}

fields::keep_unknown_fields!(Content, kind, ContentKind);

/// The fields every content has, besides its kind's.
const CONTENT_FIELDS: &[&str] = &["kind"];

/// What a content item shows, by `kind`. (Setup data, one per content
/// item: the size of the text settings doesn't matter here.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
#[allow(clippy::large_enum_variant)]
pub enum ContentKind {
    /// Text, as a text source draws it: from `text`, a `file` or a `data`
    /// key.
    #[serde(rename = "text")]
    Text(TextSource),
    /// An image file, as an image source shows it (an animated GIF shows
    /// its first frame; use an image source to play it).
    #[serde(rename = "image")]
    Image(ImageSource),
}

impl ContentKind {
    /// Every kind's name, as setups write it.
    pub const NAMES: &[&str] = &["text", "image"];

    /// Where `value`, an object of the kind named `name`, is wrong in that
    /// kind's fields (see [`crate::located`]).
    pub(crate) fn locate(name: &str, value: &serde_json::Value) -> Option<crate::located::Located> {
        match name {
            "text" => crate::located::from_value::<TextSource>(value).err(),
            "image" => crate::located::from_value::<ImageSource>(value).err(),
            _ => None,
        }
    }

    /// The kind's name, as setups write it.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Image(_) => "image",
        }
    }

    /// The kind's own fields, as setups write them.
    pub fn fields(&self) -> &'static [String] {
        static FIELDS: std::sync::OnceLock<[Vec<String>; 2]> = std::sync::OnceLock::new();
        let [text, image] = FIELDS.get_or_init(|| {
            [
                fields::names::<TextSource>(),
                fields::names::<ImageSource>(),
            ]
        });
        match self {
            Self::Text(_) => text,
            Self::Image(_) => image,
        }
    }
}

impl Content {
    /// Check the content at `path` (like `scenes[0].items[2].content`).
    pub(crate) fn check(&self, path: &str, issues: &mut Issues) {
        match &self.kind {
            ContentKind::Text(text) => text.check(path, issues),
            ContentKind::Image(image) => image.check(path, issues),
        }
        fields::report(
            issues,
            path,
            &self.unknown,
            &format!("{} content", self.kind.label()),
            self.kind.fields(),
            CONTENT_FIELDS,
        );
    }

    pub fn text(text: TextSource) -> Self {
        Self {
            kind: ContentKind::Text(text),
            unknown: Default::default(),
        }
    }

    pub fn image(image: ImageSource) -> Self {
        Self {
            kind: ContentKind::Image(image),
            unknown: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ContentKind, Setup};

    fn setup(items: &str) -> Result<Setup, crate::LoadError> {
        Setup::from_toml(&format!(
            r##"
version = 1
name = "t"
canvas = {{ width = 1280, height = 720, fps = 30 }}
sources = [{{ id = "a", kind = "test_pattern" }}]

[[scenes]]
id = "s"
items = [
{items}
]

[recording]
folder = "."
program = {{ encoder = {{ codec = "hevc", quality = 70 }} }}
"##
        ))
    }

    fn problems(items: &str) -> Vec<(String, String)> {
        setup(items)
            .unwrap()
            .validate()
            .into_iter()
            .map(|issue| (issue.path, issue.message))
            .collect()
    }

    #[test]
    fn text_and_image_content_read_and_save_unchanged() {
        let text = r##"
version = 1
name = "t"

[canvas]
width = 1280
height = 720
fps = 30

[[scenes]]
id = "s"

[[scenes.items]]
id = "title"
fit = "contain"

[scenes.items.content]
kind = "text"
data = "title"
size = 48.0
weight = 600

[scenes.items.layout]
position = "absolute"
left = 64
bottom = 64

[[scenes.items]]
id = "logo"

[scenes.items.content]
kind = "image"
path = "/tmp/logo.png"

[scenes.items.rect]
x = 0
y = 0
width = 100
height = 100

[recording]
folder = "."

[recording.program]
encoder = { codec = "hevc", quality = 70 }
"##;
        let setup = Setup::from_toml(text).unwrap();
        assert!(setup.validate().is_empty(), "{:?}", setup.validate());
        let items = &setup.scenes[0].items;
        match &items[0].content.as_ref().unwrap().kind {
            ContentKind::Text(text) => assert_eq!(text.data.as_deref(), Some("title")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            items[1].content.as_ref().unwrap().kind,
            ContentKind::Image(_)
        ));
        let toml = setup.to_toml().unwrap();
        assert_eq!(Setup::from_toml(&toml).unwrap(), setup);
        let json = serde_json::to_string(&setup).unwrap();
        assert_eq!(Setup::from_json(&json).unwrap(), setup);
    }

    #[test]
    fn content_is_checked_with_field_paths() {
        let found = problems(
            r##"
  { id = "a1", source = "a", content = { kind = "text", text = "x" }, rect = { x = 0, y = 0, width = 10, height = 10 } },
  { id = "t1", content = { kind = "text", text = "x", data = "title" }, rect = { x = 0, y = 0, width = 10, height = 10 } },
  { id = "t2", content = { kind = "text", data = "Score.Home", sise = 3 }, rect = { x = 0, y = 0, width = 10, height = 10 } },
  { id = "i1", content = { kind = "image", path = "" }, fit = "cover", crop = { left = 2 }, rect = { x = 0, y = 0, width = 10, height = 10 } },
"##,
        );
        let at = |path: &str| {
            found
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, message)| message.clone())
                .unwrap_or_else(|| panic!("no problem at {path}: {found:?}"))
        };
        assert!(at("scenes[0].items[0].content").contains("already shows a source"));
        assert_eq!(
            at("scenes[0].items[1].content"),
            "give one of text, file or data"
        );
        assert!(at("scenes[0].items[2].content.data").contains("lowercase"));
        assert!(at("scenes[0].items[2].content.sise").contains("did you mean \"size\""));
        assert_eq!(at("scenes[0].items[3].content.path"), "give the image file");
        // Content has a picture to fit and crop.
        assert!(
            !found
                .iter()
                .any(|(p, _)| p == "scenes[0].items[3].fit" || p == "scenes[0].items[3].crop"),
            "{found:?}"
        );
    }

    #[test]
    fn a_bad_kind_or_field_type_names_the_field() {
        let err = setup(
            r##"{ id = "x", content = { kind = "video", path = "a" }, rect = { x = 0, y = 0, width = 1, height = 1 } },"##,
        )
        .unwrap_err();
        assert!(err.message().contains("video"), "{}", err.message());
        let json = serde_json::json!({ "kind": "text", "size": "big" });
        let err = crate::located::from_value::<crate::Content>(&json).unwrap_err();
        assert_eq!(err.path, "size", "{err}");
    }
}
