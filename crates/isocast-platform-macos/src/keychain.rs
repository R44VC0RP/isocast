//! Secrets (stream keys) as generic passwords in the login Keychain.
//!
//! The daemon runs in the background, so it never lets the Keychain show a
//! prompt: an item another build of Isocast stored (a different code
//! signature) can't be read without one, so reading it fails with a way
//! out, and storing a key replaces the item so this build owns it. Each
//! build keeps its keys under a service of its own (see [`app_identifier`]),
//! so builds don't fight over one item (#63).

use std::path::Path;

use anyhow::{Result, anyhow};
use objc2_foundation::{NSBundle, NSString};
use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::passwords;

/// The Keychain's "the item could not be found".
const NOT_FOUND: i32 = -25300;
/// The Keychain's "user interaction is not allowed": it would have asked.
const NEEDS_PERMISSION: i32 = -25308;
/// errSecInvalidOwnerEdit: the item belongs to another build's code identity.
const NOT_OWNER: i32 = -25244;
/// What reading another build's item gives once its prompt was refused.
const AUTH_FAILED: i32 = -25293;

pub fn has_secret(service: &str, account: &str) -> Result<bool> {
    let _quiet = SecKeychain::disable_user_interaction();
    let result = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .account(account)
        .load_attributes(true)
        .limit(Limit::Max(1))
        .search();
    match result {
        Ok(items) => Ok(!items.is_empty()),
        Err(err) if err.code() == NOT_FOUND => Ok(false),
        Err(err) => Err(keychain_error("check", service, &err)),
    }
}

pub fn secret(service: &str, account: &str) -> Result<Option<String>> {
    let _quiet = SecKeychain::disable_user_interaction();
    match passwords::get_generic_password(service, account) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow!("the Keychain item for {account} is not text")),
        Err(err) if err.code() == NOT_FOUND => Ok(None),
        Err(err) if [NEEDS_PERMISSION, AUTH_FAILED].contains(&err.code()) => Err(anyhow!(
            "the stream key in the Keychain was stored by another build of Isocast, which this \
             one may not read without asking; remove the \"{service}\" item in Keychain Access, \
             then set the key again in Settings → Stream (or `isocast stream key set`)"
        )),
        Err(err) => Err(keychain_error("read", service, &err)),
    }
}

pub fn set_secret(service: &str, account: &str, secret: &str) -> Result<()> {
    let _quiet = SecKeychain::disable_user_interaction();
    // Replace rather than update, so this build owns the item and can read
    // it without a prompt.
    match passwords::delete_generic_password(service, account) {
        Ok(()) => {}
        Err(err) if err.code() == NOT_FOUND => {}
        Err(err) => return Err(keychain_error("replace", service, &err)),
    }
    passwords::set_generic_password(service, account, secret.as_bytes())
        .map_err(|err| keychain_error("store", service, &err))
}

pub fn delete_secret(service: &str, account: &str) -> Result<bool> {
    let _quiet = SecKeychain::disable_user_interaction();
    match passwords::delete_generic_password(service, account) {
        Ok(()) => Ok(true),
        Err(err) if err.code() == NOT_FOUND => Ok(false),
        Err(err) => Err(keychain_error("remove", service, &err)),
    }
}

fn keychain_error(
    action: &str,
    service: &str,
    err: &security_framework::base::Error,
) -> anyhow::Error {
    let hint = if err.code() == NOT_OWNER {
        format!(
            "; another build of Isocast saved this key, and macOS lets only that build change it: \
             remove the \"{service}\" item in Keychain Access and set the key again"
        )
    } else if err.code() == NEEDS_PERMISSION {
        format!(
            "; macOS wants permission, which Isocast doesn't ask for from the background: \
             remove the \"{service}\" item in Keychain Access and set the key again"
        )
    } else {
        String::new()
    };
    anyhow!(
        "could not {action} the stream key in the Keychain: {} (error {}){hint}",
        err.message().unwrap_or_else(|| "unknown error".into()),
        err.code()
    )
}

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
