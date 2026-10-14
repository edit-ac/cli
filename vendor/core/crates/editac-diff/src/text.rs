//! Bounded exact-source patches. Ranges are unified line ranges, never editor offsets.
//! Application returns a new buffer only after every check, without a mutable replica.
use crate::representation::{ContentHash, ImmutableRevision, Representation};
use serde::{Deserialize, Serialize};

pub const LINE_ALGORITHM: &str = "lcs-lines-delete-first-v1";
pub const WORD_ALGORITHM: &str = "unicode-whitespace-runs-lcs-delete-first-v1";
const PREFIX: &str = "# editac-exact-patch-v1 ";
const CONTEXT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchLimits {
    pub max_source_bytes: usize,
    pub max_output_bytes: usize,
    pub max_wire_bytes: usize,
    pub max_lines: usize,
    pub max_hunks: usize,
    pub max_comparison_cells: usize,
}
impl Default for PatchLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 5 * 1024 * 1024,
            max_output_bytes: 5 * 1024 * 1024,
            max_wire_bytes: 16 * 1024 * 1024,
            max_lines: 100_000,
            max_hunks: 4096,
            max_comparison_cells: 4_000_000,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchMetadata {
    pub representation: String,
    pub line_algorithm: String,
    pub word_algorithm: String,
    pub context_lines: usize,
    pub before_hash: String,
    pub after_hash: String,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub before_final_newline: bool,
    pub after_final_newline: bool,
    pub limits: PatchLimits,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Delete,
    Insert,
}
/// `text` includes its original LF (and CR when present). No-final-newline is explicit.
#[derive(Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub kind: LineKind,
    pub text: String,
}
impl std::fmt::Debug for PatchLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchLine")
            .field("kind", &self.kind)
            .field("bytes", &self.text.len())
            .finish()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<PatchLine>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPatch {
    pub metadata: PatchMetadata,
    pub hunks: Vec<UnifiedHunk>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchError {
    ResourceLimit,
    InvalidUtf8,
    BinarySource,
    IncompatibleRepresentation,
    IncompatibleScope,
    WrongBaseline,
    InvalidMetadata,
    MalformedRange,
    OverlappingHunks,
    MissingContext,
    WrongOutput,
    MalformedEncoding,
}
impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PatchError {}

fn text(bytes: &[u8], limit: usize) -> Result<&str, PatchError> {
    if bytes.len() > limit {
        return Err(PatchError::ResourceLimit);
    }
    if bytes.contains(&0) {
        return Err(PatchError::BinarySource);
    }
    std::str::from_utf8(bytes).map_err(|_| PatchError::InvalidUtf8)
}
fn lines(s: &str, limit: usize) -> Result<Vec<&str>, PatchError> {
    let mut result = Vec::new();
    for line in s.split_inclusive('\n') {
        if result.len() == limit {
            return Err(PatchError::ResourceLimit);
        }
        result.push(line);
    }
    Ok(result)
}
/// Shared bounded LCS. Trim common ends first; ties always delete from the old side.
fn operations<'a>(
    old: &[&'a str],
    new: &[&'a str],
    cells: usize,
) -> Result<Vec<(LineKind, &'a str)>, PatchError> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let mut ops = Vec::with_capacity(old.len().saturating_add(new.len()));
    ops.extend(old[..prefix].iter().map(|s| (LineKind::Context, *s)));
    if a.is_empty() {
        ops.extend(b.iter().map(|s| (LineKind::Insert, *s)));
    } else if b.is_empty() {
        ops.extend(a.iter().map(|s| (LineKind::Delete, *s)));
    } else {
        let width = b.len().checked_add(1).ok_or(PatchError::ResourceLimit)?;
        let count = a
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(width))
            .filter(|n| *n <= cells)
            .ok_or(PatchError::ResourceLimit)?;
        let mut table = vec![0u32; count];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i * width + j] = if a[i] == b[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                ops.push((LineKind::Context, a[i]));
                i += 1;
                j += 1;
            } else if i < a.len()
                && (j == b.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                ops.push((LineKind::Delete, a[i]));
                i += 1;
            } else {
                ops.push((LineKind::Insert, b[j]));
                j += 1;
            }
        }
    }
    ops.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|s| (LineKind::Context, *s)),
    );
    Ok(ops)
}
/// Optional inline word detail: maximal whitespace/non-whitespace Unicode runs.
/// Whitespace is significant; no normalization or byte-to-editor offset claim.
pub fn word_changes(
    before: &str,
    after: &str,
    limits: PatchLimits,
) -> Result<Vec<PatchLine>, PatchError> {
    fn tokens(s: &str, max: usize) -> Result<Vec<&str>, PatchError> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut class = None;
        for (i, c) in s.char_indices() {
            if class.is_some_and(|v| v != c.is_whitespace()) {
                if out.len() == max {
                    return Err(PatchError::ResourceLimit);
                }
                out.push(&s[start..i]);
                start = i;
            }
            class = Some(c.is_whitespace());
        }
        if !s.is_empty() {
            if out.len() == max {
                return Err(PatchError::ResourceLimit);
            }
            out.push(&s[start..]);
        }
        Ok(out)
    }
    text(before.as_bytes(), limits.max_source_bytes)?;
    text(after.as_bytes(), limits.max_output_bytes)?;
    Ok(operations(
        &tokens(before, limits.max_lines)?,
        &tokens(after, limits.max_lines)?,
        limits.max_comparison_cells,
    )?
    .into_iter()
    .map(|(kind, s)| PatchLine {
        kind,
        text: s.into(),
    })
    .collect())
}

impl TextPatch {
    pub fn between(
        before: &ImmutableRevision,
        after: &ImmutableRevision,
        limits: PatchLimits,
    ) -> Result<Self, PatchError> {
        if before.representation() != &Representation::ExactSourceUtf8
            || after.representation() != &Representation::ExactSourceUtf8
        {
            return Err(PatchError::IncompatibleRepresentation);
        }
        before
            .content_changed(after)
            .map_err(|_| PatchError::IncompatibleScope)?;
        let a = text(before.bytes(), limits.max_source_bytes)?;
        let b = text(after.bytes(), limits.max_output_bytes)?;
        let ops = operations(
            &lines(a, limits.max_lines)?,
            &lines(b, limits.max_lines)?,
            limits.max_comparison_cells,
        )?;
        let mut hunks = Vec::new();
        let (mut index, mut old_pos, mut new_pos) = (0, 0, 0);
        while index < ops.len() {
            let Some(change) = (index..ops.len()).find(|i| ops[*i].0 != LineKind::Context) else {
                break;
            };
            let start = change.saturating_sub(CONTEXT).max(index);
            for (kind, _) in &ops[index..start] {
                old_pos += usize::from(*kind != LineKind::Insert);
                new_pos += usize::from(*kind != LineKind::Delete);
            }
            let mut end = (change + 1 + CONTEXT).min(ops.len());
            let mut cursor = change + 1;
            while cursor < ops.len() {
                if cursor > end.saturating_add(CONTEXT) {
                    break;
                }
                if ops[cursor].0 != LineKind::Context {
                    if cursor > end.saturating_add(CONTEXT) {
                        break;
                    }
                    end = (cursor + 1 + CONTEXT).min(ops.len());
                }
                cursor += 1;
            }
            let slice = &ops[start..end];
            let old_count = slice.iter().filter(|(k, _)| *k != LineKind::Insert).count();
            let new_count = slice.iter().filter(|(k, _)| *k != LineKind::Delete).count();
            if hunks.len() == limits.max_hunks {
                return Err(PatchError::ResourceLimit);
            }
            hunks.push(UnifiedHunk {
                old_start: old_pos + usize::from(old_count > 0),
                old_count,
                new_start: new_pos + usize::from(new_count > 0),
                new_count,
                lines: slice
                    .iter()
                    .map(|(kind, s)| PatchLine {
                        kind: *kind,
                        text: (*s).into(),
                    })
                    .collect(),
            });
            old_pos += old_count;
            new_pos += new_count;
            index = end;
        }
        let patch = Self {
            metadata: PatchMetadata {
                representation: "exact-source-utf8-v1".into(),
                line_algorithm: LINE_ALGORITHM.into(),
                word_algorithm: WORD_ALGORITHM.into(),
                context_lines: CONTEXT,
                before_hash: before.hash().hex(),
                after_hash: after.hash().hex(),
                before_bytes: a.len(),
                after_bytes: b.len(),
                before_final_newline: a.ends_with('\n'),
                after_final_newline: b.ends_with('\n'),
                limits,
            },
            hunks,
        };
        patch.encode(limits)?;
        Ok(patch)
    }
    fn validate(&self, host: PatchLimits) -> Result<PatchLimits, PatchError> {
        let m = &self.metadata;
        let p = m.limits;
        if m.representation != "exact-source-utf8-v1"
            || m.line_algorithm != LINE_ALGORITHM
            || m.word_algorithm != WORD_ALGORITHM
            || m.context_lines != CONTEXT
            || [&m.before_hash, &m.after_hash].iter().any(|s| {
                s.len() != 64
                    || !s
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
        {
            return Err(PatchError::InvalidMetadata);
        }
        let l = PatchLimits {
            max_source_bytes: p.max_source_bytes.min(host.max_source_bytes),
            max_output_bytes: p.max_output_bytes.min(host.max_output_bytes),
            max_wire_bytes: p.max_wire_bytes.min(host.max_wire_bytes),
            max_lines: p.max_lines.min(host.max_lines),
            max_hunks: p.max_hunks.min(host.max_hunks),
            max_comparison_cells: p.max_comparison_cells.min(host.max_comparison_cells),
        };
        if m.before_bytes > l.max_source_bytes
            || m.after_bytes > l.max_output_bytes
            || self.hunks.len() > l.max_hunks
        {
            return Err(PatchError::ResourceLimit);
        }
        let (mut old_end, mut new_end, mut total, mut total_lines) = (0, 0, 0usize, 0usize);
        for (i, h) in self.hunks.iter().enumerate() {
            total_lines = total_lines
                .checked_add(h.lines.len())
                .ok_or(PatchError::ResourceLimit)?;
            if total_lines > l.max_lines.saturating_mul(2) {
                return Err(PatchError::ResourceLimit);
            }
            let old = range(h.old_start, h.old_count)?;
            let new = range(h.new_start, h.new_count)?;
            if i > 0
                && (old < old_end
                    || new < new_end
                    || (h.old_count == 0 && self.hunks[i - 1].old_count == 0 && old == old_end))
            {
                return Err(PatchError::OverlappingHunks);
            }
            if h.lines.is_empty()
                || !h.lines.iter().any(|line| line.kind != LineKind::Context)
                || h.lines
                    .iter()
                    .filter(|line| line.kind != LineKind::Insert)
                    .count()
                    != h.old_count
                || h.lines
                    .iter()
                    .filter(|line| line.kind != LineKind::Delete)
                    .count()
                    != h.new_count
            {
                return Err(PatchError::MalformedRange);
            }
            old_end = old
                .checked_add(h.old_count)
                .ok_or(PatchError::MalformedRange)?;
            new_end = new
                .checked_add(h.new_count)
                .ok_or(PatchError::MalformedRange)?;
            if old_end > l.max_lines || new_end > l.max_lines {
                return Err(PatchError::ResourceLimit);
            }
            for line in &h.lines {
                if line.text.is_empty()
                    || line.text.trim_end_matches('\n').contains('\n')
                    || line.text.ends_with("\n\n")
                    || line.text.contains('\0')
                {
                    return Err(PatchError::MalformedEncoding);
                }
                total = total
                    .checked_add(line.text.len() + 2)
                    .ok_or(PatchError::ResourceLimit)?;
                if total > l.max_wire_bytes {
                    return Err(PatchError::ResourceLimit);
                }
            }
        }
        Ok(l)
    }
    /// No in-place mutation: even a late after-hash failure leaves the input intact.
    pub fn apply(&self, baseline: &[u8], host: PatchLimits) -> Result<Vec<u8>, PatchError> {
        let l = self.validate(host)?;
        let source = text(baseline, l.max_source_bytes)?;
        let m = &self.metadata;
        if baseline.len() != m.before_bytes
            || ContentHash::of(baseline).hex() != m.before_hash
            || source.ends_with('\n') != m.before_final_newline
        {
            return Err(PatchError::WrongBaseline);
        }
        let old = lines(source, l.max_lines)?;
        let (mut cursor, mut produced) = (0usize, 0usize);
        let mut out = Vec::new();
        let mut closed = false;
        let mut append = |s: &str| -> Result<(), PatchError> {
            if closed {
                return Err(PatchError::MalformedEncoding);
            }
            if out
                .len()
                .checked_add(s.len())
                .is_none_or(|n| n > l.max_output_bytes)
            {
                return Err(PatchError::ResourceLimit);
            }
            out.extend_from_slice(s.as_bytes());
            closed = !s.ends_with('\n');
            Ok(())
        };
        for h in &self.hunks {
            let start = range(h.old_start, h.old_count)?;
            if start < cursor || start > old.len() {
                return Err(PatchError::MalformedRange);
            }
            for s in &old[cursor..start] {
                append(s)?;
                produced += 1;
            }
            if range(h.new_start, h.new_count)? != produced {
                return Err(PatchError::MalformedRange);
            }
            cursor = start;
            for line in &h.lines {
                if line.kind != LineKind::Insert {
                    if old.get(cursor).copied() != Some(line.text.as_str()) {
                        return Err(PatchError::MissingContext);
                    }
                    cursor += 1;
                }
                if line.kind != LineKind::Delete {
                    append(&line.text)?;
                    produced += 1;
                }
            }
        }
        for s in &old[cursor..] {
            append(s)?;
            produced += 1;
        }
        if produced > l.max_lines {
            return Err(PatchError::ResourceLimit);
        }
        if out.len() != m.after_bytes
            || out.ends_with(b"\n") != m.after_final_newline
            || ContentHash::of(&out).hex() != m.after_hash
        {
            return Err(PatchError::WrongOutput);
        }
        Ok(out)
    }
    pub fn encode(&self, host: PatchLimits) -> Result<String, PatchError> {
        let l = self.validate(host)?;
        let mut out = String::new();
        let mut append = |s: &str| -> Result<(), PatchError> {
            if out
                .len()
                .checked_add(s.len())
                .is_none_or(|n| n > l.max_wire_bytes)
            {
                return Err(PatchError::ResourceLimit);
            }
            out.push_str(s);
            Ok(())
        };
        append(PREFIX)?;
        append(&serde_json::to_string(&self.metadata).map_err(|_| PatchError::InvalidMetadata)?)?;
        append("\n--- before\n+++ after\n")?;
        for h in &self.hunks {
            append(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ))?;
            for line in &h.lines {
                append(match line.kind {
                    LineKind::Context => " ",
                    LineKind::Delete => "-",
                    LineKind::Insert => "+",
                })?;
                append(&line.text)?;
                if !line.text.ends_with('\n') {
                    append("\n\\ No newline at end of file\n")?;
                }
            }
        }
        Ok(out)
    }
    pub fn decode(wire: &[u8], host: PatchLimits) -> Result<Self, PatchError> {
        let s = text(wire, host.max_wire_bytes)?;
        if !s.ends_with('\n') {
            return Err(PatchError::MalformedEncoding);
        }
        let mut iter = s.split_inclusive('\n');
        let header = iter
            .next()
            .and_then(|v| v.strip_prefix(PREFIX))
            .ok_or(PatchError::MalformedEncoding)?;
        // Fixed metadata has no content; cap it before invoking the JSON allocator.
        if header.len() > 2048 {
            return Err(PatchError::ResourceLimit);
        }
        let metadata: PatchMetadata =
            serde_json::from_str(header).map_err(|_| PatchError::InvalidMetadata)?;
        let mut patch = Self {
            metadata,
            hunks: Vec::new(),
        };
        let l = patch.validate(host)?;
        if wire.len() > l.max_wire_bytes {
            return Err(PatchError::ResourceLimit);
        }
        if iter.next() != Some("--- before\n") || iter.next() != Some("+++ after\n") {
            return Err(PatchError::MalformedEncoding);
        }
        let mut line_count = 0usize;
        let mut last_marker = false;
        for row in iter {
            if row.starts_with("@@ ") {
                if patch.hunks.len() == l.max_hunks {
                    return Err(PatchError::ResourceLimit);
                }
                let body = row
                    .strip_prefix("@@ -")
                    .and_then(|v| v.strip_suffix(" @@\n"))
                    .ok_or(PatchError::MalformedEncoding)?;
                let (old, new) = body.split_once(" +").ok_or(PatchError::MalformedEncoding)?;
                fn pair(s: &str) -> Result<(usize, usize), PatchError> {
                    let (a, b) = s.split_once(',').ok_or(PatchError::MalformedRange)?;
                    if [a, b]
                        .iter()
                        .any(|v| v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit()))
                    {
                        return Err(PatchError::MalformedRange);
                    }
                    Ok((
                        a.parse().map_err(|_| PatchError::MalformedRange)?,
                        b.parse().map_err(|_| PatchError::MalformedRange)?,
                    ))
                }
                let (old_start, old_count) = pair(old)?;
                let (new_start, new_count) = pair(new)?;
                patch.hunks.push(UnifiedHunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    lines: Vec::new(),
                });
                last_marker = false;
            } else if row == "\\ No newline at end of file\n" {
                if last_marker {
                    return Err(PatchError::MalformedEncoding);
                }
                let line = patch
                    .hunks
                    .last_mut()
                    .and_then(|h| h.lines.last_mut())
                    .ok_or(PatchError::MalformedEncoding)?;
                line.text.pop();
                last_marker = true;
            } else {
                line_count += 1;
                if line_count > l.max_lines.saturating_mul(2) {
                    return Err(PatchError::ResourceLimit);
                }
                let kind = match row.as_bytes()[0] {
                    b' ' => LineKind::Context,
                    b'-' => LineKind::Delete,
                    b'+' => LineKind::Insert,
                    _ => return Err(PatchError::MalformedEncoding),
                };
                patch
                    .hunks
                    .last_mut()
                    .ok_or(PatchError::MalformedEncoding)?
                    .lines
                    .push(PatchLine {
                        kind,
                        text: row[1..].into(),
                    });
                last_marker = false;
            }
        }
        patch.validate(host)?;
        Ok(patch)
    }
}
fn range(start: usize, count: usize) -> Result<usize, PatchError> {
    if count == 0 {
        Ok(start)
    } else {
        start.checked_sub(1).ok_or(PatchError::MalformedRange)
    }
}
