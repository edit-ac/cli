//! Local selective history. Immutable registrations and intentions are retained;
//! creation/structural history changes their reversible entity controls instead.
use crate::{
    commands::text::TextOrigin,
    document::{Document, DocumentError, InitializedCheckpoint, ReplayUpdate},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use yrs::undo::{Options, StackItem};
use yrs::{Any, Doc, Map, Out, ReadTxn, Transact, UndoManager};

const MUTABLE_ROOTS: [&str; 4] = [
    "registry-block-attributes",
    "registry-container-attributes",
    "registry-list-item-attributes",
    "registry-entity-controls",
];
const IMMUTABLE_ROOTS: [&str; 2] = ["registry-parents", "registry-ownership-ops"];
const CONTROLS: &str = "registry-entity-controls";

/// Caller-supplied monotonic time makes capture policy independent of wall clocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryGroup {
    Explicit,
    Typing { session: u64, time_ms: u64 },
    Composition { session: u64 },
}
#[derive(Clone, Debug)]
pub struct HistoryConfig {
    pub max_groups: usize,
    /// Bound retained command payload as well as group and transaction counts.
    pub max_update_bytes: usize,
    pub max_group_commits: usize,
    pub typing_timeout_ms: u64,
}
impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max_groups: 100,
            max_update_bytes: 8 * 1024 * 1024,
            max_group_commits: 256,
            typing_timeout_ms: 500,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum HistoryError {
    UnsupportedProfile,
    InvalidConfig,
    ResourceLimit,
    /// A concurrent presentation now conflicts with the historical mutation.
    ConcurrentConflict,
    Admission(DocumentError),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryState {
    pub undo_groups: usize,
    pub redo_groups: usize,
    pub retained_update_bytes: usize,
}
pub struct HistoryCommit {
    pub revision: u64,
    pub selection: Option<crate::anchors::Selection>,
    pub update: Option<ReplayUpdate>,
}

struct Group {
    entry_selection: Option<crate::anchors::Selection>,
    result_selection: Option<crate::anchors::Selection>,
    manager: UndoManager,
    sources: BTreeSet<String>,
    created: BTreeSet<String>,
    initial: BTreeMap<(String, String), Any>,
    mode: HistoryGroup,
    origin: String,
    bytes: usize,
    commits: usize,
}
impl Drop for Group {
    fn drop(&mut self) {
        self.manager.clear_all();
    }
}
pub(crate) struct History {
    config: HistoryConfig,
    mode: HistoryGroup,
    boundary: bool,
    undo: VecDeque<Group>,
    redo: Vec<Group>,
    before: BTreeSet<String>,
}
fn sources(doc: &Doc) -> BTreeSet<String> {
    let t = doc.transact();
    t.get_map("registry-texts")
        .map(|m| m.keys(&t).map(str::to_owned).collect())
        .unwrap_or_default()
}
fn entities(doc: &Doc) -> BTreeSet<String> {
    let t = doc.transact();
    IMMUTABLE_ROOTS
        .iter()
        .flat_map(|r| {
            t.get_map(*r)
                .map(|m| m.keys(&t).map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect()
}
fn scope(manager: &mut UndoManager, doc: &Doc, ids: &BTreeSet<String>) {
    let (texts, maps) = {
        let t = doc.transact();
        let texts = t
            .get_map("registry-texts")
            .map(|m| {
                ids.iter()
                    .filter_map(|id| match m.get(&t, id) {
                        Some(Out::YXmlText(text)) => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let maps = MUTABLE_ROOTS
            .iter()
            .filter_map(|r| t.get_map(*r))
            .collect::<Vec<_>>();
        (texts, maps)
    };
    // Register observers only after releasing the read lock.
    for text in texts {
        manager.expand_scope(doc, &text);
    }
    for map in maps {
        manager.expand_scope(doc, &map);
    }
}
fn manager(
    doc: &Doc,
    ids: &BTreeSet<String>,
    undo: Vec<StackItem<()>>,
    redo: Vec<StackItem<()>>,
) -> UndoManager {
    let mut m = UndoManager::with_options(Options {
        capture_timeout_millis: u64::MAX,
        init_undo_stack: undo,
        init_redo_stack: redo,
        tracked_origins: Default::default(),
        capture_transaction: None,
        // This manager represents one already bounded group. Capture policy
        // uses HistoryGroup's supplied time, never the OS wall clock. Yrs also
        // has no default clock on wasm32-unknown-unknown.
        timestamp: std::sync::Arc::new(|| 1u64),
    });
    scope(&mut m, doc, ids);
    m
}
// Yrs 0.28 follows an item's redone ID but drops the offset inside that
// item. Preserve the CRDT clock displacement, never a captured source offset.
fn restored_displacement(
    bytes: &[u8],
    anchor: &crate::anchors::Anchor,
    deleted: &yrs::IdSet,
) -> u32 {
    use yrs::encoding::read::Read;
    use yrs::updates::decoder::{Decoder, DecoderV1};
    let Some(id) = anchor.portable_parts().2.id() else {
        return 0;
    };
    if !deleted.contains(id) {
        return 0;
    }
    let scan = || -> Result<u32, yrs::encoding::read::Error> {
        let mut d = DecoderV1::from(bytes);
        let clients: u32 = d.read_var()?;
        for _ in 0..clients {
            let count: u32 = d.read_var()?;
            let client = d.read_client()?;
            let mut clock: u32 = d.read_var()?;
            for _ in 0..count {
                let info = d.read_info()?;
                let len = if info == 10 {
                    d.read_var()?
                } else if info == 0 {
                    d.read_len()?
                } else {
                    if info & 128 != 0 {
                        d.read_left_id()?;
                    }
                    if info & 64 != 0 {
                        d.read_right_id()?;
                    }
                    if info & 192 == 0 {
                        if d.read_parent_info()? {
                            d.read_string()?;
                        } else {
                            d.read_left_id()?;
                        }
                        if info & 32 != 0 {
                            d.read_string()?;
                        }
                    }
                    yrs::block::ItemContent::decode(&mut d, info)?.len(yrs::OffsetKind::Utf16)
                };
                if client == id.client && clock <= id.clock && id.clock < clock + len {
                    return Ok(id.clock - clock);
                }
                clock += len;
            }
        }
        Ok(0)
    };
    scan().expect("trusted native checkpoint")
}

fn copy_stack(doc: &Doc, stack: &[StackItem<()>]) -> Vec<StackItem<()>> {
    stack
        .iter()
        .map(|s| StackItem::new(doc.guid(), s.deletions().clone(), s.insertions().clone()))
        .collect()
}
fn controls(doc: &Doc, g: &Group, enabled: bool) {
    let mut t = doc.transact_mut_with("history:controls");
    // Creation metadata is required by admission even while disabled. Restore
    // only absent initial tuples; never overwrite a remote tuple still present.
    for ((root, id), value) in &g.initial {
        let m = t.get_map(root.as_str()).expect("qualified root");
        if m.get(&t, id).is_none() {
            m.insert(&mut t, id.as_str(), value.clone());
        }
    }
    let m = t.get_map(CONTROLS).expect("qualified controls");
    for id in &g.created {
        m.insert(
            &mut t,
            id.as_str(),
            Any::Array(vec![Any::from("enabled"), Any::Bool(enabled)].into()),
        );
    }
}
impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("undo_groups", &self.undo.len())
            .field("redo_groups", &self.redo.len())
            .finish_non_exhaustive()
    }
}
impl History {
    fn bytes(&self) -> usize {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .map(|g| g.bytes)
            .sum()
    }
    fn begin(&mut self, doc: &Doc, origin: &TextOrigin, typing: bool) {
        self.before = entities(doc);
        let mode = if typing {
            self.mode.clone()
        } else {
            HistoryGroup::Explicit
        };
        let merge = !self.boundary
            && self.undo.back().is_some_and(|g| {
                g.origin == origin.as_str()
                    && g.commits < self.config.max_group_commits
                    && match (&g.mode, &mode) {
                        (
                            HistoryGroup::Typing {
                                session: a,
                                time_ms: x,
                            },
                            HistoryGroup::Typing {
                                session: b,
                                time_ms: y,
                            },
                        ) => a == b && y >= x && y - x <= self.config.typing_timeout_ms,
                        (
                            HistoryGroup::Composition { session: a },
                            HistoryGroup::Composition { session: b },
                        ) => a == b,
                        _ => false,
                    }
            });
        if !merge {
            let ids = sources(doc);
            self.undo.push_back(Group {
                entry_selection: None,
                result_selection: None,
                manager: manager(doc, &ids, vec![], vec![]),
                sources: ids,
                created: BTreeSet::new(),
                initial: BTreeMap::new(),
                mode: mode.clone(),
                origin: origin.as_str().to_owned(),
                bytes: 0,
                commits: 0,
            });
        }
        let g = self.undo.back_mut().unwrap();
        g.mode = mode;
        g.manager.include_origin(origin.as_str());
        self.boundary = !typing;
    }
    fn finish(&mut self, doc: &Doc, bytes: usize) {
        let g = self.undo.back_mut().unwrap();
        g.manager.exclude_origin(g.origin.as_str());
        g.created
            .extend(entities(doc).difference(&self.before).cloned());
        let t = doc.transact();
        for &root in &MUTABLE_ROOTS[..3] {
            let m = t.get_map(root).expect("qualified root");
            for id in &g.created {
                if let Some(Out::Any(v)) = m.get(&t, id) {
                    g.initial.entry((root.to_string(), id.clone())).or_insert(v);
                }
            }
        }
        drop(t);
        if !g.manager.can_undo() && g.created.is_empty() {
            self.undo.pop_back();
            return;
        }
        g.bytes += bytes;
        g.commits += 1;
        self.redo.clear();
        while self.undo.len() > self.config.max_groups
            || self.bytes() > self.config.max_update_bytes
        {
            self.undo.pop_front();
        }
    }
}
impl Document {
    /// V4/v5 supply reversible controls; legacy profiles deliberately reject.
    pub fn enable_history(&mut self, config: HistoryConfig) -> Result<(), HistoryError> {
        if !self.identity().has_editor() {
            return Err(HistoryError::UnsupportedProfile);
        }
        if config.max_groups == 0
            || config.max_groups > 1000
            || config.max_update_bytes == 0
            || config.max_update_bytes > 64 * 1024 * 1024
            || config.max_group_commits == 0
            || config.max_group_commits > 4096
        {
            return Err(HistoryError::InvalidConfig);
        }
        self.history = Some(History {
            config,
            mode: HistoryGroup::Explicit,
            boundary: true,
            undo: VecDeque::new(),
            redo: vec![],
            before: BTreeSet::new(),
        });
        Ok(())
    }
    pub fn history_state(&self) -> HistoryState {
        self.history
            .as_ref()
            .map(|h| HistoryState {
                undo_groups: h.undo.len(),
                redo_groups: h.redo.len(),
                retained_update_bytes: h.bytes(),
            })
            .unwrap_or(HistoryState {
                undo_groups: 0,
                redo_groups: 0,
                retained_update_bytes: 0,
            })
    }
    pub fn set_history_group(&mut self, group: HistoryGroup) {
        if let Some(h) = &mut self.history {
            h.mode = group;
        }
    }
    pub fn reset_history_group(&mut self) {
        if let Some(h) = &mut self.history {
            h.boundary = true;
        }
    }
    /// Clear retained history without writing document state.
    pub fn clear_history(&mut self) {
        if let Some(h) = &mut self.history {
            h.undo.clear();
            h.redo.clear();
            h.boundary = true;
        }
    }
    pub(crate) fn history_begin(&mut self, origin: &TextOrigin, typing: bool) {
        if let Some(h) = &mut self.history {
            h.begin(&self.doc, origin, typing);
        }
    }
    pub(crate) fn history_finish(&mut self, bytes: usize) {
        if let Some(h) = &mut self.history {
            h.finish(&self.doc, bytes);
        }
    }
    pub(crate) fn history_bookmarks(
        &mut self,
        entry: Option<crate::anchors::Selection>,
        result: Option<crate::anchors::Selection>,
    ) {
        if let Some(g) = self.history.as_mut().and_then(|h| h.undo.back_mut()) {
            // A typing group keeps its first transaction-entry bookmark;
            // subsequent commits update only the group's final selection.
            if g.commits == 0 {
                g.entry_selection = entry;
            }
            g.result_selection = result;
        }
    }
    pub fn undo(&mut self) -> Result<Option<HistoryCommit>, HistoryError> {
        self.history_apply(false)
    }
    pub fn redo(&mut self) -> Result<Option<HistoryCommit>, HistoryError> {
        self.history_apply(true)
    }
    fn history_apply(&mut self, redo: bool) -> Result<Option<HistoryCommit>, HistoryError> {
        let Some(h) = &self.history else {
            return Ok(None);
        };
        let Some(g) = (if redo { h.redo.last() } else { h.undo.back() }) else {
            return Ok(None);
        };
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(HistoryError::ResourceLimit)?;
        // Same writer and retained blocks, but a disposable GUID and no live
        // observers. Copy Yrs insertion/deletion sets, never semantic text.
        let stage = Doc::with_options(yrs::Options {
            client_id: self.doc.client_id(),
            offset_kind: yrs::OffsetKind::Utf16,
            skip_gc: true,
            ..yrs::Options::default()
        });
        for root in crate::editor_mapping::ROOTS {
            stage.get_or_insert_map(root);
        }
        crate::editor_mapping::apply(&stage, &self.checkpoint().bytes)
            .map_err(|_| HistoryError::ResourceLimit)?;
        let mut probe = manager(
            &stage,
            &g.sources,
            copy_stack(&stage, g.manager.undo_stack()),
            copy_stack(&stage, g.manager.redo_stack()),
        );
        if redo {
            probe.redo_blocking();
        } else {
            probe.undo_blocking();
        }
        controls(&stage, g, redo);
        let candidate = InitializedCheckpoint {
            identity: self.identity().clone(),
            initialized: true,
            writer_id: self.writer_id(),
            bytes: crate::editor_mapping::bytes(&stage),
        };
        let admitted =
            Document::restore(self.identity(), &candidate, &[]).map_err(HistoryError::Admission)?;
        crate::document::validate_immutable_structure(
            &self.doc,
            &admitted.doc,
            self.identity().has_stable_presentation(),
        )
        .map_err(HistoryError::Admission)?;
        let projected = admitted
            .snapshot()
            .map_err(|_| HistoryError::ConcurrentConflict)?;
        let separators = projected
            .visible
            .iter()
            .filter(|n| n.block_attributes == Some(crate::document::BlockAttributes::Separator))
            .collect::<Vec<_>>();
        if separators
            .iter()
            .any(|n| n.runs.iter().any(|r| !r.text.is_empty()))
            || (!separators.is_empty()
                && !projected.visible.iter().any(|n| {
                    !n.spans.is_empty()
                        && n.block_attributes != Some(crate::document::BlockAttributes::Separator)
                }))
        {
            return Err(HistoryError::ConcurrentConflict);
        }
        drop(admitted);
        let bookmark = if redo {
            g.result_selection.clone()
        } else {
            g.entry_selection.clone()
        };
        let stack = if redo {
            g.manager.redo_stack()
        } else {
            g.manager.undo_stack()
        };
        let displacement = bookmark.as_ref().map(|s| {
            let deleted = stack.last().expect("history stack").deletions();
            let bytes = self.checkpoint().bytes;
            (
                restored_displacement(&bytes, &s.anchor, deleted),
                restored_displacement(&bytes, &s.focus, deleted),
            )
        });
        // Admit restored endpoints on the same disposable undo/redo replica.
        // Validation precedes stack movement, live transactions and publication.
        if let Some((s, (a, f))) = bookmark.clone().zip(displacement) {
            let txn = stage.transact();
            for (anchor, displacement) in [(s.anchor, a), (s.focus, f)] {
                let restored = crate::anchors::restore_history_anchor(&txn, anchor, displacement)
                    .map_err(|_| HistoryError::ConcurrentConflict)?;
                if !matches!(
                    crate::anchors::resolve(&txn, self.identity(), &projected, &restored),
                    crate::anchors::Resolution::Position(_)
                ) {
                    return Err(HistoryError::ConcurrentConflict);
                }
            }
        }
        let sv = self.doc.transact().state_vector();
        let before = self.checkpoint().bytes;
        let patch_before = self
            .patch_before()
            .map_err(|_| HistoryError::ConcurrentConflict)?;
        let h = self.history.as_mut().unwrap();
        let mut g = if redo {
            h.redo.pop().unwrap()
        } else {
            h.undo.pop_back().unwrap()
        };
        if redo {
            g.manager.redo_blocking();
        } else {
            g.manager.undo_blocking();
        }
        controls(&self.doc, &g, redo);
        let selection = bookmark.zip(displacement).map(|(mut s, (a, f))| {
            s.anchor = crate::anchors::restore_history_anchor(&self.doc.transact(), s.anchor, a)
                .expect("pre-admitted history endpoint");
            s.focus = crate::anchors::restore_history_anchor(&self.doc.transact(), s.focus, f)
                .expect("pre-admitted history endpoint");
            s
        });
        if redo {
            g.result_selection = selection.clone();
        } else {
            g.entry_selection = selection.clone();
        }
        if redo {
            h.undo.push_back(g);
        } else {
            h.redo.push(g);
        }
        h.boundary = true;
        let changed = self.checkpoint().bytes != before;
        if changed {
            self.revision = revision;
        }
        let commit = HistoryCommit {
            selection,
            revision: self.revision,
            update: changed.then(|| ReplayUpdate {
                identity: self.identity().clone(),
                bytes: self.doc.transact().encode_state_as_update_v1(&sv),
            }),
        };
        if changed {
            self.patch_finish(
                patch_before,
                if redo {
                    crate::patches::PatchOperation::Redo
                } else {
                    crate::patches::PatchOperation::Undo
                },
                if redo { "history:redo" } else { "history:undo" },
                commit.update.clone(),
            );
        }
        Ok(Some(commit))
    }
}
