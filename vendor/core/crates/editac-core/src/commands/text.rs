//! Fallible, read-only planning followed by one infallible Yrs mutation phase.
//! Plans retain original shared-text identities and are single-use. Exact encoded
//! state (including the delete set), not a state vector alone, guards staleness.
use crate::Utf16Offset;
use crate::anchors::{self, Anchor, BoundaryError, Position, Resolution, Selection, Unresolvable};
use crate::document::{Document, DocumentIdentity, MAX_RESTORE_BYTES, ROOTS, ReplayUpdate};
use crate::projection::{ProjectionError, Snapshot, TouchedRange};
use std::collections::BTreeSet;
use yrs::{Map, Out, ReadTxn, StateVector, Text, XmlTextRef};

pub const MAX_EDIT_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_TEXT_DEPTH: usize = 64;
pub const MAX_ORIGIN_BYTES: usize = 128;

/// A bounded local operation tag. Hosts distinguish typing, composition, paste,
/// etc. here; history grouping is a separate gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextOrigin(String);
impl TextOrigin {
    pub fn new(tag: &str) -> Result<Self, TextError> {
        if tag.is_empty() || tag.len() > MAX_ORIGIN_BYTES || tag.chars().any(char::is_control) {
            return Err(TextError::InvalidOrigin);
        }
        Ok(Self(format!("editac:local:{tag}")))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Debug)]
pub enum TextCommand {
    Insert { at: Anchor, text: String },
    Delete { selection: Selection },
    Replace { selection: Selection, text: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextError {
    Projection(ProjectionError),
    Boundary(BoundaryError),
    DeletedTarget,
    Unresolvable(Unresolvable),
    CrossNodeRange,
    UnsupportedKind,
    ResourceLimit,
    InvalidOrigin,
    StalePlan,
    WrongReplica,
}
impl From<ProjectionError> for TextError {
    fn from(e: ProjectionError) -> Self {
        Self::Projection(e)
    }
}
impl From<BoundaryError> for TextError {
    fn from(e: BoundaryError) -> Self {
        Self::Boundary(e)
    }
}
#[derive(Debug)]
struct Edit {
    text: XmlTextRef,
    source: String,
    start: u32,
    end: u32,
    insert: String,
    literal: bool,
}
/// Opaque and deliberately not Clone: consuming commit prevents accidental replay.
#[derive(Debug)]
pub struct PreparedTextEdit {
    identity: DocumentIdentity,
    writer: u32,
    revision: u64,
    state: Vec<u8>,
    origin: TextOrigin,
    edits: Vec<Edit>,
    entry_selection: Selection,
    result_selection: Selection,
}
#[derive(Debug)]
pub struct TextCommit {
    pub revision: u64,
    pub origin: TextOrigin,
    /// None for an empty edit: no write transaction or revision increment.
    pub update: Option<ReplayUpdate>,
    /// Transaction-local UTF-16 source ranges, independent of snapshot consumption.
    pub touched_ranges: Vec<TouchedRange>,
}
pub(super) fn position(resolution: Resolution) -> Result<Position, TextError> {
    match resolution {
        Resolution::Position(p) => Ok(p),
        Resolution::Deleted { .. } => Err(TextError::DeletedTarget),
        Resolution::Unresolvable(e) => Err(TextError::Unresolvable(e)),
    }
}
pub(super) fn check_depth(snapshot: &Snapshot, node: &str) -> Result<(), TextError> {
    let mut id = node;
    let mut seen = BTreeSet::new();
    while id != "@root" {
        if !seen.insert(id) || seen.len() > MAX_TEXT_DEPTH {
            return Err(TextError::ResourceLimit);
        }
        let n = snapshot
            .visible
            .iter()
            .find(|n| n.id == id)
            .ok_or(TextError::Unresolvable(Unresolvable::MissingOwner))?;
        id = &n.parent;
    }
    Ok(())
}
impl Document {
    /// Full preparation, without a mutable transaction or consuming snapshot changes.
    pub fn can_text(&self, command: &TextCommand, origin: &TextOrigin) -> Result<(), TextError> {
        self.prepare_text(command, origin).map(|_| ())
    }
    pub fn prepare_text(
        &self,
        command: &TextCommand,
        origin: &TextOrigin,
    ) -> Result<PreparedTextEdit, TextError> {
        let (a, b, inserted) = match command {
            TextCommand::Insert { at, text } => (at, at, text.as_str()),
            TextCommand::Delete { selection } => (&selection.anchor, &selection.focus, ""),
            TextCommand::Replace { selection, text } => {
                (&selection.anchor, &selection.focus, text.as_str())
            }
        };
        if inserted.len() > MAX_EDIT_BYTES {
            return Err(TextError::ResourceLimit);
        }
        let snapshot = self.inspect()?;
        let entry_selection = Selection {
            anchor: a.clone(),
            focus: b.clone(),
        };
        let txn = self.read();
        let a = position(anchors::resolve(&txn, self.identity(), &snapshot, a))?;
        let b = position(anchors::resolve(&txn, self.identity(), &snapshot, b))?;
        if a.node != b.node {
            return Err(TextError::CrossNodeRange);
        }
        let (start, end) = if a.offset <= b.offset { (a, b) } else { (b, a) };
        // The upper right-affine cut follows the replacement end, including a
        // collapsed insertion. Retain CRDT anchors, never caller offsets.
        let result_anchor = self
            .anchor(&end.node, end.offset, anchors::Bias::Right)
            .map_err(|_| TextError::Unresolvable(Unresolvable::MissingPosition))?;
        let result_selection = Selection {
            anchor: result_anchor.clone(),
            focus: result_anchor,
        };
        if !self.identity().has_editor() {
            check_depth(&snapshot, &start.node)?;
        }
        let node = snapshot
            .visible
            .iter()
            .find(|n| n.id == start.node)
            .ok_or(TextError::Unresolvable(Unresolvable::MissingOwner))?;
        // v1 knows list/item text; unknown extensions retain content but are not editable.
        if !matches!(node.kind.as_str(), "list" | "item")
            && !(node.kind == "block" && self.identity().has_blocks())
        {
            return Err(TextError::UnsupportedKind);
        }
        if self.identity().has_editor()
            && (node.kind != "block"
                || !matches!(
                    node.block_attributes,
                    Some(
                        crate::document::BlockAttributes::Paragraph
                            | crate::document::BlockAttributes::Heading(_)
                            | crate::document::BlockAttributes::Code(_)
                    )
                ))
        {
            return Err(TextError::UnsupportedKind);
        }
        let literal = self.identity().has_editor()
            && matches!(
                node.block_attributes,
                Some(crate::document::BlockAttributes::Code(_))
            );
        let content: String = node.runs.iter().map(|r| r.text.as_str()).collect();
        if content.len() > MAX_TEXT_BYTES {
            return Err(TextError::ResourceLimit);
        }
        let bytes = anchors::validate_range(&content, start.offset..end.offset)?;
        let result_bytes = content.len() - (bytes.end - bytes.start) + inserted.len();
        if result_bytes > MAX_TEXT_BYTES {
            return Err(TextError::ResourceLimit);
        }
        let texts = txn
            .get_map(ROOTS[1])
            .ok_or(TextError::Projection(ProjectionError::InvalidSchema))?;
        let mut edits = Vec::new();
        let mut base = 0u32;
        let mut insertion_found = inserted.is_empty();
        for span in &node.spans {
            let length = span.end - span.start;
            let lo = start.offset.0.max(base);
            let hi = end.offset.0.min(base + length);
            let insertion_here = span.source == start.source
                && span.start <= start.source_offset.0
                && start.source_offset.0 <= span.end
                && !insertion_found;
            let (s, e) = if lo < hi {
                (span.start + lo - base, span.start + hi - base)
            } else {
                (start.source_offset.0, start.source_offset.0)
            };
            base += length;
            if lo >= hi && !insertion_here {
                continue;
            }
            let Some(Out::YXmlText(text)) = texts.get(&txn, &span.source) else {
                return Err(TextError::Unresolvable(Unresolvable::MissingSource));
            };
            let raw = crate::projection::plain_text(&text, &txn)?;
            let removed = anchors::validate_range(&raw, Utf16Offset(s)..Utf16Offset(e))?;
            let addition = if insertion_here { inserted } else { "" };
            if raw.len() > MAX_TEXT_BYTES
                || raw.len() - (removed.end - removed.start) + addition.len() > MAX_TEXT_BYTES
            {
                return Err(TextError::ResourceLimit);
            }
            edits.push(Edit {
                text,
                source: span.source.clone(),
                start: s,
                end: e,
                insert: addition.to_owned(),
                literal,
            });
            insertion_found |= insertion_here;
        }
        if !insertion_found {
            return Err(TextError::Unresolvable(Unresolvable::MissingOwner));
        }
        // Bound the exact state token too. Restore already bounds admitted input;
        // later edits may grow it, so reject an over-budget plan before mutation.
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        if state
            .len()
            .checked_add(inserted.len())
            .filter(|n| *n <= MAX_RESTORE_BYTES)
            .is_none()
        {
            return Err(TextError::ResourceLimit);
        }
        // A joined owner can contain several spans of one original source.
        // Apply higher source offsets first so earlier removals cannot shift them.
        edits.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then(b.start.cmp(&a.start))
                .then(b.end.cmp(&a.end))
        });
        if self.identity().has_structure() {
            self.validate_general_mutation(|t| {
                for edit in &edits {
                    let Some(Out::YXmlText(text)) =
                        t.get_map(ROOTS[1]).unwrap().get(t, &edit.source)
                    else {
                        unreachable!()
                    };
                    if edit.end > edit.start {
                        text.remove_range(t, edit.start, edit.end - edit.start);
                    }
                    if !edit.insert.is_empty() {
                        if edit.literal {
                            text.insert_with_attributes(
                                t,
                                edit.start,
                                &edit.insert,
                                Default::default(),
                            );
                        } else {
                            text.insert(t, edit.start, &edit.insert);
                        }
                    }
                }
            })
            .map_err(|_| TextError::ResourceLimit)?;
        }
        Ok(PreparedTextEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            state,
            origin: origin.clone(),
            edits,
            entry_selection,
            result_selection,
        })
    }
    /// Preconditions are rechecked before opening the single write transaction.
    /// There are no fallible operations once the first source is changed.
    pub fn commit_text(&mut self, plan: PreparedTextEdit) -> Result<TextCommit, TextError> {
        if self.identity() != &plan.identity || self.writer_id() != plan.writer {
            return Err(TextError::WrongReplica);
        }
        if self.revision() != plan.revision || self.checkpoint().bytes != plan.state {
            return Err(TextError::StalePlan);
        }
        if plan.edits.is_empty() {
            return Ok(TextCommit {
                revision: self.revision(),
                origin: plan.origin,
                update: None,
                touched_ranges: vec![],
            });
        }
        let next_revision = self
            .revision()
            .checked_add(1)
            .ok_or(TextError::ResourceLimit)?;
        let mut touched_ranges = Vec::with_capacity(plan.edits.len());
        let update;
        let patch_before = self.patch_before().map_err(TextError::from)?;
        self.history_begin(&plan.origin, true);
        {
            let mut txn = self.write(plan.origin.as_str());
            for edit in &plan.edits {
                if edit.end > edit.start {
                    edit.text
                        .remove_range(&mut txn, edit.start, edit.end - edit.start);
                }
                if !edit.insert.is_empty() {
                    if edit.literal {
                        edit.text.insert_with_attributes(
                            &mut txn,
                            edit.start,
                            &edit.insert,
                            Default::default(),
                        );
                    } else {
                        edit.text.insert(&mut txn, edit.start, &edit.insert);
                    }
                }
                touched_ranges.push(TouchedRange {
                    source: edit.source.clone(),
                    start: edit.start,
                    new_start: edit.start,
                    old_end: edit.end,
                    new_end: edit.start + edit.insert.encode_utf16().count() as u32,
                });
            }
            update = txn.encode_update_v1();
        } // observer accumulation/commit precedes any public result
        self.history_bookmarks(Some(plan.entry_selection), Some(plan.result_selection));
        self.history_finish(update.len());
        self.revision = next_revision;
        let commit = TextCommit {
            revision: next_revision,
            origin: plan.origin,
            update: Some(ReplayUpdate {
                identity: self.identity().clone(),
                bytes: update,
            }),
            touched_ranges,
        };
        self.patch_finish(
            patch_before,
            crate::patches::PatchOperation::Text,
            commit.origin.as_str(),
            commit.update.clone(),
        );
        Ok(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::Bias;
    use crate::document::{CreationPermit, InitialNode};
    use std::sync::{Arc, Mutex};
    use yrs::{Origin, Transact};
    fn fixture(epoch: &str) -> (Document, String) {
        let identity = DocumentIdentity::v1("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", epoch).unwrap();
        let d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[InitialNode {
                kind: "item".into(),
                text: "abcd".into(),
            }],
        )
        .unwrap();
        let id = d.snapshot().unwrap().visible[0].id.clone();
        (d, id)
    }
    #[test]
    fn one_explicit_origin_transaction_and_no_applicability_callbacks() {
        let (mut d, id) = fixture("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        let handle = d.write("fixture:handle").doc().clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        handle
            .observe_update_v1("text-proof", move |txn, event| {
                observed
                    .lock()
                    .unwrap()
                    .push((txn.origin().cloned(), event.update.clone()));
            })
            .unwrap();
        let transactions = Arc::new(Mutex::new(0));
        let counted = transactions.clone();
        handle
            .observe_after_transaction("transaction-proof", move |_| {
                *counted.lock().unwrap() += 1;
            })
            .unwrap();
        // Yrs registers document observers in its own empty write transaction.
        // Start counting after setup so only applicability/commit are measured.
        *transactions.lock().unwrap() = 0;
        let origin = TextOrigin::new("composition:7").unwrap();
        let command = TextCommand::Replace {
            selection: d
                .selection(
                    (&id, Utf16Offset(1), Bias::Right),
                    (&id, Utf16Offset(3), Bias::Left),
                )
                .unwrap(),
            text: "😀".into(),
        };
        d.can_text(&command, &origin).unwrap();
        let p = d.prepare_text(&command, &origin).unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(*transactions.lock().unwrap(), 0);
        let result = d.commit_text(p).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(*transactions.lock().unwrap(), 1);
        assert_eq!(seen[0].0, Some(Origin::from(origin.as_str())));
        assert_eq!(seen[0].1, result.update.unwrap().bytes);
        handle.unobserve_update_v1("text-proof").unwrap();
        handle
            .unobserve_after_transaction("transaction-proof")
            .unwrap();
    }
    #[test]
    fn delete_only_remote_update_invalidates_plan_even_with_unchanged_state_vector() {
        use yrs::updates::decoder::Decode;
        let (mut d, id) = fixture("cccccccc-cccc-4ccc-8ccc-cccccccccccc");
        let command = TextCommand::Insert {
            at: d.anchor(&id, Utf16Offset(2), Bias::Right).unwrap(),
            text: "X".into(),
        };
        let plan = d
            .prepare_text(&command, &TextOrigin::new("typing").unwrap())
            .unwrap();
        let peer = crate::new_document();
        for root in ROOTS {
            peer.get_or_insert_map(root);
        }
        peer.transact_mut()
            .apply_update(yrs::Update::decode_v1(&d.checkpoint().bytes).unwrap())
            .unwrap();
        let source = match peer
            .transact()
            .get_map(ROOTS[1])
            .unwrap()
            .get(&peer.transact(), &id)
            .unwrap()
        {
            Out::YXmlText(t) => t,
            _ => panic!("text"),
        };
        let update = {
            let mut t = peer.transact_mut();
            source.remove_range(&mut t, 0, 1);
            t.encode_update_v1()
        };
        let before_vector = d.read().state_vector();
        let before_revision = d.revision();
        d.write("fixture:remote")
            .apply_update(yrs::Update::decode_v1(&update).unwrap())
            .unwrap();
        assert_eq!(d.read().state_vector(), before_vector);
        assert_eq!(d.revision(), before_revision);
        let before = d.checkpoint().bytes;
        assert_eq!(d.commit_text(plan).unwrap_err(), TextError::StalePlan);
        assert_eq!(d.checkpoint().bytes, before);
    }
}

/// Apply validated edits only to the enclosing disposable editor stage. Source
/// references are rebound to that stage so all CRDT structs use its writer.
pub(super) fn apply_prepared_to_stage(stage: &yrs::Doc, plan: PreparedTextEdit) {
    use yrs::Transact;
    let mut t = stage.transact_mut_with("prepare:editor:text");
    for edit in plan.edits {
        let Some(Out::YXmlText(text)) = t.get_map(ROOTS[1]).unwrap().get(&t, &edit.source) else {
            unreachable!("validated retained source")
        };
        if edit.end > edit.start {
            text.remove_range(&mut t, edit.start, edit.end - edit.start);
        }
        if !edit.insert.is_empty() {
            if edit.literal {
                text.insert_with_attributes(&mut t, edit.start, &edit.insert, Default::default());
            } else {
                text.insert(&mut t, edit.start, &edit.insert);
            }
        }
    }
}
