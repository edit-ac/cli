//! Native file publication, separate from shared semantic commit.
//!
//! Unix regular files only. A persistent, private sidecar inode coordinates
//! edit.ac writers (never unlink a lock file: that would split the lock domain).
//! Exact bytes and POSIX mode/owner/group are preserved (a replacement with
//! different ownership is rejected); LF/CRLF/mixed endings are taken
//! from the user's accepted source, with no export normalization. Missing targets
//! use atomic no-clobber hard-link publication; existing targets use rename.
//!
//! Locks do not constrain other editors. Revalidation immediately before rename
//! detects observed changes, but rename is not compare-and-swap: an external
//! writer can race the final check. Parent links are revalidated too. This is a
//! supported local filesystem boundary, not a network-filesystem or hardware
//! power-loss guarantee. Saved means file sync and directory sync succeeded.
//! All errors retain the reconciled draft and baseline. A postpublication error
//! means publication is uncertain; callers must inspect/reopen, never blindly
//! retry. Recovery files are never automatically deleted here.
use crate::{
    reconcile::ReconciledEdit,
    target::{Baseline, FileIdentity, LocalTarget, ResolveOptions, resolve},
};
use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveState {
    Unchanged,
    Saved,
}
pub struct SavedEdit {
    pub state: SaveState,
    pub path: PathBuf,
    pub edit: ReconciledEdit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    UnsupportedPlatform,
    Busy,
    Stale,
    TargetChanged,
    ReadOnly,
    OverwriteRequired,
    Io,
    PublicationUncertain,
}
pub struct SaveFailure {
    pub kind: FailureKind,
    pub path: PathBuf,
    pub cause: Option<io::Error>,
    pub edit: ReconciledEdit,
}
impl std::fmt::Debug for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveFailure")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("cause", &self.cause)
            .field("draft", &self.edit.buffer.recovery().buffer())
            .finish()
    }
}
impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "save {:?} at {}; retain draft {}",
            self.kind,
            self.path.display(),
            self.edit.buffer.recovery().buffer().display()
        )?;
        if let Some(e) = &self.cause {
            write!(f, ": {e}")?;
        }
        Ok(())
    }
}
impl std::error::Error for SaveFailure {}

/// Deterministic test transport. Production uses NativeFilesystem; transports
/// must obey write-all, sync and publication semantics, not fabricate success.
/// Publication errors may have occurred after the namespace changed and are
/// always reported conservatively as uncertain.
pub trait Filesystem {
    fn write_all(&mut self, file: &mut File, source: &[u8]) -> io::Result<()> {
        file.write_all(source)
    }
    fn sync_file(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn publish(&mut self, temporary: &Path, destination: &Path, replace: bool) -> io::Result<()> {
        if replace {
            fs::rename(temporary, destination)
        } else {
            fs::hard_link(temporary, destination)
        }
    }
    fn sync_directory(&mut self, directory: &File) -> io::Result<()> {
        directory.sync_all()
    }
}
pub struct NativeFilesystem;
impl Filesystem for NativeFilesystem {}

pub fn save(edit: ReconciledEdit) -> Result<SavedEdit, SaveFailure> {
    save_with(edit, None, false, &mut NativeFilesystem)
}
/// Existing different destinations require explicit overwrite. An alias of the
/// opened path still uses the original baseline: overwrite cannot bypass stale
/// checks. Destination creation is exclusive, never an eager truncation.
pub fn export(
    edit: ReconciledEdit,
    path: &Path,
    overwrite: bool,
) -> Result<SavedEdit, SaveFailure> {
    save_with(edit, Some(path), overwrite, &mut NativeFilesystem)
}

pub fn save_with(
    edit: ReconciledEdit,
    destination: Option<&Path>,
    overwrite: bool,
    io: &mut impl Filesystem,
) -> Result<SavedEdit, SaveFailure> {
    let path = destination
        .unwrap_or(edit.buffer.target().requested_path())
        .to_owned();
    match publish(&edit, destination, overwrite, io) {
        Ok((state, path)) => Ok(SavedEdit { state, path, edit }),
        Err((kind, cause)) => Err(SaveFailure {
            kind,
            cause,
            path,
            edit,
        }),
    }
}
type Failure = (FailureKind, Option<io::Error>);
fn fail(kind: FailureKind) -> Failure {
    (kind, None)
}
fn io_failure(e: io::Error) -> Failure {
    (FailureKind::Io, Some(e))
}
#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}
#[cfg(unix)]
fn writable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o222 != 0
}
#[cfg(unix)]
fn parent_path(target: &LocalTarget) -> Result<PathBuf, Failure> {
    let parent = fs::canonicalize(target.requested_path().parent().unwrap()).map_err(io_failure)?;
    if Some(parent.as_path()) != target.resolved_path().parent() {
        return Err(fail(FailureKind::TargetChanged));
    }
    Ok(parent)
}
#[cfg(unix)]
fn revalidate(target: &LocalTarget) -> Result<Option<Permissions>, Failure> {
    let parent = parent_path(target)?;
    match target.baseline() {
        Baseline::Missing { parent_identity } => {
            if identity(&fs::metadata(&parent).map_err(io_failure)?) != *parent_identity {
                return Err(fail(FailureKind::TargetChanged));
            }
            match fs::symlink_metadata(target.resolved_path()) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Ok(_) => Err(fail(FailureKind::TargetChanged)),
                Err(e) => Err(io_failure(e)),
            }
        }
        Baseline::Existing {
            identity: expected,
            fingerprint,
            ..
        } => {
            let metadata = fs::symlink_metadata(target.resolved_path()).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    fail(FailureKind::TargetChanged)
                } else {
                    io_failure(e)
                }
            })?;
            if !metadata.is_file() || identity(&metadata) != *expected {
                return Err(fail(FailureKind::TargetChanged));
            }
            // The bounded resolver detects replacements during read as well as
            // UTF-8/resource changes. No unbounded read of an external replacement.
            let current = resolve(
                target.requested_path(),
                ResolveOptions {
                    create: false,
                    format: Some(target.format()),
                },
            )
            .map_err(|_| fail(FailureKind::Stale))?;
            if current.resolved_path() != target.resolved_path() {
                return Err(fail(FailureKind::TargetChanged));
            }
            if let Baseline::Existing {
                identity: actual, ..
            } = current.baseline()
            {
                if actual != expected {
                    return Err(fail(FailureKind::TargetChanged));
                }
            }
            if !fingerprint.matches(current.source_bytes()) {
                return Err(fail(FailureKind::Stale));
            }
            if !writable(&metadata) {
                return Err(fail(FailureKind::ReadOnly));
            }
            Ok(Some(metadata.permissions()))
        }
    }
}

/// Stable bounded sidecar names, including non-UTF-8 filenames. Hash collisions
/// only serialize unrelated writers; they cannot authorize overwriting a target.
#[cfg(unix)]
pub fn lock_path(target: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in target.file_name().unwrap().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    target.with_file_name(format!(".editac-lock-{hash:016x}"))
}
#[cfg(unix)]
fn acquire(target: &Path) -> Result<File, Failure> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = lock_path(target);
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let before = fs::symlink_metadata(&path).map_err(io_failure)?;
            if !before.is_file() || before.permissions().mode() & 0o077 != 0 {
                return Err(fail(FailureKind::TargetChanged));
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(io_failure)?;
            if identity(&before) != identity(&file.metadata().map_err(io_failure)?) {
                return Err(fail(FailureKind::TargetChanged));
            }
            file
        }
        Err(e) => return Err(io_failure(e)),
    };
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => fail(FailureKind::Busy),
        std::fs::TryLockError::Error(e) => io_failure(e),
    })?;
    let current = fs::symlink_metadata(&path).map_err(io_failure)?;
    if !current.is_file() || identity(&current) != identity(&file.metadata().map_err(io_failure)?) {
        return Err(fail(FailureKind::TargetChanged));
    }
    Ok(file)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
#[cfg(unix)]
fn temporary(parent: &Path) -> Result<(Temporary, File), Failure> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let path = parent.join(format!(
            ".editac-save-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((Temporary(path), file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_failure(e)),
        }
    }
    Err(io_failure(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary names exhausted",
    )))
}
#[cfg(unix)]
fn publish(
    edit: &ReconciledEdit,
    destination: Option<&Path>,
    overwrite: bool,
    io: &mut impl Filesystem,
) -> Result<(SaveState, PathBuf), Failure> {
    let opened = edit.buffer.target();
    let export_target;
    let target = match destination {
        None => opened,
        Some(path) => {
            export_target = resolve(
                path,
                ResolveOptions {
                    create: true,
                    format: Some(opened.format()),
                },
            )
            .map_err(|e| io_failure(io::Error::other(e)))?;
            if export_target.resolved_path() == opened.resolved_path() {
                opened
            } else {
                if matches!(export_target.baseline(), Baseline::Existing { .. }) && !overwrite {
                    return Err(fail(FailureKind::OverwriteRequired));
                }
                &export_target
            }
        }
    };
    if target.resolved_path() == opened.resolved_path() && edit.buffer.source() == opened.source() {
        return Ok((SaveState::Unchanged, opened.resolved_path().to_owned()));
    }
    let parent = parent_path(target)?;
    let directory = File::open(&parent).map_err(io_failure)?;
    let parent_identity = identity(&directory.metadata().map_err(io_failure)?);
    if !writable(&directory.metadata().map_err(io_failure)?) {
        return Err(fail(FailureKind::ReadOnly));
    }
    let _lock = acquire(target.resolved_path())?;
    let permissions = revalidate(target)?;
    let (temporary, mut file) = temporary(&parent)?;
    io.write_all(&mut file, edit.buffer.source().as_bytes())
        .map_err(io_failure)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions).map_err(io_failure)?;
    }
    file.flush().map_err(io_failure)?;
    io.sync_file(&file).map_err(io_failure)?;
    // Probe directory durability support before crossing the publication boundary.
    io.sync_directory(&directory).map_err(io_failure)?;
    revalidate(target)?;
    if parent_identity != identity(&fs::metadata(&parent).map_err(io_failure)?) {
        return Err(fail(FailureKind::TargetChanged));
    }
    // Permission changes during staging are conflicts, not implicit permission resets.
    if let Baseline::Existing { .. } = target.baseline() {
        use std::os::unix::fs::MetadataExt;
        let current = fs::symlink_metadata(target.resolved_path()).map_err(io_failure)?;
        let staged = file.metadata().map_err(io_failure)?;
        if current.permissions() != staged.permissions()
            || current.uid() != staged.uid()
            || current.gid() != staged.gid()
        {
            return Err(fail(FailureKind::TargetChanged));
        }
    }
    let replace = matches!(target.baseline(), Baseline::Existing { .. });
    io.publish(&temporary.0, target.resolved_path(), replace)
        .map_err(|e| (FailureKind::PublicationUncertain, Some(e)))?;
    // Missing publication has two names until this link is removed. Cleanup is
    // part of the directory-sync boundary, and never removes the destination.
    if !replace {
        fs::remove_file(&temporary.0).map_err(|e| (FailureKind::PublicationUncertain, Some(e)))?;
    }
    io.sync_directory(&directory)
        .map_err(|e| (FailureKind::PublicationUncertain, Some(e)))?;
    Ok((SaveState::Saved, target.resolved_path().to_owned()))
}
#[cfg(not(unix))]
fn publish(
    _: &ReconciledEdit,
    _: Option<&Path>,
    _: bool,
    _: &mut impl Filesystem,
) -> Result<(SaveState, PathBuf), Failure> {
    Err(fail(FailureKind::UnsupportedPlatform))
}
