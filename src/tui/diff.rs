//! Bounded presentation of the qualified diff model. This is never a patch engine.
use super::session::{Representation, TerminalSession};
use editac_diff::{
    portable::{ChangeInput, ChangeModel},
    representation::{
        ContentHash, ImmutableRevision, RepresentationLimits, RevisionInput, RevisionScope,
    },
    semantic::capture_projection,
    text::{LineKind, PatchLimits, TextPatch},
};
use editac_session::{revision::ObservedRevision, session::SessionStore};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_BYTES: usize = 1_048_576;
const MAX_REVIEW_BYTES: usize = 4 * MAX_BYTES;
const SERIALIZER: &str = "editac-markdown-export-v1";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewError {
    Unavailable,
    Limit,
    Stale,
    Export,
}
#[derive(Clone)]
pub struct Review {
    expected: ObservedRevision,
    candidate: ContentHash,
    model: ChangeModel,
    rows: Vec<String>,
    top: usize,
    pub label: String,
    pub base_label: String,
    pub proposed_label: String,
    pub limitation: String,
}
// Never expose document bytes in diagnostics.
impl std::fmt::Debug for Review {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Review")
            .field("rows", &self.rows.len())
            .field("top", &self.top)
            .finish()
    }
}
fn scope<S: SessionStore>(
    session: &TerminalSession<S>,
    revision: &ObservedRevision,
) -> RevisionScope {
    let id = session.service.session().document().identity();
    RevisionScope {
        document: id.document_id.clone(),
        epoch: id.epoch.clone(),
        schema: id.schema.clone(),
        module_profile: id.profile.clone(),
        observed_token: revision.to_bytes(),
    }
}
fn token_label(revision: &ObservedRevision) -> String {
    ContentHash::of(&revision.to_bytes()).hex()
}
fn limits() -> RepresentationLimits {
    RepresentationLimits {
        max_bytes: MAX_BYTES,
        ..Default::default()
    }
}
fn patch_limits() -> PatchLimits {
    PatchLimits {
        max_source_bytes: MAX_BYTES,
        max_output_bytes: MAX_BYTES,
        max_wire_bytes: MAX_REVIEW_BYTES,
        max_lines: 10_000,
        max_hunks: 1024,
        ..Default::default()
    }
}
impl Review {
    pub fn capture<S: SessionStore>(
        session: &TerminalSession<S>,
        baseline: &str,
        expected: &ObservedRevision,
        candidate: &str,
        now: u64,
    ) -> Result<Self, ReviewError> {
        let read = session
            .service
            .read(now)
            .map_err(|_| ReviewError::Unavailable)?;
        if &read.revision != expected {
            return Err(ReviewError::Stale);
        }
        let scope = scope(session, &read.revision);
        // TextPatch compares bytes; the portable envelope records their actual provenance.
        let before = ImmutableRevision::capture(
            scope.clone(),
            RevisionInput::ExactSource(baseline.as_bytes()),
            limits(),
        )
        .map_err(|_| ReviewError::Limit)?;
        let after = ImmutableRevision::capture(
            scope,
            RevisionInput::ExactSource(candidate.as_bytes()),
            limits(),
        )
        .map_err(|_| ReviewError::Limit)?;
        let patch =
            TextPatch::between(&before, &after, patch_limits()).map_err(|_| ReviewError::Limit)?;
        let wire = patch
            .encode(patch_limits())
            .map_err(|_| ReviewError::Limit)?;
        let (input, label) = match session.representation {
            Representation::ExactSource => (
                ChangeInput::ExactSource { wire },
                "Exact source (UTF-8 bytes)".to_string(),
            ),
            Representation::HostedProjection => (
                ChangeInput::ProjectedMarkdown {
                    wire,
                    serializer: SERIALIZER.into(),
                    dialect: crate::markdown::profile::PROFILE.into(),
                },
                format!(
                    "Hosted projected Markdown | {SERIALIZER} | {} | spelling may normalize",
                    crate::markdown::profile::PROFILE
                ),
            ),
        };
        let model = ChangeModel::new(input).map_err(|_| ReviewError::Unavailable)?;
        let mut rows = Vec::new();
        for hunk in &patch.hunks {
            rows.push(format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ));
            for line in &hunk.lines {
                let marker = match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Delete => '-',
                    LineKind::Insert => '+',
                };
                // Keep CR visible and missing final newline explicit; escape at drawing boundary.
                rows.push(format!(
                    "{marker}{}",
                    line.text.strip_suffix('\n').unwrap_or(&line.text)
                ));
                if !line.text.ends_with('\n') {
                    rows.push("\\ No newline at end of source".into());
                }
            }
        }
        if rows.is_empty() {
            rows.push("No source changes".into());
        }
        if rows.iter().map(String::len).sum::<usize>() > MAX_REVIEW_BYTES {
            return Err(ReviewError::Limit);
        }
        Ok(Self {
            expected: read.revision.clone(), candidate: after.hash(), model, rows, top: 0, label,
            base_label: format!("base revision {} | source {}", token_label(&read.revision), before.hash().hex()),
            proposed_label: format!("proposed source {} | uncommitted; based on same revision", after.hash().hex()),
            // Source alone cannot prove preservation of marks, tuples or unknown module content.
            limitation: "Source review omits semantic/opaque data; export retains full recovery checkpoint. Shared preparation may reject unsupported mappings.".into(),
        })
    }
    pub fn from_session<S: SessionStore>(
        session: &TerminalSession<S>,
        candidate: &str,
        now: u64,
    ) -> Result<Self, ReviewError> {
        let revision = session
            .service
            .read(now)
            .map_err(|_| ReviewError::Unavailable)?
            .revision;
        Self::capture(session, &session.source, &revision, candidate, now)
    }
    pub fn validate<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        draft: &str,
        now: u64,
    ) -> Result<(), ReviewError> {
        let revision = session
            .service
            .read(now)
            .map_err(|_| ReviewError::Unavailable)?
            .revision;
        if revision != self.expected || ContentHash::of(draft.as_bytes()) != self.candidate {
            return Err(ReviewError::Stale);
        }
        Ok(())
    }
    pub fn model(&self) -> &ChangeModel {
        &self.model
    }
    pub fn navigate(&mut self, delta: isize) {
        self.top = self
            .top
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
    }
    pub fn page(&self, height: usize) -> impl Iterator<Item = &str> {
        self.rows
            .iter()
            .skip(self.top)
            .take(height.min(512))
            .map(String::as_str)
    }
}
/// Read permission is granted by acquiring this session; readonly denies mutation,
/// not export. No credentials or host profile configuration enter the export.
pub fn copy_source<S: SessionStore>(
    session: &TerminalSession<S>,
    source: &str,
    now: u64,
) -> Result<String, ReviewError> {
    session
        .service
        .read(now)
        .map_err(|_| ReviewError::Unavailable)?;
    if source.len() > MAX_BYTES {
        return Err(ReviewError::Limit);
    }
    Ok(source.into())
}
/// Exact draft plus complete qualified semantic projection and engine checkpoint;
/// never describe an opaque fallback as a lossless Markdown export or save receipt.
pub fn export_bytes<S: SessionStore>(
    session: &TerminalSession<S>,
    source: &str,
    now: u64,
) -> Result<Vec<u8>, ReviewError> {
    let source = copy_source(session, source, now)?;
    let read = session
        .service
        .read(now)
        .map_err(|_| ReviewError::Unavailable)?;
    let semantic = capture_projection(scope(session, &read.revision), &read.snapshot, limits())
        .map_err(|_| ReviewError::Limit)?;
    let recovery = session.service.session().recovery_export();
    if recovery.checkpoint.bytes.len() > MAX_REVIEW_BYTES {
        return Err(ReviewError::Limit);
    }
    let value = serde_json::json!({"version":"editac-tui-recovery-v1", "representation":format!("{:?}", session.representation), "source":source, "revision":token_label(&read.revision), "semantic_encoding":"editac-semantic-json-v1", "semantic":std::str::from_utf8(semantic.bytes()).map_err(|_| ReviewError::Export)?, "identity":{"document_id":recovery.checkpoint.identity.document_id,"epoch":recovery.checkpoint.identity.epoch,"schema":recovery.checkpoint.identity.schema,"profile":recovery.checkpoint.identity.profile,"protocol_version":recovery.checkpoint.identity.protocol_version,"update_encoding":recovery.checkpoint.identity.update_encoding,"offsets":recovery.checkpoint.identity.offsets}, "checkpoint":recovery.checkpoint.bytes, "locally_committed":recovery.locally_committed, "remote_durability":"not asserted"});
    let bytes = serde_json::to_vec(&value).map_err(|_| ReviewError::Export)?;
    if bytes.len() > 32 * MAX_BYTES {
        return Err(ReviewError::Limit);
    }
    Ok(bytes)
}
static NEXT: AtomicU64 = AtomicU64::new(0);
pub fn export_private<S: SessionStore>(
    session: &TerminalSession<S>,
    source: &str,
    now: u64,
    root: &Path,
) -> Result<PathBuf, ReviewError> {
    let bytes = export_bytes(session, source, now)?;
    let path = root.join(format!(
        "editac-tui-export-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| ReviewError::Export)?;
    if file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .is_err()
    {
        let _ = std::fs::remove_file(&path);
        return Err(ReviewError::Export);
    }
    Ok(path)
}
