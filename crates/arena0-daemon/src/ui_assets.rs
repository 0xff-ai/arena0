//! The built web UI a daemon serves at `/`: an arena0-ui `dist` directory
//! named by `ARENA0_UI_DIR`. Files are read from disk on every request.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// A directory holding a built web UI; it held an `index.html` when opened.
#[derive(Debug)]
pub(crate) struct UiDir {
    root: PathBuf,
}

/// One UI file read for a response.
pub(crate) struct UiFile {
    pub(crate) bytes: Vec<u8>,
    /// From the file extension; `application/octet-stream` when unknown.
    pub(crate) content_type: &'static str,
}

impl UiDir {
    /// Open `dir` as the UI root. Fails, naming `dir`, unless it is a
    /// directory with an `index.html` file.
    pub(crate) fn open(dir: &Path) -> anyhow::Result<Self> {
        let root = dir.canonicalize().with_context(|| {
            format!(
                "open UI directory {}: expected a directory holding index.html",
                dir.display()
            )
        })?;
        anyhow::ensure!(
            root.is_dir() && root.join("index.html").is_file(),
            "UI directory {} must be a directory holding a regular index.html file",
            dir.display()
        );
        Ok(Self { root })
    }

    /// Read the UI file at `path`, which has no leading slash.
    ///
    /// `path` comes from the request URL, so only plain relative names are
    /// looked up: `..`, absolute or prefixed components return `None`.
    /// The caller supplies a trusted UI tree: filesystem symlinks are followed
    /// and can name files outside the root. A missing or unreadable file is
    /// `None` too; the caller decides between 404 and `index.html`.
    pub(crate) async fn find(&self, path: &str) -> Option<UiFile> {
        if !Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return None;
        }
        let bytes = tokio::fs::read(self.root.join(path)).await.ok()?;
        Some(UiFile {
            bytes,
            content_type: mime_guess::from_path(path)
                .first_raw()
                .unwrap_or("application/octet-stream"),
        })
    }
}
