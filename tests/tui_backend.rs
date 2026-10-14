//! Backend selection probe only. No TTY, event reader, network or session is opened.
use std::io::{self, Write};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::{Buffer, Cell},
    widgets::Paragraph,
};

#[test]
fn headless_frame_and_resize_use_the_selected_renderer() -> Result<(), std::convert::Infallible> {
    let mut terminal = Terminal::new(TestBackend::new(12, 2))?;
    terminal.draw(|frame| {
        frame.render_widget(Paragraph::new("source\nlocal only"), frame.area());
    })?;
    assert_eq!(
        terminal.backend().buffer(),
        &Buffer::with_lines(["source      ", "local only  "])
    );

    // Same event type as the eventual reader; TestBackend supplies dimensions
    // without querying the process's controlling terminal.
    let Event::Resize(width, height) = Event::Resize(8, 3) else {
        unreachable!()
    };
    terminal.backend_mut().resize(width, height);
    terminal.autoresize()?;
    terminal.draw(|frame| {
        frame.render_widget(Paragraph::new("source\nlocal\nhelp"), frame.area());
    })?;
    assert_eq!(
        terminal.backend().buffer(),
        &Buffer::with_lines(["source  ", "local   ", "help    "])
    );
    Ok(())
}

#[test]
fn prepared_input_retains_key_kind_paste_and_focus() {
    let events = [
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        )),
        Event::Paste("e\u{301}\n👩\u{200d}💻\r\n\t".into()),
        Event::FocusLost,
        Event::FocusGained,
    ];
    assert!(matches!(&events[0], Event::Key(key)
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL
            && key.kind == KeyEventKind::Press));
    assert!(matches!(&events[1], Event::Key(key) if key.kind == KeyEventKind::Release));
    assert!(matches!(&events[2], Event::Paste(text) if text == "e\u{301}\n👩\u{200d}💻\r\n\t"));
    assert_eq!(events[3..], [Event::FocusLost, Event::FocusGained]);
    // This proves event representation, not OS key decoding or paste bounds.
}

#[test]
fn crossterm_draw_and_input_protocols_work_with_an_in_memory_writer() -> io::Result<()> {
    let mut bytes = Vec::new();
    {
        let mut backend = CrosstermBackend::new(&mut bytes);
        let cell = Cell::new("X");
        backend.draw([(0, 0, &cell)].into_iter())?;
        Backend::flush(&mut backend)?;
    }
    assert!(bytes.contains(&b'X'));
    assert!(bytes.windows(2).any(|pair| pair == b"\x1b["));

    let mut protocol = Vec::new();
    crossterm::queue!(
        protocol,
        EnableBracketedPaste,
        EnableFocusChange,
        DisableFocusChange,
        DisableBracketedPaste
    )?;
    assert_eq!(protocol, b"\x1b[?2004h\x1b[?1004h\x1b[?1004l\x1b[?2004l");
    Ok(())
}

#[test]
fn crossterm_draw_propagates_output_failure() {
    struct BrokenOutput;
    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "prepared failure",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut backend = CrosstermBackend::new(BrokenOutput);
    let cell = Cell::new("X");
    let error = backend.draw([(0, 0, &cell)].into_iter()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}
