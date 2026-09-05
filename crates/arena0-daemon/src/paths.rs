//! Durable state and socket paths for one local Host.

use std::fs;
use std::path::{Component, Path, PathBuf};

use arena0_home::{Home, HostLocation};

/// Resolved on-disk locations for one daemon instance.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The node home: durable state and the workspace.
    pub(crate) state_dir: PathBuf,
    /// SQLite database owned by this Host's [`arena0_store::Store`].
    pub(crate) db_path: PathBuf,
    /// Keystore directory (seed files, mode 0600; the index).
    pub(crate) keys_dir: PathBuf,
    /// Unix socket the daemon listens on.
    pub(crate) socket: PathBuf,
}

impl Paths {
    /// Derive paths from an explicit home and socket.
    #[must_use]
    pub fn new(home: PathBuf, socket: PathBuf) -> Self {
        let db_path = home.join("arena0.sqlite");
        let keys_dir = home.join("keys");
        Self {
            state_dir: home,
            db_path,
            keys_dir,
            socket,
        }
    }

    /// Add daemon-owned durable paths to a resolved Host location.
    pub(crate) fn from_location(location: &HostLocation) -> anyhow::Result<Self> {
        let paths = Self::new(
            location.state_dir().to_owned(),
            location.socket().to_owned(),
        );
        validate_path(&paths.state_dir).map_err(anyhow::Error::new)?;
        validate_path(&paths.socket).map_err(anyhow::Error::new)?;
        Ok(paths)
    }

    /// Create the Host home, keystore, and socket-parent directories. The keystore
    /// directory is created with mode `0700` so only the owner can read seeds.
    pub(crate) fn ensure_dirs(&self) -> std::io::Result<()> {
        validate_path(&self.state_dir)?;
        validate_path(&self.keys_dir)?;
        validate_path(&self.db_path)?;
        validate_path(&self.socket)?;

        ensure_directory(&self.state_dir)?;
        create_private_dir(&self.keys_dir)?;
        if let Some(parent) = self.socket.parent() {
            validate_path(parent)?;
            ensure_directory(parent)?;
        }
        ensure_regular_file_if_present(&self.db_path, "database")?;
        ensure_socket_if_present(&self.socket)?;
        Ok(())
    }
}

/// Process-wide Wasmtime cache below the stable arena0 cache directory.
pub(crate) fn wasmtime_cache_dir(home: &Home) -> anyhow::Result<PathBuf> {
    let path = std::path::absolute(home.cache_dir().join("wasmtime"))?;
    validate_path(&path).map_err(anyhow::Error::new)?;
    Ok(path)
}

/// Reject path traversal and symlinked components before any path is created or
/// opened. The daemon's durable paths are intentionally simple path joins; a
/// parent component would make the resulting namespace ambiguous.
pub(crate) fn validate_path(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(invalid_path("path must not be empty"));
    }

    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(invalid_path("path must not contain parent components"));
            }
            Component::Normal(name) => current.push(name),
        }

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_path("path must not contain symlink components"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    // Never let a caller accidentally chmod the filesystem root through a
    // malformed explicit home path.
    if path == Path::new("/") || path == Path::new(".") {
        return Err(invalid_path("path must name a directory below the root"));
    }
    Ok(())
}

fn invalid_path(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn ensure_directory(dir: &Path) -> std::io::Result<()> {
    validate_path(dir)?;
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(invalid_path("directory path must not be a symlink"));
            }
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} is not a directory", dir.display()),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir)?;
        }
        Err(error) => return Err(error),
    }

    // Re-check after creation so a raced symlink is never accepted as the
    // directory owner of durable state.
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_path("created path is not a directory"));
    }
    Ok(())
}

/// Create a directory if absent and tighten it to owner-only (`0700`) on unix.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    ensure_directory(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_regular_file_if_present(path: &Path, kind: &str) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{kind} path must be a regular file: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_socket_if_present(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket path must be a Unix socket: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_socket_if_present(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_dirs_rejects_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            dir.path().join("home/../escaped"),
            dir.path().join("socket"),
        );
        let error = paths.ensure_dirs().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_dirs_rejects_symlinked_keystore_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_keys = dir.path().join("real-keys");
        std::fs::create_dir(&real_keys).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        symlink(&real_keys, home.join("keys")).unwrap();

        let paths = Paths::new(home, dir.path().join("arena0.sock"));
        assert!(paths.ensure_dirs().is_err());
    }
}
