//! `[[Wikilink]]` extraction — the pure half of the internal-link feature.
//!
//! Scans a raw note body for `[[Target]]` / `[[Target|alias]]` spans, skipping
//! anything inside fenced code blocks or inline code spans and honoring
//! backslash escapes (`\[[` stays literal). Resolution to actual note ids is the
//! storage layer's job; this module only finds the link *targets*, so it stays
//! pure and headlessly testable.

/// One `[[…]]` reference found in a note body. `target` is the (raw, untrimmed-
/// by-caller) note title to link to; `alias` is the display text after `|`, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    pub target: String,
    pub alias: Option<String>,
}

/// Normalize a wikilink target / note title for matching: trimmed and ASCII-
/// lowercased. (Full Unicode case-folding is deferred — same simplification the
/// in-note search uses.)
pub fn normalize_title(title: &str) -> String {
    title.trim().to_lowercase()
}

/// Sentinel delimiters (Unicode private-use area) that wrap a wikilink's index
/// in the *masked* source produced by [`mask_wikilinks`]. They carry no
/// markdown meaning, so pulldown-cmark keeps them contiguous with the
/// surrounding text instead of fragmenting them the way it does `[[…]]`.
pub const WIKI_OPEN: char = '\u{E000}';
pub const WIKI_CLOSE: char = '\u{E001}';

/// Extract every `[[…]]` reference from `body`, in document order, skipping
/// fenced code blocks and inline code spans. Duplicate targets are returned as
/// they appear; de-duplication is the index's concern.
pub fn extract_wikilinks(body: &str) -> Vec<WikiLink> {
    mask_wikilinks(body).1
}

/// Replace each `[[…]]` reference (outside code spans/fences) with a bracket-
/// free sentinel `\u{E000}<index>\u{E001}` and return the masked source
/// alongside the links in index order. The preview renderer masks first so
/// wikilinks survive pulldown's inline tokenizer, then expands the sentinels
/// back into styled spans.
pub fn mask_wikilinks(body: &str) -> (String, Vec<WikiLink>) {
    let mut links = Vec::new();
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;

    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        // A line whose first non-space content is ``` or ~~~ toggles a fence.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        mask_line(line, &mut out, &mut links);
    }

    (out, links)
}

/// Scan a single (fence-free) line, copying it to `out` while replacing each
/// `[[…]]` with a sentinel and recording the link. Skips inline code spans and
/// backslash-escaped characters.
fn mask_line(line: &str, out: &mut String, links: &mut Vec<WikiLink>) {
    let mut in_code = false;
    let mut chars = line.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        match c {
            // Escape: copy both the backslash and the following char verbatim,
            // so pulldown still sees (and processes) the escape.
            '\\' => {
                out.push('\\');
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            }
            // Inline code span toggle.
            '`' => {
                in_code = !in_code;
                out.push('`');
            }
            '[' if !in_code && matches!(chars.peek(), Some(&(_, '['))) => {
                chars.next(); // consume the second '['
                let mut inner = String::new();
                let mut closed = false;
                while let Some((_, ic)) = chars.next() {
                    if ic == ']' && matches!(chars.peek(), Some(&(_, ']'))) {
                        chars.next(); // consume the second ']'
                        closed = true;
                        break;
                    }
                    inner.push(ic);
                }
                match (closed, parse_inner(&inner)) {
                    (true, Some(link)) => {
                        out.push(WIKI_OPEN);
                        out.push_str(&links.len().to_string());
                        out.push(WIKI_CLOSE);
                        links.push(link);
                    }
                    // Empty target, or unterminated `[[` — emit the raw text back.
                    (true, None) => {
                        out.push_str("[[");
                        out.push_str(&inner);
                        out.push_str("]]");
                    }
                    (false, _) => {
                        out.push_str("[[");
                        out.push_str(&inner);
                    }
                }
            }
            _ => out.push(c),
        }
    }
}

/// Parse the text between `[[` and `]]` into a `WikiLink`. Returns `None` for an
/// empty target (`[[]]`, `[[  ]]`, `[[|x]]`).
fn parse_inner(inner: &str) -> Option<WikiLink> {
    let (target, alias) = match inner.split_once('|') {
        Some((t, a)) => (t.trim(), Some(a.trim().to_string())),
        None => (inner.trim(), None),
    };
    if target.is_empty() {
        return None;
    }
    Some(WikiLink {
        target: target.to_string(),
        alias: alias.filter(|a| !a.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(body: &str) -> Vec<String> {
        extract_wikilinks(body).into_iter().map(|l| l.target).collect()
    }

    #[test]
    fn plain_and_aliased() {
        let links = extract_wikilinks("See [[Foo Bar]] and [[Baz|the baz]].");
        assert_eq!(
            links,
            vec![
                WikiLink { target: "Foo Bar".into(), alias: None },
                WikiLink { target: "Baz".into(), alias: Some("the baz".into()) },
            ]
        );
    }

    #[test]
    fn trims_inner_whitespace() {
        assert_eq!(targets("[[  Spaced  ]]"), vec!["Spaced".to_string()]);
        let aliased = extract_wikilinks("[[ T | a ]]");
        assert_eq!(aliased[0].target, "T");
        assert_eq!(aliased[0].alias.as_deref(), Some("a"));
    }

    #[test]
    fn skips_code_span_and_fence() {
        let body = "\
text [[Live]]
`[[InlineCode]]` stays literal
```
[[FencedOut]]
```
after [[Also]]";
        assert_eq!(targets(body), vec!["Live".to_string(), "Also".to_string()]);
    }

    #[test]
    fn honors_backslash_escape() {
        // `\[[Escaped]]` — the first bracket is escaped, so no link opens here.
        assert!(targets("\\[[Escaped]]").is_empty());
    }

    #[test]
    fn malformed_inputs_dont_panic_or_match() {
        assert!(targets("[[").is_empty());
        assert!(targets("[[]]").is_empty());
        assert!(targets("[[   ]]").is_empty());
        assert!(targets("[[|alias]]").is_empty());
        assert!(targets("single [bracket] only").is_empty());
        // Multibyte before an escape must not split a char boundary.
        assert_eq!(targets("é\\x [[Ünïcödé]]"), vec!["Ünïcödé".to_string()]);
    }

    #[test]
    fn normalize_title_trims_and_lowercases() {
        assert_eq!(normalize_title("  Foo Bar "), "foo bar");
    }
}
