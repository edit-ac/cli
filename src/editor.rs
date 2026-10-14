//! Recoverable external-editor sessions, before shared semantic preparation.
//!
//! Configuration uses whitespace-separated words, single/double quotes and
//! backslash escapes (inside single quotes backslashes are literal). There is
//! no expansion, shell, pipeline or substitution. Empty quoted arguments are
//! retained. The first word is the executable; the absolute buffer path is
//! appended as one argument. Explicit configuration wins over nonempty VISUAL,
//! then EDITOR. Exit 130 or Unix SIGINT means cancel; other unsuccessful exits
//! are failures. Editors with another cancel convention need an explicit wrapper.
//!
//! The configured editor is trusted native code, not a sandbox. Session roots
//! must be caller-controlled. Each session directory is exclusively created with
//! Unix mode 0700, files with 0600. Failures and cancellation retain baseline and
//! draft; successful outcomes retain their workspace until explicit cleanup.
//! This is draft recovery, not committed-state durability or a saved target.

use crate::target::{Format, LocalTarget, MAX_SOURCE_BYTES};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    executable: OsString,
    arguments: Vec<OsString>,
}

impl EditorCommand {
    pub fn parse(configuration: &str) -> Result<Self, EditorError> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut started = false;
        let mut quote = None;
        let mut chars = configuration.chars();
        while let Some(ch) = chars.next() {
            if ch == '\0' {
                return Err(EditorError::configuration(
                    "editor configuration contains NUL",
                ));
            }
            match (quote, ch) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some('\''), _) => word.push(ch),
                (_, '\\') => {
                    let escaped = chars.next().ok_or_else(|| {
                        EditorError::configuration("editor configuration ends with a backslash")
                    })?;
                    if escaped == '\0' {
                        return Err(EditorError::configuration(
                            "editor configuration contains NUL",
                        ));
                    }
                    word.push(escaped);
                    started = true;
                }
                (None, '\'' | '"') => {
                    quote = Some(ch);
                    started = true;
                }
                (None, c) if c.is_whitespace() => {
                    if started {
                        words.push(OsString::from(std::mem::take(&mut word)));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            }
        }
        if quote.is_some() {
            return Err(EditorError::configuration(
                "editor configuration has an unclosed quote",
            ));
        }
        if started {
            words.push(word.into());
        }
        if words.is_empty() || words[0].is_empty() {
            return Err(EditorError::configuration(
                "configure an executable with arguments, VISUAL or EDITOR",
            ));
        }
        Ok(Self {
            executable: words.remove(0),
            arguments: words,
        })
    }

    /// Testable selection without changing process-global environment variables.
    /// Invalid selected configuration fails; it never silently falls back.
    pub fn select(
        configured: Option<&str>,
        visual: Option<&str>,
        editor: Option<&str>,
    ) -> Result<Self, EditorError> {
        let selected = configured
            .or_else(|| visual.filter(|s| !s.trim().is_empty()))
            .or_else(|| editor.filter(|s| !s.trim().is_empty()));
        Self::parse(selected.unwrap_or(""))
    }

    pub fn from_environment(configured: Option<&str>) -> Result<Self, EditorError> {
        if let Some(value) = configured {
            return Self::parse(value);
        }
        for name in ["VISUAL", "EDITOR"] {
            match std::env::var(name) {
                Ok(value) if !value.trim().is_empty() => return Self::parse(&value),
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(EditorError::configuration(
                        "selected editor configuration is not Unicode; configure it explicitly",
                    ));
                }
                _ => {}
            }
        }
        Self::select(None, None, None)
    }

    pub fn executable(&self) -> &OsStr {
        &self.executable
    }
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

/// Both exact source files remain caller-owned until explicitly discarded.
#[derive(Debug, Clone)]
pub struct RecoveryFiles {
    directory: PathBuf,
    buffer: PathBuf,
    baseline: PathBuf,
}
impl RecoveryFiles {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn buffer(&self) -> &Path {
        &self.buffer
    }
    pub fn baseline(&self) -> &Path {
        &self.baseline
    }
    /// Call only when the caller no longer needs draft recovery.
    pub fn discard(self) -> io::Result<()> {
        fs::remove_dir_all(self.directory)
    }
}

#[derive(Debug)]
pub struct ChangedBuffer {
    target: LocalTarget,
    source: String,
    recovery: RecoveryFiles,
}
impl ChangedBuffer {
    pub fn target(&self) -> &LocalTarget {
        &self.target
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn recovery(&self) -> &RecoveryFiles {
        &self.recovery
    }
    pub fn into_parts(self) -> (LocalTarget, String, RecoveryFiles) {
        (self.target, self.source, self.recovery)
    }
}

#[derive(Debug)]
pub enum EditorOutcome {
    Changed(ChangedBuffer),
    Unchanged(RecoveryFiles),
    Cancelled(RecoveryFiles),
}

#[derive(Debug)]
pub struct EditorError {
    code: &'static str,
    message: String,
    recovery: Option<RecoveryFiles>,
}
impl EditorError {
    fn configuration(message: &str) -> Self {
        Self {
            code: "editor_configuration",
            message: message.into(),
            recovery: None,
        }
    }
    fn failed(code: &'static str, message: impl Into<String>, recovery: &RecoveryFiles) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: Some(recovery.clone()),
        }
    }
    pub fn code(&self) -> &'static str {
        self.code
    }
    pub fn recovery(&self) -> Option<&RecoveryFiles> {
        self.recovery.as_ref()
    }
}
impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(files) = &self.recovery {
            write!(
                f,
                "; draft: {}; baseline: {}",
                files.buffer.display(),
                files.baseline.display()
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for EditorError {}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn workspace(root: &Path, format: Format) -> Result<RecoveryFiles, EditorError> {
    #[cfg(not(unix))]
    {
        let _ = (root, format);
        Err(EditorError::configuration(
            "private editor sessions currently require Unix",
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let root = fs::canonicalize(root).map_err(|e| EditorError {
            code: "editor_workspace",
            message: format!("choose an existing recovery directory: {e}"),
            recovery: None,
        })?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..64 {
            let directory = root.join(format!(
                "editac-editor-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    let extension = match format {
                        Format::Markdown => "md",
                        Format::Text => "txt",
                    };
                    return Ok(RecoveryFiles {
                        buffer: directory.join(format!("source.{extension}")),
                        baseline: directory.join(format!("baseline.{extension}")),
                        directory,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(EditorError {
                        code: "editor_workspace",
                        message: format!("cannot create private recovery directory: {e}"),
                        recovery: None,
                    });
                }
            }
        }
        Err(EditorError::configuration(
            "cannot allocate a unique editor workspace",
        ))
    }
}

fn write_private(path: &Path, source: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(source)?;
    file.sync_all()
}

/// Run synchronously against a private copy. Never write the original target or
/// attach an engine, account, transport or credential store. The returned changed
/// source still needs shared reconciliation and stale-target save validation.
pub fn run(
    target: &LocalTarget,
    editor: &EditorCommand,
    recovery_root: impl AsRef<Path>,
) -> Result<EditorOutcome, EditorError> {
    let recovery = workspace(recovery_root.as_ref(), target.format())?;
    // Baseline first: even a subsequent working-copy write failure retains it.
    write_private(&recovery.baseline, target.source_bytes())
        .and_then(|()| write_private(&recovery.buffer, target.source_bytes()))
        .map_err(|e| {
            EditorError::failed(
                "editor_workspace",
                format!("cannot prepare private source files: {e}"),
                &recovery,
            )
        })?;
    let status = Command::new(&editor.executable)
        .args(&editor.arguments)
        .arg(&recovery.buffer)
        .status()
        .map_err(|e| {
            EditorError::failed(
                "editor_launch",
                format!("cannot launch configured editor; check its executable and arguments: {e}"),
                &recovery,
            )
        })?;
    #[cfg(unix)]
    let interrupted = {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(2)
    };
    #[cfg(not(unix))]
    let interrupted = false;
    if status.code() == Some(130) || interrupted {
        return Ok(EditorOutcome::Cancelled(recovery));
    }
    if !status.success() {
        return Err(EditorError::failed(
            "editor_exit",
            format!("editor failed ({status}); recover the draft before retrying"),
            &recovery,
        ));
    }
    let bytes = read_buffer(&recovery.buffer).map_err(|e| {
        EditorError::failed(
            "editor_buffer",
            format!("cannot accept editor buffer: {e}"),
            &recovery,
        )
    })?;
    if bytes == target.source_bytes() {
        return Ok(EditorOutcome::Unchanged(recovery));
    }
    let source = String::from_utf8(bytes).map_err(|_| {
        EditorError::failed(
            "editor_utf8",
            "editor buffer is not UTF-8; original is unchanged",
            &recovery,
        )
    })?;
    Ok(EditorOutcome::Changed(ChangedBuffer {
        target: target.clone(),
        source,
        recovery,
    }))
}

fn read_buffer(path: &Path) -> io::Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() {
        return Err(io::Error::other(
            "working buffer must remain a regular file, not a symlink",
        ));
    }
    let file = File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(io::Error::other(
                "working buffer was replaced while opening",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(io::Error::other(
            "working buffer exceeds the 1 MiB source limit",
        ));
    }
    Ok(bytes)
}
