//! Read-only relative positions over the frozen registry, and checked Unicode
//! conversions. Anchors retain original source identity, never a projected index.
use crate::Utf16Offset;
use crate::document::{Document, DocumentIdentity, ROOTS};
use crate::projection::{ProjectionError, Snapshot};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use yrs::{
    Assoc, GetString, IndexedSequence, Map, Out, ReadTxn, StickyIndex, Text, XmlOut, XmlTextRef,
};

/// Left stays before an insertion at the cursor; right stays after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}
impl Bias {
    fn assoc(self) -> Assoc {
        match self {
            Self::Left => Assoc::Before,
            Self::Right => Assoc::After,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryError {
    OutOfBounds,
    SurrogateInterior,
    InvalidByteBoundary,
    NotGraphemeBoundary,
    ReversedRange,
    TextTooLong,
}
/// Convert a public offset without accepting half of a surrogate pair.
pub fn utf16_to_byte(text: &str, offset: Utf16Offset) -> Result<usize, BoundaryError> {
    let mut units = 0u32;
    for (byte, ch) in text.char_indices() {
        if units == offset.0 {
            return Ok(byte);
        }
        units = units
            .checked_add(ch.len_utf16() as u32)
            .ok_or(BoundaryError::TextTooLong)?;
        if units > offset.0 {
            return Err(BoundaryError::SurrogateInterior);
        }
    }
    if units == offset.0 {
        Ok(text.len())
    } else {
        Err(BoundaryError::OutOfBounds)
    }
}
pub fn byte_to_utf16(text: &str, byte: usize) -> Result<Utf16Offset, BoundaryError> {
    let prefix = text.get(..byte).ok_or(BoundaryError::InvalidByteBoundary)?;
    u32::try_from(prefix.encode_utf16().count())
        .map(Utf16Offset)
        .map_err(|_| BoundaryError::TextTooLong)
}
pub fn validate_range(
    text: &str,
    range: Range<Utf16Offset>,
) -> Result<Range<usize>, BoundaryError> {
    if range.start > range.end {
        return Err(BoundaryError::ReversedRange);
    }
    Ok(utf16_to_byte(text, range.start)?..utf16_to_byte(text, range.end)?)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Backward,
    Forward,
}
/// Logical Unicode movement (including RTL). Visual bidi layout is an adapter concern.
/// A caret within an extended grapheme is rejected rather than deleting part of it.
pub fn move_grapheme(
    text: &str,
    offset: Utf16Offset,
    direction: Direction,
) -> Result<Option<Utf16Offset>, BoundaryError> {
    let byte = utf16_to_byte(text, offset)?;
    let boundaries: Vec<_> = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let at = boundaries
        .binary_search(&byte)
        .map_err(|_| BoundaryError::NotGraphemeBoundary)?;
    let next = match direction {
        Direction::Backward => at.checked_sub(1),
        Direction::Forward => at.checked_add(1).filter(|i| *i < boundaries.len()),
    };
    next.map(|i| byte_to_utf16(text, boundaries[i])).transpose()
}
pub fn deletion_range(
    text: &str,
    offset: Utf16Offset,
    direction: Direction,
) -> Result<Option<Range<Utf16Offset>>, BoundaryError> {
    Ok(
        move_grapheme(text, offset, direction)?.map(|next| match direction {
            Direction::Backward => next..offset,
            Direction::Forward => offset..next,
        }),
    )
}
#[derive(Clone, Debug)]
pub struct Anchor {
    identity: DocumentIdentity,
    source: String,
    source_identity: String,
    position: StickyIndex,
    bias: Bias,
}
impl Anchor {
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Read-only parts for the ephemeral presence bridge. The receiving document
    /// must validate lineage and resolve the original source before retaining it.
    pub fn portable_parts(&self) -> (&str, &str, &StickyIndex) {
        (&self.source, &self.source_identity, &self.position)
    }
    pub fn bias(&self) -> Bias {
        self.bias
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub node: String,
    pub offset: Utf16Offset,
    pub source: String,
    pub source_offset: Utf16Offset,
    pub bias: Bias,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unresolvable {
    IdentityMismatch,
    MissingSource,
    ReplacedSource,
    MissingPosition,
    InvalidBoundary,
    MissingOwner,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Position(Position),
    Deleted { source: String, owner: String },
    Unresolvable(Unresolvable),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorError {
    Projection(ProjectionError),
    MissingTarget,
    DeletedTarget,
    InvalidSource,
    Boundary(BoundaryError),
    Unresolvable,
}
impl From<BoundaryError> for AnchorError {
    fn from(e: BoundaryError) -> Self {
        Self::Boundary(e)
    }
}
impl From<ProjectionError> for AnchorError {
    fn from(e: ProjectionError) -> Self {
        Self::Projection(e)
    }
}
#[derive(Clone, Debug)]
pub struct Selection {
    pub anchor: Anchor,
    pub focus: Anchor,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub anchor: Resolution,
    pub focus: Resolution,
}

fn source<T: ReadTxn>(txn: &T, id: &str) -> Option<XmlTextRef> {
    match txn.get_map(ROOTS[1])?.get(txn, id)? {
        Out::YXmlText(text) => Some(text),
        _ => None,
    }
}
fn text_identity(text: &XmlTextRef) -> String {
    format!("{:?}", XmlOut::Text(text.clone()).id())
}
pub(crate) fn capture<T: ReadTxn>(
    txn: &T,
    identity: &DocumentIdentity,
    snapshot: &Snapshot,
    node: &str,
    offset: Utf16Offset,
    bias: Bias,
) -> Result<Anchor, AnchorError> {
    let target = snapshot
        .visible
        .iter()
        .find(|n| n.id == node)
        .ok_or_else(|| {
            if snapshot.recovery.iter().any(|n| n.id == node) {
                AnchorError::DeletedTarget
            } else {
                AnchorError::MissingTarget
            }
        })?;
    let content: String = target.runs.iter().map(|r| r.text.as_str()).collect();
    utf16_to_byte(&content, offset)?;
    let mut base = 0;
    let mut candidates = Vec::new();
    for span in &target.spans {
        let end = base + span.end - span.start;
        if base <= offset.0 && offset.0 <= end {
            candidates.push((span, span.start + offset.0 - base));
        }
        base = end;
    }
    let &(span, index) = match bias {
        Bias::Left => candidates.first(),
        Bias::Right => candidates.last(),
    }
    .ok_or(AnchorError::InvalidSource)?;
    let text = source(txn, &span.source).ok_or(AnchorError::InvalidSource)?;
    if text_identity(&text) != span.identity {
        return Err(AnchorError::InvalidSource);
    }
    utf16_to_byte(&text.get_string(txn), Utf16Offset(index))?;
    let position = if bias == Bias::Right && index == text.len(txn) {
        // Pinned Yrs sticky_index at a nonempty right endpoint returns None.
        StickyIndex::from_type(txn, &text, Assoc::After)
    } else {
        text.sticky_index(txn, index, bias.assoc())
            .ok_or(AnchorError::Unresolvable)?
    };
    Ok(Anchor {
        identity: identity.clone(),
        source: span.source.clone(),
        source_identity: span.identity.clone(),
        position,
        bias,
    })
}
pub(crate) fn resolve<T: ReadTxn>(
    txn: &T,
    identity: &DocumentIdentity,
    snapshot: &Snapshot,
    anchor: &Anchor,
) -> Resolution {
    let fail = Resolution::Unresolvable;
    if &anchor.identity != identity {
        return fail(Unresolvable::IdentityMismatch);
    }
    let Some(text) = source(txn, &anchor.source) else {
        return fail(Unresolvable::MissingSource);
    };
    if text_identity(&text) != anchor.source_identity {
        return fail(Unresolvable::ReplacedSource);
    }
    let Some(offset) = anchor.position.get_offset(txn) else {
        return fail(Unresolvable::MissingPosition);
    };
    if offset.branch.id() != AsRef::<yrs::branch::Branch>::as_ref(&text).id() {
        return fail(Unresolvable::ReplacedSource);
    }
    if utf16_to_byte(&text.get_string(txn), Utf16Offset(offset.index)).is_err() {
        return fail(Unresolvable::InvalidBoundary);
    }
    if identity.has_structure() {
        let mut interior = Vec::new();
        let mut edge = Vec::new();
        let mut endpoints = Vec::new();
        for (deleted, nodes) in [(false, &snapshot.visible), (true, &snapshot.recovery)] {
            for node in nodes {
                let mut base = 0;
                for (span_index, span) in node.spans.iter().enumerate() {
                    if span.source == anchor.source
                        && span.identity == anchor.source_identity
                        && span.start <= offset.index
                        && offset.index <= span.end
                    {
                        let candidate = (
                            node.order,
                            span_index,
                            node,
                            base + offset.index - span.start,
                            deleted,
                        );
                        endpoints.push(candidate);
                        if span.start < offset.index && offset.index < span.end {
                            interior.push(candidate);
                        } else if (anchor.bias == Bias::Left && offset.index == span.end)
                            || (anchor.bias == Bias::Right && offset.index == span.start)
                        {
                            edge.push(candidate);
                        }
                    }
                    base += span.end - span.start;
                }
            }
        }
        interior.sort_by_key(|v| (v.0, v.1));
        edge.sort_by_key(|v| (v.0, v.1));
        endpoints.sort_by_key(|v| (v.0, v.1));
        let candidates = if !interior.is_empty() {
            &interior
        } else if !edge.is_empty() {
            &edge
        } else {
            &endpoints
        };
        let chosen = if anchor.bias == Bias::Left {
            candidates.first()
        } else {
            candidates.last()
        };
        return match chosen {
            Some((_, _, node, _, true)) => Resolution::Deleted {
                source: anchor.source.clone(),
                owner: node.id.clone(),
            },
            Some((_, _, node, index, false)) => Resolution::Position(Position {
                node: node.id.clone(),
                offset: Utf16Offset(*index),
                source: anchor.source.clone(),
                source_offset: Utf16Offset(offset.index),
                bias: anchor.bias,
            }),
            None => fail(Unresolvable::MissingOwner),
        };
    }
    // At a split cut, left selects the ending span and right the starting span.
    // Include hidden spans so deleted content cannot redirect to a visible sibling.
    let mut candidates = Vec::new();
    for (deleted, nodes) in [(false, &snapshot.visible), (true, &snapshot.recovery)] {
        for node in nodes {
            let mut base = 0;
            for span in &node.spans {
                if span.source == anchor.source
                    && span.identity == anchor.source_identity
                    && span.start <= offset.index
                    && offset.index <= span.end
                {
                    let preferred = match anchor.bias {
                        Bias::Left => offset.index > span.start,
                        Bias::Right => offset.index < span.end,
                    };
                    candidates.push((
                        preferred,
                        span.start,
                        node.as_ref(),
                        base + offset.index - span.start,
                        deleted,
                    ));
                }
                base += span.end - span.start;
            }
        }
    }
    candidates.sort_by_key(|(preferred, start, node, _, _)| (*preferred, *start, node.id.as_str()));
    let chosen = if anchor.bias == Bias::Left {
        candidates
            .iter()
            .rev()
            .find(|c| c.0)
            .or_else(|| candidates.first())
    } else {
        candidates.last()
    };
    let Some((_, _, node, index, deleted)) = chosen else {
        return fail(Unresolvable::MissingOwner);
    };
    if *deleted {
        return Resolution::Deleted {
            source: anchor.source.clone(),
            owner: node.id.clone(),
        };
    }
    Resolution::Position(Position {
        node: node.id.clone(),
        offset: Utf16Offset(*index),
        source: anchor.source.clone(),
        source_offset: Utf16Offset(offset.index),
        bias: anchor.bias,
    })
}
impl Document {
    /// Capture a currently visible semantic position. No mutation transaction is opened.
    pub fn anchor(
        &self,
        node: &str,
        offset: Utf16Offset,
        bias: Bias,
    ) -> Result<Anchor, AnchorError> {
        capture(
            &self.read(),
            self.identity(),
            &self.inspect()?,
            node,
            offset,
            bias,
        )
    }
    pub fn import_presence_anchor(
        &self,
        identity: DocumentIdentity,
        source: String,
        source_identity: String,
        position: StickyIndex,
    ) -> Result<Anchor, AnchorError> {
        let bias = if position.assoc == Assoc::Before {
            Bias::Left
        } else {
            Bias::Right
        };
        let anchor = Anchor {
            identity,
            source,
            source_identity,
            position,
            bias,
        };
        if !matches!(self.resolve_anchor(&anchor)?, Resolution::Position(_)) {
            return Err(AnchorError::Unresolvable);
        }
        Ok(anchor)
    }
    pub fn resolve_anchor(&self, anchor: &Anchor) -> Result<Resolution, AnchorError> {
        Ok(resolve(
            &self.read(),
            self.identity(),
            &self.inspect()?,
            anchor,
        ))
    }
    /// Keep anchor/focus order (including backward selections) and each endpoint's bias.
    pub fn selection(
        &self,
        anchor: (&str, Utf16Offset, Bias),
        focus: (&str, Utf16Offset, Bias),
    ) -> Result<Selection, AnchorError> {
        let snapshot = self.inspect()?;
        let txn = self.read();
        Ok(Selection {
            anchor: capture(
                &txn,
                self.identity(),
                &snapshot,
                anchor.0,
                anchor.1,
                anchor.2,
            )?,
            focus: capture(&txn, self.identity(), &snapshot, focus.0, focus.1, focus.2)?,
        })
    }
    pub fn resolve_selection(
        &self,
        selection: &Selection,
    ) -> Result<ResolvedSelection, AnchorError> {
        let snapshot = self.inspect()?;
        let txn = self.read();
        Ok(ResolvedSelection {
            anchor: resolve(&txn, self.identity(), &snapshot, &selection.anchor),
            focus: resolve(&txn, self.identity(), &snapshot, &selection.focus),
        })
    }
}
