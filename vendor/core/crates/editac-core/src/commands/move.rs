//! Identity-preserving v4 moves. Versions are structural preconditions, not
//! local revision numbers; text edits do not invalidate a captured move.
use super::{editor::*, text::TextOrigin};
use crate::{
    anchors::{AnchorError, Resolution, Selection},
    document::Document,
    editor_mapping as m,
};
use yrs::Any;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveTarget {
    pub id: String,
    /// `base` or the last successful structural operation ID.
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveCommand {
    pub target: MoveTarget,
    /// `@root` with version `base`, or a visible list/quote container.
    pub parent: MoveTarget,
    /// None appends. A sibling must remain under the destination parent.
    pub before: Option<MoveTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveRejection {
    TargetGone,
    DestinationGone,
    BeforeGone,
    StaleTarget,
    StaleDestination,
    StaleBefore,
    UnsupportedTarget,
    InvalidDestination,
    InvalidBefore,
    Cycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveError {
    Editor(EditorError),
    Anchor(AnchorError),
    UnresolvableSelection,
}

pub struct PreparedMove {
    edit: PreparedEditorEdit,
    selection: Option<Selection>,
}

pub struct MoveCommit {
    pub edit: EditorCommit,
    /// Original source-relative endpoints, never reconstructed from indexes.
    /// Resolve on the current replica, including after subsequent remote edits.
    pub selection: Option<Selection>,
}

fn reject(e: MoveRejection) -> EditorError {
    EditorError::Move(e)
}
fn tag(a: &Any) -> &str {
    match a {
        Any::Array(v) => match v.first() {
            Some(Any::String(s)) => s,
            _ => "",
        },
        _ => "",
    }
}
fn view(d: &yrs::Doc, stable: bool) -> Result<m::View, EditorError> {
    crate::editor::view(d, stable).map_err(|e| {
        if e == "resource-limit" {
            EditorError::ResourceLimit
        } else {
            EditorError::InvalidCommand
        }
    })
}
fn live<'a>(v: &'a m::View, id: &str, gone: MoveRejection) -> Result<&'a m::Owner, EditorError> {
    v.owners
        .iter()
        .find(|o| o.id == id && !o.deleted)
        .ok_or_else(|| reject(gone))
}
fn container(o: &m::Owner) -> bool {
    matches!(o.kind.as_str(), "list" | "quote")
        && matches!(
            tag(&o.attrs),
            "ordered" | "unordered" | "checklist" | "quote"
        )
}

pub(crate) fn expand(
    d: &yrs::Doc,
    stable: bool,
    command: &MoveCommand,
) -> Result<EditorCommand, EditorError> {
    let v = view(d, stable)?;
    let t = live(&v, &command.target.id, MoveRejection::TargetGone)?;
    if !(container(t)
        || t.kind == "block"
            && !t.spans.is_empty()
            && matches!(
                tag(&t.attrs),
                "paragraph" | "heading" | "code" | "separator"
            ))
    {
        return Err(reject(MoveRejection::UnsupportedTarget));
    }
    if t.version != command.target.version {
        return Err(reject(MoveRejection::StaleTarget));
    }
    if command.parent.id == "@root" {
        if command.parent.version != "base" {
            return Err(reject(MoveRejection::StaleDestination));
        }
    } else {
        let p = live(&v, &command.parent.id, MoveRejection::DestinationGone)?;
        if !container(p) {
            return Err(reject(MoveRejection::InvalidDestination));
        }
        if p.version != command.parent.version {
            return Err(reject(MoveRejection::StaleDestination));
        }
    }
    // Iterative ancestor walk over the already bounded canonical tree.
    let mut parent = command.parent.id.as_str();
    while parent != "@root" {
        if parent == t.id {
            return Err(reject(MoveRejection::Cycle));
        }
        parent = &live(&v, parent, MoveRejection::DestinationGone)?.parent;
    }
    if let Some(before) = &command.before {
        let b = live(&v, &before.id, MoveRejection::BeforeGone)?;
        if b.id == t.id || b.parent != command.parent.id {
            return Err(reject(MoveRejection::InvalidBefore));
        }
        if b.version != before.version {
            return Err(reject(MoveRejection::StaleBefore));
        }
    }
    Ok(EditorCommand::Place {
        target: command.target.id.clone(),
        parent: command.parent.id.clone(),
        before: command.before.as_ref().map(|b| b.id.clone()),
    })
}

impl Document {
    /// Capture all structural versions now for an asynchronous move. Preparation
    /// later rejects changed target/destination/sibling versions without writes.
    pub fn move_command(
        &self,
        target: &str,
        parent: &str,
        before: Option<&str>,
    ) -> Result<MoveCommand, EditorError> {
        if !self.identity().has_editor() {
            return Err(EditorError::UnsupportedProfile);
        }
        let v = view(&self.doc, self.identity().has_stable_presentation())?;
        let capture = |id: &str, gone| {
            live(&v, id, gone).map(|o| MoveTarget {
                id: id.into(),
                version: o.version.clone(),
            })
        };
        let command = MoveCommand {
            target: capture(target, MoveRejection::TargetGone)?,
            parent: if parent == "@root" {
                MoveTarget {
                    id: parent.into(),
                    version: "base".into(),
                }
            } else {
                capture(parent, MoveRejection::DestinationGone)?
            },
            before: before
                .map(|id| capture(id, MoveRejection::BeforeGone))
                .transpose()?,
        };
        expand(
            &self.doc,
            self.identity().has_stable_presentation(),
            &command,
        )?;
        Ok(command)
    }

    pub fn can_move(
        &self,
        command: &MoveCommand,
        origin: &TextOrigin,
        selection: Option<&Selection>,
    ) -> Result<(), MoveError> {
        self.prepare_move(command, origin, selection).map(|_| ())
    }

    pub fn prepare_move(
        &self,
        command: &MoveCommand,
        origin: &TextOrigin,
        selection: Option<&Selection>,
    ) -> Result<PreparedMove, MoveError> {
        // Validate even unrelated selection endpoints before the live mutation.
        if let Some(s) = selection {
            let resolved = self.resolve_selection(s).map_err(MoveError::Anchor)?;
            if !matches!(resolved.anchor, Resolution::Position(_))
                || !matches!(resolved.focus, Resolution::Position(_))
            {
                return Err(MoveError::UnresolvableSelection);
            }
        }
        let edit = self
            .prepare_editor(&[EditorCommand::Move(command.clone())], origin)
            .map_err(MoveError::Editor)?;
        Ok(PreparedMove {
            edit,
            selection: selection.cloned(),
        })
    }

    pub fn commit_move(&mut self, plan: PreparedMove) -> Result<MoveCommit, MoveError> {
        // No fallible selection conversion after commit: anchors remain attached
        // to their original sources, including nested/derived owners.
        let edit = self.commit_editor(plan.edit).map_err(MoveError::Editor)?;
        Ok(MoveCommit {
            edit,
            selection: plan.selection,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CreationPermit, DocumentIdentity, InitialNode};
    use std::sync::{Arc, Mutex};
    use yrs::{Origin, ReadTxn, Transact};

    #[test]
    fn move_preparation_is_silent_and_commit_has_one_local_origin_and_writer() {
        let identity = DocumentIdentity::v4(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa10",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbb10",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[
                InitialNode {
                    kind: "block".into(),
                    text: "a😀b".into(),
                },
                InitialNode {
                    kind: "block".into(),
                    text: "other".into(),
                },
            ],
        )
        .unwrap();
        let ids = d
            .snapshot()
            .unwrap()
            .visible
            .into_iter()
            .map(|n| n.id.clone())
            .collect::<Vec<_>>();
        let command = d.move_command(&ids[0], "@root", None).unwrap();
        let observed = Arc::new(Mutex::new(vec![]));
        let seen = observed.clone();
        d.doc
            .observe_update_v1("move-origin", move |t, e| {
                seen.lock()
                    .unwrap()
                    .push((t.origin().cloned(), e.update.clone()))
            })
            .unwrap();
        let origin = TextOrigin::new("move:explicit").unwrap();
        let baseline = d.checkpoint().bytes;
        let revision = d.revision();
        let sv = d.doc.transact().state_vector();
        d.can_move(&command, &origin, None).unwrap();
        let plan = d.prepare_move(&command, &origin, None).unwrap();
        assert_eq!(d.checkpoint().bytes, baseline);
        assert_eq!(d.revision(), revision);
        assert!(observed.lock().unwrap().is_empty());
        let result = d.commit_move(plan).unwrap();
        let events = observed.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, Some(Origin::from(origin.as_str())));
        assert_eq!(events[0].1, result.edit.update.bytes);
        assert_eq!(result.edit.revision, revision + 1);
        let after = d.doc.transact().state_vector();
        for (id, clock) in after.iter() {
            if id.get() != d.writer_id() as u64 {
                assert_eq!(sv.get(id), *clock);
            }
        }
        assert_eq!(
            d.snapshot()
                .unwrap()
                .visible
                .into_iter()
                .map(|n| n.id.clone())
                .collect::<Vec<_>>(),
            [ids[1].clone(), ids[0].clone()]
        );
    }
}
