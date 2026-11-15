use std::path::{Path, PathBuf};

/// Spell the same verified short local Windows database path ordinarily.
/// SQLite 3.53.2 treats a verbatim drive prefix as UNC and can select its
/// shared-handle SHM lock race. This grants no directory/file authority.
/// Other platforms, UNC/device paths and verbatim-only names stay unchanged.
pub fn sqlite_file_path(directory: &Path, filename: &str) -> PathBuf {
    let original = directory.join(filename);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Component, Prefix};
        let one_filename = matches!(
            Path::new(filename).components().next(),
            Some(Component::Normal(_))
        ) && Path::new(filename).components().count() == 1;
        if one_filename
            && matches!(directory.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::VerbatimDisk(_)))
        {
            if let Some(normal) = directory.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
                let normal = Path::new(normal);
                let database = normal.join(filename);
                // The longest SQLite sidecar suffix is -journal. Do not turn
                // a working long/verbatim path into a different Win32 name.
                if database.as_os_str().encode_wide().count() + "-journal".len() + 1 < 260
                    && ordinary_components(normal)
                    && ordinary_components(Path::new(filename))
                    && normal.canonicalize().ok().as_deref() == Some(directory)
                {
                    return database;
                }
            }
        }
    }
    original
}

#[cfg(windows)]
fn ordinary_components(path: &Path) -> bool {
    path.components().all(|part| {
        let std::path::Component::Normal(name) = part else {
            return true;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device_number = stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"));
        !name.ends_with(['.', ' '])
            && !name.contains(':')
            && !device_number.is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
            && !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn verified_short_disk_alias_preserves_identity_and_special_names() {
        let root =
            std::env::temp_dir().join(format!("link-sdk-sqlite-path-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap();
        let file = sqlite_file_path(&canonical, "shared-data.sqlite");
        assert!(!file.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(file.parent().unwrap().canonicalize().unwrap(), canonical);
        for name in [
            "NUL.sqlite".to_owned(),
            "trailing.".to_owned(),
            "x".repeat(240),
        ] {
            assert_eq!(sqlite_file_path(&canonical, &name), canonical.join(name));
        }
        std::fs::remove_dir(root).unwrap();
    }
}
