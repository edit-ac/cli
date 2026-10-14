//! Read-only local target resolution. No credential, editor or transport access.
//!
//! `.md` and `.txt` select Markdown and literal text respectively (ASCII case
//! insensitive); other suffixes need an explicit format. Creation is a pending
//! target, never an eager filesystem mutation. Final-component symlinks and
//! non-regular files are refused. Parent links are resolved to a canonical path;
//! future publication must revalidate both that path and native file identity.
//!
//! Native identities currently require Unix device/inode metadata. Other hosts
//! return an explicit unsupported result rather than inventing an identity.

use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Bound source acquisition before parsing/preparation. Larger inputs are rejected.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Text,
}

impl Format {
    /// Exact public option values; unsupported formats never fall back to Markdown.
    pub fn parse(value: &str) -> Result<Self, TargetError> {
        match value {
            "markdown" => Ok(Self::Markdown),
            "text" => Ok(Self::Text),
            _ => Err(TargetError::UnsupportedFormat(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOptions {
    pub create: bool,
    pub format: Option<Format>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Collision-free byte fingerprint, deliberately retaining the exact UTF-8
/// baseline rather than a platform-dependent or lossy hash. Clones share storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFingerprint(Arc<str>);

impl ContentFingerprint {
    pub fn matches(&self, bytes: &[u8]) -> bool {
        self.0.as_bytes() == bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Baseline {
    Existing {
        identity: FileIdentity,
        fingerprint: ContentFingerprint,
        modified: Option<SystemTime>,
        readonly: bool,
    },
    Missing {
        parent_identity: FileIdentity,
    },
}

#[derive(Debug, Clone)]
pub struct LocalTarget {
    requested_path: PathBuf,
    resolved_path: PathBuf,
    format: Format,
    source: Arc<str>,
    baseline: Baseline,
}

impl LocalTarget {
    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }
    pub fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn source_bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }
    pub fn baseline(&self) -> &Baseline {
        &self.baseline
    }
}

#[derive(Debug)]
pub enum TargetError {
    UnsupportedTarget,
    UnsupportedPlatform,
    UnsupportedFormat(String),
    FormatRequired,
    CreationRequired,
    InvalidUtf8,
    TooLarge,
    ChangedDuringOpen,
    Io(io::Error),
}

impl TargetError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedTarget => "unsupported-target",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::UnsupportedFormat(_) => "unsupported-format",
            Self::FormatRequired => "format-required",
            Self::CreationRequired => "creation-required",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::TooLarge => "source-too-large",
            Self::ChangedDuringOpen => "changed-during-open",
            Self::Io(_) => "target-io",
        }
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedTarget => write!(
                f,
                "Use a local regular file; URLs, directories and symlinks are unsupported."
            ),
            Self::UnsupportedPlatform => write!(
                f,
                "Native target identity is currently supported on Unix only."
            ),
            Self::UnsupportedFormat(value) => {
                write!(f, "Unsupported format {value:?}; choose markdown or text.")
            }
            Self::FormatRequired => write!(
                f,
                "Choose --format markdown or --format text for this suffix."
            ),
            Self::CreationRequired => write!(
                f,
                "Target is missing; use --create to authorize later creation."
            ),
            Self::InvalidUtf8 => write!(
                f,
                "Target is not valid UTF-8; original bytes are untouched."
            ),
            Self::TooLarge => write!(
                f,
                "Target exceeds the {MAX_SOURCE_BYTES}-byte source limit."
            ),
            Self::ChangedDuringOpen => {
                write!(f, "Target changed while opening; reopen before editing.")
            }
            Self::Io(error) => write!(f, "Cannot open local target: {error}"),
        }
    }
}

impl std::error::Error for TargetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TargetError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(unix)]
fn identity(metadata: &Metadata) -> Result<FileIdentity, TargetError> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn identity(_: &Metadata) -> Result<FileIdentity, TargetError> {
    Err(TargetError::UnsupportedPlatform)
}

fn selected_format(path: &Path, declared: Option<Format>) -> Result<Format, TargetError> {
    if let Some(format) = declared {
        return Ok(format);
    }
    match path.extension().and_then(|v| v.to_str()) {
        Some(value) if value.eq_ignore_ascii_case("md") => Ok(Format::Markdown),
        Some(value) if value.eq_ignore_ascii_case("txt") => Ok(Format::Text),
        _ => Err(TargetError::FormatRequired),
    }
}

/// Resolve a local source without writing, parsing Markdown, reading credentials
/// or attaching any transport. `--create` never truncates an existing target.
///
/// Open-time metadata checks detect ordinary replacements/concurrent writes;
/// they are not a lock or compare-and-swap against non-cooperating writers.
pub fn resolve(
    path: impl AsRef<Path>,
    options: ResolveOptions,
) -> Result<LocalTarget, TargetError> {
    let path = path.as_ref();
    if path.as_os_str().is_empty()
        || path
            .to_str()
            .is_some_and(|v| v.contains("://") || v.starts_with("ac:"))
        || path.file_name().is_none()
    {
        return Err(TargetError::UnsupportedTarget);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().ends_with(b"/") {
            return Err(TargetError::UnsupportedTarget);
        }
    }
    let format = selected_format(path, options.format)?;
    let requested_path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = requested_path
        .parent()
        .ok_or(TargetError::UnsupportedTarget)?;
    let canonical_parent = fs::canonicalize(parent)?;
    let resolved_path = canonical_parent.join(
        requested_path
            .file_name()
            .ok_or(TargetError::UnsupportedTarget)?,
    );
    let metadata = match fs::symlink_metadata(&requested_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !options.create {
                return Err(TargetError::CreationRequired);
            }
            let parent_metadata = fs::metadata(&canonical_parent)?;
            return Ok(LocalTarget {
                requested_path,
                resolved_path,
                format,
                source: Arc::from(""),
                baseline: Baseline::Missing {
                    parent_identity: identity(&parent_metadata)?,
                },
            });
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(TargetError::UnsupportedTarget);
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(TargetError::TooLarge);
    }
    let file = File::open(&resolved_path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || identity(&metadata)? != identity(&opened)? {
        return Err(TargetError::ChangedDuringOpen);
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(TargetError::TooLarge);
    }
    let after = file.metadata()?;
    let current = fs::symlink_metadata(&requested_path)?;
    if !current.is_file()
        || identity(&opened)? != identity(&current)?
        || opened.len() != after.len()
        || after.len() != bytes.len() as u64
        || opened.modified().ok() != after.modified().ok()
        || after.len() != current.len()
        || after.modified().ok() != current.modified().ok()
    {
        return Err(TargetError::ChangedDuringOpen);
    }
    let source: Arc<str> =
        Arc::from(String::from_utf8(bytes).map_err(|_| TargetError::InvalidUtf8)?);
    Ok(LocalTarget {
        requested_path,
        resolved_path,
        format,
        baseline: Baseline::Existing {
            identity: identity(&after)?,
            fingerprint: ContentFingerprint(source.clone()),
            modified: after.modified().ok(),
            readonly: after.permissions().readonly(),
        },
        source,
    })
}
