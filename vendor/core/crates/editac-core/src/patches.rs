//! Immutable semantic commit batches. No adapter code runs in a Yrs observer.
//! Callbacks run synchronously after commit, with nested commits queued in order.
use crate::{
    anchors::{AnchorError, ResolvedSelection, Selection},
    document::{Document, DocumentIdentity, ReplayUpdate},
    projection::{ProjectedNode, ProjectionError, Snapshot, TouchedRange},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

pub const PATCH_VERSION: u32 = 1;
const MAX_SUBSCRIPTIONS: usize = 1024;
const MAX_FAILURES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchOperation {
    Text,
    Format,
    Structure,
    Editor,
    Remote,
    Undo,
    Redo,
    Selection,
}
#[derive(Clone, Debug)]
pub enum NodePatch {
    Visible(Arc<ProjectedNode>),
    Recovery(Arc<ProjectedNode>),
    Removed(String),
}
#[derive(Clone, Debug)]
pub struct Patch {
    pub version: u32,
    pub sequence: u64,
    pub revision: u64,
    pub identity: DocumentIdentity,
    pub writer_id: u32,
    pub operation: PatchOperation,
    pub origin: String,
    pub changed_nodes: BTreeSet<String>,
    pub nodes: Vec<NodePatch>,
    pub changed_sources: BTreeSet<String>,
    /// UTF-16 transaction ranges; undo can contain multiple underlying transactions.
    pub touched_ranges: Vec<TouchedRange>,
    pub selection: Option<Selection>,
    /// Deleted endpoints stay explicit rather than moving the selection elsewhere.
    pub resolved_selection: Option<Result<ResolvedSelection, AnchorError>>,
    pub selection_changed: bool,
    /// Owned bytes may be retained independently of the document and callbacks.
    pub update: Option<ReplayUpdate>,
    /// An internal projection failure is observable without undoing a committed update.
    pub projection_error: Option<ProjectionError>,
}
#[derive(Clone, Debug)]
pub enum PatchScope {
    All,
    Nodes(BTreeSet<String>),
    Sources(BTreeSet<String>),
    Selection,
    Updates,
}
impl PatchScope {
    fn matches(&self, p: &Patch) -> bool {
        match self {
            Self::All => true,
            Self::Nodes(ids) => !ids.is_disjoint(&p.changed_nodes),
            Self::Sources(ids) => !ids.is_disjoint(&p.changed_sources),
            Self::Selection => p.selection_changed,
            Self::Updates => p.update.is_some(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionError {
    ResourceLimit,
}
/// Drop or dispose to unsubscribe. Disposal also works during another callback.
pub struct Subscription {
    active: Arc<AtomicBool>,
    callback: Weak<Mutex<Option<Callback>>>,
}
impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("active", &self.is_active())
            .finish()
    }
}
impl Subscription {
    pub fn dispose(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(callback) = self.callback.upgrade() {
            let removed = callback.lock().unwrap().take();
            drop(removed);
        }
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.dispose();
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObserverFailureKind {
    Returned(String),
    Panicked,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserverFailure {
    pub subscription: u64,
    pub sequence: u64,
    pub kind: ObserverFailureKind,
}
type Callback = Box<dyn FnMut(&mut Document, &Patch) -> Result<(), String>>;
struct Listener {
    id: u64,
    after_sequence: u64,
    active: Arc<AtomicBool>,
    scope: PatchScope,
    callback: Arc<Mutex<Option<Callback>>>,
}
#[derive(Default)]
pub(crate) struct PatchState {
    listeners: Vec<Listener>,
    next_id: u64,
    sequence: u64,
    dispatching: bool,
    dispatch_listener_count: usize,
    queue: VecDeque<Arc<Patch>>,
    last: Option<Arc<Patch>>,
    selection: Option<Selection>,
    failures: VecDeque<ObserverFailure>,
}
impl std::fmt::Debug for PatchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchState")
            .field("sequence", &self.sequence)
            .field("listeners", &self.listeners.len())
            .finish_non_exhaustive()
    }
}
impl Drop for PatchState {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.active.store(false, Ordering::Release);
        }
    }
}
pub(crate) struct PatchBefore {
    snapshot: Snapshot,
    selection: Option<Result<ResolvedSelection, AnchorError>>,
}
fn nodes(snapshot: &Snapshot) -> BTreeMap<String, (bool, Arc<ProjectedNode>)> {
    snapshot
        .visible
        .iter()
        .map(|n| (n.id.clone(), (true, n.clone())))
        .chain(
            snapshot
                .recovery
                .iter()
                .map(|n| (n.id.clone(), (false, n.clone()))),
        )
        .collect()
}
impl Document {
    /// The callback may read or commit through the supplied document. Reentrant
    /// batches reach every current listener only after the current batch finishes.
    pub fn subscribe<F>(
        &mut self,
        scope: PatchScope,
        callback: F,
    ) -> Result<Subscription, SubscriptionError>
    where
        F: FnMut(&mut Document, &Patch) -> Result<(), String> + 'static,
    {
        self.patches
            .listeners
            .retain(|l| l.active.load(Ordering::Acquire));
        if self.patches.listeners.len() + self.patches.dispatch_listener_count >= MAX_SUBSCRIPTIONS
            || self.patches.next_id == u64::MAX
        {
            return Err(SubscriptionError::ResourceLimit);
        }
        self.patches.next_id += 1;
        let active = Arc::new(AtomicBool::new(true));
        let callback = Arc::new(Mutex::new(Some(Box::new(callback) as Callback)));
        self.patches.listeners.push(Listener {
            id: self.patches.next_id,
            after_sequence: self.patches.sequence,
            active: active.clone(),
            scope,
            callback: callback.clone(),
        });
        Ok(Subscription {
            active,
            callback: Arc::downgrade(&callback),
        })
    }
    pub fn last_patch(&self) -> Option<Arc<Patch>> {
        self.patches.last.clone()
    }
    /// Bounded diagnostics; observer errors never become command failures.
    pub fn take_observer_failures(&mut self) -> Vec<ObserverFailure> {
        self.patches.failures.drain(..).collect()
    }
    pub fn current_selection(&self) -> Option<&Selection> {
        self.patches.selection.as_ref()
    }
    /// Local selection state is not content history or a CRDT mutation.
    pub fn set_selection(&mut self, selection: Option<Selection>) -> Result<(), AnchorError> {
        if let Some(s) = &selection {
            self.resolve_selection(s)?;
        }
        let before = self.patch_before()?;
        self.patches.selection = selection;
        self.patch_finish(before, PatchOperation::Selection, "selection", None);
        Ok(())
    }
    pub(crate) fn patch_before(&self) -> Result<PatchBefore, ProjectionError> {
        Ok(PatchBefore {
            snapshot: self.inspect()?,
            selection: self
                .patches
                .selection
                .as_ref()
                .map(|s| self.resolve_selection(s)),
        })
    }
    pub(crate) fn patch_finish(
        &mut self,
        before: PatchBefore,
        operation: PatchOperation,
        origin: &str,
        update: Option<ReplayUpdate>,
    ) {
        let after = self.inspect();
        let old = nodes(&before.snapshot);
        let (changed_nodes, touched_ranges, projection_error, new) = match after {
            Ok(after) => {
                let new = nodes(&after);
                let changed: BTreeSet<String> = old
                    .keys()
                    .chain(new.keys())
                    .filter(|id| old.get(*id) != new.get(*id))
                    .cloned()
                    .collect();
                let ranges: Vec<TouchedRange> = after
                    .touched_ranges
                    .into_iter()
                    .skip(before.snapshot.touched_ranges.len())
                    .collect();
                (changed, ranges, None, new)
            }
            Err(e) => (
                old.keys().cloned().collect(),
                vec![],
                Some(e),
                BTreeMap::new(),
            ),
        };
        let mut changed_sources: BTreeSet<_> = touched_ranges
            .iter()
            .map(|r: &TouchedRange| r.source.clone())
            .collect();
        for id in &changed_nodes {
            for (_, node) in old.get(id).into_iter().chain(new.get(id)) {
                changed_sources.extend(node.spans.iter().map(|s| s.source.clone()));
            }
        }
        let resolved_selection = self
            .patches
            .selection
            .as_ref()
            .map(|s| self.resolve_selection(s));
        let selection_changed =
            operation == PatchOperation::Selection || before.selection != resolved_selection;
        self.patches.sequence += 1;
        let node_patches = changed_nodes
            .iter()
            .map(|id| match new.get(id) {
                Some((true, n)) => NodePatch::Visible(n.clone()),
                Some((false, n)) => NodePatch::Recovery(n.clone()),
                None => NodePatch::Removed(id.clone()),
            })
            .collect();
        let patch = Arc::new(Patch {
            version: PATCH_VERSION,
            sequence: self.patches.sequence,
            revision: self.revision(),
            identity: self.identity().clone(),
            writer_id: self.writer_id(),
            operation,
            origin: origin.into(),
            changed_nodes,
            nodes: node_patches,
            changed_sources,
            touched_ranges,
            selection: self.patches.selection.clone(),
            resolved_selection,
            selection_changed,
            update,
            projection_error,
        });
        self.patches.last = Some(patch.clone());
        self.patches.queue.push_back(patch);
        if self.patches.dispatching {
            return;
        }
        self.patches.dispatching = true;
        while let Some(patch) = self.patches.queue.pop_front() {
            let mut listeners = std::mem::take(&mut self.patches.listeners);
            self.patches.dispatch_listener_count = listeners.len();
            for listener in &mut listeners {
                if !listener.active.load(Ordering::Acquire)
                    || patch.sequence <= listener.after_sequence
                    || !listener.scope.matches(&patch)
                {
                    continue;
                }
                let Some(mut callback) = listener.callback.lock().unwrap().take() else {
                    continue;
                };
                let result = catch_unwind(AssertUnwindSafe(|| callback(self, &patch)));
                if listener.active.load(Ordering::Acquire) && matches!(result, Ok(Ok(()))) {
                    *listener.callback.lock().unwrap() = Some(callback);
                }
                let kind = match result {
                    Ok(Ok(())) => continue,
                    Ok(Err(message)) => {
                        ObserverFailureKind::Returned(message.chars().take(1024).collect())
                    }
                    Err(_) => ObserverFailureKind::Panicked,
                };
                // Faulting observers are removed, avoiding repeated failures.
                listener.active.store(false, Ordering::Release);
                if self.patches.failures.len() == MAX_FAILURES {
                    self.patches.failures.pop_front();
                }
                self.patches.failures.push_back(ObserverFailure {
                    subscription: listener.id,
                    sequence: patch.sequence,
                    kind,
                });
            }
            listeners.retain(|l| l.active.load(Ordering::Acquire));
            listeners.append(&mut self.patches.listeners);
            self.patches.listeners = listeners;
            self.patches.dispatch_listener_count = 0;
        }
        self.patches.dispatching = false;
    }
}
