//! Minimal filesystem ownership and identity-format support; no application capabilities.
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

pub(crate) fn identity_id(public: &str) -> Result<String, String> {
    let id = public.split(':').next().ok_or("invalid identity")?;
    if id.len() != 10 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid identity".into());
    }
    Ok(id.to_ascii_lowercase())
}

pub(crate) fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Durable replacement in the same directory. Profiles must not be shared between nodes.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temp = path.with_extension("pending");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        File::open(path.parent().ok_or("missing parent")?)
            .and_then(|d| d.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// OS advisory lock released on normal exit and process crash.
pub struct ProfileLock {
    file: File,
    pub(crate) home: PathBuf,
}
impl ProfileLock {
    pub fn acquire(home: &Path) -> Result<Self, String> {
        private_dir(home)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.join("profile.lock"))
            .map_err(|e| e.to_string())?;
        file.try_lock()
            .map_err(|_| "profile already in use".to_string())?;
        Ok(Self {
            file,
            home: home.to_path_buf(),
        })
    }
}
impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
