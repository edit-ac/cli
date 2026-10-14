//! Local source is a temporary projection. Preparation never mutates the replica,
//! and successful reconciliation is not a saved-file or durability acknowledgment.
//! The qualified profile retains block count/container topology; unsupported
//! changes return the caller's buffer and private recovery files unchanged.
use crate::{
    editor::ChangedBuffer,
    engine::{
        Utf16Offset,
        anchors::{Bias, Selection},
        commands::{editor::EditorCommit, text::TextOrigin},
        document::{CreationPermit, Document, InitialNode},
        history::HistoryConfig,
    },
    markdown::{
        FragmentBlock, FragmentRun,
        import::import,
        optional::Codecs,
        reconcile::{Conflict, ConflictKind, PreparedSourceEdit, SourceBaseline, SourceMode},
    },
    target::{Format, LocalTarget},
};

#[derive(Debug)]
pub enum ReconcileError {
    Shared(Conflict),
    /// Opening import would discard unsupported source semantics.
    UnsupportedOpening,
    TargetMismatch,
    Engine(String),
}
impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shared(c) => write!(
                f,
                "source reconciliation rejected: {:?}; retain the draft",
                c.kind
            ),
            Self::UnsupportedOpening => f.write_str(
                "unsupported opening source; keep the original and use an explicit conversion",
            ),
            Self::TargetMismatch => f.write_str(
                "editor buffer belongs to a different opening target; retain its recovery files",
            ),
            Self::Engine(e) => write!(f, "cannot initialize local reconciliation: {e}"),
        }
    }
}
impl std::error::Error for ReconcileError {}
fn engine(error: impl std::fmt::Debug) -> ReconcileError {
    ReconcileError::Engine(format!("{error:?}"))
}
fn mode(format: Format) -> SourceMode {
    match format {
        Format::Markdown => SourceMode::Markdown(Codecs::default()),
        Format::Text => SourceMode::Literal,
    }
}
fn same_target(a: &LocalTarget, b: &LocalTarget) -> bool {
    a.requested_path() == b.requested_path()
        && a.resolved_path() == b.resolved_path()
        && a.format() == b.format()
        && a.source_bytes() == b.source_bytes()
        && a.baseline() == b.baseline()
}

pub struct LocalBaseline {
    target: LocalTarget,
    baseline: SourceBaseline,
}
impl LocalBaseline {
    /// Import a local file once into a fresh engine document. The caller supplies
    /// the authorized new identity; no transport or credential store is accessed.
    pub fn import(
        target: LocalTarget,
        permit: CreationPermit,
    ) -> Result<(Document, Self), ReconcileError> {
        let mut blocks = match target.format() {
            Format::Markdown => {
                let parsed = import(target.source()).map_err(|error| match error {
                    crate::markdown::import::ImportError::ResourceLimit => {
                        ReconcileError::Shared(Conflict {
                            kind: ConflictKind::ResourceLimit,
                            target: None,
                            detail: Some(
                                "opening Markdown exceeds the shared parser limits".into(),
                            ),
                            source: None,
                        })
                    }
                    crate::markdown::import::ImportError::InvalidStructure => {
                        ReconcileError::UnsupportedOpening
                    }
                })?;
                if !parsed.diagnostics.is_empty() {
                    return Err(ReconcileError::UnsupportedOpening);
                }
                parsed.blocks
            }
            Format::Text => vec![FragmentBlock {
                runs: vec![FragmentRun {
                    text: target.source().into(),
                    marks: vec![],
                }],
                attributes: Some(crate::engine::document::BlockAttributes::Paragraph),
                containers: vec![],
                checked: None,
            }],
        };
        if blocks.is_empty() {
            blocks.push(FragmentBlock {
                runs: vec![],
                attributes: Some(crate::engine::document::BlockAttributes::Paragraph),
                containers: vec![],
                checked: None,
            });
        }
        // The qualified source mapper admits at most 128 bound blocks. Reject
        // before allocating committed identity/state in the engine.
        if blocks.len() > 128 {
            return Err(ReconcileError::Shared(Conflict {
                kind: ConflictKind::ResourceLimit,
                target: None,
                detail: Some("local source reconciliation supports at most 128 blocks".into()),
                source: None,
            }));
        }
        let mut document = Document::create(
            permit,
            &[InitialNode {
                kind: "block".into(),
                text: String::new(),
            }],
        )
        .map_err(engine)?;
        document
            .enable_history(HistoryConfig::default())
            .map_err(engine)?;
        let first = document.snapshot().map_err(engine)?.visible[0].id.clone();
        let selection = Selection {
            anchor: document
                .anchor(&first, Utf16Offset(0), Bias::Right)
                .map_err(engine)?,
            focus: document
                .anchor(&first, Utf16Offset(0), Bias::Left)
                .map_err(engine)?,
        };
        let origin = TextOrigin::new("cli:local-import").map_err(engine)?;
        let prepared = document
            .prepare_fragment(&selection, &blocks, &origin)
            .map_err(engine)?;
        document.commit_editor(prepared).map_err(engine)?;
        document.clear_history();
        // IDs come from this exact fresh import, never from source offsets alone.
        let owners = document
            .snapshot()
            .map_err(engine)?
            .visible
            .iter()
            .filter(|n| n.kind == "block")
            .map(|n| n.id.clone())
            .collect::<Vec<_>>();
        let baseline = Self::capture(&document, target, &owners)?;
        Ok((document, baseline))
    }

    /// Bind an existing opening replica using explicit ordered semantic owners.
    /// This preserves opaque engine data; it never reimports the replica.
    pub fn capture(
        document: &Document,
        target: LocalTarget,
        owners: &[String],
    ) -> Result<Self, ReconcileError> {
        let baseline =
            SourceBaseline::capture(document, target.source(), mode(target.format()), owners)
                .map_err(ReconcileError::Shared)?;
        Ok(Self { target, baseline })
    }
    /// Bind recovered committed spelling while retaining the current guarded file target.
    pub fn capture_recovered(
        document: &Document,
        target: LocalTarget,
        source: &str,
        owners: &[String],
    ) -> Result<Self, ReconcileError> {
        let baseline = SourceBaseline::capture(document, source, mode(target.format()), owners)
            .map_err(ReconcileError::Shared)?;
        Ok(Self { target, baseline })
    }
    pub fn target(&self) -> &LocalTarget {
        &self.target
    }
    pub fn source(&self) -> &str {
        self.baseline.source()
    }

    pub fn prepare(
        &self,
        document: &Document,
        buffer: ChangedBuffer,
    ) -> Result<PreparedLocalEdit, RejectedEdit> {
        let result = if !same_target(&self.target, buffer.target()) {
            Err(ReconcileError::TargetMismatch)
        } else {
            self.baseline
                .prepare(
                    document,
                    buffer.source(),
                    &TextOrigin::new("cli:source-edit").expect("static origin"),
                )
                .map_err(ReconcileError::Shared)
        };
        match result {
            Ok(plan) => Ok(PreparedLocalEdit { plan, buffer }),
            Err(error) => Err(RejectedEdit { error, buffer }),
        }
    }
}

/// Every failure keeps the original target, exact draft and recovery paths.
#[derive(Debug)]
pub struct RejectedEdit {
    pub error: ReconcileError,
    pub buffer: ChangedBuffer,
}
impl std::fmt::Display for RejectedEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; draft: {}",
            self.error,
            self.buffer.recovery().buffer().display()
        )
    }
}
impl std::error::Error for RejectedEdit {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub struct PreparedLocalEdit {
    plan: PreparedSourceEdit,
    buffer: ChangedBuffer,
}
impl PreparedLocalEdit {
    pub fn changed_blocks(&self) -> usize {
        self.plan.changed
    }
    /// One guarded shared history operation. The caller must still run the
    /// separate stale-file/durable publication boundary before reporting saved.
    pub fn commit(self, document: &mut Document) -> Result<ReconciledEdit, RejectedEdit> {
        match self.plan.commit(document) {
            Ok(result) => Ok(ReconciledEdit {
                buffer: self.buffer,
                commit: result.commit,
            }),
            Err(error) => Err(RejectedEdit {
                error: ReconcileError::Shared(error),
                buffer: self.buffer,
            }),
        }
    }
}
pub struct ReconciledEdit {
    /// Exact edited spelling, original target baseline and private recovery files.
    pub buffer: ChangedBuffer,
    /// None for a semantic no-op (including spelling-only edits).
    pub commit: Option<EditorCommit>,
}

/// A deterministic hosted projection plus complete retained CRDT evidence.
/// Markdown alone cannot represent unknown attributes, marks or module data.
#[derive(Clone, Debug)]
pub struct HostedProjection {
    pub source: String,
    pub checkpoint: crate::engine::document::InitializedCheckpoint,
    pub snapshot: crate::engine::projection::Snapshot,
    pub diagnostics: Vec<crate::markdown::import::Diagnostic>,
}
impl HostedProjection {
    pub fn capture(document: &Document) -> Result<Self, ReconcileError> {
        let snapshot = document.snapshot().map_err(engine)?;
        let mut diagnostics = Vec::new();
        let mut blocks = Vec::new();
        for node in snapshot.visible.iter().filter(|n| n.kind == "block") {
            match document.fragment_for_node(&node.id) {
                Ok(block) => blocks.push(block),
                Err(_) => {
                    // An inert text projection for an opaque block. The shared
                    // mapper marks this binding unsupported; editing it fails.
                    diagnostics.push(crate::markdown::import::Diagnostic {
                        code: "opaque-hosted-block",
                        range: 0..0,
                    });
                    blocks.push(FragmentBlock {
                        runs: node
                            .runs
                            .iter()
                            .map(|r| FragmentRun {
                                text: r.text.clone(),
                                marks: vec![],
                            })
                            .collect(),
                        attributes: Some(crate::engine::document::BlockAttributes::Paragraph),
                        containers: vec![],
                        checked: None,
                    });
                }
            }
        }
        let exported = crate::markdown::export::export(&blocks);
        diagnostics.extend(exported.diagnostics);
        Ok(Self {
            source: exported.markdown,
            checkpoint: document.checkpoint(),
            snapshot,
            diagnostics,
        })
    }
}

/// Opening source/semantic bindings stay fixed while remote updates advance
/// the existing replica. No hosted save ever imports a replacement document.
pub struct HostedBaseline {
    opening: HostedProjection,
    baseline: SourceBaseline,
}
impl HostedBaseline {
    pub fn capture(document: &Document) -> Result<Self, ReconcileError> {
        let opening = HostedProjection::capture(document)?;
        let owners = opening
            .snapshot
            .visible
            .iter()
            .filter(|n| n.kind == "block")
            .map(|n| n.id.clone())
            .collect::<Vec<_>>();
        let baseline = SourceBaseline::capture(
            document,
            &opening.source,
            SourceMode::Markdown(Codecs::default()),
            &owners,
        )
        .map_err(ReconcileError::Shared)?;
        Ok(Self { opening, baseline })
    }
    pub fn opening(&self) -> &HostedProjection {
        &self.opening
    }
    pub fn prepare(
        &self,
        document: &Document,
        source: &str,
    ) -> Result<PreparedHostedEdit, HostedConflict> {
        match self.baseline.prepare(
            document,
            source,
            &TextOrigin::new("cli:hosted-source-edit").expect("static origin"),
        ) {
            Ok(plan) => Ok(PreparedHostedEdit {
                plan,
                opening: self.opening.clone(),
                source: source.into(),
            }),
            Err(error) => Err(HostedConflict::capture(
                ReconcileError::Shared(error),
                self.opening.clone(),
                source.into(),
                document,
            )),
        }
    }
}

/// Every conflict retains exact base/local source and complete current state,
/// including unsupported semantics. Projection failure never loses the draft.
#[derive(Debug)]
pub struct HostedConflict {
    pub error: ReconcileError,
    pub base: HostedProjection,
    pub local: String,
    pub current: Result<HostedProjection, ReconcileError>,
    pub current_checkpoint: crate::engine::document::InitializedCheckpoint,
}
impl HostedConflict {
    fn capture(
        error: ReconcileError,
        base: HostedProjection,
        local: String,
        document: &Document,
    ) -> Self {
        Self {
            error,
            base,
            local,
            current: HostedProjection::capture(document),
            current_checkpoint: document.checkpoint(),
        }
    }
}
impl std::fmt::Display for HostedConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; base/local/current retained", self.error)
    }
}
impl std::error::Error for HostedConflict {}

pub struct PreparedHostedEdit {
    plan: PreparedSourceEdit,
    opening: HostedProjection,
    source: String,
}
impl PreparedHostedEdit {
    pub fn changed_blocks(&self) -> usize {
        self.plan.changed
    }
    /// Commit one guarded history operation. Native durability and upload are
    /// separate boundaries; the caller retains the draft until they succeed.
    pub fn commit(
        self,
        document: &mut Document,
    ) -> Result<crate::markdown::reconcile::SourceCommit, HostedConflict> {
        self.plan.commit(document).map_err(|error| {
            HostedConflict::capture(
                ReconcileError::Shared(error),
                self.opening,
                self.source,
                document,
            )
        })
    }
}
