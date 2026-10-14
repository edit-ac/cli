//! Presentation only: shared session state is the authority for status.
use super::session::{Representation, TerminalSession};
use editac_session::session::SessionStore;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

#[derive(Clone, Debug)]
pub struct View {
    pub source: String,
    pub status: String,
    pub focused: bool,
    pub input: Option<super::input::InputState>,
    pub input_message: Option<String>,
    pub draft_changed: bool,
    pub review: Option<super::diff::Review>,
}
impl View {
    pub fn from_session<S: SessionStore>(session: &TerminalSession<S>, now: u64) -> Self {
        let state = session.service.session().state(now);
        let capabilities = session.service.capabilities(now);
        let representation = match session.representation {
            Representation::ExactSource => "exact source",
            Representation::HostedProjection => "hosted projection",
        };
        Self {
            source: session.source.clone(),
            status: format!(
                "{representation} | {:?} | local {:?} | remote {:?} | pending {} | {}",
                state.permission,
                state.local,
                state.remote,
                state.pending_upload,
                if capabilities.can_apply {
                    "write capability"
                } else {
                    "read only"
                }
            ),
            focused: true,
            input: Some(super::input::InputState::new(
                capabilities.can_apply,
                session.source.len(),
            )),
            input_message: None,
            draft_changed: false,
            review: None,
        }
    }
}

/// Escape C0/C1, DEL and invisible directional/format controls. Newlines alone
/// become line boundaries; input mapping expands tabs and treats CRLF as a unit. Never pass untrusted escape bytes to a backend writer.
pub fn escape_line(line: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for c in line.chars().take(max_chars) {
        match c {
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
            {
                if u32::from(c) <= 0xff {
                    out.push_str(&format!("\\x{:02x}", u32::from(c)));
                } else {
                    out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Bottom two rows are status/help. Tiny frames prioritize status then help;
/// source is clipped without wrapping. Rendering cost is bounded by the viewport.
pub fn render(frame: &mut Frame<'_>, view: &View) {
    let area = frame.area();
    let source_height = area.height.saturating_sub(2);
    let width = usize::from(area.width);
    if source_height > 0 {
        if let Some(review) = &view.review {
            let mut rows = vec![
                review.label.as_str(),
                review.base_label.as_str(),
                review.proposed_label.as_str(),
                review.limitation.as_str(),
            ];
            rows.extend(review.page(usize::from(source_height.saturating_sub(4))));
            let rows = rows
                .into_iter()
                .take(usize::from(source_height))
                .map(|row| Line::raw(escape_line(row, width)))
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(rows),
                Rect::new(area.x, area.y, area.width, source_height),
            );
        } else if let Some(input) = &view.input {
            let map = super::positions::SourceMap::new(&view.source);
            let (row, column) = map.locate(input.caret).unwrap_or((0, 0));
            let top = row.saturating_sub(usize::from(source_height) - 1);
            let left = column.saturating_sub(width.saturating_sub(1));
            let selection = input.selection();
            let lines: Vec<_> = map
                .lines
                .iter()
                .skip(top)
                .take(usize::from(source_height))
                .map(|line| {
                    let mut spans = Vec::new();
                    let mut shown = left;
                    for glyph in &line.glyphs {
                        if glyph.column + glyph.width <= left {
                            continue;
                        }
                        if glyph.column >= left + width {
                            break;
                        }
                        let selected = glyph.start < selection.end && glyph.end > selection.start;
                        let style = if selected {
                            Style::default().bg(Color::Blue).fg(Color::White)
                        } else {
                            Style::default()
                        };
                        // Partial wide clusters/escapes become blank cells; never emit
                        // half a grapheme or place the following glyph at the wrong column.
                        let end = (glyph.column + glyph.width).min(left + width);
                        let start = glyph.column.max(left);
                        if start > shown {
                            spans.push(Span::raw(" ".repeat(start - shown)));
                        }
                        let text =
                            if glyph.column < left || glyph.column + glyph.width > left + width {
                                " ".repeat(end - start)
                            } else {
                                glyph.text.clone()
                            };
                        spans.push(Span::styled(text, style));
                        shown = end;
                    }
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines),
                Rect::new(area.x, area.y, area.width, source_height),
            );
            if view.focused && width > 0 {
                frame.set_cursor_position((
                    area.x + (column - left) as u16,
                    area.y + (row - top) as u16,
                ));
            }
        } else {
            let source = view
                .source
                .split('\n')
                .take(usize::from(source_height))
                .map(|line| escape_line(line, width))
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(source),
                Rect::new(area.x, area.y, area.width, source_height),
            );
        }
    }
    if area.height > 0 {
        let focus = if view.focused { "focused" } else { "unfocused" };
        frame.render_widget(
            Paragraph::new(format!(
                "{focus} | {}{}{}",
                escape_line(&view.status, width),
                if view.draft_changed {
                    " | draft (not committed)"
                } else {
                    ""
                },
                view.input_message
                    .as_ref()
                    .map(|m| format!(" | {}", escape_line(m, width)))
                    .unwrap_or_default()
            )),
            Rect::new(area.x, area.y + source_height, area.width, 1),
        );
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(if view.review.is_some() {
                "Up/Down/Pg: review | Ctrl-Enter: apply | Ctrl-E: export | Esc: return"
            } else {
                "Ctrl: S save Z/Y history R recover D diff E export | Esc quit"
            }),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}
