//! Logical grapheme positions. Source bytes, public UTF-16 and display columns
//! remain separate; bidi is shown in logical order with format controls escaped.
use editac_core::{
    Utf16Offset,
    anchors::{self, BoundaryError},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Glyph {
    pub start: Utf16Offset,
    pub end: Utf16Offset,
    pub column: usize,
    pub width: usize,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct Line {
    pub start: Utf16Offset,
    pub end: Utf16Offset,
    pub width: usize,
    pub glyphs: Vec<Glyph>,
}
#[derive(Clone, Debug)]
pub struct SourceMap {
    pub lines: Vec<Line>,
}
impl SourceMap {
    pub fn new(source: &str) -> Self {
        let mut lines = Vec::new();
        let mut line = Line {
            start: Utf16Offset(0),
            end: Utf16Offset(0),
            width: 0,
            glyphs: vec![],
        };
        let mut units = 0u32;
        for grapheme in source.graphemes(true) {
            let start = Utf16Offset(units);
            units += grapheme.encode_utf16().count() as u32;
            let end = Utf16Offset(units);
            if matches!(grapheme, "\n" | "\r\n") {
                line.end = start;
                lines.push(line);
                line = Line {
                    start: end,
                    end,
                    width: 0,
                    glyphs: vec![],
                };
                continue;
            }
            let mut text = if grapheme == "\t" {
                " ".repeat(4 - line.width % 4)
            } else {
                super::view::escape_line(grapheme, usize::MAX)
            };
            // Standalone zero-width clusters have a visible cell and caret stop.
            if text.width() == 0 {
                text.insert(0, '\u{25cc}');
            }
            let width = text.width();
            line.glyphs.push(Glyph {
                start,
                end,
                column: line.width,
                width,
                text,
            });
            line.width += width;
            line.end = end;
        }
        lines.push(line);
        Self { lines }
    }
    pub fn locate(&self, offset: Utf16Offset) -> Result<(usize, usize), BoundaryError> {
        for (row, line) in self.lines.iter().enumerate() {
            if offset == line.end {
                return Ok((row, line.width));
            }
            if let Some(g) = line.glyphs.iter().find(|g| g.start == offset) {
                return Ok((row, g.column));
            }
            if offset == line.start {
                return Ok((row, 0));
            }
        }
        Err(BoundaryError::NotGraphemeBoundary)
    }
    /// Floor to a grapheme start; never place a caret inside a wide glyph/tab.
    pub fn at_column(&self, row: usize, column: usize) -> Utf16Offset {
        let line = &self.lines[row.min(self.lines.len() - 1)];
        if column >= line.width {
            return line.end;
        }
        line.glyphs
            .iter()
            .rev()
            .find(|g| g.column <= column)
            .map_or(line.start, |g| g.start)
    }
}
pub fn byte_to_offset(source: &str, byte: usize) -> Result<Utf16Offset, BoundaryError> {
    anchors::byte_to_utf16(source, byte)
}
pub fn offset_to_byte(source: &str, offset: Utf16Offset) -> Result<usize, BoundaryError> {
    anchors::utf16_to_byte(source, offset)
}
/// Inserting a combining/joining sequence can join the following cluster. Move
/// to the first complete grapheme boundary at/after the inserted byte range.
pub fn after_insert(source: &str, byte: usize) -> Result<Utf16Offset, BoundaryError> {
    let boundary = source
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(source.len()))
        .find(|i| *i >= byte)
        .ok_or(BoundaryError::OutOfBounds)?;
    byte_to_offset(source, boundary)
}
