//! Local filesystem namespace shared by arena0 processes.
//!
//! [`Home`] captures the process environment once and derives the location of
//! each validated [`HostName`], the process-wide cache, and temporary homes.
//! Environment-derived paths are absolute so that every process reaches the
//! same namespace regardless of its cwd. This crate only resolves paths. The
//! daemon owns filesystem validation, directory creation, permissions, and
//! durable files.

use std::borrow::Borrow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

const DEFAULT_HOST_NAME: &str = "host-01";

/// A daemon-local Host name that is safe to use as one path component.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostName(String);

impl HostName {
    /// Borrow the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct the stable name for a zero-based local Ensemble position.
    #[must_use]
    pub fn for_local_index(index: usize) -> Self {
        let ordinal = index.checked_add(1).expect("local Host index overflow");
        Self(format!("host-{ordinal:02}"))
    }
}

impl AsRef<str> for HostName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for HostName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Default for HostName {
    fn default() -> Self {
        Self::for_local_index(0)
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HostName {
    type Err = HostNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(HostNameError::Empty);
        }
        if value.chars().any(char::is_control) {
            return Err(HostNameError::ControlCharacter);
        }
        if value.contains(['/', '\\']) {
            return Err(HostNameError::PathSeparator);
        }

        let mut components = Path::new(value).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(HostNameError::PathComponent);
        }

        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<&str> for HostName {
    type Error = HostNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<String> for HostName {
    type Error = HostNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Why a raw Host name cannot identify a local Host namespace.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HostNameError {
    /// The name contained no characters.
    #[error("Host name must not be empty")]
    Empty,
    /// The name contained terminal or other control characters.
    #[error("Host name must not contain control characters")]
    ControlCharacter,
    /// The name contained a platform path separator.
    #[error("Host name must not contain path separators")]
    PathSeparator,
    /// The name was a reserved or otherwise non-normal path component.
    #[error("Host name must be one normal path component")]
    PathComponent,
}

/// One captured arena0 home and its primary-Host socket override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Home {
    root: PathBuf,
    cache_dir: PathBuf,
    default_socket: Option<PathBuf>,
}

impl Home {
    /// Resolve an explicit home without reading or changing process environment.
    pub fn from_root(root: PathBuf) -> Result<Self, HomeError> {
        Self::from_environment(Some(root.into_os_string()), None, None, None)
    }

    /// Resolve the arena0 home from the current process environment.
    ///
    /// `ARENA0_HOME` wins over `HOME`. Without `ARENA0_HOME`, the root is
    /// `$HOME/.arena0`. `ARENA0_CACHE_DIR` may keep the process-wide cache
    /// outside a temporary state root. Environment-derived paths must be
    /// absolute. A leading `~` expands only when the value is exactly `~` or
    /// begins with `~/`; its `HOME` base must also be absolute.
    pub fn from_env() -> Result<Self, HomeError> {
        Self::from_environment(
            std::env::var_os("ARENA0_HOME"),
            std::env::var_os("HOME"),
            std::env::var_os("ARENA0_SOCKET"),
            std::env::var_os("ARENA0_CACHE_DIR"),
        )
    }

    /// The root that contains all Host namespaces and process-wide caches.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The stable process-wide cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Parent directory for disposable arena0 homes.
    #[must_use]
    pub fn temporary_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Derive the state directory and socket for one Host.
    #[must_use]
    pub fn host(&self, name: &HostName) -> HostLocation {
        let state_dir = self.root.join("hosts").join(name.as_str());
        let socket = if name.as_str() == DEFAULT_HOST_NAME {
            self.default_socket
                .clone()
                .unwrap_or_else(|| state_dir.join("arena0.sock"))
        } else {
            state_dir.join("arena0.sock")
        };
        HostLocation { state_dir, socket }
    }

    fn from_environment(
        arena0_home: Option<OsString>,
        user_home: Option<OsString>,
        default_socket: Option<OsString>,
        cache_dir: Option<OsString>,
    ) -> Result<Self, HomeError> {
        let root = match arena0_home {
            Some(value) => {
                let root = expand_home(value, user_home.as_deref())?;
                validate_root(&root)?;
                if !root.is_absolute() {
                    return Err(HomeError::RelativeArena0Home);
                }
                root
            }
            None => {
                let user_home = user_home
                    .as_deref()
                    .map(PathBuf::from)
                    .ok_or(HomeError::MissingHome)?;
                if !user_home.is_absolute() {
                    return Err(HomeError::RelativeUserHome);
                }
                let root = user_home.join(".arena0");
                validate_root(&root)?;
                root
            }
        };
        let default_socket = default_socket.map(PathBuf::from);
        if default_socket
            .as_deref()
            .is_some_and(|socket| !socket.is_absolute())
        {
            return Err(HomeError::RelativeSocket);
        }
        let cache_dir = match cache_dir {
            Some(value) => {
                let cache_dir = expand_home(value, user_home.as_deref())?;
                validate_root(&cache_dir).map_err(|_| HomeError::InvalidCacheDir)?;
                if !cache_dir.is_absolute() {
                    return Err(HomeError::RelativeCacheDir);
                }
                cache_dir
            }
            None => root.join("cache"),
        };
        Ok(Self {
            root,
            cache_dir,
            default_socket,
        })
    }
}

/// Resolved local paths shared by a Host daemon and its clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLocation {
    state_dir: PathBuf,
    socket: PathBuf,
}

impl HostLocation {
    /// The Host's durable state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The Unix socket used by local clients.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

/// Why the process environment cannot identify an arena0 home.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HomeError {
    /// Neither supported home variable exists.
    #[error("neither ARENA0_HOME nor HOME is set")]
    MissingHome,
    /// An arena0 path requires tilde expansion but `HOME` is absent.
    #[error("an arena0 path starts with '~' but HOME is not set")]
    MissingHomeForTilde,
    /// The resolved root is empty, the filesystem root, or contains `..`.
    #[error("arena0 home must be an absolute directory below the filesystem root without '..'")]
    InvalidRoot,
    /// `ARENA0_HOME` was supplied as a relative path.
    #[error("ARENA0_HOME must be an absolute path; set it to an absolute Host home")]
    RelativeArena0Home,
    /// `HOME` was supplied as a relative path.
    #[error("HOME must be an absolute path; set it to an absolute user home")]
    RelativeUserHome,
    /// `ARENA0_CACHE_DIR` was empty, root, or traversed a parent directory.
    #[error("arena0 cache directory must be below the filesystem root without '..'")]
    InvalidCacheDir,
    /// `ARENA0_CACHE_DIR` was supplied as a relative path.
    #[error("ARENA0_CACHE_DIR must be an absolute path")]
    RelativeCacheDir,
    /// `ARENA0_SOCKET` was supplied as a relative path.
    #[error(
        "ARENA0_SOCKET must be an absolute path; pass the CLI --socket option for a cwd-relative socket"
    )]
    RelativeSocket,
}

fn expand_home(value: OsString, user_home: Option<&OsStr>) -> Result<PathBuf, HomeError> {
    let Some(value_str) = value.to_str() else {
        return Ok(PathBuf::from(value));
    };
    if value_str != "~" && !value_str.starts_with("~/") {
        return Ok(PathBuf::from(value));
    }

    let user_home = user_home.ok_or(HomeError::MissingHomeForTilde)?;
    if !Path::new(user_home).is_absolute() {
        return Err(HomeError::RelativeUserHome);
    }
    let rest = value_str
        .strip_prefix('~')
        .unwrap_or_default()
        .trim_start_matches('/');
    Ok(if rest.is_empty() {
        PathBuf::from(user_home)
    } else {
        PathBuf::from(user_home).join(rest)
    })
}

fn validate_root(root: &Path) -> Result<(), HomeError> {
    if root.as_os_str().is_empty()
        || root == Path::new(".")
        || root == Path::new("/")
        || root
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(HomeError::InvalidRoot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(
        arena0_home: Option<&str>,
        user_home: Option<&str>,
        socket: Option<&str>,
        cache_dir: Option<&str>,
    ) -> Result<Home, HomeError> {
        Home::from_environment(
            arena0_home.map(OsString::from),
            user_home.map(OsString::from),
            socket.map(OsString::from),
            cache_dir.map(OsString::from),
        )
    }

    #[test]
    fn environment_precedence_and_tilde_expansion_are_exact() {
        assert_eq!(
            home(Some("/srv/arena0"), Some("/home/alice"), None, None)
                .unwrap()
                .root(),
            Path::new("/srv/arena0")
        );
        assert_eq!(
            home(None, Some("/home/alice"), None, None).unwrap().root(),
            Path::new("/home/alice/.arena0")
        );
        assert_eq!(
            home(Some("~/state"), Some("/home/alice"), None, None)
                .unwrap()
                .root(),
            Path::new("/home/alice/state")
        );
        assert_eq!(
            home(Some("~service/state"), Some("/home/alice"), None, None,),
            Err(HomeError::RelativeArena0Home)
        );
    }

    #[test]
    fn missing_or_unsafe_home_is_rejected() {
        assert_eq!(home(None, None, None, None), Err(HomeError::MissingHome));
        assert_eq!(
            home(Some("~/state"), None, None, None),
            Err(HomeError::MissingHomeForTilde)
        );
        for root in ["", ".", "/", "/srv/../tmp"] {
            assert_eq!(
                home(Some(root), None, None, None),
                Err(HomeError::InvalidRoot)
            );
        }
    }

    #[test]
    fn environment_paths_must_be_absolute() {
        assert_eq!(
            home(Some("relative/state"), Some("/home/alice"), None, None,),
            Err(HomeError::RelativeArena0Home)
        );
        assert_eq!(
            home(None, Some("relative"), None, None),
            Err(HomeError::RelativeUserHome)
        );
        assert_eq!(
            home(Some("~/state"), Some("relative"), None, None),
            Err(HomeError::RelativeUserHome)
        );
        assert_eq!(
            home(Some("/srv/arena0"), None, Some("run/arena0.sock"), None,),
            Err(HomeError::RelativeSocket)
        );
        assert_eq!(
            home(Some("/srv/arena0"), None, None, Some("cache/arena0"),),
            Err(HomeError::RelativeCacheDir)
        );
    }

    #[test]
    fn absolute_environment_locations_resolve_exact_host_paths() {
        let home = home(
            Some("/srv/arena0"),
            Some("/home/alice"),
            Some("/run/arena0.sock"),
            None,
        )
        .unwrap();
        let host = "host-02".parse().unwrap();

        assert_eq!(home.root(), Path::new("/srv/arena0"));
        assert_eq!(home.cache_dir(), Path::new("/srv/arena0/cache"));
        assert_eq!(
            home.host(&host).state_dir(),
            Path::new("/srv/arena0/hosts/host-02")
        );
        assert_eq!(
            home.host(&host).socket(),
            Path::new("/srv/arena0/hosts/host-02/arena0.sock")
        );
    }

    #[test]
    fn default_socket_override_does_not_apply_to_named_hosts() {
        let home = home(Some("/srv/arena0"), None, Some("/run/arena0.sock"), None).unwrap();
        assert_eq!(
            home.host(&HostName::default()).socket(),
            Path::new("/run/arena0.sock")
        );
        assert_eq!(
            home.host(&"host-02".parse().unwrap()).socket(),
            Path::new("/srv/arena0/hosts/host-02/arena0.sock")
        );
    }

    #[test]
    fn cache_and_temporary_paths_have_distinct_owners() {
        let default = home(Some("/srv/arena0"), None, None, None).unwrap();
        assert_eq!(default.cache_dir(), Path::new("/srv/arena0/cache"));
        assert_eq!(default.temporary_dir(), Path::new("/srv/arena0/tmp"));

        let temporary = home(
            Some("/srv/arena0/tmp/run-1"),
            None,
            None,
            Some("/srv/arena0/cache"),
        )
        .unwrap();
        assert_eq!(temporary.root(), Path::new("/srv/arena0/tmp/run-1"));
        assert_eq!(temporary.cache_dir(), Path::new("/srv/arena0/cache"));
    }

    #[test]
    fn host_names_are_portable_single_components() {
        assert_eq!(HostName::default().as_str(), "host-01");
        assert_eq!(HostName::for_local_index(1).as_str(), "host-02");
        for name in ["", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(name.parse::<HostName>().is_err(), "accepted {name:?}");
        }
    }
}
