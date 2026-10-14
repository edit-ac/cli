//! Prepared v4 list/quote policy and read-only hierarchy validation.
//!
//! The legacy validator retains its frozen rule. Production policy expands on
//! a disposable v4 replica and uses the shared atomic editor commit boundary.
use super::text::MAX_TEXT_DEPTH;
use crate::projection::Snapshot;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_HIERARCHY_NODES: usize = 8192;
const ROOT: &str = "@root";

/// A proposed parent relation; this does not write a placement or move text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentChange {
    pub node: String,
    pub parent: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HierarchyError {
    ResourceLimit,
    InvalidSnapshot,
    DuplicateTarget(String),
    MissingNode(String),
    DeletedNode(String),
    UnsupportedTarget(String),
    IllegalParent(String),
    Cycle(String),
    DepthLimit(String),
}

/// Validate parent changes over a derived snapshot without altering it or Yrs.
///
/// A known parent must be a visible `list`; `@root` is the only sentinel.
/// Unknown content is retained but cannot be a semantic command target. Check
/// every descendant as well as changed nodes so moving a shallow subtree cannot
/// push its deepest children beyond the existing editing depth limit.
/// Complexity is bounded by node count and MAX_TEXT_DEPTH, without recursion.
pub fn validate_hierarchy(
    snapshot: &Snapshot,
    changes: &[ParentChange],
) -> Result<(), HierarchyError> {
    validate(snapshot, changes, false)
}

/// Read-only schema-v4 hierarchy prerequisite. This is not profile admission or
/// a production command; only a qualified caller may commit the prepared batch.
/// Root children have depth zero; lists and quotes can contain blocks/containers.
pub fn validate_editor_hierarchy(
    snapshot: &Snapshot,
    changes: &[ParentChange],
) -> Result<(), HierarchyError> {
    validate(snapshot, changes, true)
}

fn validate(
    snapshot: &Snapshot,
    changes: &[ParentChange],
    editor_profile: bool,
) -> Result<(), HierarchyError> {
    let max_path = if editor_profile { 129 } else { MAX_TEXT_DEPTH };
    if snapshot
        .visible
        .len()
        .saturating_add(snapshot.recovery.len())
        > MAX_HIERARCHY_NODES
        || changes.len() > MAX_HIERARCHY_NODES
    {
        return Err(HierarchyError::ResourceLimit);
    }
    let mut nodes = BTreeMap::new();
    for node in &snapshot.visible {
        if node.id == ROOT || nodes.insert(node.id.as_str(), node.as_ref()).is_some() {
            return Err(HierarchyError::InvalidSnapshot);
        }
    }
    let mut deleted = BTreeSet::new();
    for node in &snapshot.recovery {
        if node.id == ROOT
            || nodes.contains_key(node.id.as_str())
            || !deleted.insert(node.id.as_str())
        {
            return Err(HierarchyError::InvalidSnapshot);
        }
    }
    let require_visible = |id: &str| {
        if deleted.contains(id) {
            Err(HierarchyError::DeletedNode(id.to_owned()))
        } else if !nodes.contains_key(id) {
            Err(HierarchyError::MissingNode(id.to_owned()))
        } else {
            Ok(())
        }
    };
    let mut parents: BTreeMap<&str, &str> = nodes
        .iter()
        .map(|(id, node)| (*id, node.parent.as_str()))
        .collect();
    let mut targets = BTreeSet::new();
    for change in changes {
        if !targets.insert(change.node.as_str()) {
            return Err(HierarchyError::DuplicateTarget(change.node.clone()));
        }
        require_visible(&change.node)?;
        if !(if editor_profile {
            matches!(
                nodes[change.node.as_str()].kind.as_str(),
                "block" | "list" | "quote"
            )
        } else {
            matches!(nodes[change.node.as_str()].kind.as_str(), "list" | "item")
        }) {
            return Err(HierarchyError::UnsupportedTarget(change.node.clone()));
        }
        parents.insert(change.node.as_str(), change.parent.as_str());
    }
    // Validate the final graph, including unchanged relations. Do not project
    // illegal local intentions through the remote orphan/cycle repair policy.
    for parent in parents.values() {
        if *parent != ROOT {
            require_visible(parent)?;
            if nodes[*parent].kind != "list" && !(editor_profile && nodes[*parent].kind == "quote")
            {
                return Err(HierarchyError::IllegalParent((*parent).to_owned()));
            }
        }
    }
    let mut depths: BTreeMap<&str, usize> = BTreeMap::new();
    for id in nodes.keys() {
        let mut cursor = *id;
        let mut path = Vec::new();
        let mut seen = BTreeSet::new();
        while cursor != ROOT && !depths.contains_key(cursor) {
            if !seen.insert(cursor) {
                return Err(HierarchyError::Cycle(cursor.to_owned()));
            }
            path.push(cursor);
            if path.len() > max_path {
                return Err(HierarchyError::DepthLimit((*id).to_owned()));
            }
            cursor = parents[cursor];
        }
        let mut depth = depths.get(cursor).copied().unwrap_or(0);
        for node in path.into_iter().rev() {
            depth += 1;
            if depth > max_path {
                return Err(HierarchyError::DepthLimit((*id).to_owned()));
            }
            depths.insert(node, depth);
        }
    }
    Ok(())
}

use super::editor::{EditorCommand, EditorCommit, EditorError, PreparedEditorEdit};
use super::text::TextOrigin;
use crate::{
    anchors::Anchor,
    document::{BlockAttributes, Document, DocumentIdentity},
    editor_mapping as m,
};
use yrs::{Any, Map, ReadTxn, Transact};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerStyle {
    Unordered,
    Ordered { start: u32 },
    Checklist,
    Quote,
}
impl ContainerStyle {
    fn tuple(&self) -> Result<Any, EditorError> {
        Ok(match self {
            Self::Unordered => m::arr(vec![Any::from("unordered")]),
            Self::Ordered { start } if (1..=2147483647).contains(start) => {
                m::arr(vec![Any::from("ordered"), Any::from(*start)])
            }
            Self::Ordered { .. } => return Err(EditorError::InvalidCommand),
            Self::Checklist => m::arr(vec![Any::from("checklist")]),
            Self::Quote => m::arr(vec![Any::from("quote")]),
        })
    }
}
#[derive(Clone, Debug)]
pub enum ListCommand {
    /// Wrap a contiguous run of direct siblings, preserving subtrees and text.
    Wrap {
        targets: Vec<String>,
        style: ContainerStyle,
    },
    SetStyle {
        container: String,
        style: ContainerStyle,
    },
    SetChecked {
        owner: String,
        checked: bool,
    },
    /// Rich item Enter splits at a stable cut; an empty item exits one level.
    Enter {
        owner: String,
        at: Anchor,
    },
    /// At the item start, join its preceding direct editable sibling or exit.
    Backspace {
        owner: String,
        at: Anchor,
    },
    /// Indent below the preceding item using an adjacent matching container.
    Indent {
        owner: String,
    },
    /// Exit one container level, retaining following siblings in a tail container.
    /// Empty containers remain registered/visible; pruning cannot invalidate
    /// historical placement dependencies. A first-item exit reuses the tail.
    Outdent {
        owner: String,
    },
}
impl Document {
    pub fn prepare_lists(
        &self,
        commands: &[ListCommand],
        origin: &TextOrigin,
    ) -> Result<PreparedEditorEdit, EditorError> {
        let steps = commands
            .iter()
            .cloned()
            .map(EditorCommand::List)
            .collect::<Vec<_>>();
        self.prepare_editor(&steps, origin)
    }
    pub fn can_lists(
        &self,
        commands: &[ListCommand],
        origin: &TextOrigin,
    ) -> Result<(), EditorError> {
        self.prepare_lists(commands, origin).map(|_| ())
    }
    pub fn commit_lists(&mut self, plan: PreparedEditorEdit) -> Result<EditorCommit, EditorError> {
        self.commit_editor(plan)
    }
}
fn fresh(d: &yrs::Doc, writer: u32) -> Result<String, EditorError> {
    let t = d.transact();
    let max = t
        .get_map(m::ROOTS[0])
        .unwrap()
        .keys(&t)
        .filter_map(|s| m::id(s, "c").ok())
        .filter(|(w, _)| *w == writer)
        .map(|(_, c)| c)
        .max()
        .unwrap_or(0);
    Ok(format!(
        "c:{writer}:{}",
        max.checked_add(1).ok_or(EditorError::ResourceLimit)?
    ))
}
fn place(node: &str, parent: &str, before: Option<&str>) -> EditorCommand {
    EditorCommand::Place {
        target: node.into(),
        parent: parent.into(),
        before: before.map(String::from),
    }
}
fn create(id: &str, kind: &str, tuple: Any) -> EditorCommand {
    EditorCommand::Create {
        id: id.into(),
        kind: kind.into(),
        text: String::new(),
        tuple,
    }
}
fn known_container(n: &crate::projection::ProjectedNode) -> bool {
    n.editor
        .as_ref()
        .and_then(|e| e.container_attributes.as_ref())
        .is_some_and(|a| {
            matches!(a, Any::Array(v) if matches!(v.first(), Some(Any::String(s)) if
            (n.kind == "list" && matches!(s.as_ref(), "unordered" | "ordered" | "checklist")) ||
            (n.kind == "quote" && s.as_ref() == "quote")))
        })
}
fn rich_item(n: &crate::projection::ProjectedNode) -> bool {
    n.kind == "block"
        && matches!(
            n.block_attributes,
            Some(BlockAttributes::Paragraph | BlockAttributes::Heading(_))
        )
}
/// Expand against the current disposable stage, so subsequent commands see prior
/// prepared effects without opening a live transaction or reserving live IDs.
pub(crate) fn expand(
    d: &yrs::Doc,
    identity: &DocumentIdentity,
    writer: u32,
    c: &ListCommand,
) -> Result<Vec<EditorCommand>, EditorError> {
    let s = crate::projection::ProjectionIndex::with_identity(d, identity)
        .read()
        .map_err(|_| EditorError::InvalidCommand)?;
    let node = |id: &str| {
        s.visible
            .iter()
            .find(|n| n.id == id)
            .ok_or(EditorError::UnsupportedTarget)
    };
    let siblings = |parent: &str| {
        s.visible
            .iter()
            .filter(|n| n.parent == parent)
            .collect::<Vec<_>>()
    };
    let outdent = |id: &str| -> Result<Vec<EditorCommand>, EditorError> {
        let n = node(id)?;
        if !rich_item(n) {
            return Err(EditorError::UnsupportedTarget);
        }
        let p = node(&n.parent)?;
        if !known_container(p) {
            return Err(EditorError::UnsupportedTarget);
        }
        let children = siblings(&p.id);
        let index = children.iter().position(|x| x.id == id).unwrap();
        let outer = siblings(&p.parent);
        let at = outer.iter().position(|x| x.id == p.id).unwrap();
        let before = outer.get(at + 1).map(|n| n.id.as_str());
        let mut steps = vec![];
        // Never disable an emptied container as an automatic cleanup: earlier
        // placements depend on its enabled/versioned identity during replay.
        // For a first-item exit the existing container is already the tail.
        if index == 0 {
            steps.push(place(id, &p.parent, Some(&p.id)));
        } else if index + 1 < children.len() {
            let tail = fresh(d, writer)?;
            steps.push(create(
                &tail,
                &p.kind,
                p.editor
                    .as_ref()
                    .unwrap()
                    .container_attributes
                    .clone()
                    .unwrap(),
            ));
            steps.push(place(&tail, &p.parent, before));
            for child in &children[index + 1..] {
                steps.push(place(&child.id, &tail, None));
            }
            steps.push(place(id, &p.parent, Some(&tail)));
        } else {
            steps.push(place(id, &p.parent, before));
        }
        Ok(steps)
    };
    match c {
        ListCommand::Wrap { targets, style } => {
            if targets.is_empty() || targets.len() > 4096 {
                return Err(EditorError::InvalidCommand);
            }
            let tuple = style.tuple()?;
            let first = node(&targets[0])?;
            let peers = siblings(&first.parent);
            let start = peers.iter().position(|n| n.id == first.id).unwrap();
            for (i, id) in targets.iter().enumerate() {
                let n = node(id)?;
                if peers.get(start + i).map(|n| n.id.as_str()) != Some(id.as_str())
                    || !(rich_item(n) || known_container(n))
                {
                    return Err(EditorError::InvalidCommand);
                }
            }
            let id = fresh(d, writer)?;
            let kind = if *style == ContainerStyle::Quote {
                "quote"
            } else {
                "list"
            };
            let mut steps = vec![
                create(&id, kind, tuple),
                place(&id, &first.parent, Some(&first.id)),
            ];
            for target in targets {
                steps.push(place(target, &id, None));
            }
            Ok(steps)
        }
        ListCommand::SetStyle { container, style } => {
            let n = node(container)?;
            if !known_container(n) || (n.kind == "quote") != (*style == ContainerStyle::Quote) {
                return Err(EditorError::UnsupportedTarget);
            }
            Ok(vec![EditorCommand::Container {
                target: container.clone(),
                tuple: style.tuple()?,
            }])
        }
        ListCommand::SetChecked { owner, checked } => Ok(vec![EditorCommand::Check {
            target: owner.clone(),
            checked: *checked,
        }]),
        ListCommand::Outdent { owner } => outdent(owner),
        ListCommand::Indent { owner } => {
            let n = node(owner)?;
            let p = node(&n.parent)?;
            if !rich_item(n) || !known_container(p) {
                return Err(EditorError::UnsupportedTarget);
            }
            let peers = siblings(&p.id);
            let at = peers.iter().position(|n| n.id == *owner).unwrap();
            let prev = at
                .checked_sub(1)
                .and_then(|i| peers.get(i))
                .ok_or(EditorError::InvalidCommand)?;
            let tuple = p
                .editor
                .as_ref()
                .unwrap()
                .container_attributes
                .clone()
                .unwrap();
            if known_container(prev)
                && prev.kind == p.kind
                && prev.editor.as_ref().unwrap().container_attributes.as_ref() == Some(&tuple)
            {
                return Ok(vec![place(owner, &prev.id, None)]);
            }
            if !rich_item(prev) {
                return Err(EditorError::InvalidCommand);
            }
            let id = fresh(d, writer)?;
            Ok(vec![
                create(&id, &p.kind, tuple),
                place(&id, &p.id, Some(owner)),
                place(owner, &id, None),
            ])
        }
        ListCommand::Enter { owner, at } | ListCommand::Backspace { owner, at } => {
            let n = node(owner)?;
            let p = node(&n.parent)?;
            if !rich_item(n) || !known_container(p) {
                return Err(EditorError::UnsupportedTarget);
            }
            let selected = Snapshot {
                visible: vec![n.clone()],
                ..Default::default()
            };
            let position = super::text::position(crate::anchors::resolve(
                &d.transact(),
                identity,
                &selected,
                at,
            ))
            .map_err(EditorError::Text)?;
            if matches!(c, ListCommand::Enter { .. }) {
                if n.runs.iter().all(|r| r.text.is_empty()) {
                    outdent(owner)
                } else {
                    Ok(vec![EditorCommand::Split {
                        owner: owner.clone(),
                        at: at.clone(),
                    }])
                }
            } else {
                if position.offset.0 != 0 {
                    return Err(EditorError::InvalidCommand);
                }
                let peers = siblings(&p.id);
                let i = peers.iter().position(|n| n.id == *owner).unwrap();
                if let Some(prev) = i
                    .checked_sub(1)
                    .and_then(|i| peers.get(i))
                    .filter(|n| rich_item(n))
                {
                    Ok(vec![EditorCommand::Join {
                        first: prev.id.clone(),
                        second: owner.clone(),
                    }])
                } else {
                    outdent(owner)
                }
            }
        }
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::{
        Utf16Offset,
        anchors::Bias,
        document::{CreationPermit, InitialNode},
    };
    use std::sync::{Arc, Mutex};
    #[test]
    fn list_expansion_is_silent_and_compound_commit_emits_one_transaction_with_origin() {
        let identity = DocumentIdentity::v4(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa28",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbb28",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[
                InitialNode {
                    kind: "block".into(),
                    text: "a😀".into(),
                },
                InitialNode {
                    kind: "block".into(),
                    text: "b".into(),
                },
            ],
        )
        .unwrap();
        let ids = d
            .snapshot()
            .unwrap()
            .visible
            .iter()
            .map(|n| n.id.clone())
            .collect::<Vec<_>>();
        let updates = Arc::new(Mutex::new(vec![]));
        let seen = updates.clone();
        d.doc
            .observe_update_v1("list-origin", move |t, e| {
                seen.lock()
                    .unwrap()
                    .push((t.origin().cloned(), e.update.clone()))
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let seen = count.clone();
        d.doc
            .observe_after_transaction("list-count", move |_| *seen.lock().unwrap() += 1)
            .unwrap();
        *count.lock().unwrap() = 0;
        let origin = TextOrigin::new("lists:compound").unwrap();
        let container = d.next_editor_id(true).unwrap();
        let old = d.checkpoint().bytes;
        let invalid = [
            ListCommand::Wrap {
                targets: ids.clone(),
                style: ContainerStyle::Checklist,
            },
            ListCommand::Outdent {
                owner: "missing".into(),
            },
        ];
        assert!(d.prepare_lists(&invalid, &origin).is_err());
        assert_eq!(d.checkpoint().bytes, old);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(updates.lock().unwrap().is_empty());
        let c = vec![
            ListCommand::Wrap {
                targets: ids.clone(),
                style: ContainerStyle::Checklist,
            },
            ListCommand::SetChecked {
                owner: ids[0].clone(),
                checked: true,
            },
            ListCommand::Enter {
                owner: ids[0].clone(),
                at: d.anchor(&ids[0], Utf16Offset(1), Bias::Right).unwrap(),
            },
            ListCommand::Indent {
                owner: ids[1].clone(),
            },
            ListCommand::SetStyle {
                container,
                style: ContainerStyle::Ordered { start: 5 },
            },
        ];
        let p = d.prepare_lists(&c, &origin).unwrap();
        assert_eq!(d.checkpoint().bytes, old);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(updates.lock().unwrap().is_empty());
        d.commit_lists(p).unwrap();
        assert_eq!(*count.lock().unwrap(), 1);
        let seen = updates.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Some(yrs::Origin::from(origin.as_str())));
        assert_eq!(
            d.read()
                .state_vector()
                .iter()
                .map(|(id, _)| id.get() as u32)
                .collect::<Vec<_>>(),
            vec![d.writer_id()]
        );
    }
}
