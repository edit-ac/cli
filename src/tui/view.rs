//! Presentation only: shared session state is the authority for status.
use super::session::{Representation, TerminalSession};
use editac_session::session::SessionStore;
use ratatui::{Frame, layout::Rect, widgets::Paragraph};

#[derive(Clone, Debug)]
pub struct View {
    pub source: String,
    pub status: String,
    pub focused: bool,
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
        }
    }
}

/// Escape C0/C1, DEL and invisible directional/format controls. Newlines alone
/// become line boundaries; tabs/CR stay visibly escaped until position mapping
/// is implemented. Never pass untrusted escape bytes to a backend writer.
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
    if area.height > 0 {
        let focus = if view.focused { "focused" } else { "unfocused" };
        frame.render_widget(
            Paragraph::new(format!("{focus} | {}", escape_line(&view.status, width))),
            Rect::new(area.x, area.y + source_height, area.width, 1),
        );
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new("Esc / Ctrl-Q: quit | source shell; editing pending"),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}
