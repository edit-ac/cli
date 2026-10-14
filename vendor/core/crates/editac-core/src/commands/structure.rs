//! Prepared retained-source ownership changes for explicit registry profiles.
//!
//! Structural changes retain every original rich-text identity. Legacy profiles
//! keep their qualified fixed-pair bounds; v3 uses the qualified operation log.
use super::text::{TextError, TextOrigin, check_depth, position};
use crate::anchors::{self, Anchor};
use crate::document::BlockAttributes;
use crate::document::{Document, DocumentIdentity, MAX_RESTORE_BYTES, ROOTS, ReplayUpdate};
use yrs::updates::encoder::Encode;
use yrs::{Any, Assoc, IndexedSequence, Map, MapRef, Out, ReadTxn, StateVector};

const FIRST: &str = "block-1";
const SECOND: &str = "block-2";
const SPLIT: &str = "block-split";

#[derive(Clone, Debug)]
pub enum StructureCommand {
    /// Split the resolved owner (first source only on legacy profiles). The cut
    /// has left affinity, so future inserts exactly at the cut belong to the right.
    Split { at: Anchor },
    /// Join resolved adjacent owners (fixed pair on legacy profiles). Anchors name
    /// the targets; their offsets do not restrict the concatenated source spans.
    Join { first: Anchor, second: Anchor },
    /// V3 explicit logical target plus a stable original-text cut. An empty owner
    /// can share its binary position with siblings; its ID disambiguates structure
    /// without changing the documented Before/After cursor resolution policy.
    SplitOwner { owner: String, at: Anchor },
    /// V3 stable logical owner IDs, validated against current versions/adjacency.
    JoinOwners { first: String, second: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructureError {
    Text(TextError),
    UnsupportedPair,
    AlreadyTransformed,
    OrderOverflow,
    DeletedOutput,
    UnsupportedProfile,
    ConflictingTarget,
    InvalidTarget,
    UnsupportedTarget,
}
impl From<TextError> for StructureError {
    fn from(value: TextError) -> Self {
        Self::Text(value)
    }
}

/// Single-use plan over one replica, guarded by exact state including deletions.
#[derive(Debug)]
pub struct PreparedStructureEdit {
    identity: DocumentIdentity,
    writer: u32,
    revision: u64,
    state: Vec<u8>,
    origin: TextOrigin,
    layouts: MapRef,
    value: Any,
    key: String,
    general: bool,
}

#[derive(Debug)]
pub struct StructureCommit {
    pub revision: u64,
    pub origin: TextOrigin,
    pub update: ReplayUpdate,
}

impl Document {
    pub fn can_structure(
        &self,
        command: &StructureCommand,
        origin: &TextOrigin,
    ) -> Result<(), StructureError> {
        self.prepare_structure(command, origin).map(|_| ())
    }

    pub fn prepare_structure(
        &self,
        command: &StructureCommand,
        origin: &TextOrigin,
    ) -> Result<PreparedStructureEdit, StructureError> {
        if self.identity().has_structure() {
            return self.prepare_general_structure(command, origin);
        }
        let snapshot = self.inspect().map_err(TextError::from)?;
        let txn = self.read();
        let layouts = txn
            .get_map(ROOTS[4])
            .ok_or(StructureError::UnsupportedPair)?;
        match layouts.get(&txn, "pair") {
            Some(Out::Any(Any::Array(a))) if a.as_ref() == [Any::from("original")] => {}
            Some(_) => return Err(StructureError::AlreadyTransformed),
            None => return Err(StructureError::UnsupportedPair),
        }
        let first_anchor = match command {
            StructureCommand::Split { at } => at,
            StructureCommand::Join { first, .. } => first,
            _ => return Err(StructureError::UnsupportedProfile),
        };
        let first = position(anchors::resolve(
            &txn,
            self.identity(),
            &snapshot,
            first_anchor,
        ))?;
        if first.node != FIRST || first.source != FIRST {
            return Err(StructureError::UnsupportedPair);
        }
        check_depth(&snapshot, FIRST)?;
        let node = snapshot.visible.iter().find(|n| n.id == FIRST).unwrap();
        if !matches!(node.kind.as_str(), "item" | "list")
            && !(node.kind == "block" && self.identity().profile == crate::document::BLOCK_PROFILE)
        {
            return Err(TextError::UnsupportedKind.into());
        }
        let value = match command {
            StructureCommand::Split { .. } => {
                if node.order == u32::MAX {
                    return Err(StructureError::OrderOverflow);
                }
                if txn
                    .get_map(ROOTS[3])
                    .is_some_and(|m| m.contains_key(&txn, SPLIT))
                {
                    return Err(StructureError::DeletedOutput);
                }
                let texts = txn
                    .get_map(ROOTS[1])
                    .ok_or(StructureError::UnsupportedPair)?;
                let Some(Out::YXmlText(text)) = texts.get(&txn, FIRST) else {
                    return Err(StructureError::UnsupportedPair);
                };
                // Resolution checked the source identity and UTF-16 boundary.
                // Construct a fresh left-affinity cut even for a right-biased input.
                let cut = text
                    .sticky_index(&txn, first.source_offset.0, Assoc::Before)
                    .ok_or(TextError::Unresolvable(
                        anchors::Unresolvable::MissingPosition,
                    ))?;
                Any::Array(vec![Any::from("split"), Any::Buffer(cut.encode_v1().into())].into())
            }
            StructureCommand::Join { second, .. } => {
                let second = position(anchors::resolve(&txn, self.identity(), &snapshot, second))?;
                if second.node != SECOND || second.source != SECOND {
                    return Err(StructureError::UnsupportedPair);
                }
                check_depth(&snapshot, SECOND)?;
                let node = snapshot.visible.iter().find(|n| n.id == SECOND).unwrap();
                if !matches!(node.kind.as_str(), "item" | "list")
                    && !(node.kind == "block"
                        && self.identity().profile == crate::document::BLOCK_PROFILE)
                {
                    return Err(TextError::UnsupportedKind.into());
                }
                Any::Array(vec![Any::from("join")].into())
            }
            _ => return Err(StructureError::UnsupportedProfile),
        };
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        // Ownership adds only a bounded descriptor, never a copy of source content.
        if state
            .len()
            .checked_add(128)
            .filter(|n| *n <= MAX_RESTORE_BYTES)
            .is_none()
        {
            return Err(TextError::ResourceLimit.into());
        }
        Ok(PreparedStructureEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            state,
            origin: origin.clone(),
            layouts,
            value,
            key: "pair".into(),
            general: false,
        })
    }

    fn prepare_general_structure(
        &self,
        command: &StructureCommand,
        origin: &TextOrigin,
    ) -> Result<PreparedStructureEdit, StructureError> {
        let snapshot = self.inspect().map_err(TextError::from)?;
        let txn = self.read();
        let view = self
            .general_view()
            .map_err(|_| StructureError::ConflictingTarget)?;
        let (first, left_id, right_id) = match command {
            StructureCommand::Split { at } => {
                let p = position(anchors::resolve(&txn, self.identity(), &snapshot, at))?;
                let id = p.node.clone();
                (Some(p), id, None)
            }
            StructureCommand::SplitOwner { owner, at } => {
                // Restrict ownership routing to the named logical target while
                // retaining all source-identity/epoch/UTF-16 anchor validation.
                let target = snapshot
                    .visible
                    .iter()
                    .find(|n| n.id == *owner)
                    .ok_or_else(|| {
                        if snapshot.recovery.iter().any(|n| n.id == *owner) {
                            StructureError::Text(TextError::DeletedTarget)
                        } else {
                            StructureError::ConflictingTarget
                        }
                    })?;
                let selected = crate::projection::Snapshot {
                    visible: vec![target.clone()],
                    ..Default::default()
                };
                let p = position(anchors::resolve(&txn, self.identity(), &selected, at))?;
                (Some(p), owner.clone(), None)
            }
            StructureCommand::Join { first, second } => {
                let a = position(anchors::resolve(&txn, self.identity(), &snapshot, first))?;
                let b = position(anchors::resolve(&txn, self.identity(), &snapshot, second))?;
                (None, a.node, Some(b.node))
            }
            StructureCommand::JoinOwners { first, second } => {
                (None, first.clone(), Some(second.clone()))
            }
        };
        let left = view
            .owners
            .iter()
            .find(|o| o.id == left_id)
            .ok_or(StructureError::ConflictingTarget)?;
        if left.deleted {
            return Err(TextError::DeletedTarget.into());
        }
        if self.identity().has_editor() {
            for id in std::iter::once(&left_id).chain(right_id.iter()) {
                let target = snapshot
                    .visible
                    .iter()
                    .find(|n| n.id == *id)
                    .ok_or(StructureError::ConflictingTarget)?;
                if target.kind != "block"
                    || !matches!(
                        target.block_attributes,
                        Some(
                            BlockAttributes::Paragraph
                                | BlockAttributes::Heading(_)
                                | BlockAttributes::Code(_)
                        )
                    )
                {
                    return Err(StructureError::UnsupportedTarget);
                }
            }
        }
        let counter = self.next_structure_counter;
        counter.checked_add(1).ok_or(TextError::ResourceLimit)?;
        let key = format!("o:{}:{}", self.writer_id(), counter);
        let layouts = txn
            .get_map(crate::ownership::ROOTS[5])
            .ok_or(StructureError::UnsupportedProfile)?;
        if layouts.contains_key(&txn, &key) {
            return Err(StructureError::ConflictingTarget);
        }
        let value = if let Some(first) = first {
            let mut base = 0;
            let mut spans = vec![];
            for (i, span) in left.spans.iter().enumerate() {
                if span.source == first.source
                    && span.start <= first.source_offset.0
                    && first.source_offset.0 <= span.end
                    && base + first.source_offset.0 - span.start == first.offset.0
                {
                    spans.push((i, span));
                }
                base += span.end - span.start;
            }
            let (index, span) = if first.bias == anchors::Bias::Left {
                spans.first()
            } else {
                spans.last()
            }
            .ok_or(StructureError::InvalidTarget)?;
            let Some(Out::YXmlText(text)) = txn.get_map(ROOTS[1]).unwrap().get(&txn, &span.source)
            else {
                return Err(StructureError::InvalidTarget);
            };
            let cut = text
                .sticky_index(&txn, first.source_offset.0, Assoc::Before)
                .ok_or(StructureError::InvalidTarget)?;
            let mut tuple = vec![
                Any::from("split"),
                Any::from(left.id.clone()),
                Any::from(left.version.clone()),
                Any::from(*index as u32),
                Any::from(span.source.clone()),
                Any::Buffer(cut.encode_v1().into()),
            ];
            if self.identity().has_stable_presentation() {
                crate::stable_editor_mapping::witness_tuple(&left.attrs)
                    .map_err(|_| StructureError::ConflictingTarget)?;
                tuple.push(left.attrs.clone());
            }
            crate::ownership::arr(tuple)
        } else {
            let i = view.owners.iter().position(|o| o.id == left.id).unwrap();
            let right = view
                .owners
                .iter()
                .skip(i + 1)
                .find(|o| {
                    !o.deleted
                        && (!self.identity().has_editor()
                            || snapshot.visible.iter().any(|n| {
                                n.id == o.id
                                    && n.kind == "block"
                                    && n.parent
                                        == snapshot
                                            .visible
                                            .iter()
                                            .find(|n| n.id == left.id)
                                            .unwrap()
                                            .parent
                            }))
                })
                .filter(|o| Some(&o.id) == right_id.as_ref())
                .ok_or(StructureError::ConflictingTarget)?;
            let mut tuple = vec![
                Any::from("join"),
                Any::from(left.id.clone()),
                Any::from(left.version.clone()),
                Any::from(right.id.clone()),
                Any::from(right.version.clone()),
            ];
            if self.identity().has_stable_presentation() {
                crate::stable_editor_mapping::witness_pair(&left.attrs, &right.attrs)
                    .map_err(|_| StructureError::ConflictingTarget)?;
                tuple.extend([left.attrs.clone(), right.attrs.clone()]);
            }
            crate::ownership::arr(tuple)
        };
        if self.identity().has_editor() {
            let stage = crate::editor_mapping::fork(&self.doc, self.writer_id() as u64);
            crate::editor_mapping::set(&stage, 5, &key, value.clone());
            let staged = crate::editor::view(&stage, self.identity().has_stable_presentation())
                .map_err(|e| {
                    if e == "resource-limit" {
                        StructureError::Text(TextError::ResourceLimit)
                    } else {
                        StructureError::ConflictingTarget
                    }
                })?;
            if staged.outcomes.get(&key).map(String::as_str) != Some("applied") {
                return Err(StructureError::ConflictingTarget);
            }
        }
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        self.validate_general_mutation(|t| {
            t.get_map(crate::ownership::ROOTS[5])
                .unwrap()
                .insert(t, key.clone(), value.clone());
        })
        .map_err(|e| {
            if e == crate::document::DocumentError::ResourceLimit {
                StructureError::Text(TextError::ResourceLimit)
            } else {
                StructureError::ConflictingTarget
            }
        })?;
        Ok(PreparedStructureEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            state,
            origin: origin.clone(),
            layouts,
            value,
            key,
            general: true,
        })
    }

    pub fn commit_structure(
        &mut self,
        plan: PreparedStructureEdit,
    ) -> Result<StructureCommit, StructureError> {
        if self.identity() != &plan.identity || self.writer_id() != plan.writer {
            return Err(TextError::WrongReplica.into());
        }
        if self.revision() != plan.revision || self.checkpoint().bytes != plan.state {
            return Err(TextError::StalePlan.into());
        }
        let revision = self
            .revision()
            .checked_add(1)
            .ok_or(TextError::ResourceLimit)?;
        let bytes;
        let patch_before = self.patch_before().map_err(TextError::from)?;
        self.history_begin(&plan.origin, false);
        {
            let mut txn = self.write(plan.origin.as_str());
            plan.layouts.insert(&mut txn, plan.key, plan.value);
            bytes = txn.encode_update_v1();
        }
        if plan.general {
            self.next_structure_counter += 1;
        }
        self.history_finish(bytes.len());
        self.revision = revision;
        let commit = StructureCommit {
            revision,
            origin: plan.origin,
            update: ReplayUpdate {
                identity: self.identity().clone(),
                bytes,
            },
        };
        self.patch_finish(
            patch_before,
            crate::patches::PatchOperation::Structure,
            commit.origin.as_str(),
            Some(commit.update.clone()),
        );
        Ok(commit)
    }
}

#[cfg(test)]
mod general_transaction_tests {
    use super::*;
    use crate::{
        Utf16Offset,
        anchors::{Bias, Selection},
        commands::{
            format::{BlockFormatCommand, FormatAction, FormatCommand, Mark},
            text::TextCommand,
        },
        document::{BlockAttributes, CreationPermit, InitialNode},
    };
    use std::sync::{Arc, Mutex};
    use yrs::Origin;
    #[test]
    fn v3_preparation_and_rejection_are_silent_and_each_commit_has_one_explicit_origin() {
        for scenario in 0..4 {
            let identity = DocumentIdentity::v3(
                format!("aaaaaaaa-aaaa-4aaa-8aaa-{:012}", 1800 + scenario),
                "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            )
            .unwrap();
            let mut d = Document::create(
                CreationPermit::authorize_new(identity).unwrap(),
                &[
                    InitialNode {
                        kind: "block".into(),
                        text: "abc".into(),
                    },
                    InitialNode {
                        kind: "block".into(),
                        text: "".into(),
                    },
                ],
            )
            .unwrap();
            let ids: Vec<_> = d
                .snapshot()
                .unwrap()
                .visible
                .iter()
                .map(|n| n.id.clone())
                .collect();
            let handle = d.write("fixture:handle").doc().clone();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let observed = seen.clone();
            handle
                .observe_update_v1("v3-proof", move |t, e| {
                    observed
                        .lock()
                        .unwrap()
                        .push((t.origin().cloned(), e.update.clone()));
                })
                .unwrap();
            let count = Arc::new(Mutex::new(0));
            let counted = count.clone();
            handle
                .observe_after_transaction("v3-count", move |_| {
                    *counted.lock().unwrap() += 1;
                })
                .unwrap();
            // Observer registration itself opens a setup transaction in Yrs.
            *count.lock().unwrap() = 0;
            let origin = TextOrigin::new("v3:transaction-proof").unwrap();
            let before = d.checkpoint().bytes;
            let bad = StructureCommand::JoinOwners {
                first: ids[1].clone(),
                second: ids[0].clone(),
            };
            assert_eq!(
                d.prepare_structure(&bad, &origin).unwrap_err(),
                StructureError::ConflictingTarget
            );
            let update = if scenario < 2 {
                let c = if scenario == 0 {
                    StructureCommand::SplitOwner {
                        owner: ids[0].clone(),
                        at: d.anchor(&ids[0], Utf16Offset(1), Bias::Right).unwrap(),
                    }
                } else {
                    StructureCommand::JoinOwners {
                        first: ids[0].clone(),
                        second: ids[1].clone(),
                    }
                };
                d.can_structure(&c, &origin).unwrap();
                let p = d.prepare_structure(&c, &origin).unwrap();
                assert_eq!(before, d.checkpoint().bytes);
                assert!(seen.lock().unwrap().is_empty());
                assert_eq!(*count.lock().unwrap(), 0);
                d.commit_structure(p).unwrap().update.bytes
            } else if scenario == 2 {
                let c = TextCommand::Insert {
                    at: d.anchor(&ids[0], Utf16Offset(1), Bias::Right).unwrap(),
                    text: "X".into(),
                };
                d.can_text(&c, &origin).unwrap();
                let p = d.prepare_text(&c, &origin).unwrap();
                assert_eq!(before, d.checkpoint().bytes);
                assert!(seen.lock().unwrap().is_empty());
                assert_eq!(*count.lock().unwrap(), 0);
                d.commit_text(p).unwrap().update.unwrap().bytes
            } else {
                let marks = [FormatCommand {
                    selection: Selection {
                        anchor: d.anchor(&ids[0], Utf16Offset(0), Bias::Right).unwrap(),
                        focus: d.anchor(&ids[0], Utf16Offset(3), Bias::Left).unwrap(),
                    },
                    mark: Mark::Bold,
                    action: FormatAction::Apply,
                    href: None,
                }];
                let blocks = [BlockFormatCommand {
                    targets: ids.clone(),
                    attributes: BlockAttributes::Heading(3),
                    action: FormatAction::Apply,
                }];
                d.can_format_batch(&marks, &blocks, &origin).unwrap();
                let p = d.prepare_format_batch(&marks, &blocks, &origin).unwrap();
                assert_eq!(before, d.checkpoint().bytes);
                assert!(seen.lock().unwrap().is_empty());
                assert_eq!(*count.lock().unwrap(), 0);
                d.commit_format(p).unwrap().update.unwrap().bytes
            };
            assert_eq!(*count.lock().unwrap(), 1);
            let events = seen.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, Some(Origin::from(origin.as_str())));
            assert_eq!(events[0].1, update);
            handle.unobserve_update_v1("v3-proof").unwrap();
            handle.unobserve_after_transaction("v3-count").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Utf16Offset, anchors::Bias, document::InitializedCheckpoint};
    use std::sync::{Arc, Mutex};
    use yrs::updates::decoder::Decode;
    use yrs::{ClientID, Doc, OffsetKind, Options, Origin, Text, Transact, Update, XmlTextPrelim};

    fn fixture() -> Document {
        let seed = Doc::with_options(Options {
            client_id: ClientID::new(11),
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        });
        for root in ROOTS {
            seed.get_or_insert_map(root);
        }
        {
            let mut t = seed.transact_mut();
            for (id, value, order) in [(FIRST, "abcd", 0), (SECOND, "EF", 1)] {
                t.get_map(ROOTS[0]).unwrap().insert(&mut t, id, "item");
                t.get_map(ROOTS[1])
                    .unwrap()
                    .insert(&mut t, id, XmlTextPrelim::new(value));
                t.get_map(ROOTS[2]).unwrap().insert(
                    &mut t,
                    id,
                    Any::Array(vec![Any::from("@root"), Any::from(order)].into()),
                );
            }
            t.get_map(ROOTS[4]).unwrap().insert(
                &mut t,
                "pair",
                Any::Array(vec![Any::from("original")].into()),
            );
        }
        let identity = DocumentIdentity::v1(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        )
        .unwrap();
        let bytes = seed
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        Document::restore(
            &identity,
            &InitializedCheckpoint {
                identity: identity.clone(),
                initialized: true,
                writer_id: 11,
                bytes,
            },
            &[],
        )
        .unwrap()
    }
    fn command(d: &Document) -> StructureCommand {
        StructureCommand::Split {
            at: d.anchor(FIRST, Utf16Offset(2), Bias::Left).unwrap(),
        }
    }
    #[test]
    fn preparation_is_read_only_and_commit_has_one_explicit_origin_transaction() {
        for split in [true, false] {
            let mut d = fixture();
            let c = if split {
                command(&d)
            } else {
                StructureCommand::Join {
                    first: d.anchor(FIRST, Utf16Offset(0), Bias::Right).unwrap(),
                    second: d.anchor(SECOND, Utf16Offset(0), Bias::Right).unwrap(),
                }
            };
            let handle = d.write("fixture:handle").doc().clone();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let observed = seen.clone();
            handle
                .observe_update_v1("structure-proof", move |t, e| {
                    observed
                        .lock()
                        .unwrap()
                        .push((t.origin().cloned(), e.update.clone()));
                })
                .unwrap();
            let count = Arc::new(Mutex::new(0));
            let counted = count.clone();
            handle
                .observe_after_transaction("structure-count", move |_| {
                    *counted.lock().unwrap() += 1;
                })
                .unwrap();
            *count.lock().unwrap() = 0;
            let origin = TextOrigin::new("structure:proof").unwrap();
            let before = d.checkpoint().bytes;
            d.can_structure(&c, &origin).unwrap();
            let plan = d.prepare_structure(&c, &origin).unwrap();
            let invalid = StructureCommand::Split {
                at: d.anchor(SECOND, Utf16Offset(0), Bias::Right).unwrap(),
            };
            assert!(d.prepare_structure(&invalid, &origin).is_err());
            assert_eq!(d.checkpoint().bytes, before);
            assert!(seen.lock().unwrap().is_empty());
            assert_eq!(*count.lock().unwrap(), 0);
            let result = d.commit_structure(plan).unwrap();
            assert_eq!(*count.lock().unwrap(), 1);
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, Some(Origin::from(origin.as_str())));
            assert_eq!(seen[0].1, result.update.bytes);
            assert_eq!(result.revision, 1);
            handle.unobserve_update_v1("structure-proof").unwrap();
            handle
                .unobserve_after_transaction("structure-count")
                .unwrap();
        }
    }
    #[test]
    fn deletion_only_remote_state_invalidates_plan_and_anchor_repreparation_is_safe() {
        let mut d = fixture();
        let c = command(&d);
        let origin = TextOrigin::new("structure:stale").unwrap();
        let plan = d.prepare_structure(&c, &origin).unwrap();
        let handle = d.write("fixture:handle").doc().clone();
        let vector = d.read().state_vector();
        let peer = Doc::with_options(Options {
            client_id: ClientID::new(42),
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        });
        for root in ROOTS {
            peer.get_or_insert_map(root);
        }
        peer.transact_mut()
            .apply_update(Update::decode_v1(&d.checkpoint().bytes).unwrap())
            .unwrap();
        let deletion = {
            let mut t = peer.transact_mut();
            let Some(Out::YXmlText(text)) = t.get_map(ROOTS[1]).unwrap().get(&t, FIRST) else {
                panic!("source");
            };
            text.remove_range(&mut t, 0, 1);
            t.encode_update_v1()
        };
        handle
            .transact_mut_with("fixture:remote")
            .apply_update(Update::decode_v1(&deletion).unwrap())
            .unwrap();
        assert_eq!(d.read().state_vector(), vector);
        assert_eq!(d.revision(), 0);
        let after_remote = d.checkpoint().bytes;
        assert_eq!(
            d.commit_structure(plan).unwrap_err(),
            StructureError::Text(TextError::StalePlan)
        );
        assert_eq!(d.checkpoint().bytes, after_remote);
        let plan = d.prepare_structure(&c, &origin).unwrap();
        d.commit_structure(plan).unwrap();
        let s = d.snapshot().unwrap();
        let contents: Vec<_> = s
            .visible
            .iter()
            .map(|n| {
                (
                    n.id.as_str(),
                    n.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                )
            })
            .collect();
        assert_eq!(
            contents,
            [
                (FIRST, "b".into()),
                (SECOND, "EF".into()),
                (SPLIT, "cd".into())
            ]
        );
    }
    #[test]
    fn revision_exhaustion_rejects_before_ownership_mutation() {
        let mut d = fixture();
        d.revision = u64::MAX;
        let c = command(&d);
        let plan = d
            .prepare_structure(&c, &TextOrigin::new("structure:limit").unwrap())
            .unwrap();
        let before = d.checkpoint().bytes;
        assert_eq!(
            d.commit_structure(plan).unwrap_err(),
            StructureError::Text(TextError::ResourceLimit)
        );
        assert_eq!(d.checkpoint().bytes, before);
    }
}
