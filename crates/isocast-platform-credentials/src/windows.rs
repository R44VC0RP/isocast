//! Secrets (stream keys) as generic credentials in the Windows Credential
//! Manager, the counterpart of the macOS Keychain items.
//!
//! Credentials belong to the signed-in Windows user and are encrypted by the
//! system. They persist on this machine (`CRED_PERSIST_LOCAL_MACHINE`), so a
//! key outlives the session but never roams to other machines. Items are
//! addressed by one target name built from the service and the account (see
//! [`target_name`]); secrets are never logged or put into errors.

use anyhow::{Result, anyhow, bail};
use windows::Win32::Foundation::{E_UNEXPECTED, ERROR_NO_SUCH_LOGON_SESSION, ERROR_NOT_FOUND};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_MAX_GENERIC_TARGET_NAME_LENGTH,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

/// Whether a credential is stored for `service` and `account`.
pub fn has(service: &str, account: &str) -> Result<bool> {
    let target = target_name(service, account)?;
    match read(&target) {
        Ok(credential) => Ok(credential.is_some()),
        Err(err) => Err(credential_error("check", service, &err)),
    }
}

/// The stored secret, `None` when there is none.
pub fn get(service: &str, account: &str) -> Result<Option<String>> {
    let target = target_name(service, account)?;
    let credential = match read(&target) {
        Ok(Some(credential)) => credential,
        Ok(None) => return Ok(None),
        Err(err) => return Err(credential_error("read", service, &err)),
    };
    let blob = credential.blob()?;
    std::str::from_utf8(blob)
        .map(|text| Some(text.to_owned()))
        .map_err(|_| anyhow!("the Credential Manager item for {service} is not valid UTF-8 text"))
}

/// Store `secret`, replacing any credential already stored for the same
/// service and account.
pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
    let target = target_name(service, account)?;
    let bytes = secret.as_bytes();
    if bytes.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
        bail!(
            "the stream key is {} bytes, more than the {CRED_MAX_CREDENTIAL_BLOB_SIZE} bytes \
             Windows Credential Manager can hold",
            bytes.len()
        );
    }
    let mut wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let credential = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(wide.as_mut_ptr()),
        CredentialBlobSize: bytes.len() as u32,
        // An empty secret is a null blob of size zero.
        CredentialBlob: if bytes.is_empty() {
            std::ptr::null_mut()
        } else {
            bytes.as_ptr().cast_mut()
        },
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    // SAFETY: `credential` points at `wide` (NUL-terminated) and `bytes`,
    // both alive for the call; CredWriteW only reads them and copies.
    unsafe { CredWriteW(&credential, 0) }.map_err(|err| credential_error("store", service, &err))
}

/// Remove the credential. `false` when there was none.
pub fn delete(service: &str, account: &str) -> Result<bool> {
    let target = target_name(service, account)?;
    let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    match unsafe { CredDeleteW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(true),
        Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
        Err(err) => Err(credential_error("remove", service, &err)),
    }
}

/// Credential targets are case-insensitive, but RTMP paths and our service/
/// account pairs are not. Encode their UTF-8 bytes as lowercase hex, separated
/// by `/`, so case differences, Unicode and delimiters remain distinct. This
/// is a namespace encoding, not encryption; Windows protects the secret blob.
fn target_name(service: &str, account: &str) -> Result<String> {
    for (field, value) in [("service", service), ("account", account)] {
        if value.is_empty() {
            bail!("credential {field} is empty");
        }
        if value.contains('\0') {
            bail!("credential {field} has a NUL character in it");
        }
    }
    const PREFIX: &str = "Isocast/v1/";
    let length = service
        .len()
        .checked_add(account.len())
        .and_then(|length| length.checked_mul(2))
        .and_then(|length| length.checked_add(PREFIX.len() + 1));
    let Some(length) =
        length.filter(|length| *length <= CRED_MAX_GENERIC_TARGET_NAME_LENGTH as usize)
    else {
        bail!(
            "credential service and account together are too long for Windows Credential \
             Manager (at most {CRED_MAX_GENERIC_TARGET_NAME_LENGTH} UTF-16 units in the target \
             name)"
        );
    };
    use std::fmt::Write;
    let mut target = String::with_capacity(length);
    target.push_str(PREFIX);
    for (index, value) in [service, account].into_iter().enumerate() {
        if index != 0 {
            target.push('/');
        }
        for byte in value.bytes() {
            write!(&mut target, "{byte:02x}")?;
        }
    }
    Ok(target)
}

/// A credential returned by CredReadW, freed on drop.
struct Credential(*mut CREDENTIALW);

impl Credential {
    /// The secret's bytes, after checking the record is what was asked for.
    fn blob(&self) -> Result<&[u8]> {
        // SAFETY: non-null (checked in `read`) and valid until CredFree in drop.
        let record = unsafe { &*self.0 };
        if record.Type != CRED_TYPE_GENERIC {
            bail!("the Credential Manager item is not a generic credential");
        }
        if record.CredentialBlobSize > CRED_MAX_CREDENTIAL_BLOB_SIZE {
            bail!("the Credential Manager item is larger than Windows allows");
        }
        if record.CredentialBlobSize == 0 {
            return Ok(&[]);
        }
        if record.CredentialBlob.is_null() {
            bail!("the Credential Manager item has a size but no data");
        }
        // SAFETY: Windows returned a blob of exactly this many bytes, which
        // stays valid until CredFree in drop (it borrows `self`).
        Ok(unsafe {
            std::slice::from_raw_parts(record.CredentialBlob, record.CredentialBlobSize as usize)
        })
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful CredReadW and is freed once.
        unsafe { CredFree(self.0.cast_const().cast()) };
    }
}

/// The generic credential for `target`, `None` when there is none.
fn read(target: &str) -> windows::core::Result<Option<Credential>> {
    let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let mut record: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call; on success
    // `record` receives a buffer that `Credential` frees.
    match unsafe { CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None, &mut record) } {
        Ok(()) if record.is_null() => Err(windows::core::Error::from_hresult(E_UNEXPECTED)),
        Ok(()) => Ok(Some(Credential(record))),
        Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => Ok(None),
        Err(err) => Err(err),
    }
}

/// Names the action and service, never the key.
fn credential_error(action: &str, service: &str, err: &windows::core::Error) -> anyhow::Error {
    if err.code() == ERROR_NO_SUCH_LOGON_SESSION.to_hresult() {
        return anyhow!(
            "could not {action} the stream key in Windows Credential Manager ({service}): \
             this logon has no credential store; run Isocast in the signed-in Windows \
             desktop session, not a network-only SSH or service logon ({err})"
        );
    }
    anyhow!("could not {action} the stream key in Windows Credential Manager ({service}): {err}")
}
