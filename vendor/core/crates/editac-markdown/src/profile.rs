//! Named grammar switches. GFM callouts, math and other optional syntax remain off.
use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::ops::Range;
pub const PROFILE: &str = "edit-ac-gfm-v1";
pub fn options() -> Options {
    Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES
}

fn domain(s: &str) -> bool {
    let parts: Vec<_> = s.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        && parts[parts.len() - 2..].iter().all(|p| !p.contains('_'))
}

// GFM 0.29 extended autolink path rules, applied only to text events outside
// existing links, images and code. No scanning of remote or live documents.
fn autolink(s: &str) -> Option<(usize, String)> {
    let prefix = ["https://", "http://", "ftp://", "www."]
        .into_iter()
        .find(|p| s.starts_with(p));
    if let Some(prefix) = prefix {
        let mut end = s
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(s.len());
        loop {
            let old = end;
            while end > 0 && "?!.,:*_~".contains(s.as_bytes()[end - 1] as char) {
                end -= 1;
            }
            if s[..end].ends_with(')') {
                let opens = s[..end].bytes().filter(|b| *b == b'(').count();
                let mut closes = s[..end].bytes().filter(|b| *b == b')').count();
                while closes > opens && s[..end].ends_with(')') {
                    end -= 1;
                    closes -= 1;
                }
            }
            if s[..end].ends_with(';') {
                if let Some(i) = s[..end - 1].rfind('&') {
                    if i + 1 < end - 1
                        && s[i + 1..end - 1].bytes().all(|b| b.is_ascii_alphanumeric())
                    {
                        end = i;
                    }
                }
            }
            if end == old {
                break;
            }
        }
        let host_start = if prefix == "www." { 0 } else { prefix.len() };
        if end <= host_start {
            return None;
        }
        let tail = &s[host_start..end];
        let host = tail.split(['/', ':', '?', '#']).next().unwrap_or("");
        if !domain(host) {
            return None;
        }
        let label = &s[..end];
        return Some((
            end,
            if prefix == "www." {
                format!("http://{label}")
            } else {
                label.into()
            },
        ));
    }
    let local = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || b".-_+".contains(b))
        .count();
    if local == 0 || s.as_bytes().get(local) != Some(&b'@') {
        return None;
    }
    let mut end = local
        + 1
        + s[local + 1..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || b".-_".contains(b))
            .count();
    while end > local + 1 && s.as_bytes()[end - 1] == b'.' {
        end -= 1;
    }
    let host = &s[local + 1..end];
    if !host.contains('.') || host.ends_with(['-', '_']) || host.split('.').any(str::is_empty) {
        return None;
    }
    Some((end, format!("mailto:{}", &s[..end])))
}

pub fn events(source: &str) -> Vec<(Event<'_>, Range<usize>)> {
    let mut result = Vec::new();
    let mut excluded = 0usize;
    // Entities and punctuation can split adjacent text events. Autolink rules
    // apply to the whole text node, never to a prematurely shortened prefix.
    let mut input: Vec<(Event<'_>, Range<usize>)> = Vec::new();
    let mut strikes = Vec::new();
    for (mut event, span) in Parser::new_ext(source, options()).into_offset_iter() {
        if matches!(event, Event::Start(Tag::Strikethrough)) {
            let enabled = source[span.clone()].starts_with("~~");
            strikes.push(enabled);
            if !enabled {
                event = Event::Text("~".into());
            }
        } else if matches!(event, Event::End(TagEnd::Strikethrough))
            && !strikes.pop().unwrap_or(true)
        {
            event = Event::Text("~".into());
        }
        if let Event::Text(text) = &event {
            if let Some((Event::Text(previous), previous_span)) = input.last_mut() {
                *previous = CowStr::from(format!("{previous}{text}"));
                previous_span.end = span.end;
                continue;
            }
        }
        input.push((event, span));
    }
    for (event, span) in input {
        match &event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. } | Tag::CodeBlock(_)) => excluded += 1,
            Event::End(TagEnd::Link | TagEnd::Image | TagEnd::CodeBlock) => excluded -= 1,
            _ => {}
        }
        if let Event::Text(text) = &event {
            if excluded == 0 {
                let mut from = 0;
                let mut at = 0;
                while at < text.len() {
                    let prior = text[..at].chars().next_back();
                    let boundary = prior.is_none_or(|c| c.is_whitespace() || "*_~(".contains(c));
                    let email_boundary =
                        prior.is_none_or(|c| !c.is_ascii_alphanumeric() && !".-_+".contains(c));
                    if (boundary || email_boundary)
                        && let Some((len, href)) = autolink(&text[at..])
                    {
                        if boundary || href.starts_with("mailto:") {
                            if from < at {
                                result.push((
                                    Event::Text(CowStr::from(text[from..at].to_string())),
                                    span.clone(),
                                ));
                            }
                            result.push((
                                Event::Start(Tag::Link {
                                    link_type: LinkType::Autolink,
                                    dest_url: href.into(),
                                    title: "".into(),
                                    id: "".into(),
                                }),
                                span.clone(),
                            ));
                            result.push((
                                Event::Text(CowStr::from(text[at..at + len].to_string())),
                                span.clone(),
                            ));
                            result.push((Event::End(TagEnd::Link), span.clone()));
                            at += len;
                            from = at;
                            continue;
                        }
                    }
                    at += text[at..].chars().next().unwrap().len_utf8();
                }
                if from < text.len() {
                    result.push((Event::Text(CowStr::from(text[from..].to_string())), span));
                }
                continue;
            }
        }
        result.push((event, span));
    }
    result
}
