//! The media backend's credential-store seam and bundle identity.
use objc2_foundation::{NSBundle, NSString};
use std::path::Path;

pub use isocast_platform_credentials::{delete_secret, has_secret, secret, set_secret};

/// The bundle identifier of the outermost app this executable is in:
/// `dev.isocast.release` for the engine inside Isocast.app, the dev app's
/// own for the development engine; `None` for a bare binary. The bundled
/// engine's own identifier is the same in every build, so it can't tell
/// them apart.
pub fn app_identifier() -> Option<String> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe
        .ancestors()
        .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .last()?;
    bundle_identifier(app)
}

fn bundle_identifier(app: &Path) -> Option<String> {
    let path = NSString::from_str(app.to_str()?);
    let bundle = NSBundle::bundleWithPath(&path)?;
    Some(bundle.bundleIdentifier()?.to_string()).filter(|id| !id.is_empty())
}
