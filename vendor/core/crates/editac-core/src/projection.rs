//! Disposable read indexes over the frozen registry. Observers collect dirtiness
//! only; projection reads happen after the Yrs mutation transaction has ended.
use crate::document::{BLOCK_ATTRIBUTES, BlockAttributes, ROOTS, parse_block_tuple};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use yrs::types::{Delta, Event, PathSegment};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, DeepObservable, Doc, Map, MapRef, Out, ReadTxn, StickyIndex, Text, Transact, XmlOut,
    XmlTextRef,
};

const ROOT: &str = "@root";
const FIRST: &str = "block-1";
const SECOND: &str = "block-2";
const SPLIT: &str = "block-split";
static NEXT_INDEX: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq)]
pub struct AttributedRun {
    pub text: String,
    pub attributes: BTreeMap<String, Any>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSpan {
    pub source: String,
    /// Integrated shared-type identity, retained across ownership and moves.
    pub identity: String,
    pub start: u32,
    pub end: u32,
}
/// Versioned v4 container and item presentation; retained opaque tuples stay inert.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorMetadata {
    pub version: String,
    pub container_attributes: Option<Any>,
    pub checked: Option<bool>,
    pub number: Option<u32>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedNode {
    pub id: String,
    pub kind: String,
    pub parent: String,
    pub order: u32,
    pub spans: Vec<SourceSpan>,
    pub runs: Vec<AttributedRun>,
    /// None on legacy/structural kinds; unknown tuples remain explicit opaque state.
    pub block_attributes: Option<BlockAttributes>,
    pub editor: Option<EditorMetadata>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TouchedRange {
    pub source: String,
    /// UTF-16 range in the before/after text for this transaction event.
    pub start: u32,
    pub new_start: u32,
    pub old_end: u32,
    pub new_end: u32,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub visible: Vec<Arc<ProjectedNode>>,
    pub recovery: Vec<Arc<ProjectedNode>>,
    /// Semantic changes since the preceding successful read (including removals).
    pub changed_nodes: BTreeSet<String>,
    /// Transaction-local source ranges, in delivery order; never stable anchors.
    pub touched_ranges: Vec<TouchedRange>,
    /// Number of shared texts read by this refresh, for incremental-work evidence.
    pub source_reads: usize,
    pub structure_rebuilt: bool,
    /// Complete retained v3 intentions, source identities, tuples and outcomes.
    pub retained_structure: Option<Any>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    InvalidSchema,
    InvalidOwnership,
    InvalidText,
    InvalidBoundary,
}
#[derive(Clone, Debug, Default)]
struct Pending {
    structural: bool,
    texts: BTreeSet<String>,
    ranges: Vec<TouchedRange>,
}
#[derive(Clone, Debug)]
struct Source {
    text: XmlTextRef,
    identity: String,
    len: u32,
    runs: Vec<AttributedRun>,
}
#[derive(Clone, Debug)]
struct Placement {
    kind: String,
    parent: String,
    order: u32,
}
#[derive(Clone, Debug, Default)]
struct Cache {
    ready: bool,
    sources: BTreeMap<String, Source>,
    places: BTreeMap<String, Placement>,
    deleted: BTreeSet<String>,
    owners: BTreeMap<String, Vec<SourceSpan>>,
    by_source: BTreeMap<String, BTreeSet<String>>,
    visible: BTreeMap<String, Arc<ProjectedNode>>,
    recovery: BTreeMap<String, Arc<ProjectedNode>>,
    order: Vec<String>,
}
/// Attach once to a replica. Caches contain only derived values and shared-type
/// references; snapshots cannot mutate the replica. Drop removes all observers.
#[derive(Debug)]
pub struct ProjectionIndex {
    // A handle to the same replica keeps observer branches alive through teardown.
    doc: Doc,
    roots: Vec<MapRef>,
    stable_presentation: bool,
    key: String,
    pending: Arc<Mutex<Pending>>,
    cache: Mutex<Cache>,
}
impl ProjectionIndex {
    pub fn new(doc: &Doc) -> Self {
        Self::with_profile(doc, false)
    }
    pub(crate) fn with_identity(doc: &Doc, identity: &crate::document::DocumentIdentity) -> Self {
        Self::with_profile(doc, identity.has_stable_presentation())
    }
    fn with_profile(doc: &Doc, stable_presentation: bool) -> Self {
        let txn = doc.transact();
        let roots: Vec<_> = ROOTS
            .iter()
            .copied()
            .chain([
                BLOCK_ATTRIBUTES,
                "registry-ownership-ops",
                "registry-owner-origins",
                "registry-container-attributes",
                "registry-list-item-attributes",
                "registry-entity-controls",
            ])
            .filter_map(|name| txn.get_map(name))
            .collect();
        drop(txn);
        let pending = Arc::new(Mutex::new(Pending {
            structural: true,
            ..Default::default()
        }));
        let key = format!(
            "editac-projection-{}",
            NEXT_INDEX.fetch_add(1, Ordering::Relaxed)
        );
        for (i, map) in roots.iter().enumerate() {
            let pending = pending.clone();
            map.observe_deep(key.clone(), move |txn, events| {
                let mut p = pending.lock().unwrap();
                for event in events.iter() {
                    if i == 1 {
                        if let Event::Map(map) = event {
                            p.texts
                                .extend(map.keys(txn).keys().map(|id| id.to_string()));
                        }
                        if let Event::XmlText(text) = event {
                            if let Some(PathSegment::Key(id)) = text.path().front() {
                                p.texts.insert(id.to_string());
                                p.ranges.extend(touched(id, text.delta(txn)));
                                continue;
                            }
                        }
                    }
                    p.structural = true;
                }
            });
        }
        Self {
            doc: doc.clone(),
            stable_presentation,
            roots,
            key,
            pending,
            cache: Mutex::new(Cache::default()),
        }
    }
    /// Call only outside a write transaction. Errors retain the previous cache
    /// and pending work; no fallback ownership or repair updates are produced.
    pub fn read(&self) -> Result<Snapshot, ProjectionError> {
        let mut pending = self.pending.lock().unwrap();
        let mut cache = self.cache.lock().unwrap();
        self.refresh(&mut pending, &mut cache)
    }
    /// Inspect current semantics without consuming dirty ranges or publishing a cache.
    /// Prepared commands and applicability must not steal the next snapshot's evidence.
    pub(crate) fn inspect(&self) -> Result<Snapshot, ProjectionError> {
        let mut pending = self.pending.lock().unwrap().clone();
        let mut cache = self.cache.lock().unwrap().clone();
        self.refresh(&mut pending, &mut cache)
    }
    fn refresh(
        &self,
        pending: &mut Pending,
        cache: &mut Cache,
    ) -> Result<Snapshot, ProjectionError> {
        if self
            .doc
            .transact()
            .get_map("registry-owner-origins")
            .is_some()
        {
            return self.refresh_general(pending, cache);
        }
        let txn = self.doc.transact();
        if self.roots.len() < ROOTS.len() {
            return Err(ProjectionError::InvalidSchema);
        }
        let structural = pending.structural || !cache.ready;
        // Stage fallible work before publishing a new cache.
        let mut sources = BTreeMap::new();
        let mut reads = 0;
        let places = if structural {
            Some(read_places(&txn)?)
        } else {
            None
        };
        let ids: Vec<_> = match &places {
            Some(p) => p.keys().cloned().collect(),
            None => pending.texts.iter().cloned().collect(),
        };
        let texts = txn
            .get_map(ROOTS[1])
            .ok_or(ProjectionError::InvalidSchema)?;
        if structural && texts.len(&txn) as usize != places.as_ref().unwrap().len() {
            return Err(ProjectionError::InvalidSchema);
        }
        for id in ids {
            if structural && cache.sources.contains_key(&id) && !pending.texts.contains(&id) {
                continue;
            }
            sources.insert(id.clone(), read_source(&txn, &texts, &id)?);
            reads += 1;
        }
        let next_places = places.as_ref().unwrap_or(&cache.places);
        let next_sources = SourceView {
            base: &cache.sources,
            delta: &sources,
        };
        let owners = ownership(
            &txn,
            &next_sources,
            next_places,
            if structural {
                None
            } else {
                Some(&pending.texts)
            },
        )?;
        let updated_deleted = if structural {
            Some(read_deleted(&txn)?)
        } else {
            None
        };
        let deleted = updated_deleted.as_ref().unwrap_or(&cache.deleted);
        let affected: BTreeSet<String> = if structural {
            cache
                .visible
                .keys()
                .chain(cache.recovery.keys())
                .chain(owners.keys())
                .cloned()
                .collect()
        } else {
            pending
                .texts
                .iter()
                .flat_map(|s| cache.by_source.get(s).into_iter().flatten().cloned())
                .collect()
        };
        let mut replacements = BTreeMap::new();
        for id in &affected {
            if let Some(spans) = owners.get(id) {
                replacements.insert(
                    id.clone(),
                    project_owner(&txn, id, spans, next_places, &next_sources, &deleted)?,
                );
            }
        }
        {
            let visible_ids: Option<BTreeSet<_>> = if structural {
                replacements
                    .iter()
                    .filter(|(_, (v, _))| v.is_some())
                    .map(|(id, _)| id.clone())
                    .collect::<BTreeSet<_>>()
                    .into()
            } else {
                None
            };
            for (visible, _) in replacements.values_mut() {
                if let Some(node) = visible {
                    if !visible_ids.as_ref().map_or_else(
                        || cache.visible.contains_key(&node.parent),
                        |ids| ids.contains(&node.parent),
                    ) {
                        node.parent = ROOT.into();
                    }
                }
            }
        }
        let mut changed = BTreeSet::new();
        // Compare candidates before modifying the cache. Preserve Arc identity for
        // untouched nodes, even if structural metadata had to be reconsidered.
        for id in &affected {
            let candidate = replacements.get(id);
            let v = candidate.and_then(|(v, _)| v.as_ref());
            let r = candidate.and_then(|(_, r)| r.as_ref());
            if cache.visible.get(id).map(AsRef::as_ref) != v
                || cache.recovery.get(id).map(AsRef::as_ref) != r
            {
                changed.insert(id.clone());
            }
        }
        for id in &changed {
            cache.visible.remove(id);
            cache.recovery.remove(id);
            if let Some((v, r)) = replacements.remove(id) {
                if let Some(v) = v {
                    cache.visible.insert(id.clone(), Arc::new(v));
                }
                if let Some(r) = r {
                    cache.recovery.insert(id.clone(), Arc::new(r));
                }
            }
        }
        cache.sources.extend(sources);
        if structural {
            cache.places = places.unwrap();
            let ids: BTreeSet<_> = cache.places.keys().cloned().collect();
            cache.sources.retain(|id, _| ids.contains(id));
            cache.deleted = updated_deleted.unwrap();
            cache.order = preorder(&cache.visible);
        }
        if structural {
            cache.owners = owners;
            cache.by_source.clear();
            let mut by_source: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (owner, spans) in &cache.owners {
                for span in spans {
                    by_source
                        .entry(span.source.clone())
                        .or_default()
                        .insert(owner.clone());
                }
            }
            cache.by_source = by_source;
        } else {
            cache.owners.extend(owners);
        }
        cache.ready = true;
        let snapshot = Snapshot {
            visible: cache
                .order
                .iter()
                .map(|id| cache.visible[id].clone())
                .collect(),
            recovery: cache.recovery.values().cloned().collect(),
            changed_nodes: changed,
            touched_ranges: std::mem::take(&mut pending.ranges),
            source_reads: reads,
            structure_rebuilt: structural,
            retained_structure: None,
        };
        pending.texts.clear();
        pending.structural = false;
        Ok(snapshot)
    }
    fn refresh_general(
        &self,
        pending: &mut Pending,
        cache: &mut Cache,
    ) -> Result<Snapshot, ProjectionError> {
        let view = crate::editor::project(&self.doc, self.stable_presentation)
            .map_err(|_| ProjectionError::InvalidOwnership)?;
        let retained = crate::editor::snapshot(&self.doc, self.stable_presentation)
            .map_err(|_| ProjectionError::InvalidOwnership)?;
        let editor = if crate::editor::is_editor(&self.doc) {
            Some(
                crate::editor::view(&self.doc, self.stable_presentation)
                    .map_err(|_| ProjectionError::InvalidOwnership)?,
            )
        } else {
            None
        };
        let txn = self.doc.transact();
        let texts = txn
            .get_map(ROOTS[1])
            .ok_or(ProjectionError::InvalidSchema)?;
        let mut sources = BTreeMap::new();
        for id in view.sources.keys() {
            sources.insert(id.clone(), read_source(&txn, &texts, id)?);
        }
        let mut visible = BTreeMap::new();
        let mut recovery = BTreeMap::new();
        let mut order = vec![];
        let mut changed = BTreeSet::new();
        for (index, owner) in view.owners.iter().enumerate() {
            let mut runs = vec![];
            let mut spans = vec![];
            for span in &owner.spans {
                let source = &sources[&span.source];
                runs.extend(slice(source, span.start, span.end)?);
                spans.push(SourceSpan {
                    source: span.source.clone(),
                    identity: source.identity.clone(),
                    start: span.start,
                    end: span.end,
                });
            }
            let structural = editor
                .as_ref()
                .and_then(|v| v.owners.iter().find(|n| n.id == owner.id));
            let editor_metadata = structural.map(|o| {
                let Any::Map(raw) = &retained else {
                    unreachable!()
                };
                let Any::Array(nodes) = &raw[if o.deleted { "deleted" } else { "visible" }] else {
                    unreachable!()
                };
                let node = nodes
                    .iter()
                    .find_map(|n| match n {
                        Any::Map(n) if n.get("id") == Some(&Any::from(o.id.clone())) => Some(n),
                        _ => None,
                    })
                    .unwrap();
                EditorMetadata {
                    version: o.version.clone(),
                    container_attributes: if o.id.starts_with("c:") {
                        Some(o.attrs.clone())
                    } else {
                        None
                    },
                    checked: match node["check"] {
                        Any::Bool(b) => Some(b),
                        _ => None,
                    },
                    number: match &node["number"] {
                        Any::Number(n) => n.as_i64().and_then(|n| u32::try_from(n).ok()),
                        _ => None,
                    },
                }
            });
            let node = ProjectedNode {
                editor: editor_metadata,
                id: owner.id.clone(),
                kind: structural
                    .map(|o| o.kind.clone())
                    .unwrap_or_else(|| "block".into()),
                parent: structural
                    .map(|o| o.parent.clone())
                    .unwrap_or_else(|| ROOT.into()),
                order: index as u32,
                spans,
                runs,
                block_attributes: if structural.is_some_and(|o| o.kind != "block") {
                    None
                } else {
                    Some(
                        (if editor.is_some() {
                            crate::document::parse_editor_tuple(&owner.attrs)
                        } else {
                            parse_block_tuple(&owner.attrs)
                        })
                        .map_err(|_| ProjectionError::InvalidSchema)?,
                    )
                },
            };
            let previous = if owner.deleted {
                cache.recovery.get(&owner.id)
            } else {
                cache.visible.get(&owner.id)
            };
            let value = if previous.is_some_and(|v| v.as_ref() == &node) {
                previous.unwrap().clone()
            } else {
                changed.insert(owner.id.clone());
                Arc::new(node)
            };
            if owner.deleted {
                recovery.insert(owner.id.clone(), value);
            } else {
                order.push(owner.id.clone());
                visible.insert(owner.id.clone(), value);
            }
        }
        for id in cache.visible.keys().chain(cache.recovery.keys()) {
            if !visible.contains_key(id) && !recovery.contains_key(id) {
                changed.insert(id.clone());
            }
        }
        let snapshot = Snapshot {
            visible: order.iter().map(|id| visible[id].clone()).collect(),
            recovery: {
                let mut nodes: Vec<_> = recovery.values().cloned().collect();
                nodes.sort_by_key(|n| n.order);
                nodes
            },
            changed_nodes: changed,
            touched_ranges: std::mem::take(&mut pending.ranges),
            source_reads: sources.len(),
            structure_rebuilt: pending.structural || !cache.ready,
            retained_structure: Some(retained),
        };
        cache.sources = sources;
        cache.visible = visible;
        cache.recovery = recovery;
        cache.order = order;
        cache.ready = true;
        pending.structural = false;
        pending.texts.clear();
        Ok(snapshot)
    }
}
impl Drop for ProjectionIndex {
    fn drop(&mut self) {
        for root in &self.roots {
            root.unobserve_deep(self.key.clone());
        }
    }
}
fn touched(id: &str, delta: &[Delta]) -> Vec<TouchedRange> {
    let (mut old, mut new) = (0, 0);
    let mut range: Option<TouchedRange> = None;
    let mut result = vec![];
    for d in delta {
        let changed = !matches!(d, Delta::Retain(_, None));
        if !changed {
            if let Some(r) = range.take() {
                result.push(r);
            }
        } else if range.is_none() {
            range = Some(TouchedRange {
                source: id.into(),
                start: old,
                new_start: new,
                old_end: old,
                new_end: new,
            });
        }
        match d {
            Delta::Inserted(Out::Any(Any::String(s)), _) => new += s.encode_utf16().count() as u32,
            Delta::Deleted(n) => old += n,
            Delta::Retain(n, _) => {
                old += n;
                new += n;
            }
            _ => {}
        }
        if let Some(r) = &mut range {
            r.old_end = old;
            r.new_end = new;
        }
    }
    if let Some(r) = range {
        result.push(r);
    }
    result
}
/// Scalar content of an admitted shared text. `XmlTextRef::get_string` renders
/// formatting as XML tags, so it cannot validate public UTF-16 coordinates or
/// measure semantic text limits.
pub(crate) fn plain_text<T: ReadTxn>(
    text: &XmlTextRef,
    txn: &T,
) -> Result<String, ProjectionError> {
    let mut content = String::new();
    for run in text.diff(txn, |_| ()) {
        let Out::Any(Any::String(s)) = run.insert else {
            return Err(ProjectionError::InvalidText);
        };
        content.push_str(&s);
    }
    Ok(content)
}
fn read_source<T: ReadTxn>(txn: &T, texts: &MapRef, id: &str) -> Result<Source, ProjectionError> {
    let Some(Out::YXmlText(text)) = texts.get(txn, id) else {
        return Err(ProjectionError::InvalidSchema);
    };
    let mut runs = vec![];
    for run in text.diff(txn, |_| ()) {
        let Out::Any(Any::String(s)) = run.insert else {
            return Err(ProjectionError::InvalidText);
        };
        runs.push(AttributedRun {
            text: s.to_string(),
            attributes: run
                .attributes
                .into_iter()
                .flat_map(|a| *a)
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        });
    }
    Ok(Source {
        identity: format!("{:?}", XmlOut::Text(text.clone()).id()),
        len: text.len(txn),
        text,
        runs,
    })
}
fn read_places<T: ReadTxn>(txn: &T) -> Result<BTreeMap<String, Placement>, ProjectionError> {
    let kinds = txn
        .get_map(ROOTS[0])
        .ok_or(ProjectionError::InvalidSchema)?;
    let places = txn
        .get_map(ROOTS[2])
        .ok_or(ProjectionError::InvalidSchema)?;
    let mut nodes: BTreeMap<String, Placement> = BTreeMap::new();
    for (id, kind) in kinds.iter(txn) {
        if !crate::document::valid_node_id(id) || id == SPLIT {
            return Err(ProjectionError::InvalidSchema);
        }
        let Out::Any(Any::String(kind)) = kind else {
            return Err(ProjectionError::InvalidSchema);
        };
        let parsed = match places.get(txn, id) {
            Some(Out::Any(Any::Array(v))) => match v.as_ref() {
                [Any::String(p), Any::Number(n)] => n
                    .as_i64()
                    .and_then(|n| u32::try_from(n).ok())
                    .map(|n| (p.to_string(), n)),
                _ => None,
            },
            _ => None,
        };
        let (parent, order) = parsed.unwrap_or((ROOT.into(), 0));
        nodes.insert(
            id.into(),
            Placement {
                kind: kind.to_string(),
                parent,
                order,
            },
        );
    }
    for id in nodes.keys().cloned().collect::<Vec<_>>() {
        let parent = nodes[&id].parent.clone();
        if parent != ROOT && !nodes.get(&parent).is_some_and(|p| p.kind == "list") {
            nodes.get_mut(&id).unwrap().parent = ROOT.into();
        }
    }
    for id in nodes.keys().cloned().collect::<Vec<_>>() {
        let mut seen = BTreeMap::new();
        let mut path = vec![];
        let mut current = id;
        while current != ROOT {
            if let Some(&start) = seen.get(&current) {
                let cut = path[start..].iter().min().unwrap();
                nodes.get_mut(cut).unwrap().parent = ROOT.into();
                break;
            }
            seen.insert(current.clone(), path.len());
            path.push(current.clone());
            current = nodes[&current].parent.clone();
        }
    }
    Ok(nodes)
}
fn read_deleted<T: ReadTxn>(txn: &T) -> Result<BTreeSet<String>, ProjectionError> {
    let map = txn
        .get_map(ROOTS[3])
        .ok_or(ProjectionError::InvalidSchema)?;
    map.iter(txn)
        .map(|(id, v)| {
            if v == Out::Any(true.into()) {
                Ok(id.to_string())
            } else {
                Err(ProjectionError::InvalidSchema)
            }
        })
        .collect()
}
struct SourceView<'a> {
    base: &'a BTreeMap<String, Source>,
    delta: &'a BTreeMap<String, Source>,
}
impl SourceView<'_> {
    fn contains_key(&self, id: &str) -> bool {
        self.delta.contains_key(id) || self.base.contains_key(id)
    }
}
impl std::ops::Index<&str> for SourceView<'_> {
    type Output = Source;
    fn index(&self, id: &str) -> &Source {
        self.delta
            .get(id)
            .or_else(|| self.base.get(id))
            .expect("registered source")
    }
}
fn ownership<T: ReadTxn>(
    txn: &T,
    sources: &SourceView,
    places: &BTreeMap<String, Placement>,
    touched: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, Vec<SourceSpan>>, ProjectionError> {
    let span = |id: &str, start, end| SourceSpan {
        source: id.into(),
        identity: sources[id].identity.clone(),
        start,
        end,
    };
    let ids: Vec<_> = match touched {
        Some(ids) => ids.iter().collect(),
        None => places.keys().collect(),
    };
    let mut owners: BTreeMap<_, _> = ids
        .into_iter()
        .map(|id| (id.clone(), vec![span(id, 0, sources[id.as_str()].len)]))
        .collect();
    let map = txn
        .get_map(ROOTS[4])
        .ok_or(ProjectionError::InvalidSchema)?;
    if map.len(txn) == 0 {
        return Ok(owners);
    }
    if map.len(txn) != 1 || !sources.contains_key(FIRST) || !sources.contains_key(SECOND) {
        return Err(ProjectionError::InvalidOwnership);
    }
    let Some(Out::Any(Any::Array(v))) = map.get(txn, "pair") else {
        return Err(ProjectionError::InvalidOwnership);
    };
    if touched.is_some_and(|t| !t.contains(FIRST) && !t.contains(SECOND)) {
        return Ok(owners);
    }
    owners.insert(FIRST.into(), vec![span(FIRST, 0, sources[FIRST].len)]);
    owners.insert(SECOND.into(), vec![span(SECOND, 0, sources[SECOND].len)]);
    match v.as_ref() {
        [Any::String(mode)] if mode.as_ref() == "original" => {}
        [Any::String(mode)] if mode.as_ref() == "join" => {
            let second = owners.remove(SECOND).unwrap();
            owners.get_mut(FIRST).unwrap().extend(second);
        }
        [Any::String(mode), Any::Buffer(bytes)] if mode.as_ref() == "split" => {
            let sticky =
                StickyIndex::decode_v1(bytes).map_err(|_| ProjectionError::InvalidOwnership)?;
            let offset = sticky
                .get_offset(txn)
                .ok_or(ProjectionError::InvalidOwnership)?;
            let source = &sources[FIRST];
            if offset.branch.id() != AsRef::<yrs::branch::Branch>::as_ref(&source.text).id()
                || offset.index > source.len
                || places[FIRST].order == u32::MAX
            {
                return Err(ProjectionError::InvalidOwnership);
            }
            slice(source, 0, offset.index)?;
            owners.insert(FIRST.into(), vec![span(FIRST, 0, offset.index)]);
            owners.insert(SPLIT.into(), vec![span(FIRST, offset.index, source.len)]);
        }
        _ => return Err(ProjectionError::InvalidOwnership),
    }
    Ok(owners)
}
fn slice(source: &Source, start: u32, end: u32) -> Result<Vec<AttributedRun>, ProjectionError> {
    let mut pos = 0;
    let mut result = vec![];
    for run in &source.runs {
        let mut text = String::new();
        for c in run.text.chars() {
            let next = pos + c.len_utf16() as u32;
            if next > start && pos < end {
                if pos < start || next > end {
                    return Err(ProjectionError::InvalidBoundary);
                }
                text.push(c);
            }
            pos = next;
        }
        if !text.is_empty() {
            result.push(AttributedRun {
                text,
                attributes: run.attributes.clone(),
            });
        }
    }
    Ok(result)
}
fn project_owner(
    txn: &yrs::Transaction<'_>,
    id: &str,
    spans: &[SourceSpan],
    places: &BTreeMap<String, Placement>,
    sources: &SourceView,
    deleted: &BTreeSet<String>,
) -> Result<(Option<ProjectedNode>, Option<ProjectedNode>), ProjectionError> {
    let place = &places[if id == SPLIT { FIRST } else { id }];
    let make = |hidden: bool| -> Result<Option<ProjectedNode>, ProjectionError> {
        let selected: Vec<_> = spans
            .iter()
            .filter(|s| (deleted.contains(id) || deleted.contains(&s.source)) == hidden)
            .cloned()
            .collect();
        if selected.is_empty() {
            return Ok(None);
        }
        let mut runs = vec![];
        for s in &selected {
            runs.extend(slice(&sources[s.source.as_str()], s.start, s.end)?);
        }
        let mut parent = place.parent.clone();
        if !hidden && (deleted.contains(&parent) || !places.contains_key(&parent)) {
            parent = ROOT.into();
        }
        Ok(Some(ProjectedNode {
            editor: None,
            id: id.into(),
            kind: place.kind.clone(),
            parent,
            order: place.order + u32::from(id == SPLIT),
            spans: selected,
            runs,
            block_attributes: if place.kind == "block" && txn.get_map(BLOCK_ATTRIBUTES).is_some() {
                let attributes = txn.get_map(BLOCK_ATTRIBUTES).unwrap();
                let value = attributes.get(txn, id).or_else(|| {
                    if id == SPLIT {
                        attributes.get(txn, FIRST)
                    } else {
                        None
                    }
                });
                Some(match value {
                    None => BlockAttributes::Paragraph,
                    Some(Out::Any(value)) => {
                        parse_block_tuple(&value).map_err(|_| ProjectionError::InvalidSchema)?
                    }
                    _ => return Err(ProjectionError::InvalidSchema),
                })
            } else {
                None
            },
        }))
    };
    Ok((make(false)?, make(true)?))
}
fn preorder(nodes: &BTreeMap<String, Arc<ProjectedNode>>) -> Vec<String> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, n) in nodes {
        let parent = if nodes.contains_key(&n.parent) {
            &n.parent
        } else {
            ROOT
        };
        children.entry(parent.into()).or_default().push(id.clone());
    }
    for ids in children.values_mut() {
        ids.sort_by_key(|id| (nodes[id].order, id.clone()));
    }
    let mut stack: Vec<_> = children
        .get(ROOT)
        .into_iter()
        .flatten()
        .rev()
        .cloned()
        .collect();
    let mut ordered = vec![];
    while let Some(id) = stack.pop() {
        stack.extend(children.get(&id).into_iter().flatten().rev().cloned());
        ordered.push(id);
    }
    ordered
}
