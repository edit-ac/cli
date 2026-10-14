//! Exact source is temporary input. Only proved semantic deltas reach the replica.
use crate::{
    FragmentBlock, FragmentRun,
    export::equivalent,
    import::PreparedContent,
    optional::{Codecs, import_with_codecs},
};
use editac_core::{
    Utf16Offset,
    anchors::{Bias, Resolution, Selection, utf16_to_byte},
    commands::{
        editor::{EditorCommit, PreparedEditorEdit},
        format::Mark,
        text::TextOrigin,
    },
    document::{Document, InitializedCheckpoint},
    projection::ProjectedNode,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
pub enum SourceMode {
    Markdown(Codecs),
    Literal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    AmbiguousMapping,
    UnsupportedContent,
    Overlap,
    DeletedTarget,
    MovedTarget,
    ResourceLimit,
    IdentityMismatch,
    StalePlan,
    EngineRejected,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub source: Option<String>,
}
fn conflict(kind: ConflictKind, target: Option<&str>) -> Conflict {
    Conflict {
        kind,
        target: target.map(str::to_owned),
        detail: None,
        source: None,
    }
}
fn parse(source: &str, mode: SourceMode) -> Result<PreparedContent, Conflict> {
    if source.len() > 1_048_576 {
        return Err(conflict(ConflictKind::ResourceLimit, None));
    }
    if matches!(mode, SourceMode::Literal) {
        return Ok(PreparedContent {
            source: source.into(),
            blocks: vec![FragmentBlock {
                runs: vec![FragmentRun {
                    text: source.into(),
                    marks: vec![],
                }],
                attributes: Some(editac_core::document::BlockAttributes::Paragraph),
                containers: vec![],
                checked: None,
            }],
            ranges: vec![0..source.len()],
            diagnostics: vec![],
        });
    }
    let SourceMode::Markdown(codecs) = mode else {
        unreachable!()
    };
    let mut result = import_with_codecs(source, codecs)
        .map_err(|_| conflict(ConflictKind::ResourceLimit, None))?;
    if result.blocks.is_empty() {
        result.blocks.push(FragmentBlock {
            runs: vec![],
            attributes: Some(editac_core::document::BlockAttributes::Paragraph),
            containers: vec![],
            checked: None,
        });
        result.ranges.push(0..source.len());
    }
    Ok(result)
}
type Cell<'a> = (char, &'a [(Mark, Option<String>)]);
fn cells(block: &FragmentBlock) -> Vec<Cell<'_>> {
    block
        .runs
        .iter()
        .flat_map(|r| r.text.chars().map(move |c| (c, r.marks.as_slice())))
        .collect()
}
fn same(a: &Cell<'_>, b: &Cell<'_>) -> bool {
    a.0 == b.0 && a.1.iter().all(|m| b.1.contains(m)) && b.1.iter().all(|m| a.1.contains(m))
}
fn slice(block: &FragmentBlock, start: u32, end: u32) -> Result<Vec<FragmentRun>, Conflict> {
    let mut offset = 0;
    let mut result = Vec::new();
    for run in &block.runs {
        let len = run.text.encode_utf16().count() as u32;
        let from = start.saturating_sub(offset).min(len);
        let to = end.saturating_sub(offset).min(len);
        if from < to {
            let a = utf16_to_byte(&run.text, Utf16Offset(from))
                .map_err(|_| conflict(ConflictKind::AmbiguousMapping, None))?;
            let b = utf16_to_byte(&run.text, Utf16Offset(to))
                .map_err(|_| conflict(ConflictKind::AmbiguousMapping, None))?;
            result.push(FragmentRun {
                text: run.text[a..b].into(),
                marks: run.marks.clone(),
            });
        }
        offset += len;
    }
    Ok(result)
}
struct Binding {
    node: ProjectedNode,
    supported: bool,
}
pub struct SourceBaseline {
    source: String,
    mode: SourceMode,
    checkpoint: InitializedCheckpoint,
    parsed: PreparedContent,
    bindings: Vec<Binding>,
}
pub struct PreparedSourceEdit {
    pub source: String,
    pub changed: usize,
    plan: Option<PreparedEditorEdit>,
    identity: editac_core::document::DocumentIdentity,
}
pub struct SourceCommit {
    pub source: String,
    pub commit: Option<EditorCommit>,
}
impl PreparedSourceEdit {
    pub fn commit(self, document: &mut Document) -> Result<SourceCommit, Conflict> {
        let error = if document.identity() != &self.identity {
            Some(conflict(ConflictKind::IdentityMismatch, None))
        } else {
            None
        };
        if let Some(mut error) = error {
            error.source = Some(self.source);
            return Err(error);
        }
        let result = self
            .plan
            .map(|plan| document.commit_editor(plan))
            .transpose();
        match result {
            Ok(commit) => Ok(SourceCommit {
                source: self.source,
                commit,
            }),
            Err(error) => {
                let kind = if matches!(
                    error,
                    editac_core::commands::editor::EditorError::Text(
                        editac_core::commands::text::TextError::StalePlan
                    )
                ) {
                    ConflictKind::StalePlan
                } else {
                    ConflictKind::EngineRejected
                };
                let mut result = conflict(kind, None);
                result.detail = Some(format!("{error:?}").chars().take(512).collect());
                result.source = Some(self.source);
                Err(result)
            }
        }
    }
}
impl SourceBaseline {
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Hosts supply the ordered owner IDs of their opening source representation.
    pub fn capture(
        document: &Document,
        source: &str,
        mode: SourceMode,
        owners: &[String],
    ) -> Result<Self, Conflict> {
        let parsed = parse(source, mode)?;
        if owners.len() != parsed.blocks.len()
            || owners.len() > 128
            || parsed.ranges.len() != parsed.blocks.len()
        {
            return Err(conflict(ConflictKind::AmbiguousMapping, None));
        }
        let snapshot = document
            .snapshot()
            .map_err(|_| conflict(ConflictKind::EngineRejected, None))?;
        let mut seen = BTreeSet::new();
        let mut last = None;
        let mut bindings = Vec::new();
        for (index, owner) in owners.iter().enumerate() {
            let Some(position) = snapshot
                .visible
                .iter()
                .position(|n| n.id == *owner && n.kind == "block")
            else {
                return Err(conflict(ConflictKind::DeletedTarget, Some(owner)));
            };
            if !seen.insert(owner) || last.is_some_and(|last| position <= last) {
                return Err(conflict(ConflictKind::AmbiguousMapping, Some(owner)));
            }
            last = Some(position);
            let range = &parsed.ranges[index];
            if range.end > source.len()
                || range.start > range.end
                || !source.is_char_boundary(range.start)
                || !source.is_char_boundary(range.end)
            {
                return Err(conflict(ConflictKind::AmbiguousMapping, Some(owner)));
            }
            let supported = matches!(
                snapshot.visible[position].block_attributes,
                Some(
                    editac_core::document::BlockAttributes::Paragraph
                        | editac_core::document::BlockAttributes::Heading(_)
                        | editac_core::document::BlockAttributes::Code(_)
                )
            ) && document
                .fragment_for_node(owner)
                .is_ok_and(|block| equivalent(&[block], &[parsed.blocks[index].clone()]))
                && !parsed
                    .diagnostics
                    .iter()
                    .any(|d| d.range.start < range.end && range.start < d.range.end);
            bindings.push(Binding {
                node: snapshot.visible[position].as_ref().clone(),
                supported,
            });
        }
        Ok(Self {
            source: source.into(),
            mode,
            checkpoint: document.checkpoint(),
            parsed,
            bindings,
        })
    }
    pub fn prepare(
        &self,
        document: &Document,
        edited: &str,
        origin: &TextOrigin,
    ) -> Result<PreparedSourceEdit, Conflict> {
        self.prepare_inner(document, edited, origin)
            .map_err(|mut error| {
                if edited.len() <= 1_048_576 {
                    error.source = Some(edited.into());
                }
                error
            })
    }
    fn prepare_inner(
        &self,
        document: &Document,
        edited: &str,
        origin: &TextOrigin,
    ) -> Result<PreparedSourceEdit, Conflict> {
        if document.identity() != &self.checkpoint.identity {
            return Err(conflict(ConflictKind::IdentityMismatch, None));
        }
        if edited == self.source {
            return Ok(PreparedSourceEdit {
                source: edited.into(),
                changed: 0,
                plan: None,
                identity: document.identity().clone(),
            });
        }
        let candidate = parse(edited, self.mode)?;
        if candidate.blocks.len() != self.parsed.blocks.len() {
            return Err(conflict(ConflictKind::AmbiguousMapping, None));
        }
        let current = document
            .snapshot()
            .map_err(|_| conflict(ConflictKind::EngineRejected, None))?;
        let opening = Document::restore(&self.checkpoint.identity, &self.checkpoint, &[])
            .map_err(|_| conflict(ConflictKind::EngineRejected, None))?;
        let mut edits = Vec::new();
        for (index, (old, new)) in self.parsed.blocks.iter().zip(&candidate.blocks).enumerate() {
            if equivalent(&[old.clone()], &[new.clone()]) {
                continue;
            }
            let binding = &self.bindings[index];
            let owner = &binding.node.id;
            if !binding.supported
                || candidate.diagnostics.iter().any(|d| {
                    d.range.start < candidate.ranges[index].end
                        && candidate.ranges[index].start < d.range.end
                })
            {
                return Err(conflict(ConflictKind::UnsupportedContent, Some(owner)));
            }
            if old
                .containers
                .iter()
                .map(|c| &c.1)
                .ne(new.containers.iter().map(|c| &c.1))
            {
                return Err(conflict(ConflictKind::UnsupportedContent, Some(owner)));
            }
            let Some(position) = current.visible.iter().position(|n| n.id == *owner) else {
                return Err(conflict(ConflictKind::DeletedTarget, Some(owner)));
            };
            let now = &current.visible[position];
            if now.parent != binding.node.parent
                || now.editor.as_ref().map(|e| &e.version)
                    != binding.node.editor.as_ref().map(|e| &e.version)
            {
                return Err(conflict(ConflictKind::MovedTarget, Some(owner)));
            }
            for (other_index, other) in self.bindings.iter().enumerate() {
                if other_index == index {
                    continue;
                }
                if let Some(other_position) =
                    current.visible.iter().position(|n| n.id == other.node.id)
                {
                    if (other_index < index) != (other_position < position) {
                        return Err(conflict(ConflictKind::MovedTarget, Some(owner)));
                    }
                }
            }
            let current_block = document
                .fragment_for_node(owner)
                .map_err(|_| conflict(ConflictKind::UnsupportedContent, Some(owner)))?;
            if current_block.attributes != old.attributes
                || current_block.checked != old.checked
                || current_block
                    .containers
                    .iter()
                    .map(|c| &c.1)
                    .ne(old.containers.iter().map(|c| &c.1))
            {
                return Err(conflict(ConflictKind::Overlap, Some(owner)));
            }
            let old_cells = cells(old);
            let new_cells = cells(new);
            let mut prefix = old_cells
                .iter()
                .zip(&new_cells)
                .take_while(|(a, b)| same(a, b))
                .count();
            let mut suffix = old_cells[prefix..]
                .iter()
                .rev()
                .zip(new_cells[prefix..].iter().rev())
                .take_while(|(a, b)| same(a, b))
                .count();
            if old.attributes != new.attributes || old.checked != new.checked {
                prefix = 0;
                suffix = 0;
            }
            let units =
                |cells: &[Cell<'_>]| cells.iter().map(|c| c.0.len_utf16() as u32).sum::<u32>();
            let start = units(&old_cells[..prefix]);
            let end = units(&old_cells[..old_cells.len() - suffix]);
            let new_end = units(&new_cells[..new_cells.len() - suffix]);
            let selection = Selection {
                anchor: opening
                    .anchor(owner, Utf16Offset(start), Bias::Right)
                    .map_err(|_| conflict(ConflictKind::DeletedTarget, Some(owner)))?,
                focus: opening
                    .anchor(owner, Utf16Offset(end), Bias::Left)
                    .map_err(|_| conflict(ConflictKind::DeletedTarget, Some(owner)))?,
            };
            let (a, b) = match (
                document.resolve_anchor(&selection.anchor),
                document.resolve_anchor(&selection.focus),
            ) {
                (Ok(Resolution::Position(a)), Ok(Resolution::Position(b)))
                    if a.node == *owner && b.node == *owner && a.offset.0 <= b.offset.0 =>
                {
                    (a, b)
                }
                _ => return Err(conflict(ConflictKind::DeletedTarget, Some(owner))),
            };
            let expected = slice(old, start, end)?;
            let observed = slice(&current_block, a.offset.0, b.offset.0)?;
            let compare = |runs| FragmentBlock {
                runs,
                attributes: None,
                containers: vec![],
                checked: None,
            };
            if !equivalent(&[compare(expected)], &[compare(observed)]) {
                return Err(conflict(ConflictKind::Overlap, Some(owner)));
            }
            // Only the changed span receives fresh ownership; untouched source spans remain intact.
            let block = FragmentBlock {
                runs: slice(new, start, new_end)?,
                attributes: new.attributes.clone(),
                containers: vec![],
                checked: new.checked,
            };
            edits.push((selection, vec![block]));
        }
        if edits.is_empty() {
            return Ok(PreparedSourceEdit {
                source: edited.into(),
                changed: 0,
                plan: None,
                identity: document.identity().clone(),
            });
        }
        let changed = edits.len();
        let plan = document
            .prepare_source_edits(&edits, origin)
            .map_err(|error| {
                let mut result = conflict(ConflictKind::EngineRejected, None);
                result.detail = Some(format!("{error:?}").chars().take(512).collect());
                result
            })?;
        Ok(PreparedSourceEdit {
            source: edited.into(),
            changed,
            plan: Some(plan),
            identity: document.identity().clone(),
        })
    }
}

/// Review fragments must explicitly close fenced code, even though CommonMark
/// permits an implicit EOF close. Use grammar events so indented code and nested
/// quote/list fences retain their actual interpretation. This validates syntax
/// only; source reconciliation still proves semantic mapping and identity.
pub fn review_syntax_is_complete(source: &str) -> bool {
    use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};
    if source.len() > 1_048_576 {
        return false;
    }
    let mut fenced: Option<(std::ops::Range<usize>, usize)> = None;
    for (event, span) in crate::profile::events(source) {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                fenced = Some((span.clone(), span.start));
            }
            Event::Text(_) if fenced.is_some() => {
                if let Some((_, end)) = &mut fenced {
                    *end = (*end).max(span.end);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((range, content_end)) = fenced.take() {
                    let raw = &source[range.clone()];
                    let trimmed = raw.trim_end_matches(['\n', '\r']);
                    let Some(last_start) = trimmed.rfind('\n').map(|i| range.start + i + 1) else {
                        return false;
                    };
                    // An actual closing delimiter is excluded from code text.
                    // EOF and a merely fence-like literal final line are not.
                    if content_end > last_start {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    fenced.is_none()
}
