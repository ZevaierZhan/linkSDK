//! Local storage is usable before, during and after network connectivity.
use crate::platform::ProfileLock;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Sdk {
    _lock: Arc<ProfileLock>,
    home: PathBuf,
}
impl Sdk {
    pub fn open(home: impl AsRef<Path>) -> Result<Self, String> {
        let lock = ProfileLock::acquire(home.as_ref())?;
        Ok(Self {
            home: lock.home.clone(),
            _lock: Arc::new(lock),
        })
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    /// Open a local replica with an explicitly supplied space, without starting libzt.
    pub fn open_replica(home: impl AsRef<Path>, space: crate::SpaceConfig) -> Result<Self, String> {
        if space.network_id == 0 {
            return Err("space ID must be nonzero".into());
        }
        let sdk = Self::open(home)?;
        let dir = sdk.home.join("space");
        crate::platform::private_dir(&dir)?;
        let path = dir.join("space.json");
        if path.exists() {
            let saved: crate::SpaceConfig =
                serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if saved.network_id != space.network_id {
                return Err("profile belongs to another space".into());
            }
        } else {
            crate::platform::write_private(
                &path,
                &serde_json::to_vec(&space).map_err(|e| e.to_string())?,
            )?;
        }
        Ok(sdk)
    }
    #[cfg(feature = "data")]
    pub fn dataset(&self, app: &str, dataset: &str) -> crate::data::Dataset {
        crate::data::Dataset::new(self._lock.clone(), app, dataset)
    }
}
