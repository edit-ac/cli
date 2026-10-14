//! Code/separator policy over the single v4 prepared transaction.
//! Enter/hard break inserts a literal LF inside code; splitting is explicit.
//! Separator insertion preserves its reference owner's identity and text.
use super::{
    editor::{EditorCommand, EditorCommit, EditorError, PreparedEditorEdit},
    text::{TextCommand, TextOrigin},
};
use crate::{
    Utf16Offset,
    anchors::{Anchor, Bias},
    document::{BlockAttributes, Document, DocumentIdentity},
    editor_mapping as m,
    projection::{ProjectedNode, ProjectionIndex},
};
use yrs::{Any, Map, ReadTxn, Transact};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}
#[derive(Clone, Debug)]
pub enum BlockCommand {
    /// Canonical v5 paragraph presentation; other alignments are not admitted.
    AlignLeft {
        owner: String,
    },
    SetCode {
        owner: String,
        language: String,
    },
    SetParagraph {
        owner: String,
    },
    /// Stable anchors route edits through retained sources, including joined spans.
    Text(TextCommand),
    HardBreak {
        at: Anchor,
    },
    Split {
        owner: String,
        at: Anchor,
    },
    Join {
        first: String,
        second: String,
    },
    ConvertSeparator {
        owner: String,
    },
    InsertSeparator {
        beside: String,
        side: Side,
    },
    /// Explicit typing position beside a noneditable separator, in its parent.
    InsertParagraph {
        separator: String,
        side: Side,
        text: String,
    },
    /// Reversibly hides only the named separator; retained identities survive.
    DeleteSeparator {
        owner: String,
    },
}
impl Document {
    pub fn prepare_blocks(
        &self,
        commands: &[BlockCommand],
        origin: &TextOrigin,
    ) -> Result<PreparedEditorEdit, EditorError> {
        if commands.len() > 4096 {
            return Err(EditorError::ResourceLimit);
        }
        self.prepare_editor(
            &commands
                .iter()
                .cloned()
                .map(EditorCommand::BlockCommand)
                .collect::<Vec<_>>(),
            origin,
        )
    }
    pub fn can_blocks(
        &self,
        commands: &[BlockCommand],
        origin: &TextOrigin,
    ) -> Result<(), EditorError> {
        self.prepare_blocks(commands, origin).map(|_| ())
    }
    pub fn commit_blocks(&mut self, plan: PreparedEditorEdit) -> Result<EditorCommit, EditorError> {
        self.commit_editor(plan)
    }
    /// Nearest editable position in document preorder in the requested direction.
    /// At an outer edge, use the other direction. Containers and opaque nodes
    /// are skipped; no text is copied or synthesized by navigation.
    pub fn navigate_separator(&self, separator: &str, side: Side) -> Result<Anchor, EditorError> {
        if !self.identity().has_editor() {
            return Err(EditorError::UnsupportedProfile);
        }
        let s = self.snapshot().map_err(|_| EditorError::InvalidCommand)?;
        let at = s
            .visible
            .iter()
            .position(|n| {
                n.id == separator && n.block_attributes == Some(BlockAttributes::Separator)
            })
            .ok_or(EditorError::UnsupportedTarget)?;
        let before = || s.visible[..at].iter().rev().find(|n| editable(n));
        let after = || s.visible[at + 1..].iter().find(|n| editable(n));
        let (n, end) = match side {
            Side::Before => before()
                .map(|n| (n, true))
                .or_else(|| after().map(|n| (n, false))),
            Side::After => after()
                .map(|n| (n, false))
                .or_else(|| before().map(|n| (n, true))),
        }
        .ok_or(EditorError::UnsupportedTarget)?;
        let offset = if end {
            n.runs
                .iter()
                .map(|r| r.text.encode_utf16().count() as u32)
                .sum()
        } else {
            0
        };
        self.anchor(
            &n.id,
            Utf16Offset(offset),
            if end { Bias::Left } else { Bias::Right },
        )
        .map_err(|_| EditorError::InvalidCommand)
    }
}
fn editable(n: &ProjectedNode) -> bool {
    n.kind == "block"
        && matches!(
            n.block_attributes,
            Some(
                BlockAttributes::Paragraph | BlockAttributes::Heading(_) | BlockAttributes::Code(_)
            )
        )
}
fn tuple(tag: &str) -> Any {
    m::arr(vec![Any::from(tag)])
}
fn fresh(d: &yrs::Doc, writer: u32) -> Result<String, EditorError> {
    let t = d.transact();
    let max = t
        .get_map(m::ROOTS[0])
        .unwrap()
        .keys(&t)
        .filter_map(|s| m::id(s, "n").ok())
        .filter(|(w, _)| *w == writer)
        .map(|(_, c)| c)
        .max()
        .unwrap_or(0);
    Ok(format!(
        "n:{writer}:{}",
        max.checked_add(1).ok_or(EditorError::ResourceLimit)?
    ))
}
pub(crate) fn expand(
    d: &yrs::Doc,
    identity: &DocumentIdentity,
    writer: u32,
    c: &BlockCommand,
) -> Result<Vec<EditorCommand>, EditorError> {
    let s = ProjectionIndex::with_identity(d, identity)
        .read()
        .map_err(|_| EditorError::InvalidCommand)?;
    let node = |id: &str| {
        s.visible
            .iter()
            .find(|n| n.id == id)
            .ok_or(EditorError::UnsupportedTarget)
    };
    let presentation = |id: &str, t: Any| EditorCommand::Block {
        target: id.into(),
        tuple: t,
    };
    let beside = |n: &ProjectedNode,
                  side: Side,
                  t: Any,
                  text: String|
     -> Result<Vec<EditorCommand>, EditorError> {
        let id = fresh(d, writer)?;
        let before = if side == Side::Before {
            Some(n.id.clone())
        } else {
            let at = s.visible.iter().position(|x| x.id == n.id).unwrap();
            s.visible[at + 1..]
                .iter()
                .find(|x| x.parent == n.parent)
                .map(|x| x.id.clone())
        };
        Ok(vec![
            EditorCommand::Create {
                id: id.clone(),
                kind: "block".into(),
                text,
                tuple: t,
            },
            EditorCommand::Place {
                target: id,
                parent: n.parent.clone(),
                before,
            },
        ])
    };
    Ok(match c {
        BlockCommand::AlignLeft { owner } => {
            let n = node(owner)?;
            if n.kind != "block" || n.block_attributes != Some(BlockAttributes::Paragraph) {
                return Err(EditorError::UnsupportedTarget);
            }
            // Canonical left alignment is already represented by this tuple.
            // Do not manufacture a metadata update or a history action.
            vec![]
        }
        BlockCommand::SetCode { owner, language } => vec![presentation(
            owner,
            m::arr(vec![Any::from("code"), Any::from(language.as_str())]),
        )],
        BlockCommand::SetParagraph { owner } => vec![presentation(owner, tuple("paragraph"))],
        BlockCommand::Text(c) => vec![EditorCommand::Text(c.clone())],
        BlockCommand::HardBreak { at } => vec![EditorCommand::Text(TextCommand::Insert {
            at: at.clone(),
            text: "\n".into(),
        })],
        BlockCommand::Split { owner, at } => vec![EditorCommand::Split {
            owner: owner.clone(),
            at: at.clone(),
        }],
        BlockCommand::Join { first, second } => vec![EditorCommand::Join {
            first: first.clone(),
            second: second.clone(),
        }],
        BlockCommand::ConvertSeparator { owner } => vec![presentation(owner, tuple("separator"))],
        BlockCommand::InsertSeparator {
            beside: owner,
            side,
        } => beside(node(owner)?, *side, tuple("separator"), String::new())?,
        BlockCommand::InsertParagraph {
            separator,
            side,
            text,
        } => {
            let n = node(separator)?;
            if n.block_attributes != Some(BlockAttributes::Separator) {
                return Err(EditorError::UnsupportedTarget);
            }
            beside(n, *side, tuple("paragraph"), text.clone())?
        }
        BlockCommand::DeleteSeparator { owner } => {
            let n = node(owner)?;
            if n.block_attributes != Some(BlockAttributes::Separator) {
                return Err(EditorError::UnsupportedTarget);
            }
            let mut steps = vec![];
            if !s.visible.iter().any(|n| editable(n)) {
                steps = beside(n, Side::After, tuple("paragraph"), String::new())?;
            }
            steps.push(EditorCommand::Enable {
                target: owner.clone(),
                enabled: false,
            });
            steps
        }
    })
}
