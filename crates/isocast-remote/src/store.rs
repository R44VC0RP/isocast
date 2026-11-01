//! Public trust metadata and credential-store orchestration. The file lock
//! serializes first-use identity creation across concurrent CLI processes.
use crate::{Identity, fingerprint};
use base64::{Engine, engine::general_purpose::STANDARD};
use isocast_protocol::remote::{RemoteIdentity, RemoteSettings};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const MAX_SETTINGS: usize = 1024 * 1024;

pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}
pub fn decode(value: &str) -> io::Result<Vec<u8>> {
    STANDARD
        .decode(value)
        .map_err(|_| io::Error::other("invalid base64 identity"))
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct Store {
    path: PathBuf,
    _lock: File,
}
impl Store {
    pub fn lock(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path.with_extension("lock"))?;
        lock.lock()?;
        Ok(Self {
            path: path.into(),
            _lock: lock,
        })
    }
    pub fn read(&self) -> io::Result<RemoteSettings> {
        read(&self.path)
    }
    pub fn save(&self, settings: &RemoteSettings) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("remote settings need a parent directory"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        let bytes = serde_json::to_vec_pretty(settings)?;
        if bytes.len() > MAX_SETTINGS {
            return Err(io::Error::other("remote settings exceed 1 MB"));
        }
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        // A failed persist hands the temporary file back for the next try.
        let mut file = Some(file);
        isocast_protocol::fs::retry_replace(|| {
            let temporary = file.take().expect("returned by the failed persist");
            temporary.persist(&self.path).map_err(|error| {
                file = Some(error.file);
                error.error
            })
        })?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    /// `read_key` and `write_key` are platform credential-store operations.
    /// Never replace a malformed/inaccessible key or silently rotate a known id.
    pub fn identity(
        &self,
        name: &str,
        read_key: impl Fn() -> io::Result<Option<String>>,
        write_key: impl Fn(&str) -> io::Result<()>,
    ) -> io::Result<(Identity, RemoteIdentity)> {
        let mut settings = self.read()?;
        let encoded = match read_key()? {
            Some(key) => Zeroizing::new(key),
            None if settings.identity.is_some() => {
                return Err(io::Error::other(
                    "remote identity key is missing; restore the credential or explicitly reset and pair again",
                ));
            }
            None => {
                let bytes = Identity::generate()?;
                let key = Zeroizing::new(encode(&bytes));
                write_key(&key)?;
                // A successful credential write must be readable by this binary.
                let saved = Zeroizing::new(
                    read_key()?
                        .ok_or_else(|| io::Error::other("identity credential was not saved"))?,
                );
                if *saved != *key {
                    return Err(io::Error::other(
                        "identity credential changed during creation",
                    ));
                }
                key
            }
        };
        let bytes = Zeroizing::new(decode(&encoded)?);
        let key = Identity::from_pkcs8(&bytes)?;
        let public = RemoteIdentity {
            id: key.id(),
            name: name.into(),
            spki: encode(key.spki()),
        };
        if settings
            .identity
            .as_ref()
            .is_some_and(|previous| previous.id != public.id)
        {
            return Err(io::Error::other(
                "remote identity changed; restore the original credential or explicitly reset and pair again",
            ));
        }
        if settings.identity.as_ref() != Some(&public) {
            settings.identity = Some(public.clone());
            self.save(&settings)?;
        }
        Ok((key, public))
    }
}

pub fn read(path: &Path) -> io::Result<RemoteSettings> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RemoteSettings::default());
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SETTINGS {
        return Err(io::Error::other("remote settings exceed 1 MB"));
    }
    let settings: RemoteSettings = serde_json::from_slice(&bytes)?;
    for peer in &settings.peers {
        let spki = decode(&peer.spki)?;
        if fingerprint(&spki) != peer.id {
            return Err(io::Error::other(
                "paired identity id does not match its pin",
            ));
        }
    }
    Ok(settings)
}

pub fn machine_name() -> String {
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        // SAFETY: gethostname writes at most the supplied buffer length.
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0 {
            let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
            return String::from_utf8_lossy(&name[..end])
                .trim_end_matches(".local")
                .into();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Isocast".into())
}
