//! Revision-bound acceptance of exact source hunks through semantic reconciliation.
//! Source buffers are recovery inputs, never a replacement live-document import.
use crate::{
    edit_service::{EditService, PreparedEdit, ServiceError, SourceTarget},
    operations::HostOperation,
    ports::LocalCommit,
    revision::ObservedRevision,
    session::SessionStore,
};
use editac_core::{
    commands::{editor::EditorError, text::TextOrigin},
    projection::{ProjectionError, Snapshot},
};
use editac_diff::text::{LineKind, PatchError, PatchLimits, TextPatch};
use editac_markdown::reconcile::{SourceMode, review_syntax_is_complete};

/// Single opening revision and its proved source-to-owner bindings.
pub struct ReviewTarget {
    source: SourceTarget,
    revision: ObservedRevision,
    mode: SourceMode,
}
impl ReviewTarget {
    pub fn source(&self) -> &str {
        self.source.source()
    }
    pub fn revision(&self) -> &ObservedRevision {
        &self.revision
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Acceptance<'a> {
    All,
    /// Zero-based hunk indexes, strictly increasing. Empty means no changes.
    Hunks(&'a [usize]),
}
pub enum ReviewFailure {
    Patch(PatchError),
    InvalidSelection,
    UnsafeSyntax,
    Service(ServiceError),
}
/// Owned recovery is returned to the authorized caller, not logged implicitly.
/// Current is semantic: it is deliberately not mislabeled as original source.
pub struct ReviewRecovery {
    pub base: String,
    /// Projection failure is explicit; no current source is fabricated.
    pub current: Result<Snapshot, ProjectionError>,
    /// Selected candidate, absent if malformed patch input cannot be materialized.
    pub proposed: Option<String>,
    pub full_proposal: Option<String>,
}
pub struct ReviewError {
    pub failure: ReviewFailure,
    pub recovery: ReviewRecovery,
}
impl std::fmt::Debug for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match &self.failure {
            ReviewFailure::Patch(_) => "patch",
            ReviewFailure::InvalidSelection => "selection",
            ReviewFailure::UnsafeSyntax => "syntax",
            ReviewFailure::Service(_) => "service",
        };
        f.debug_struct("ReviewError")
            .field("reason", &reason)
            .field("base_bytes", &self.recovery.base.len())
            .field(
                "proposed_bytes",
                &self.recovery.proposed.as_ref().map(String::len),
            )
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReviewError {}
/// Opaque and consumed on apply. Neither revision nor semantic plan is rewritable.
pub struct PreparedReview {
    edit: PreparedEdit,
    base: String,
    proposed: String,
    full_proposal: String,
}
impl PreparedReview {
    pub fn preview(&self) -> &Snapshot {
        self.edit.preview()
    }
    pub fn expected_revision(&self) -> &ObservedRevision {
        self.edit.expected_revision()
    }
    pub fn proposed_source(&self) -> &str {
        &self.proposed
    }
}
impl<S: SessionStore> EditService<S> {
    pub fn capture_review(
        &self,
        source: &str,
        mode: SourceMode,
        owners: &[String],
        now: u64,
    ) -> Result<ReviewTarget, ServiceError> {
        let revision = self.read(now)?.revision;
        let source = self.capture_source(source, mode, owners, now)?;
        Ok(ReviewTarget {
            source,
            revision,
            mode,
        })
    }
    /// Validate exact bytes first; recompute a complete semantic plan for selected hunks.
    /// Host patch limits are intersected with current endpoint input limits.
    pub fn prepare_review(
        &self,
        target: &ReviewTarget,
        patch: &TextPatch,
        acceptance: Acceptance<'_>,
        mut limits: PatchLimits,
        origin: &TextOrigin,
        now: u64,
    ) -> Result<PreparedReview, ReviewError> {
        let current = self.session().document().snapshot();
        let mut recovery = ReviewRecovery {
            base: target.source().into(),
            current,
            proposed: None,
            full_proposal: None,
        };
        if let Err(error) = &recovery.current {
            return Err(ReviewError {
                failure: ReviewFailure::Service(ServiceError::Engine(EditorError::Text(
                    error.clone().into(),
                ))),
                recovery,
            });
        }
        let cap = self.capabilities(now).policy.max_input_bytes;
        limits.max_source_bytes = limits.max_source_bytes.min(cap);
        // Source reconciliation budgets base plus selected candidate.
        limits.max_lines = limits.max_lines.min(patch.metadata.limits.max_lines);
        limits.max_output_bytes = limits
            .max_output_bytes
            .min(patch.metadata.limits.max_output_bytes);
        limits.max_output_bytes = limits
            .max_output_bytes
            .min(cap.saturating_sub(target.source().len()));
        let full = match patch.apply(target.source().as_bytes(), limits) {
            Ok(bytes) => String::from_utf8(bytes).expect("validated UTF-8 patch"),
            Err(error) => {
                return Err(ReviewError {
                    failure: ReviewFailure::Patch(error),
                    recovery,
                });
            }
        };
        recovery.full_proposal = Some(full.clone());
        let candidate = match acceptance {
            Acceptance::All => full.clone(),
            Acceptance::Hunks(indexes) => match selected(target.source(), patch, indexes, limits) {
                Ok(source) => source,
                Err(failure) => return Err(ReviewError { failure, recovery }),
            },
        };
        recovery.proposed = Some(candidate.clone());
        // Revision checks precede syntax/reconciliation, including deletion-only changes.
        if let Err(error) = self.session().compare_revision(&target.revision) {
            return Err(ReviewError {
                failure: ReviewFailure::Service(error.into()),
                recovery,
            });
        }
        if matches!(target.mode, SourceMode::Markdown(_) if !review_syntax_is_complete(&candidate))
        {
            return Err(ReviewError {
                failure: ReviewFailure::UnsafeSyntax,
                recovery,
            });
        }
        let edit = self
            .prepare_source(&target.source, &target.revision, &candidate, origin, now)
            .map_err(|error| ReviewError {
                failure: ReviewFailure::Service(error),
                recovery,
            })?;
        Ok(PreparedReview {
            edit,
            base: target.source().into(),
            proposed: candidate,
            full_proposal: full,
        })
    }
    /// Reuses live policy, revision, history and trusted-host provenance admission.
    pub fn apply_review_as(
        &mut self,
        prepared: PreparedReview,
        host: HostOperation,
        now: u64,
    ) -> Result<Option<LocalCommit>, ReviewError> {
        let PreparedReview {
            edit,
            base,
            proposed,
            full_proposal,
        } = prepared;
        self.apply_as(edit, host, now).map_err(|error| ReviewError {
            failure: ReviewFailure::Service(error),
            recovery: ReviewRecovery {
                base,
                current: self.session().document().snapshot(),
                proposed: Some(proposed),
                full_proposal: Some(full_proposal),
            },
        })
    }
}
fn selected(
    source: &str,
    patch: &TextPatch,
    indexes: &[usize],
    limits: PatchLimits,
) -> Result<String, ReviewFailure> {
    if indexes.windows(2).any(|pair| pair[0] >= pair[1])
        || indexes.last().is_some_and(|i| *i >= patch.hunks.len())
    {
        return Err(ReviewFailure::InvalidSelection);
    }
    // The entire patch has already passed range/context/hash checks; original line
    // positions remain authoritative even when earlier hunks were not selected.
    let old: Vec<_> = source.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut cursor = 0;
    let mut append = |text: &str| {
        if !out.is_empty() && !out.ends_with('\n') {
            return Err(ReviewFailure::Patch(PatchError::MalformedEncoding));
        }
        if out.len().saturating_add(text.len()) > limits.max_output_bytes {
            return Err(ReviewFailure::Patch(PatchError::ResourceLimit));
        }
        out.push_str(text);
        Ok(())
    };
    for index in indexes {
        let hunk = &patch.hunks[*index];
        let start = if hunk.old_count == 0 {
            hunk.old_start
        } else {
            hunk.old_start - 1
        };
        for line in &old[cursor..start] {
            append(line)?;
        }
        for line in &hunk.lines {
            if line.kind != LineKind::Delete {
                append(&line.text)?;
            }
        }
        cursor = start + hunk.old_count;
    }
    for line in &old[cursor..] {
        append(line)?;
    }
    if out.split_inclusive('\n').count() > limits.max_lines {
        return Err(ReviewFailure::Patch(PatchError::ResourceLimit));
    }
    Ok(out)
}
