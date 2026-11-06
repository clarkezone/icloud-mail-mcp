use crate::error::{ErrorCode, MailError, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, fs, io::Write, path::Path};
use zeroize::Zeroizing;
pub struct Credentials {
    pub(crate) username: Zeroizing<String>,
    pub(crate) password: Zeroizing<String>,
}
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials([REDACTED])")
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}
impl Credentials {
    pub fn new(username: String, password: String) -> Result<Self> {
        let username = Zeroizing::new(username);
        let password = Zeroizing::new(password);
        let valid = |s: &str| {
            !s.is_empty() && s.len() <= 1024 && s.is_ascii() && !s.chars().any(char::is_control)
        };
        if !valid(&username) || !valid(&password) {
            return Err(MailError(ErrorCode::ConfigurationError));
        }
        Ok(Self { username, password })
    }
    pub fn load(path: &Path) -> Result<Self> {
        let error = || MailError(ErrorCode::ConfigurationError);
        outside_checkout(path.parent().ok_or_else(error)?)?;
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let mut file = opts.open(path).map_err(|_| error())?;
        let meta = file.metadata().map_err(|_| error())?;
        if !meta.is_file() || meta.len() > 8192 {
            return Err(error());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let parent =
                fs::symlink_metadata(path.parent().ok_or_else(error)?).map_err(|_| error())?;
            if meta.uid() != rustix::process::geteuid().as_raw()
                || meta.mode() & 0o077 != 0
                || !parent.is_dir()
                || parent.uid() != meta.uid()
                || parent.mode() & 0o077 != 0
            {
                return Err(error());
            }
        }
        use std::io::Read;
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::Read::by_ref(&mut file)
            .take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| error())?;
        if bytes.len() > 8192 {
            return Err(error());
        }
        let raw: FileCredentials = serde_json::from_slice(&bytes).map_err(|_| error())?;
        if raw.username.is_empty()
            || raw.password.is_empty()
            || raw.username.len() > 1024
            || raw.password.len() > 1024
            || raw
                .username
                .chars()
                .chain(raw.password.chars())
                .any(|c| !c.is_ascii() || c.is_control())
        {
            return Err(error());
        }
        Ok(Self {
            username: raw.username,
            password: raw.password,
        })
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let error = || MailError(ErrorCode::ConfigurationError);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(error)?;
        if !parent.exists() {
            let mut b = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(parent).map_err(|_| error())?;
        }
        let meta = fs::symlink_metadata(parent).map_err(|_| error())?;
        outside_checkout(parent)?;
        if !meta.is_dir() {
            return Err(error());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
                return Err(error());
            }
        }
        if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
            return Err(error());
        }
        #[derive(Serialize)]
        struct Borrowed<'a> {
            username: &'a str,
            password: &'a str,
        }
        let raw = Zeroizing::new(
            serde_json::to_vec(&Borrowed {
                username: &self.username,
                password: &self.password,
            })
            .map_err(|_| error())?,
        );
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| error())?;
        file.write_all(&raw).map_err(|_| error())?;
        file.as_file().sync_all().map_err(|_| error())?;
        file.persist(path).map_err(|_| error())?;
        Ok(())
    }
}

fn outside_checkout(parent: &Path) -> Result<()> {
    let parent = fs::canonicalize(parent).map_err(|_| MailError(ErrorCode::ConfigurationError))?;
    if parent.ancestors().any(|p| p.join(".git").exists()) {
        return Err(MailError(ErrorCode::ConfigurationError));
    }
    Ok(())
}
