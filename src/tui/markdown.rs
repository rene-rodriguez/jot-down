//! Markdown rendering for the preview pane.
//!
//! The editor keeps source markdown plain; this module turns that source into
//! styled, already-wrapped Ratatui lines for reading in the preview pane.
//!
//! # Pipeline
//!
//! 1. **Pre-passes** (source-level): front-matter strip, abbreviation extraction,
//!    definition-list detection, custom-container extraction.
//! 2. **Pulldown parse** with expanded options (strikethrough, tables, tasklists,
//!    footnotes, smart punctuation, YAML/+++ metadata blocks).
//! 3. **Inline text post-passes** (on `Event::Text` only, never code/URLs):
//!    emoji shortcodes → typographer symbols → `==mark==` → `++ins++` →
//!    super/subscript.
//! 4. **Wrap** to pane width with hanging indents.

use std::borrow::Cow;

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::wrap;

#[cfg(feature = "syntax-highlight")]
use std::sync::OnceLock;

pub use crate::config::LinkUrlMode;

/// Options that affect preview rendering.
#[derive(Debug, Clone)]
pub struct PreviewOptions {
    /// When false, markdown source is rendered as plain wrapped text.
    pub render_markdown: bool,
    /// How to display link URLs.
    pub link_urls: LinkUrlMode,
    /// Enable smart quotes, dashes, and ellipsis.
    pub typographer: bool,
    /// Enable :shortcode: emoji substitution.
    pub emoji: bool,
    /// Enable ==mark== highlighting.
    pub mark: bool,
    /// Enable ++ins++ as underlined text.
    pub ins: bool,
    /// Enable ^super^script and ~sub~script.
    pub sup_sub: bool,
    /// Strip `*[ABBR]: expansion` lines from source.
    pub abbreviations: bool,
    /// Render definition lists (`term\n:   def`).
    pub definition_lists: bool,
    /// Render `::: type` custom container callouts.
    pub custom_containers: bool,
    /// Style bare URLs in running text.
    pub linkify: bool,
    /// Render `[[wikilinks]]` as styled internal links (live vs dangling).
    pub wikilinks: bool,
    /// Convert `$…$` / `$$…$$` LaTeX-ish math to Unicode (Greek, symbols,
    /// super/subscripts). Off by default — keep literal `$` (e.g. prices) intact.
    pub math: bool,
    /// Name of the syntect theme for code-block highlighting (only consulted in
    /// `syntax-highlight` builds; falls back to `base16-ocean.dark`).
    #[cfg_attr(not(feature = "syntax-highlight"), allow(dead_code))]
    pub syntax_theme: String,
    /// Render local images as half-block cells instead of a placeholder (only
    /// honored in `images` builds).
    #[cfg_attr(not(feature = "images"), allow(dead_code))]
    pub images: bool,
    /// Render fenced `mermaid` flowcharts as ASCII diagrams (falls back to the
    /// raw source for unsupported diagram types).
    pub mermaid: bool,
    /// Index of the fenced code block to highlight as "focused" (for the
    /// preview's code-block actions). `None` highlights nothing.
    pub focused_code_block: Option<usize>,
    /// Index (document order) of the wikilink to highlight as "focused" for
    /// navigation. `None` highlights nothing.
    pub focused_wikilink: Option<usize>,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            render_markdown: true,
            link_urls: LinkUrlMode::default(),
            typographer: true,
            emoji: true,
            mark: true,
            ins: true,
            sup_sub: true,
            abbreviations: true,
            definition_lists: true,
            custom_containers: true,
            linkify: false,
            wikilinks: false,
            math: false,
            syntax_theme: "base16-ocean.dark".to_string(),
            images: false,
            mermaid: true,
            focused_code_block: None,
            focused_wikilink: None,
        }
    }
}

/// A fenced code block extracted from note source, for the preview's code-block
/// actions (copy / run). `end_byte` is the source byte offset just past the
/// block (its closing fence), used to splice captured output back into the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub lang: Option<String>,
    pub code: String,
    pub end_byte: usize,
}

impl CodeBlock {
    /// Whether this block is runnable as a shell command (shell-ish language or
    /// no language tag at all).
    pub fn is_runnable(&self) -> bool {
        match self.lang.as_deref() {
            None => true,
            Some(l) => matches!(l, "sh" | "bash" | "zsh" | "shell" | "console"),
        }
    }

    /// A short, single-line label for status/confirm prompts.
    pub fn label(&self) -> String {
        let lang = self.lang.as_deref().unwrap_or("shell");
        let first = self.code.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            format!("[{lang}] (empty)")
        } else {
            format!("[{lang}] {first}")
        }
    }
}

/// Extract fenced code blocks from markdown source, in document order. Walks the
/// pulldown parser with byte offsets so each block records the source position
/// just past its closing fence. Pure and panic-free on malformed input.
pub fn extract_code_blocks(source: &str) -> Vec<CodeBlock> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut blocks: Vec<CodeBlock> = Vec::new();
    let mut current: Option<(Option<String>, String)> = None;

    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => {
                        Some(lang.trim().to_string())
                    }
                    _ => None,
                };
                current = Some((lang, String::new()));
            }
            Event::Text(text) => {
                if let Some((_, code)) = current.as_mut() {
                    code.push_str(text.as_ref());
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, mut code)) = current.take() {
                    // Drop a single trailing newline so re-emitted source is tidy.
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    blocks.push(CodeBlock {
                        lang,
                        code,
                        end_byte: range.end,
                    });
                }
            }
            _ => {}
        }
    }

    blocks
}

/// Splice captured run output into `source` immediately after the code block
/// whose closing fence is at `block_end`. If an ` ```output ` fence already
/// follows the block (from a prior run), it is replaced; otherwise a fresh
/// output block is inserted. Pure and tested.
pub fn insert_or_replace_output(source: &str, block_end: usize, output: &str) -> String {
    let block_end = block_end.min(source.len());
    let head = &source[..block_end];
    let new_block = format!("\n\n```output\n{}\n```\n", output.trim_end_matches('\n'));

    // From the tail, skip leading blank lines to find the first content line.
    let tail = &source[block_end..];
    let mut consumed = 0usize;
    for line in tail.split_inclusive('\n') {
        if line.trim().is_empty() {
            consumed += line.len();
        } else {
            break;
        }
    }
    let content = &tail[consumed..];

    // If a prior ```output block is adjacent, replace through its closing fence.
    if content.starts_with("```output") {
        if let Some(close_rel) = find_closing_fence(content) {
            let fence_end = block_end + consumed + close_rel;
            return format!("{}{}{}", head, new_block, &source[fence_end..]);
        }
    }

    format!("{}{}{}", head, new_block, tail)
}

/// Given source starting at an opening ```output fence, return the byte offset
/// just past the matching closing ``` fence (including its trailing newline).
fn find_closing_fence(s: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut first = true;
    loop {
        let line_end = s[offset..].find('\n').map(|i| offset + i + 1);
        let (line, next) = match line_end {
            Some(end) => (&s[offset..end], end),
            None => (&s[offset..], s.len()),
        };
        if !first && line.trim() == "```" {
            return Some(next);
        }
        first = false;
        if next >= s.len() {
            // Unterminated fence — treat the rest as the block.
            return Some(s.len());
        }
        offset = next;
    }
}

/// A heading collected during rendering, for outline / current-section features.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Heading level, 1–6.
    pub level: u8,
    /// Plain (unstyled) heading text.
    pub text: String,
    /// Index of this heading's first output row within `lines`.
    pub row: usize,
}

/// Rich result of rendering markdown: the styled lines plus positional metadata
/// the preview uses for code-block focus, cursor-synced scrolling, current-
/// section highlighting, and outline navigation.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    /// Styled, pre-wrapped output rows.
    pub lines: Vec<Line<'static>>,
    /// First output-row index of each fenced code block, in document order.
    pub code_rows: Vec<usize>,
    /// For each 0-based source line, the index of its first output row in
    /// `lines`. Lets the editor map its cursor line to a preview row for synced
    /// scrolling. Indexed against the pre-pass-transformed source, so it is exact
    /// unless a line-count-changing pre-pass (abbreviations, definition lists,
    /// custom containers) applies — those only shift the mapping approximately.
    pub source_line_rows: Vec<usize>,
    /// Headings in document order, for outline + current-section features.
    pub headings: Vec<Heading>,
}

/// Render markdown source into styled, pre-wrapped Ratatui lines.
pub fn render(source: &str, width: u16, opts: &PreviewOptions) -> Vec<Line<'static>> {
    render_with_code_rows(source, width, opts).0
}

/// Like [`render`], but also returns the first output-row index of each fenced
/// code block (in document order). The preview uses these to scroll a focused
/// code block into view.
pub fn render_with_code_rows(
    source: &str,
    width: u16,
    opts: &PreviewOptions,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let out = render_full(source, width, opts, &std::collections::HashSet::new());
    (out.lines, out.code_rows)
}

/// Like [`render_with_code_rows`], but with a `link_targets` set of normalized
/// (see [`crate::notes::wikilinks::normalize_title`]) titles of notes that
/// exist, so `[[wikilinks]]` can be styled as live (resolvable) vs dangling.
/// Only consulted when `opts.wikilinks` is true.
pub fn render_full(
    source: &str,
    width: u16,
    opts: &PreviewOptions,
    link_targets: &std::collections::HashSet<String>,
) -> RenderOutput {
    let width = usize::from(width.max(1));
    if !opts.render_markdown {
        return RenderOutput {
            lines: render_plain(source, width),
            ..RenderOutput::default()
        };
    }

    // Pre-passes (source-level transforms).
    let source = strip_abbreviations(source, opts.abbreviations);
    let source = strip_definition_lists(&source, opts.definition_lists);
    let source = convert_custom_containers(&source, opts.custom_containers);
    let source = convert_github_callouts(&source, opts.custom_containers);
    let source = convert_math(&source, opts.math);

    // Mask `[[wikilinks]]` into bracket-free sentinels before parsing, so they
    // survive pulldown's inline tokenizer; the renderer expands them back into
    // styled spans. Skipped (no allocation, no table) when wikilinks are off.
    let (source, wikilink_table) = if opts.wikilinks {
        let (masked, table) = crate::notes::wikilinks::mask_wikilinks(&source);
        (Cow::Owned(masked), table)
    } else {
        (source, Vec::new())
    };

    Renderer::new(width, opts.clone(), link_targets.clone(), wikilink_table).render(&source)
}

// ── Pre-passes ──────────────────────────────────────────────────────────────

/// Remove `*[ABBR]: expansion` abbreviation definition lines from source.
fn strip_abbreviations(source: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled {
        return Cow::Borrowed(source);
    }
    let mut result = String::with_capacity(source.len());
    let mut changed = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("*[") && trimmed.contains("]:") {
            changed = true;
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    if changed {
        // Preserve trailing newline if original had one.
        if !source.ends_with('\n') {
            result.truncate(result.len().saturating_sub(1));
        }
        Cow::Owned(result)
    } else {
        Cow::Borrowed(source)
    }
}

/// Convert GFM definition list syntax (`term\n:   def`) to raw HTML `<dl>`
/// blocks so pulldown-cmark passes them through as `Event::Html`.
fn strip_definition_lists(source: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled {
        return Cow::Borrowed(source);
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    let mut consumed = vec![false; lines.len()];

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty()
            || lines.get(i + 1).is_none_or(|n| {
                let n = n.trim();
                !(n.starts_with(": ") || n.starts_with("~ ") || n.starts_with(":\t") || n.starts_with("~\t"))
            })
        {
            i += 1;
            continue;
        }

        // Term line and at least one definition line found.
        consumed[i] = true;
        i += 1;
        while i < lines.len() {
            let n = lines[i].trim();
            let is_def = n.starts_with(": ")
                || n.starts_with("~ ")
                || n.starts_with(":\t")
                || n.starts_with("~\t");
            let is_blank_before_def = n.is_empty()
                && lines.get(i + 1).is_some_and(|n| {
                    let n = n.trim();
                    n.starts_with(": ") || n.starts_with("~ ")
                });
            if is_def || is_blank_before_def {
                consumed[i] = true;
                i += 1;
            } else {
                break;
            }
        }
    }

    if !consumed.iter().any(|&c| c) {
        return Cow::Borrowed(source);
    }

    let mut result = String::with_capacity(source.len());
    let mut in_dl = false;
    for (i, line) in lines.iter().enumerate() {
        if consumed[i] {
            if !in_dl {
                result.push_str("<dl>\n");
                in_dl = true;
            }
            let trimmed = line.trim();
            if trimmed.starts_with(": ") || trimmed.starts_with("~ ") || trimmed.starts_with(":\t") || trimmed.starts_with("~\t") {
                result.push_str("<dd>");
                result.push_str(trimmed[1..].trim());
                result.push_str("</dd>\n");
            } else {
                result.push_str("<dt>");
                result.push_str(line);
                result.push_str("</dt>\n");
            }
        } else {
            if in_dl {
                result.push_str("</dl>\n");
                in_dl = false;
            }
            result.push_str(line);
            result.push('\n');
        }
    }
    if in_dl {
        result.push_str("</dl>\n");
    }

    // Preserve trailing newline.
    if !source.ends_with('\n') {
        result.truncate(result.len().saturating_sub(1));
    }

    Cow::Owned(result)
}

/// Convert `::: type` custom container fences to HTML comments so pulldown-cmark
/// passes them through as `Event::Html`.
fn convert_custom_containers(source: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled {
        return Cow::Borrowed(source);
    }

    let mut result = String::with_capacity(source.len());
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    let mut changed = false;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("::: ") && trimmed.len() > 4 {
            // Opening fence: `::: warning`
            changed = true;
            let kind = trimmed[4..].trim().to_lowercase();
            result.push_str("<!-- CUSTOM_CONTAINER_START:");
            result.push_str(&kind);
            result.push_str(" -->\n");
            i += 1;

            // Collect inner lines until closing `:::`.
            while i < lines.len() {
                let inner = lines[i].trim();
                if inner == ":::" {
                    changed = true;
                    result.push_str("<!-- CUSTOM_CONTAINER_END -->\n");
                    i += 1;
                    break;
                }
                result.push_str(lines[i]);
                result.push('\n');
                i += 1;
            }
        } else {
            result.push_str(lines[i]);
            result.push('\n');
            i += 1;
        }
    }

    if changed {
        if !source.ends_with('\n') {
            result.truncate(result.len().saturating_sub(1));
        }
        Cow::Owned(result)
    } else {
        Cow::Borrowed(source)
    }
}

/// Convert GitHub-style alert blockquotes (`> [!NOTE]` followed by `>` lines)
/// into the same HTML container markers as `:::` callouts, so they render
/// through [`Renderer::finish_callout`]. Reuses the `custom_containers` toggle.
fn convert_github_callouts(source: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled || !source.contains("[!") {
        return Cow::Borrowed(source);
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut result = String::with_capacity(source.len());
    let mut i = 0;
    let mut changed = false;

    while i < lines.len() {
        if let Some(kind) = github_alert_kind(lines[i]) {
            changed = true;
            result.push_str("<!-- CUSTOM_CONTAINER_START:");
            result.push_str(&kind);
            result.push_str(" -->\n");
            i += 1;
            // The remaining `>`-prefixed lines are the callout body, de-quoted.
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some(rest) = t.strip_prefix('>') {
                    result.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    result.push('\n');
                    i += 1;
                } else {
                    break;
                }
            }
            result.push_str("<!-- CUSTOM_CONTAINER_END -->\n");
        } else {
            result.push_str(lines[i]);
            result.push('\n');
            i += 1;
        }
    }

    if changed {
        if !source.ends_with('\n') {
            result.truncate(result.len().saturating_sub(1));
        }
        Cow::Owned(result)
    } else {
        Cow::Borrowed(source)
    }
}

/// If `line` is a GitHub alert marker (`> [!NOTE]` alone on the line), return the
/// lowercased alert type (`note`, `warning`, …).
fn github_alert_kind(line: &str) -> Option<String> {
    let after = line.trim_start().strip_prefix('>')?.trim();
    let inner = after.strip_prefix("[!")?.strip_suffix(']')?;
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(inner.to_ascii_lowercase())
}

/// Convert `$…$` and `$$…$$` math spans to Unicode in source, skipping fenced
/// code blocks and inline code. Uses the CommonMark-ish rule that an inline `$`
/// pair must not have whitespace just inside the delimiters, so prices like
/// "$5 and $10" are left untouched.
fn convert_math(source: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled || !source.contains('$') {
        return Cow::Borrowed(source);
    }
    let mut result = String::with_capacity(source.len());
    let mut changed = false;
    let mut in_fence = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence {
            let (converted, did) = convert_math_in_line(line);
            changed |= did;
            result.push_str(&converted);
            result.push('\n');
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    if changed {
        if !source.ends_with('\n') {
            result.truncate(result.len().saturating_sub(1));
        }
        Cow::Owned(result)
    } else {
        Cow::Borrowed(source)
    }
}

/// Convert math spans within a single line, skipping inline-code (backtick)
/// regions. Returns the rewritten line and whether anything changed.
fn convert_math_in_line(line: &str) -> (String, bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut changed = false;
    let mut in_code = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            in_code = !in_code;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '$' && !in_code {
            let double = chars.get(i + 1) == Some(&'$');
            let content_start = i + if double { 2 } else { 1 };
            // Inline `$` must not be immediately followed by whitespace.
            let opens = double
                || chars
                    .get(content_start)
                    .is_some_and(|c| !c.is_whitespace());
            if opens {
                if let Some((end, inner)) = find_math_close(&chars, content_start, double) {
                    out.push_str(&latex_to_unicode(&inner));
                    changed = true;
                    i = end;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    (out, changed)
}

/// Find the closing `$`/`$$` for a math span opened at `start`, returning the
/// index just past the closer and the inner text. An inline closer must be
/// preceded by a non-space and enclose non-empty content.
fn find_math_close(chars: &[char], start: usize, double: bool) -> Option<(usize, String)> {
    let mut j = start;
    while j < chars.len() {
        if double {
            if chars[j] == '$' && chars.get(j + 1) == Some(&'$') {
                return (j > start).then(|| (j + 2, chars[start..j].iter().collect()));
            }
        } else if chars[j] == '$' && j > start && !chars[j - 1].is_whitespace() {
            return Some((j + 1, chars[start..j].iter().collect()));
        }
        j += 1;
    }
    None
}

/// Translate a LaTeX-ish math fragment to Unicode: Greek/symbol commands,
/// `^`/`_` super/subscripts (single char or `{group}`), and `\frac{a}{b}`.
fn latex_to_unicode(inner: &str) -> String {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let name: String = chars[start..j].iter().collect();
                if name == "frac" {
                    let (num, k1) = read_brace_group(&chars, j);
                    let (den, k2) = read_brace_group(&chars, k1);
                    out.push_str(&latex_to_unicode(&num));
                    out.push('⁄');
                    out.push_str(&latex_to_unicode(&den));
                    i = k2;
                } else if let Some(sym) = latex_command(&name) {
                    out.push_str(sym);
                    i = j;
                } else if name.is_empty() {
                    // Backslash + non-letter (e.g. `\,` spacing, `\\`): thin space.
                    out.push(' ');
                    i = (j + 1).min(chars.len());
                } else {
                    out.push_str(&name);
                    i = j;
                }
            }
            '^' => {
                let (grp, k) = read_brace_group(&chars, i + 1);
                out.push_str(&to_superscript_str(&grp));
                i = k;
            }
            '_' => {
                let (grp, k) = read_brace_group(&chars, i + 1);
                out.push_str(&to_subscript_str(&grp));
                i = k;
            }
            '{' | '}' => i += 1, // strip stray braces
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Read a `{group}` (honoring nesting) or a single character starting at `pos`
/// (skipping leading spaces). Returns the content and the index just past it.
fn read_brace_group(chars: &[char], mut pos: usize) -> (String, usize) {
    while pos < chars.len() && chars[pos] == ' ' {
        pos += 1;
    }
    if chars.get(pos) == Some(&'{') {
        let mut depth = 1;
        pos += 1;
        let start = pos;
        while pos < chars.len() && depth > 0 {
            match chars[pos] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            pos += 1;
        }
        let content: String = chars[start..pos].iter().collect();
        (content, (pos + 1).min(chars.len()))
    } else if pos < chars.len() {
        (chars[pos].to_string(), pos + 1)
    } else {
        (String::new(), pos)
    }
}

/// Map a LaTeX command name (without the backslash) to its Unicode symbol.
fn latex_command(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
        "epsilon" => "ε", "zeta" => "ζ", "eta" => "η", "theta" => "θ",
        "iota" => "ι", "kappa" => "κ", "lambda" => "λ", "mu" => "μ", "nu" => "ν",
        "xi" => "ξ", "omicron" => "ο", "pi" => "π", "rho" => "ρ", "sigma" => "σ",
        "tau" => "τ", "upsilon" => "υ", "phi" => "φ", "chi" => "χ", "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
        "Xi" => "Ξ", "Pi" => "Π", "Sigma" => "Σ", "Phi" => "Φ", "Psi" => "Ψ",
        "Omega" => "Ω",
        "times" => "×", "cdot" => "·", "div" => "÷", "pm" => "±", "mp" => "∓",
        "leq" => "≤", "le" => "≤", "geq" => "≥", "ge" => "≥", "neq" => "≠",
        "ne" => "≠", "approx" => "≈", "equiv" => "≡", "cong" => "≅",
        "sim" => "∼", "propto" => "∝", "infty" => "∞", "partial" => "∂",
        "nabla" => "∇", "sum" => "∑", "prod" => "∏", "int" => "∫", "sqrt" => "√",
        "to" => "→", "rightarrow" => "→", "leftarrow" => "←",
        "leftrightarrow" => "↔", "Rightarrow" => "⇒", "Leftarrow" => "⇐",
        "in" => "∈", "notin" => "∉", "subset" => "⊂", "supset" => "⊃",
        "subseteq" => "⊆", "supseteq" => "⊇", "cup" => "∪", "cap" => "∩",
        "emptyset" => "∅", "forall" => "∀", "exists" => "∃", "angle" => "∠",
        "cdots" => "⋯", "ldots" => "…", "dots" => "…", "perp" => "⊥",
        "parallel" => "∥", "wedge" => "∧", "vee" => "∨", "neg" => "¬",
        "oplus" => "⊕", "otimes" => "⊗", "circ" => "∘", "deg" => "°",
        _ => return None,
    })
}

fn render_plain(source: &str, width: usize) -> Vec<Line<'static>> {
    if source.is_empty() {
        return vec![Line::from("")];
    }

    let mut lines = Vec::new();
    for line in source.lines() {
        let wrapped = wrap::wrap(line, width);
        for row in wrapped {
            lines.push(Line::from(line[row.start..row.end].to_string()));
        }
    }
    if source.ends_with('\n') {
        lines.push(Line::from(""));
    }
    lines
}

#[derive(Debug, Clone)]
struct ListFrame {
    next: Option<u64>,
}

#[derive(Debug, Clone)]
struct ItemPrefix {
    indent: String,
    marker: String,
}

impl ItemPrefix {
    fn first(&self) -> String {
        format!("{}{}", self.indent, self.marker)
    }

    fn continuation(&self) -> String {
        let marker_width = self.marker.chars().count();
        format!("{}{}", self.indent, " ".repeat(marker_width))
    }
}

/// Buffered state while parsing a GFM table. Cell content is captured as plain
/// text (inline styling inside cells is dropped) so columns can be aligned.
#[derive(Debug, Default)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
}

struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    opts: PreviewOptions,
    quote_depth: usize,
    list_stack: Vec<ListFrame>,
    item_stack: Vec<ItemPrefix>,
    pending_first_item_line: bool,
    heading_level: Option<u8>,
    code_block: Option<String>,
    code_lang: Option<String>,
    link_url: Option<String>,
    image_alt: Option<String>,
    image_url: Option<String>,
    footnotes: Vec<(String, Vec<Line<'static>>)>,
    /// Reference names in order of first appearance; index + 1 is the number.
    footnote_order: Vec<String>,
    /// Collected footnote definitions: (number, rendered body lines).
    footnote_defs: Vec<(usize, Vec<Line<'static>>)>,
    pending_footnote_lines: Vec<Line<'static>>,
    in_footnote_def: bool,
    current_footnote_name: Option<String>,
    table: Option<TableState>,
    suppress_output: usize,
    /// When > 0, we are inside a `<dl>` block and each `<dd>` line gets indented.
    in_dl: usize,
    /// When Some, we are rendering a custom container callout with the given type.
    callout_type: Option<String>,
    /// Accumulated inner lines for the custom container body.
    callout_lines: Vec<Line<'static>>,
    /// Running count of fenced code blocks flushed (for focus matching).
    code_block_index: usize,
    /// First output-row index of each flushed code block, in document order.
    code_block_rows: Vec<usize>,
    /// Byte offset of each source line's start (sorted), to map an event's
    /// source byte offset to a source line number.
    line_starts: Vec<usize>,
    /// For each source line, the output row it first emitted to. Built densely
    /// (every source line gets an entry) as events stream in source order.
    source_line_rows: Vec<usize>,
    /// Next source line awaiting an entry in `source_line_rows`.
    next_src_line: usize,
    /// Headings collected in document order (level, text, output row).
    headings: Vec<Heading>,
    /// Normalized titles of existing notes, for resolving `[[wikilinks]]` as
    /// live vs dangling. Empty unless `opts.wikilinks` is on.
    link_targets: std::collections::HashSet<String>,
    /// Wikilinks masked out of the source, indexed by the sentinel number; the
    /// `add_text` pass expands `\u{E000}<i>\u{E001}` back into styled spans.
    wikilink_table: Vec<crate::notes::wikilinks::WikiLink>,
}

/// Map a byte offset to its 0-based source line, given sorted line-start offsets
/// (`line_starts[0]` is always 0). Used to attribute parser events to lines.
fn line_of(line_starts: &[usize], byte: usize) -> usize {
    line_starts.partition_point(|&s| s <= byte).saturating_sub(1)
}

impl Renderer {
    fn new(
        width: usize,
        opts: PreviewOptions,
        link_targets: std::collections::HashSet<String>,
        wikilink_table: Vec<crate::notes::wikilinks::WikiLink>,
    ) -> Self {
        Self {
            width,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![Style::default()],
            opts,
            quote_depth: 0,
            list_stack: Vec::new(),
            item_stack: Vec::new(),
            pending_first_item_line: false,
            heading_level: None,
            code_block: None,
            code_lang: None,
            link_url: None,
            image_alt: None,
            image_url: None,
            footnotes: Vec::new(),
            footnote_order: Vec::new(),
            footnote_defs: Vec::new(),
            pending_footnote_lines: Vec::new(),
            in_footnote_def: false,
            current_footnote_name: None,
            table: None,
            suppress_output: 0,
            in_dl: 0,
            callout_type: None,
            callout_lines: Vec::new(),
            code_block_index: 0,
            code_block_rows: Vec::new(),
            line_starts: Vec::new(),
            source_line_rows: Vec::new(),
            next_src_line: 0,
            headings: Vec::new(),
            link_targets,
            wikilink_table,
        }
    }

    fn render(mut self, source: &str) -> RenderOutput {
        // Byte offset of each source line start, so we can attribute each parser
        // event to a source line (powers `source_line_rows`).
        self.line_starts = std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter(|&(_, b)| b == b'\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_FOOTNOTES);
        options.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
        options.insert(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS);
        if self.opts.typographer {
            options.insert(Options::ENABLE_SMART_PUNCTUATION);
        }

        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            // Record the output row where each source line first emits, in source
            // order. The forward-only guard skips events whose range starts on an
            // already-recorded line (e.g. End tags that span a whole block).
            let line = line_of(&self.line_starts, range.start);
            let row = self.lines.len();
            while self.next_src_line <= line {
                self.source_line_rows.push(row);
                self.next_src_line += 1;
            }
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => {
                    if let Some(alt) = self.image_alt.as_mut() {
                        alt.push_str(text.as_ref());
                    } else if self.code_block.is_some() {
                        self.add_code_block_text(text.as_ref());
                    } else if self.in_table_cell() {
                        self.append_cell(text.as_ref());
                    } else {
                        self.add_text(text.as_ref());
                    }
                }
                Event::Code(code) => {
                    if self.in_table_cell() {
                        self.append_cell(code.as_ref());
                    } else {
                        self.add_inline_code(code.as_ref());
                    }
                }
                Event::Rule => self.add_rule(),
                Event::SoftBreak => {
                    if self.in_table_cell() {
                        self.append_cell(" ");
                    } else {
                        self.add_text(" ");
                    }
                }
                Event::HardBreak => {
                    if self.in_table_cell() {
                        self.append_cell(" ");
                    } else {
                        self.flush_line();
                    }
                }
                Event::TaskListMarker(checked) => self.set_task_marker(checked),
                Event::Html(html) => self.handle_html(html.as_ref()),
                Event::InlineHtml(html) => self.add_text(html.as_ref()),
                Event::FootnoteReference(name) => {
                    let n = self.footnote_number(name.as_ref());
                    if !self.is_suppressed() {
                        self.ensure_prefix();
                        self.current.push(Span::styled(
                            to_superscript(n),
                            Style::default().fg(Color::Cyan),
                        ));
                    }
                }
            }
        }

        // Any trailing source lines that emitted no events (e.g. a blank tail)
        // map to the end of the body.
        let end_row = self.lines.len();
        while self.next_src_line < self.line_starts.len() {
            self.source_line_rows.push(end_row);
            self.next_src_line += 1;
        }

        self.flush_line();
        self.render_footnote_defs();
        self.render_footnotes();
        trim_trailing_blank_lines(&mut self.lines);
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        RenderOutput {
            lines: self.lines,
            code_rows: self.code_block_rows,
            source_line_rows: self.source_line_rows,
            headings: self.headings,
        }
    }

    /// Handle block-level HTML from pre-passes (definition lists, custom containers).
    fn handle_html(&mut self, html: &str) {
        let trimmed = html.trim();

        // Custom container markers.
        if let Some(kind) = trimmed.strip_prefix("<!-- CUSTOM_CONTAINER_START:") {
            if let Some(kind) = kind.strip_suffix(" -->") {
                self.flush_line();
                self.callout_type = Some(kind.to_string());
                self.callout_lines.clear();
                return;
            }
        }
        if trimmed == "<!-- CUSTOM_CONTAINER_END -->" {
            self.finish_callout();
            return;
        }

        // Definition list tags from the pre-pass.
        if trimmed == "<dl>" {
            self.flush_line();
            self.in_dl += 1;
            self.push_blank_if_needed();
            return;
        }
        if trimmed == "</dl>" {
            self.flush_line();
            self.in_dl = self.in_dl.saturating_sub(1);
            self.push_blank_if_needed();
            return;
        }
        if let Some(content) = trimmed.strip_prefix("<dt>") {
            if let Some(content) = content.strip_suffix("</dt>") {
                self.ensure_prefix();
                self.current.push(Span::styled(
                    content.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                self.flush_line();
                return;
            }
        }
        if let Some(content) = trimmed.strip_prefix("<dd>") {
            if let Some(content) = content.strip_suffix("</dd>") {
                self.ensure_prefix();
                self.current.push(Span::styled("  ", Style::default()));
                self.current.push(Span::styled(
                    content.to_string(),
                    Style::default().fg(Color::Gray),
                ));
                self.flush_line();
                return;
            }
        }

        // Generic HTML — render as inline text (pulldown passes raw HTML through
        // for unknown tags; we just treat them as plain text).
        self.add_text(html);
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.push_blank_if_needed();
                let level = heading_level_number(level);
                self.heading_level = Some(level);
                self.push_style(heading_style(level));
            }
            Tag::BlockQuote => {
                self.flush_line();
                self.push_blank_if_needed();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.push_blank_if_needed();
                self.code_block = Some(String::new());
                match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => {
                        self.code_lang = Some(lang.trim().to_string());
                    }
                    _ => self.code_lang = None,
                };
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(ListFrame { next: start });
            }
            Tag::Item => self.start_item(),
            Tag::Emphasis => {
                self.push_modifier(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.push_modifier(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.push_modifier(Modifier::CROSSED_OUT);
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.into_string());
                self.push_style(
                    self.current_style()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Table(alignments) => {
                self.flush_line();
                self.push_blank_if_needed();
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.in_cell = true;
                    table.current_cell = String::new();
                }
            }
            Tag::Image { dest_url, .. } => {
                // Buffer alt text from subsequent Event::Text; on close either
                // render the image (in `images` builds) or emit a placeholder.
                self.image_alt = Some(String::new());
                self.image_url = Some(dest_url.into_string());
                self.link_url = None;
            }
            Tag::FootnoteDefinition(name) => {
                self.flush_line();
                self.in_footnote_def = true;
                self.current_footnote_name = Some(name.into_string());
                self.pending_footnote_lines.clear();
            }
            Tag::MetadataBlock(_) => {
                self.suppress_output = self.suppress_output.saturating_add(1);
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                if self.list_stack.is_empty() {
                    self.push_blank_if_needed();
                }
            }
            TagEnd::Heading(level) => {
                // Collect the heading (plain text + the row it lands on) for the
                // outline and current-section features, before `flush_line`
                // drains `current`.
                let text: String = self
                    .current
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    self.headings.push(Heading {
                        level: heading_level_number(level),
                        text,
                        row: self.lines.len(),
                    });
                }
                self.flush_line();
                self.pop_style();
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.lines.push(Line::from(Span::styled(
                        "─".repeat(self.width),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                self.heading_level = None;
                self.push_blank_if_needed();
            }
            TagEnd::BlockQuote => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.push_blank_if_needed();
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.push_blank_if_needed();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.push_blank_if_needed();
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.item_stack.pop();
                self.pending_first_item_line = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
                self.finish_link();
            }
            TagEnd::TableCell => {
                let raw_cell = self.table.as_mut().map(|t| {
                    t.in_cell = false;
                    std::mem::take(&mut t.current_cell).trim().to_string()
                });
                if let Some(raw_cell) = raw_cell {
                    // Tables render to flat strings; expand any masked wikilinks
                    // to their display text (styling isn't carried in cells).
                    let cell = self.wikilink_plain(&raw_cell);
                    if let Some(table) = self.table.as_mut() {
                        table.current_row.push(cell);
                    }
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.current_row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                    self.push_blank_if_needed();
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_line();
                let name = self.current_footnote_name.take().unwrap_or_default();
                let number = self.footnote_number(&name);
                let lines = std::mem::take(&mut self.pending_footnote_lines);
                self.footnote_defs.push((number, lines));
                self.in_footnote_def = false;
            }
            TagEnd::MetadataBlock(_) => {
                self.suppress_output = self.suppress_output.saturating_sub(1);
            }
            TagEnd::Image => {
                let alt = self.image_alt.take();
                let url = self.image_url.take();
                if self.is_suppressed() {
                    return;
                }
                let Some(alt) = alt else {
                    return;
                };

                // In `images` builds with the option on, try to render the file
                // as half-block cells; fall through to the placeholder otherwise.
                #[cfg(feature = "images")]
                if self.opts.images {
                    if let Some(img_lines) =
                        url.as_deref().and_then(|u| images::render_image_lines(u, self.width))
                    {
                        self.flush_line();
                        self.lines.extend(img_lines.iter().cloned());
                        return;
                    }
                }
                #[cfg(not(feature = "images"))]
                let _ = &url;

                let label = if alt.is_empty() { "image" } else { &alt };
                self.ensure_prefix();
                self.current.push(Span::styled(
                    format!("🖼 {} ", label),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            _ => {}
        }
    }

    fn start_item(&mut self) {
        self.flush_line();
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let marker = match self
            .list_stack
            .last_mut()
            .and_then(|frame| frame.next.as_mut())
        {
            Some(next) => {
                let marker = format!("{}. ", next);
                *next = next.saturating_add(1);
                marker
            }
            None => "• ".to_string(),
        };
        self.item_stack.push(ItemPrefix { indent, marker });
        self.pending_first_item_line = true;
    }

    /// Finish rendering a custom container callout. Renders the callout with
    /// an appropriate label and border color based on type.
    fn finish_callout(&mut self) {
        let Some(ref kind) = self.callout_type.take() else {
            return;
        };

        // Pick label and accent color for known callout types.
        let (label, accent) = match kind.as_str() {
            "note" | "info" => ("ℹ Note", Color::Blue),
            "warning" | "warn" => ("⚠ Warning", Color::Yellow),
            "danger" | "error" | "caution" => ("☠ Danger", Color::Red),
            "tip" | "hint" => ("💡 Tip", Color::Green),
            "important" => ("❗ Important", Color::Magenta),
            "question" => ("❓ Question", Color::Cyan),
            other => (other, Color::DarkGray),
        };

        // Emit callout header with indicator.
        self.flush_line();
        self.push_blank_if_needed();
        self.lines.push(Line::from(Span::styled(
            format!(" {} ", label),
            Style::default()
                .fg(accent)
                .add_modifier(Modifier::BOLD),
        )));

        // Emit accumulated body lines with a vertical bar prefix.
        for line in std::mem::take(&mut self.callout_lines) {
            let styled_line = Line::from(
                std::iter::once(Span::styled("▎ ", Style::default().fg(accent)))
                    .chain(line)
                    .collect::<Vec<_>>(),
            );
            self.lines.push(styled_line);
        }
        // If there was no content, still push an empty line under the label.
        self.push_blank_if_needed();
    }

    fn set_task_marker(&mut self, checked: bool) {
        if self.is_suppressed() {
            return;
        }
        if let Some(item) = self.item_stack.last_mut() {
            item.marker = if checked {
                "☑ ".to_string()
            } else {
                "☐ ".to_string()
            };
            self.pending_first_item_line = true;
        }
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() || self.is_suppressed() {
            return;
        }
        // Apply post-passes that transform text (emoji).
        let text = if self.opts.emoji {
            replace_emoji(text)
        } else {
            text.to_string()
        };

        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            if part.is_empty() {
                continue;
            }
            self.ensure_prefix();

            // Apply post-passes that split into multiple spans (mark/ins/sup_sub).
            let mut spans = self.post_process_text(part);

            // Apply bare-URL linkify as a span-level transform (only when enabled).
            if self.opts.linkify {
                spans = spans.into_iter().flat_map(linkify_span).collect();
            }

            // Expand masked [[wikilink]] sentinels into styled (live/dangling)
            // spans. The sentinels are bracket-free, so they always sit intact
            // within a single span here.
            if self.opts.wikilinks && !self.wikilink_table.is_empty() {
                spans = spans
                    .into_iter()
                    .flat_map(|s| self.expand_wikilinks(s))
                    .collect();
            }

            self.current.extend(spans);
        }
    }

    /// Expand masked `[[wikilink]]` sentinels (`\u{E000}<i>\u{E001}`) in a
    /// span's text into styled spans: live (resolvable, cyan-underlined like
    /// external links) or dangling (dim, `(?)`-marked). Surrounding text keeps
    /// the span's original style. Called only when `opts.wikilinks` is on.
    fn expand_wikilinks(&self, span: Span<'static>) -> Vec<Span<'static>> {
        use crate::notes::wikilinks::{normalize_title, WIKI_CLOSE, WIKI_OPEN};

        let text = span.content.as_ref();
        if !text.contains(WIKI_OPEN) {
            return vec![span];
        }
        let base = span.style;
        let live_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::UNDERLINED);
        let dangling_style = Style::default().fg(Color::DarkGray);

        let mut out: Vec<Span<'static>> = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find(WIKI_OPEN) {
            let after = &rest[open + WIKI_OPEN.len_utf8()..];
            let Some(close_rel) = after.find(WIKI_CLOSE) else {
                break;
            };
            if open > 0 {
                out.push(Span::styled(rest[..open].to_string(), base));
            }
            let idx = after[..close_rel].parse::<usize>().ok();
            if let Some(link) = idx.and_then(|i| self.wikilink_table.get(i)) {
                // The focused link is reverse-highlighted for navigation.
                let focused = idx == self.opts.focused_wikilink;
                let style = |base: Style| {
                    if focused {
                        base.add_modifier(Modifier::REVERSED)
                    } else {
                        base
                    }
                };
                let display = link.alias.clone().unwrap_or_else(|| link.target.clone());
                if self.link_targets.contains(&normalize_title(&link.target)) {
                    out.push(Span::styled(display, style(live_style)));
                } else {
                    out.push(Span::styled(format!("{display} (?)"), style(dangling_style)));
                }
            }
            rest = &after[close_rel + WIKI_CLOSE.len_utf8()..];
        }
        if !rest.is_empty() {
            out.push(Span::styled(rest.to_string(), base));
        }
        out
    }

    /// Replace masked wikilink sentinels with their plain display text (no
    /// styling) — for contexts that render to a flat string, like table cells.
    fn wikilink_plain(&self, text: &str) -> String {
        use crate::notes::wikilinks::{WIKI_CLOSE, WIKI_OPEN};
        if self.wikilink_table.is_empty() || !text.contains(WIKI_OPEN) {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        loop {
            let Some(open) = rest.find(WIKI_OPEN) else {
                out.push_str(rest);
                break;
            };
            let after = &rest[open + WIKI_OPEN.len_utf8()..];
            let Some(close_rel) = after.find(WIKI_CLOSE) else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..open]);
            if let Some(link) = after[..close_rel]
                .parse::<usize>()
                .ok()
                .and_then(|i| self.wikilink_table.get(i))
            {
                out.push_str(link.alias.as_ref().unwrap_or(&link.target));
            }
            rest = &after[close_rel + WIKI_CLOSE.len_utf8()..];
        }
        out
    }

    /// Apply ==mark==, ++ins++, ^sup^, ~sub~ post-passes on plain text,
    /// returning a list of styled spans.  These patterns are not understood
    /// by pulldown_cmark, so we handle them here.
    fn post_process_text(&self, text: &str) -> Vec<Span<'static>> {
        let base_style = self.effective_style();
        if !self.opts.mark && !self.opts.ins && !self.opts.sup_sub {
            return vec![Span::styled(text.to_string(), base_style)];
        }

        // We scan character-by-character looking for opening markers.
        // Priority: ==  >>  ++  >>  ^...^  >>  ~...~
        let mut spans: Vec<Span<'static>> = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let mut i = 0;

        // Track the current style modifiers being applied
        let mut active_mark = false;
        let mut active_ins = false;
        let mut active_sup = false;
        let mut active_sub = false;

        let mut buf = String::new();

        let flush_buf = |buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(buf), style));
            }
        };

        while i < len {
            // Check for ==mark== (only if enabled)
            if self.opts.mark && i + 2 <= len && chars[i] == '=' && chars[i + 1] == '=' {
                if active_mark {
                    // Closing marker
                    flush_buf(&mut buf, &mut spans, base_style.add_modifier(Modifier::REVERSED));
                    active_mark = false;
                } else {
                    // Opening marker
                    flush_buf(&mut buf, &mut spans, if active_ins { base_style.add_modifier(Modifier::REVERSED) } else { base_style });
                    active_mark = true;
                }
                i += 2;
                continue;
            }

            // Check for ++ins++ (only if enabled)
            if self.opts.ins && i + 2 <= len && chars[i] == '+' && chars[i + 1] == '+' {
                if active_ins {
                    flush_buf(&mut buf, &mut spans, base_style.add_modifier(Modifier::UNDERLINED));
                    active_ins = false;
                } else {
                    flush_buf(&mut buf, &mut spans, if active_mark { base_style.add_modifier(Modifier::REVERSED) } else { base_style });
                    active_ins = true;
                }
                i += 2;
                continue;
            }

            // ^superscript^ → Unicode superscript. Only open when a closing
            // caret exists ahead, so a stray `^` stays literal.
            if self.opts.sup_sub && chars[i] == '^' {
                if active_sup {
                    if !buf.is_empty() {
                        spans.push(Span::styled(
                            to_superscript_str(&std::mem::take(&mut buf)),
                            base_style,
                        ));
                    }
                    active_sup = false;
                    i += 1;
                    continue;
                } else if matches!(chars[i + 1..].iter().position(|&c| c == '^'), Some(p) if p > 0) {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sup = true;
                    i += 1;
                    continue;
                }
            }

            // ~subscript~ → Unicode subscript, same closing-marker rule.
            if self.opts.sup_sub && chars[i] == '~' {
                if active_sub {
                    if !buf.is_empty() {
                        spans.push(Span::styled(
                            to_subscript_str(&std::mem::take(&mut buf)),
                            base_style,
                        ));
                    }
                    active_sub = false;
                    i += 1;
                    continue;
                } else if matches!(chars[i + 1..].iter().position(|&c| c == '~'), Some(p) if p > 0) {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sub = true;
                    i += 1;
                    continue;
                }
            }

            buf.push(chars[i]);
            i += 1;
        }

        // Flush remaining buffer with the appropriate style
        let final_style = if active_mark {
            base_style.add_modifier(Modifier::REVERSED)
        } else if active_ins {
            base_style.add_modifier(Modifier::UNDERLINED)
        } else {
            base_style
        };
        flush_buf(&mut buf, &mut spans, final_style);

        spans
    }

    fn add_inline_code(&mut self, code: &str) {
        if self.is_suppressed() {
            return;
        }
        self.ensure_prefix();
        self.current.push(Span::styled(
            format!(" {} ", code),
            Style::default().fg(Color::Yellow).bg(Color::Black),
        ));
    }

    fn add_rule(&mut self) {
        if self.is_suppressed() {
            return;
        }
        self.flush_line();
        self.push_blank_if_needed();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(self.width),
            Style::default().fg(Color::DarkGray),
        )));
        self.push_blank_if_needed();
    }

    fn add_code_block_text(&mut self, text: &str) {
        let code = self.code_block.get_or_insert_with(String::new);
        code.push_str(text);
    }

    fn flush_code_block(&mut self) {
        if self.is_suppressed() {
            return;
        }
        let Some(code) = self.code_block.take() else {
            return;
        };

        let lang = self.code_lang.take();

        // Record this block's position and whether it's the focused one. The
        // focused block gets a cyan `┃` gutter instead of the plain indent.
        let block_index = self.code_block_index;
        self.code_block_index += 1;
        self.code_block_rows.push(self.lines.len());
        let focused = self.opts.focused_code_block == Some(block_index);
        let (gutter, gutter_style) = code_gutter(focused);

        // Mermaid flowcharts render as ASCII art when we can parse them; on any
        // failure we fall through to showing the raw source below.
        if self.opts.mermaid && lang.as_deref() == Some("mermaid") {
            if let Some(diagram) = render_mermaid(&code, self.width) {
                self.lines.push(Line::from(Span::styled(
                    format!("{}mermaid", if focused { "▶ " } else { "" }),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
                self.lines.extend(diagram);
                return;
            }
        }

        // Optional language label above the block.
        if let Some(ref lang) = lang {
            let marker = if focused { "▶ " } else { "" };
            self.lines.push(Line::from(vec![
                Span::styled(gutter.to_string(), gutter_style),
                Span::styled(
                    format!("{marker}{lang}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }

        // Syntax-highlighted path (feature-gated).
        #[cfg(feature = "syntax-highlight")]
        if let Some(ref lang) = lang {
            let (ss, ts) = highlighter();
            let theme = ts
                .themes
                .get(self.opts.syntax_theme.as_str())
                .or_else(|| ts.themes.get("base16-ocean.dark"))
                .or_else(|| ts.themes.values().next())
                .expect("syntect ships default themes");
            if let Some(syntax) = ss.find_syntax_by_token(lang) {
                self.highlight_code_block(&code, ss, syntax, theme, gutter, gutter_style);
                if code.ends_with('\n') {
                    self.lines.push(Line::from(Span::styled(
                        gutter.to_string(),
                        gutter_style,
                    )));
                }
                return;
            }
        }

        // Fallback: uniform gray styling.
        let style = Style::default().fg(Color::Gray).bg(Color::Black);
        for raw in code.lines() {
            let line = Line::from(vec![
                Span::styled(gutter.to_string(), gutter_style),
                Span::styled(raw.to_string(), style),
            ]);
            self.lines.extend(wrap_styled_line(line, self.width));
        }
        if code.ends_with('\n') {
            self.lines
                .push(Line::from(Span::styled(gutter.to_string(), gutter_style)));
        }
    }

    /// Highlight a fenced code block using syntect (feature-gated).
    #[cfg(feature = "syntax-highlight")]
    fn highlight_code_block(
        &mut self,
        code: &str,
        ss: &syntect::parsing::SyntaxSet,
        syntax: &syntect::parsing::SyntaxReference,
        theme: &syntect::highlighting::Theme,
        gutter: &str,
        gutter_style: Style,
    ) {
        use syntect::easy::HighlightLines;
        use syntect::util::LinesWithEndings;

        let mut hl = HighlightLines::new(syntax, theme);
        for line_with_endings in LinesWithEndings::from(code) {
            let line_str = line_with_endings.trim_end();
            if line_str.is_empty() {
                self.lines
                    .push(Line::from(Span::styled(gutter.to_string(), gutter_style)));
                continue;
            }
            let Ok(tokens) = hl.highlight_line(line_with_endings, ss) else {
                // Fallback to uniform gray for this line.
                let line = Line::from(vec![
                    Span::styled(gutter.to_string(), gutter_style),
                    Span::styled(
                        line_str.to_string(),
                        Style::default().fg(Color::Gray).bg(Color::Black),
                    ),
                ]);
                self.lines.extend(wrap_styled_line(line, self.width));
                continue;
            };
            let mut spans = vec![Span::styled(gutter.to_string(), gutter_style)];
            for (sty, text) in &tokens {
                if text.is_empty() || *text == "\n" {
                    continue;
                }
                // Only pull foreground; leave background alone.
                let fg = Color::Rgb(sty.foreground.r, sty.foreground.g, sty.foreground.b);
                spans.push(Span::styled(text.to_string(), Style::default().fg(fg)));
            }
            let line = Line::from(spans);
            self.lines.extend(wrap_styled_line(line, self.width));
        }
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() || self.is_suppressed() {
            return;
        }
        let line = Line::from(std::mem::take(&mut self.current));
        let target = if self.in_footnote_def {
            &mut self.pending_footnote_lines
        } else if self.callout_type.is_some() {
            &mut self.callout_lines
        } else {
            &mut self.lines
        };
        target.extend(wrap_styled_line(line, self.width));
    }

    fn push_blank_if_needed(&mut self) {
        if self.is_suppressed() {
            return;
        }
        let target: &mut Vec<Line<'static>> = if self.in_footnote_def {
            &mut self.pending_footnote_lines
        } else if self.callout_type.is_some() {
            &mut self.callout_lines
        } else {
            &mut self.lines
        };
        if target.last().is_some_and(|line| !line_is_blank(line)) {
            target.push(Line::from(""));
        }
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() || self.is_suppressed() {
            return;
        }

        if self.quote_depth > 0 {
            self.current.push(Span::styled(
                "▎ ".repeat(self.quote_depth),
                Style::default().fg(Color::DarkGray),
            ));
        }

        if let Some(item) = self.item_stack.last() {
            let prefix = if self.pending_first_item_line {
                item.first()
            } else {
                item.continuation()
            };
            self.current.push(Span::styled(
                prefix,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            self.pending_first_item_line = false;
        }
    }

    fn is_suppressed(&self) -> bool {
        self.suppress_output > 0
    }

    fn in_table_cell(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.in_cell)
    }

    fn append_cell(&mut self, text: &str) {
        if let Some(table) = self.table.as_mut() {
            table.current_cell.push_str(text);
        }
    }

    /// Append a link's URL marker (or record a footnote) after its text.
    fn finish_link(&mut self) {
        if self.is_suppressed() {
            return;
        }
        let Some(url) = self.link_url.take() else {
            return;
        };
        // Inside a table cell links render as their text only.
        if url.is_empty() || self.in_table_cell() {
            return;
        }
        match self.opts.link_urls {
            LinkUrlMode::Inline => {
                self.current
                    .push(Span::styled(format!(" ({url})"), dim_style()));
            }
            LinkUrlMode::Footnote => {
                self.footnotes.push((url.clone(), Vec::new()));
                let n = self.footnotes.len();
                self.current
                    .push(Span::styled(format!("[{n}]"), dim_style()));
            }
            LinkUrlMode::Hide => {}
        }
    }

    /// Map a footnote reference name to a stable number by order of first use.
    fn footnote_number(&mut self, name: &str) -> usize {
        if let Some(pos) = self.footnote_order.iter().position(|n| n == name) {
            pos + 1
        } else {
            self.footnote_order.push(name.to_string());
            self.footnote_order.len()
        }
    }

    /// Emit collected footnote definitions as a numbered section at the bottom,
    /// separated from the body by a rule. Distinct from the link-URL footnote
    /// list (`render_footnotes`) so the two numbering schemes never collide.
    fn render_footnote_defs(&mut self) {
        if self.footnote_defs.is_empty() {
            return;
        }
        let mut defs = std::mem::take(&mut self.footnote_defs);
        defs.sort_by_key(|(n, _)| *n);

        self.push_blank_if_needed();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(self.width),
            Style::default().fg(Color::DarkGray),
        )));

        for (number, body) in defs {
            let marker = format!("{number}. ");
            let indent = " ".repeat(marker.chars().count());
            if body.is_empty() {
                self.lines.push(Line::from(Span::styled(
                    marker,
                    Style::default().fg(Color::Cyan),
                )));
                continue;
            }
            for (row, line) in body.into_iter().enumerate() {
                let prefix = if row == 0 {
                    Span::styled(marker.clone(), Style::default().fg(Color::Cyan))
                } else {
                    Span::styled(indent.clone(), Style::default())
                };
                let spans: Vec<Span<'static>> =
                    std::iter::once(prefix).chain(line.spans).collect();
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn render_footnotes(&mut self) {
        if self.opts.link_urls != LinkUrlMode::Footnote || self.footnotes.is_empty() {
            return;
        }
        self.push_blank_if_needed();
        let footnotes = std::mem::take(&mut self.footnotes);
        for (i, (url, _body_lines)) in footnotes.iter().enumerate() {
            let line = Line::from(Span::styled(format!("[{}] {}", i + 1, url), dim_style()));
            self.lines.extend(wrap_styled_line(line, self.width));
        }
    }

    /// Render a collected table: aligned monospace columns when they fit the
    /// pane, otherwise a stacked "header: value" fallback.
    fn render_table(&mut self, table: TableState) {
        let rows = &table.rows;
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if cols == 0 {
            return;
        }

        let mut widths = vec![0usize; cols];
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        const SEP: &str = " │ ";
        let sep_cols = SEP.chars().count();
        let total: usize = widths.iter().sum::<usize>() + sep_cols * cols.saturating_sub(1);

        if total <= self.width {
            self.render_table_aligned(rows, &widths, &table.alignments);
        } else {
            self.render_table_stacked(rows);
        }
    }

    fn render_table_aligned(
        &mut self,
        rows: &[Vec<String>],
        widths: &[usize],
        alignments: &[Alignment],
    ) {
        const SEP: &str = " │ ";
        let cols = widths.len();
        for (ri, row) in rows.iter().enumerate() {
            let mut spans = Vec::new();
            for (i, &w) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(SEP, Style::default().fg(Color::DarkGray)));
                }
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let align = alignments.get(i).copied().unwrap_or(Alignment::None);
                let style = if ri == 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(pad(cell, w, align), style));
            }
            self.lines.push(Line::from(spans));

            // Rule under the header row.
            if ri == 0 {
                let mut rule = String::new();
                for (i, &w) in widths.iter().enumerate() {
                    if i > 0 {
                        rule.push_str("─┼─");
                    }
                    rule.push_str(&"─".repeat(w));
                }
                let _ = cols;
                self.lines.push(Line::from(Span::styled(
                    rule,
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    fn render_table_stacked(&mut self, rows: &[Vec<String>]) {
        let header = rows.first().cloned().unwrap_or_default();
        for row in rows.iter().skip(1) {
            for (i, cell) in row.iter().enumerate() {
                let label = header.get(i).map(String::as_str).unwrap_or("");
                let text = if label.is_empty() {
                    cell.clone()
                } else {
                    format!("{label}: {cell}")
                };
                for vr in wrap::wrap(&text, self.width) {
                    self.lines
                        .push(Line::from(text[vr.start..vr.end].to_string()));
                }
            }
            self.lines.push(Line::from(""));
        }
    }

    fn push_modifier(&mut self, modifier: Modifier) {
        let style = self.current_style().add_modifier(modifier);
        self.styles.push(style);
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(style);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn current_style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn effective_style(&self) -> Style {
        let mut style = self.current_style();
        if self.quote_depth > 0 {
            style = style.fg(Color::Gray);
        }
        style
    }
}

/// Split a span into multiple spans, styling detected bare URLs with
/// link styling (cyan + underline).
fn linkify_span(span: Span<'static>) -> Vec<Span<'static>> {
    let text = span.content.to_string();
    let style = span.style;

    // Quick check: no http/https/ftp/www → return as-is.
    let lower = text.to_lowercase();
    if !lower.contains("http://")
        && !lower.contains("https://")
        && !lower.contains("ftp://")
        && !lower.contains("www.")
    {
        return vec![span];
    }

    let url_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    let mut result = Vec::new();
    let mut remaining = text.as_str();

    while !remaining.is_empty() {
        // Look for the next URL-like token.
        let (before, url) = split_url(remaining);
        if let Some(url) = url {
            if !before.is_empty() {
                result.push(Span::styled(before.to_string(), style));
            }
            result.push(Span::styled(url.to_string(), url_style));
            remaining = &remaining[before.len() + url.len()..];
        } else {
            result.push(Span::styled(remaining.to_string(), style));
            break;
        }
    }

    result
}

/// Try to split off a bare URL from the start of `s`.
/// Returns `(prefix, url)` where `prefix` is text before the URL,
/// and `url` is the detected URL (or None).
fn split_url(s: &str) -> (&str, Option<&str>) {
    let url_chars: &[char] = &[
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm',
        'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
        'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M',
        'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
        '-', '.', '_', '~', ':', '/', '?', '#', '[', ']', '@', '!', '$',
        '&', '\'', '(', ')', '*', '+', ',', ';', '=', '%',
    ];

    // Find first occurrence of a URL scheme or www.
    let lower = s.to_lowercase();
    let schemes = ["https://", "http://", "ftp://", "www."];
    let mut earliest: Option<(usize, &str)> = None;

    for scheme in &schemes {
        if let Some(pos) = lower.find(scheme) {
            let actual_scheme = &s[pos..pos + scheme.len()];
            match earliest {
                Some((earliest_pos, _)) if pos < earliest_pos => {
                    earliest = Some((pos, actual_scheme));
                }
                None => {
                    earliest = Some((pos, actual_scheme));
                }
                _ => {}
            }
        }
    }

    let (start, scheme) = match earliest {
        Some(pair) => pair,
        None => return (s, None),
    };

    // Scan forward from the end of the scheme to find the end of the URL.
    let after_scheme = start + scheme.len();
    let end = after_scheme
        + s[after_scheme..]
            .find(|c: char| !url_chars.contains(&c))
            .unwrap_or(s.len() - after_scheme);

    let url = &s[start..end];

    // Validate: after the scheme we need at least one character.
    if url.len() <= scheme.len() {
        return (s, None);
    }

    // www. requires a dot somewhere after it.
    if scheme.starts_with("www") && !url[scheme.len()..].contains('.') {
        return (s, None);
    }

    (&s[..start], Some(url))
}

fn replace_emoji(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == ':' {
            i += 1;
            let mut code = String::new();
            while i < len && chars[i] != ':' {
                code.push(chars[i]);
                i += 1;
            }
            if i < len && chars[i] == ':' {
                // We found a matching ':code:' - check our map
                let emoji = EMOJI_MAP.get(code.as_str());
                if let Some(e) = emoji {
                    out.push_str(e);
                    i += 1;
                    continue;
                }
                // Not in map, emit the raw `:code:`
                out.push(':');
                out.push_str(&code);
                out.push(':');
                i += 1;
            } else {
                // No closing ':' found
                out.push(':');
                out.push_str(&code);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Lookup table for `:shortcode:` → Unicode emoji.
/// Covers the most common GFM shortcodes plus a few extras.
static EMOJI_MAP: phf::Map<&'static str, &'static str> = phf::phf_map! {
    "smile" => "😊",
    "slight_smile" => "🙂",
    "laughing" => "😄",
    "joy" => "😂",
    "wink" => "😉",
    "heart_eyes" => "😍",
    "heart" => "❤️",
    "broken_heart" => "💔",
    "thumbsup" => "👍",
    "thumbsdown" => "👎",
    "ok_hand" => "👌",
    "clap" => "👏",
    "wave" => "👋",
    "fire" => "🔥",
    "sparkles" => "✨",
    "star" => "⭐",
    "check" => "✅",
    "x" => "❌",
    "warning" => "⚠️",
    "rocket" => "🚀",
    "question" => "❓",
    "exclamation" => "❗",
    "point_up" => "☝️",
    "point_down" => "👇",
    "point_left" => "👈",
    "point_right" => "👉",
    "pray" => "🙏",
    "muscle" => "💪",
    "eyes" => "👀",
    "cry" => "😢",
    "sob" => "😭",
    "angry" => "😠",
    "rage" => "😡",
    "confused" => "😕",
    "thinking" => "🤔",
    "blush" => "😊",
    "sunglasses" => "😎",
    "nerd" => "🤓",
    "party" => "🎉",
    "tada" => "🎉",
    "confetti" => "🎊",
    "balloon" => "🎈",
    "gift" => "🎁",
    "book" => "📖",
    "page" => "📄",
    "clipboard" => "📋",
    "pencil" => "✏️",
    "memo" => "📝",
    "computer" => "💻",
    "phone" => "📱",
    "email" => "📧",
    "mail" => "✉️",
    "calendar" => "📅",
    "clock" => "🕐",
    "alarm" => "⏰",
    "hourglass" => "⌛",
    "lock" => "🔒",
    "unlock" => "🔓",
    "key" => "🔑",
    "bell" => "🔔",
    "no_bell" => "🔕",
    "bug" => "🐛",
    "beetle" => "🪲",
    "ant" => "🐜",
    "honeybee" => "🐝",
    "snake" => "🐍",
    "turtle" => "🐢",
    "fish" => "🐟",
    "bird" => "🐦",
    "dog" => "🐕",
    "cat" => "🐈",
    "mouse" => "🐁",
    "hamster" => "🐹",
    "rabbit" => "🐇",
    "fox" => "🦊",
    "bear" => "🐻",
    "panda" => "🐼",
    "monkey" => "🐒",
    "frog" => "🐸",
    "pig" => "🐷",
    "cow" => "🐄",
    "wolf" => "🐺",
    "lion" => "🦁",
    "tiger" => "🐅",
    "horse" => "🐎",
    "unicorn" => "🦄",
    "zap" => "⚡",
    "snowflake" => "❄️",
    "sunny" => "☀️",
    "cloud" => "☁️",
    "umbrella" => "☂️",
    "rainbow" => "🌈",
    "moon" => "🌙",
    "earth" => "🌍",
    "globe" => "🌐",
    "info" => "ℹ️",
    "new" => "🆕",
    "up" => "🆙",
    "cool" => "🆒",
    "free" => "🆓",
    "100" => "💯",
    "soon" => "🔜",
    "top" => "🔝",
    "beginner" => "🔰",
    "yellow_circle" => "🟡",
    "green_circle" => "🟢",
    "red_circle" => "🔴",
    "blue_circle" => "🔵",
    "white_circle" => "⚪",
    "black_circle" => "⚫",
    "coffee" => "☕",
    "beer" => "🍺",
    "wine" => "🍷",
    "pizza" => "🍕",
    "apple" => "🍎",
    "orange" => "🍊",
    "lemon" => "🍋",
    "banana" => "🍌",
    "grape" => "🍇",
    "cherry" => "🍒",
    "strawberry" => "🍓",
    "cake" => "🎂",
    "cookie" => "🍪",
    "chocolate" => "🍫",
    "candy" => "🍬",
    "icecream" => "🍦",
    "shopping" => "🛒",
    "cart" => "🛒",
    "airplane" => "✈️",
    "car" => "🚗",
    "bus" => "🚌",
    "train" => "🚆",
    "bicycle" => "🚲",
    "house" => "🏠",
    "office" => "🏢",
    "hospital" => "🏥",
    "bank" => "🏦",
    "school" => "🏫",
    "camping" => "🏕️",
    "tent" => "⛺",
    "mountain" => "⛰️",
    "beach" => "🏖️",
    "sunrise" => "🌅",
    "sunset" => "🌇",
    "city" => "🏙️",
    "art" => "🎨",
    "music" => "🎵",
    "headphones" => "🎧",
    "guitar" => "🎸",
    "soccer" => "⚽",
    "basketball" => "🏀",
    "football" => "🏈",
    "baseball" => "⚾",
    "tennis" => "🎾",
    "golf" => "⛳",
    "trophy" => "🏆",
    "medal" => "🥇",
    "microphone" => "🎤",
    "video" => "🎬",
    "clapper" => "🎬",
    "tv" => "📺",
    "radio" => "📻",
    "loudspeaker" => "📢",
    "megaphone" => "📣",
    "speaker" => "🔈",
    "mute" => "🔇",
    "chart" => "📊",
    "money" => "💰",
    "dollar" => "💵",
    "euro" => "💶",
    "pound" => "💷",
    "yen" => "💴",
    "credit_card" => "💳",
    "gem" => "💎",
    "wrench" => "🔧",
    "hammer" => "🔨",
    "nut_and_bolt" => "🔩",
    "gear" => "⚙️",
    "link" => "🔗",
    "magnifier" => "🔍",
    "scissors" => "✂️",
    "trash" => "🗑️",
    "broom" => "🧹",
};

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// The left gutter (prefix + style) for a code-block line. The focused block
/// (code-block actions) gets a cyan `┃`; others keep the plain two-space indent.
fn code_gutter(focused: bool) -> (&'static str, Style) {
    if focused {
        (
            "┃ ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default())
    }
}

/// Render a number using Unicode superscript digits (e.g. 12 → "¹²"), so a
/// footnote reference reads as `text¹` rather than colliding with the `[n]`
/// markers used for link-URL footnotes.
fn to_superscript(n: usize) -> String {
    const SUP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    n.to_string()
        .chars()
        .map(|c| c.to_digit(10).map(|d| SUP[d as usize]).unwrap_or(c))
        .collect()
}

/// The Unicode superscript form of `c`, if one exists (digits, common symbols,
/// and most ASCII letters).
fn superscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴',
        '5' => '⁵', '6' => '⁶', '7' => '⁷', '8' => '⁸', '9' => '⁹',
        '+' => '⁺', '-' => '⁻', '=' => '⁼', '(' => '⁽', ')' => '⁾', 'n' => 'ⁿ',
        'a' => 'ᵃ', 'b' => 'ᵇ', 'c' => 'ᶜ', 'd' => 'ᵈ', 'e' => 'ᵉ', 'f' => 'ᶠ',
        'g' => 'ᵍ', 'h' => 'ʰ', 'i' => 'ⁱ', 'j' => 'ʲ', 'k' => 'ᵏ', 'l' => 'ˡ',
        'm' => 'ᵐ', 'o' => 'ᵒ', 'p' => 'ᵖ', 'r' => 'ʳ', 's' => 'ˢ', 't' => 'ᵗ',
        'u' => 'ᵘ', 'v' => 'ᵛ', 'w' => 'ʷ', 'x' => 'ˣ', 'y' => 'ʸ', 'z' => 'ᶻ',
        'A' => 'ᴬ', 'B' => 'ᴮ', 'D' => 'ᴰ', 'E' => 'ᴱ', 'G' => 'ᴳ', 'H' => 'ᴴ',
        'I' => 'ᴵ', 'J' => 'ᴶ', 'K' => 'ᴷ', 'L' => 'ᴸ', 'M' => 'ᴹ', 'N' => 'ᴺ',
        'O' => 'ᴼ', 'P' => 'ᴾ', 'R' => 'ᴿ', 'T' => 'ᵀ', 'U' => 'ᵁ', 'V' => 'ⱽ',
        'W' => 'ᵂ',
        _ => return None,
    })
}

/// The Unicode subscript form of `c`, if one exists (digits, common symbols, and
/// the limited set of subscript letters Unicode provides).
fn subscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀', '1' => '₁', '2' => '₂', '3' => '₃', '4' => '₄',
        '5' => '₅', '6' => '₆', '7' => '₇', '8' => '₈', '9' => '₉',
        '+' => '₊', '-' => '₋', '=' => '₌', '(' => '₍', ')' => '₎',
        'a' => 'ₐ', 'e' => 'ₑ', 'h' => 'ₕ', 'i' => 'ᵢ', 'j' => 'ⱼ', 'k' => 'ₖ',
        'l' => 'ₗ', 'm' => 'ₘ', 'n' => 'ₙ', 'o' => 'ₒ', 'p' => 'ₚ', 'r' => 'ᵣ',
        's' => 'ₛ', 't' => 'ₜ', 'u' => 'ᵤ', 'v' => 'ᵥ', 'x' => 'ₓ',
        _ => return None,
    })
}

/// Map every character of `s` to its Unicode superscript form, leaving
/// unmappable characters unchanged.
fn to_superscript_str(s: &str) -> String {
    s.chars().map(|c| superscript_char(c).unwrap_or(c)).collect()
}

/// Map every character of `s` to its Unicode subscript form, leaving unmappable
/// characters unchanged.
fn to_subscript_str(s: &str) -> String {
    s.chars().map(|c| subscript_char(c).unwrap_or(c)).collect()
}

/// Pad `text` to `width` columns according to `align` (None/Left → left).
fn pad(text: &str, width: usize, align: Alignment) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let total = width - len;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(total), text),
        Alignment::Center => {
            let left = total / 2;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(total - left))
        }
        Alignment::Left | Alignment::None => format!("{}{}", text, " ".repeat(total)),
    }
}

fn heading_style(level: u8) -> Style {
    let base = match level {
        1 => Style::default().fg(Color::Cyan),
        2 => Style::default().fg(Color::LightCyan),
        3 => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    };
    base.add_modifier(Modifier::BOLD)
}

fn heading_level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Build syntect's syntax set and theme once (lazy).
#[cfg(feature = "syntax-highlight")]
fn highlighter() -> &'static (syntect::parsing::SyntaxSet, syntect::highlighting::ThemeSet) {
    static H: OnceLock<(syntect::parsing::SyntaxSet, syntect::highlighting::ThemeSet)> =
        OnceLock::new();
    H.get_or_init(|| {
        let ss = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let ts = syntect::highlighting::ThemeSet::load_defaults();
        (ss, ts)
    })
}

/// Render a fenced `mermaid` flowchart as ASCII art, or `None` if it isn't a
/// flowchart we can lay out (the caller then shows the raw source).
fn render_mermaid(code: &str, width: usize) -> Option<Vec<Line<'static>>> {
    let fc = mermaid::parse(code)?;
    Some(mermaid::render(&fc, width))
}

/// Minimal Mermaid flowchart support: parse `graph`/`flowchart` node/edge
/// statements and render them as box-drawing ASCII — a vertical boxed flow for
/// simple linear charts, an arrow edge-list for branching ones. Pure Rust.
mod mermaid {
    use super::*;
    use std::collections::HashMap;

    pub struct Node {
        pub id: String,
        pub label: String,
    }
    pub struct Edge {
        pub from: String,
        pub to: String,
        pub label: Option<String>,
    }
    pub struct Flowchart {
        pub nodes: Vec<Node>,
        pub edges: Vec<Edge>,
    }

    enum Tok {
        Node(String, Option<String>),
        Arrow(bool), // has an arrowhead (`>`/`<`)
        Pipe(String),
    }

    /// Parse a mermaid flowchart, or `None` if the header isn't `graph` /
    /// `flowchart` or nothing usable is found.
    pub fn parse(code: &str) -> Option<Flowchart> {
        let raw: Vec<&str> = code
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("%%"))
            .collect();
        let header = raw.first()?.to_ascii_lowercase();
        if !(header.starts_with("graph") || header.starts_with("flowchart")) {
            return None;
        }

        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        for line in &raw[1..] {
            for stmt in line.split(';') {
                let stmt = stmt.trim();
                if stmt.is_empty() || skip_statement(stmt) {
                    continue;
                }
                parse_statement(stmt, &mut nodes, &mut edges, &mut seen);
            }
        }

        if nodes.is_empty() {
            return None;
        }
        Some(Flowchart { nodes, edges })
    }

    /// Lines that are valid mermaid but not node/edge statements we render.
    fn skip_statement(stmt: &str) -> bool {
        let first = stmt.split_whitespace().next().unwrap_or("");
        matches!(
            first,
            "subgraph" | "end" | "direction" | "style" | "classDef" | "class" | "click"
                | "linkStyle"
        )
    }

    fn ensure_node(
        nodes: &mut Vec<Node>,
        seen: &mut HashMap<String, usize>,
        id: &str,
        label: Option<String>,
    ) {
        if let Some(&i) = seen.get(id) {
            if let Some(l) = label {
                if !l.is_empty() {
                    nodes[i].label = l;
                }
            }
        } else {
            let label = label.filter(|l| !l.is_empty()).unwrap_or_else(|| id.to_string());
            seen.insert(id.to_string(), nodes.len());
            nodes.push(Node {
                id: id.to_string(),
                label,
            });
        }
    }

    fn parse_statement(
        stmt: &str,
        nodes: &mut Vec<Node>,
        edges: &mut Vec<Edge>,
        seen: &mut HashMap<String, usize>,
    ) {
        let toks = tokenize(stmt);
        let mut prev: Option<String> = None;
        let mut pending_label: Option<String> = None;
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tok::Node(id, label) => {
                    ensure_node(nodes, seen, id, label.clone());
                    if let Some(from) = prev.take() {
                        edges.push(Edge {
                            from,
                            to: id.clone(),
                            label: pending_label.take(),
                        });
                    }
                    prev = Some(id.clone());
                }
                Tok::Arrow(head) => {
                    // Inline edge label: `A -- text --> B` tokenizes as
                    // Arrow(no head), Node(text), Arrow(head); fold the middle
                    // node into this edge's label.
                    if !head {
                        if let (Some(Tok::Node(lbl, None)), Some(Tok::Arrow(true))) =
                            (toks.get(i + 1), toks.get(i + 2))
                        {
                            pending_label = Some(lbl.clone());
                            i += 2;
                        }
                    }
                }
                Tok::Pipe(label) => pending_label = Some(label.clone()),
            }
            i += 1;
        }
    }

    fn tokenize(stmt: &str) -> Vec<Tok> {
        let chars: Vec<char> = stmt.chars().collect();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
            } else if c == '|' {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '|' {
                    j += 1;
                }
                toks.push(Tok::Pipe(chars[start..j].iter().collect::<String>().trim().to_string()));
                i = (j + 1).min(chars.len());
            } else if matches!(c, '-' | '=' | '.' | '>' | '<') {
                let mut j = i;
                let mut head = false;
                while j < chars.len() && matches!(chars[j], '-' | '=' | '.' | '>' | '<') {
                    head |= matches!(chars[j], '>' | '<');
                    j += 1;
                }
                toks.push(Tok::Arrow(head));
                i = j;
            } else if c.is_alphanumeric() || c == '_' {
                let start = i;
                let mut j = i;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let id: String = chars[start..j].iter().collect();
                let (label, k) = read_shape(&chars, j);
                toks.push(Tok::Node(id, label));
                i = k;
            } else {
                i += 1;
            }
        }
        toks
    }

    /// Read a node-shape group (`[x]`, `(x)`, `{x}`, `((x))`, `([x])`, …) after
    /// an id, returning its inner label. Tolerant: skips repeated open/close
    /// brackets and takes the text between.
    fn read_shape(chars: &[char], pos: usize) -> (Option<String>, usize) {
        if !matches!(chars.get(pos), Some('[' | '(' | '{')) {
            return (None, pos);
        }
        let mut j = pos;
        while matches!(chars.get(j), Some('[' | '(' | '{')) {
            j += 1;
        }
        let start = j;
        while j < chars.len() && !matches!(chars[j], ']' | ')' | '}') {
            j += 1;
        }
        let inner: String = chars[start..j].iter().collect();
        while matches!(chars.get(j), Some(']' | ')' | '}')) {
            j += 1;
        }
        (Some(inner.trim().to_string()), j)
    }

    /// Render a parsed flowchart to styled lines.
    pub fn render(fc: &Flowchart, width: usize) -> Vec<Line<'static>> {
        if let Some(order) = linear_order(fc) {
            render_linear(fc, &order, width)
        } else {
            render_edge_list(fc, width)
        }
    }

    /// If the chart is a single A→B→C path, return the node ids in order.
    fn linear_order(fc: &Flowchart) -> Option<Vec<String>> {
        if fc.nodes.len() < 2 || fc.edges.len() != fc.nodes.len() - 1 {
            return None;
        }
        let mut out: HashMap<&str, usize> = HashMap::new();
        let mut inc: HashMap<&str, usize> = HashMap::new();
        let mut next: HashMap<&str, &str> = HashMap::new();
        for e in &fc.edges {
            *out.entry(&e.from).or_default() += 1;
            *inc.entry(&e.to).or_default() += 1;
            next.insert(&e.from, &e.to);
        }
        if out.values().any(|&v| v > 1) || inc.values().any(|&v| v > 1) {
            return None;
        }
        let start = fc.nodes.iter().find(|n| !inc.contains_key(n.id.as_str()))?;
        let mut order = vec![start.id.clone()];
        let mut cur = start.id.as_str();
        while let Some(&nx) = next.get(cur) {
            order.push(nx.to_string());
            cur = nx;
        }
        (order.len() == fc.nodes.len()).then_some(order)
    }

    fn label_of<'a>(fc: &'a Flowchart, id: &str) -> &'a str {
        fc.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.label.as_str())
            .unwrap_or("?")
    }

    fn truncate(s: &str, max: usize) -> String {
        let max = max.max(3);
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let keep: String = s.chars().take(max - 1).collect();
            format!("{keep}…")
        }
    }

    fn render_linear(fc: &Flowchart, order: &[String], width: usize) -> Vec<Line<'static>> {
        let box_style = Style::default().fg(Color::Cyan);
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = Vec::new();
        let label_cap = width.saturating_sub(6).max(3);

        // Edge labels keyed by source id (linear → one out-edge per node).
        let edge_label: HashMap<&str, &str> = fc
            .edges
            .iter()
            .filter_map(|e| e.label.as_deref().map(|l| (e.from.as_str(), l)))
            .collect();

        for (idx, id) in order.iter().enumerate() {
            let label = truncate(label_of(fc, id), label_cap);
            let inner_w = label.chars().count();
            let bar = "─".repeat(inner_w + 2);
            let center = inner_w / 2 + 1; // column of the box's center
            lines.push(Line::from(Span::styled(format!("┌{bar}┐"), box_style)));
            lines.push(Line::from(Span::styled(format!("│ {label} │"), box_style)));
            lines.push(Line::from(Span::styled(format!("└{bar}┘"), box_style)));

            if idx + 1 < order.len() {
                let pad = " ".repeat(center);
                lines.push(Line::from(Span::styled(format!("{pad}│"), dim)));
                let tail = match edge_label.get(id.as_str()) {
                    Some(l) => format!("{pad}▼ {}", truncate(l, label_cap)),
                    None => format!("{pad}▼"),
                };
                lines.push(Line::from(Span::styled(tail, dim)));
            }
        }
        lines
    }

    fn render_edge_list(fc: &Flowchart, width: usize) -> Vec<Line<'static>> {
        let arrow_style = Style::default().fg(Color::Cyan);
        let dim = Style::default().fg(Color::DarkGray);
        let label_cap = (width / 2).max(6);
        let mut lines = Vec::new();

        for e in &fc.edges {
            let from = truncate(label_of(fc, &e.from), label_cap);
            let to = truncate(label_of(fc, &e.to), label_cap);
            let mid = match e.label.as_deref() {
                Some(l) if !l.is_empty() => format!(" ──{}──▶ ", truncate(l, label_cap)),
                _ => " ──▶ ".to_string(),
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  {from}")),
                Span::styled(mid, arrow_style),
                Span::raw(to),
            ]));
        }

        // Nodes that appear in no edge are listed on their own.
        let connected: std::collections::HashSet<&str> = fc
            .edges
            .iter()
            .flat_map(|e| [e.from.as_str(), e.to.as_str()])
            .collect();
        for n in &fc.nodes {
            if !connected.contains(n.id.as_str()) {
                lines.push(Line::from(vec![
                    Span::styled("  • ", dim),
                    Span::raw(truncate(&n.label, label_cap)),
                ]));
            }
        }
        lines
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn plain(lines: &[Line]) -> String {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn non_flowchart_returns_none() {
            assert!(parse("sequenceDiagram\n A->>B: hi").is_none());
            assert!(parse("not mermaid at all").is_none());
        }

        #[test]
        fn linear_chain_renders_boxed_flow() {
            let fc = parse("graph TD\n A[Start] --> B[Work]\n B --> C[Done]").unwrap();
            assert_eq!(fc.nodes.len(), 3);
            assert_eq!(fc.edges.len(), 2);
            let out = plain(&render(&fc, 40));
            assert!(out.contains("Start") && out.contains("Work") && out.contains("Done"));
            assert!(out.contains('┌') && out.contains("▼"), "boxed flow: {out}");
        }

        #[test]
        fn edge_labels_parsed_pipe_and_inline() {
            let fc = parse("flowchart TD\n A -->|yes| B\n A -- no --> C").unwrap();
            // A has two out-edges → branching → edge list.
            let out = plain(&render(&fc, 60));
            assert!(out.contains("yes"), "pipe label: {out}");
            assert!(out.contains("no"), "inline label: {out}");
            assert!(out.contains("▶"));
        }

        #[test]
        fn branching_uses_edge_list() {
            let fc = parse("graph LR\n A --> B\n A --> C").unwrap();
            assert!(linear_order(&fc).is_none());
            let out = plain(&render(&fc, 40));
            assert_eq!(out.matches("▶").count(), 2);
        }
    }
}

/// Inline image rendering: decode local image files and convert them to
/// half-block (`▀`/`▄`) cells that flow as normal lines in the preview, so they
/// scroll and clip like text and work in any terminal. Feature-gated.
#[cfg(feature = "images")]
mod images {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type Cache = Mutex<HashMap<(String, usize), Option<Arc<Vec<Line<'static>>>>>>;

    fn cache() -> &'static Cache {
        static C: OnceLock<Cache> = OnceLock::new();
        C.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Resolve a markdown image destination to a local file path, expanding a
    /// leading `~`. Returns `None` for remote / `data:` URLs or empty input.
    pub fn resolve_image_path(dest: &str) -> Option<std::path::PathBuf> {
        let d = dest.trim();
        if d.is_empty()
            || d.starts_with("http://")
            || d.starts_with("https://")
            || d.starts_with("data:")
        {
            return None;
        }
        if let Some(rest) = d.strip_prefix("~/") {
            Some(home_dir()?.join(rest))
        } else if d == "~" {
            home_dir()
        } else {
            Some(std::path::PathBuf::from(d))
        }
    }

    fn home_dir() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }

    /// Render an image destination to cached half-block lines at `width` cells.
    /// Failures (missing file, decode error, remote URL) are cached as `None` so
    /// the per-frame preview doesn't retry them.
    pub fn render_image_lines(dest: &str, width: usize) -> Option<Arc<Vec<Line<'static>>>> {
        let path = resolve_image_path(dest)?;
        let key = (path.to_string_lossy().into_owned(), width);
        let mut guard = cache().lock().ok()?;
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
        let rendered = image::open(&path)
            .ok()
            .map(|img| Arc::new(image_to_halfblock_lines(&img, width, 24)));
        guard.insert(key, rendered.clone());
        rendered
    }

    /// Convert an image to half-block lines: each cell is `▀` with the top pixel
    /// as foreground and the bottom pixel as background, so one text row shows
    /// two pixel rows. Width is in cells; height is capped at `max_h_cells`.
    pub fn image_to_halfblock_lines(
        img: &image::DynamicImage,
        max_w: usize,
        max_h_cells: usize,
    ) -> Vec<Line<'static>> {
        use image::GenericImageView;
        let (iw, ih) = img.dimensions();
        if iw == 0 || ih == 0 || max_w == 0 {
            return Vec::new();
        }
        let cols = max_w as u32;
        let mut px_w = cols;
        let mut px_h = (px_w * ih / iw).max(1);
        let max_px_h = (max_h_cells.max(1) as u32) * 2;
        if px_h > max_px_h {
            px_h = max_px_h;
            px_w = (px_h * iw / ih).max(1).min(cols);
        }
        let rgba = img
            .resize_exact(px_w, px_h, image::imageops::FilterType::Triangle)
            .to_rgba8();

        let mut lines = Vec::with_capacity((px_h as usize).div_ceil(2));
        let mut y = 0;
        while y < px_h {
            let mut spans = Vec::with_capacity(px_w as usize);
            for x in 0..px_w {
                let top = rgba.get_pixel(x, y).0;
                let bottom = if y + 1 < px_h {
                    rgba.get_pixel(x, y + 1).0
                } else {
                    [0, 0, 0, 0]
                };
                spans.push(half_block_span(top, bottom));
            }
            lines.push(Line::from(spans));
            y += 2;
        }
        lines
    }

    /// A single half-block cell for a (top, bottom) RGBA pixel pair, honoring
    /// transparency by leaving the corresponding half the terminal default.
    fn half_block_span(top: [u8; 4], bottom: [u8; 4]) -> Span<'static> {
        let top_opaque = top[3] > 16;
        let bottom_opaque = bottom[3] > 16;
        let top_fg = Color::Rgb(top[0], top[1], top[2]);
        let bottom_fg = Color::Rgb(bottom[0], bottom[1], bottom[2]);
        match (top_opaque, bottom_opaque) {
            (true, true) => Span::styled("▀", Style::default().fg(top_fg).bg(bottom_fg)),
            (true, false) => Span::styled("▀", Style::default().fg(top_fg)),
            (false, true) => Span::styled("▄", Style::default().fg(bottom_fg)),
            (false, false) => Span::raw(" "),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn resolve_skips_remote_and_expands_home() {
            assert!(resolve_image_path("https://example.com/a.png").is_none());
            assert!(resolve_image_path("  ").is_none());
            assert_eq!(
                resolve_image_path("/abs/a.png"),
                Some(std::path::PathBuf::from("/abs/a.png"))
            );
            if let Some(home) = home_dir() {
                assert_eq!(resolve_image_path("~/pics/a.png"), Some(home.join("pics/a.png")));
            }
        }

        #[test]
        fn halfblock_lines_match_dimensions() {
            // A 4-wide, 6-tall image at width 4 → 4 spans/line, 3 lines (6px/2).
            let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                4,
                6,
                image::Rgba([10, 20, 30, 255]),
            ));
            let lines = image_to_halfblock_lines(&img, 4, 24);
            assert_eq!(lines.len(), 3);
            assert!(lines.iter().all(|l| l.spans.len() == 4));
            // Opaque pixels → a half-block glyph carrying the color.
            assert_eq!(lines[0].spans[0].content.as_ref(), "▀");
            assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));
        }
    }
}

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut plain = String::new();
    let mut segments = Vec::new();

    for span in line.spans {
        let text = span.content.into_owned();
        let start = plain.len();
        plain.push_str(&text);
        let end = plain.len();
        segments.push((start, end, text, span.style));
    }

    if plain.is_empty() {
        return vec![Line::from("")];
    }

    wrap::wrap(&plain, width)
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (segment_start, segment_end, text, style) in &segments {
                let start = row.start.max(*segment_start);
                let end = row.end.min(*segment_end);
                if start < end {
                    let local_start = start - *segment_start;
                    let local_end = end - *segment_start;
                    spans.push(Span::styled(
                        text[local_start..local_end].to_string(),
                        *style,
                    ));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.as_ref().trim().is_empty())
}

fn trim_trailing_blank_lines(lines: &mut Vec<Line<'static>>) {
    while lines.last().is_some_and(line_is_blank) {
        lines.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn link_targets(titles: &[&str]) -> std::collections::HashSet<String> {
        titles
            .iter()
            .map(|t| crate::notes::wikilinks::normalize_title(t))
            .collect()
    }

    #[test]
    fn source_line_rows_map_each_line_to_its_output_row() {
        // Three paragraphs separated by blank lines (5 source lines total).
        let src = "alpha\n\nbravo\n\ncharlie";
        let out = render_full(src, 80, &PreviewOptions::default(), &Default::default());
        let text = rendered_text(&out.lines);

        // One dense entry per source line, monotonic non-decreasing.
        assert_eq!(out.source_line_rows.len(), 5);
        assert!(out.source_line_rows.windows(2).all(|w| w[0] <= w[1]));

        // The mapped row for each content line actually holds that line's text.
        assert!(text[out.source_line_rows[0]].contains("alpha"));
        assert!(text[out.source_line_rows[2]].contains("bravo"));
        assert!(text[out.source_line_rows[4]].contains("charlie"));
    }

    #[test]
    fn headings_collected_with_level_text_and_row() {
        let src = "# Title\n\nintro\n\n## Section\n\nbody";
        let out = render_full(src, 80, &PreviewOptions::default(), &Default::default());
        let text = rendered_text(&out.lines);

        assert_eq!(out.headings.len(), 2);
        assert_eq!((out.headings[0].level, out.headings[0].text.as_str()), (1, "Title"));
        assert_eq!((out.headings[1].level, out.headings[1].text.as_str()), (2, "Section"));

        // Each heading's recorded row holds its text.
        assert!(text[out.headings[0].row].contains("Title"));
        assert!(text[out.headings[1].row].contains("Section"));
    }

    #[test]
    fn superscript_and_subscript_render_unicode() {
        let opts = PreviewOptions::default(); // sup_sub defaults on
        let text = rendered_text(&render("x^2^ and H~2~O", 80, &opts)).join("\n");
        assert!(text.contains("x²"), "superscript: {text}");
        assert!(text.contains("H₂O"), "subscript: {text}");

        // A caret with no closing partner stays literal (no false transform).
        let stray = rendered_text(&render("3 ^ 4 caret", 80, &opts)).join("\n");
        assert!(stray.contains("3 ^ 4 caret"), "stray caret: {stray}");

        // Disabling the toggle leaves the markers untouched.
        let off = PreviewOptions { sup_sub: false, ..Default::default() };
        let raw = rendered_text(&render("x^2^", 80, &off)).join("\n");
        assert!(raw.contains("x^2^"), "disabled: {raw}");
    }

    #[test]
    fn latex_to_unicode_handles_symbols_scripts_and_frac() {
        assert_eq!(latex_to_unicode("\\frac{a}{b}"), "a⁄b");
        assert_eq!(latex_to_unicode("x_i"), "xᵢ");
        assert_eq!(latex_to_unicode("\\sum_{i=1}^{n}"), "∑ᵢ₌₁ⁿ");
        assert_eq!(latex_to_unicode("\\theta + \\pi"), "θ + π");
    }

    #[test]
    fn math_renders_when_enabled_and_preserves_prices() {
        let opts = PreviewOptions { math: true, ..Default::default() };
        let text = rendered_text(&render("Let $E = mc^2$ and $\\alpha+\\beta$.", 80, &opts)).join("\n");
        assert!(text.contains("E = mc²"), "math: {text}");
        assert!(text.contains("α+β"), "greek: {text}");

        // A `$` pair with a space before the closing `$` is not math (prices).
        let money = rendered_text(&render("Costs $5 and $10 now.", 80, &opts)).join("\n");
        assert!(money.contains("$5 and $10"), "prices preserved: {money}");
    }

    #[test]
    fn math_off_by_default_leaves_dollars_literal() {
        let text = rendered_text(&render("$E=mc^2$", 80, &PreviewOptions::default())).join("\n");
        assert!(text.contains("$E=mc^2$"), "math off: {text}");
    }

    #[test]
    fn mermaid_block_renders_diagram_when_on_else_source() {
        let src = "```mermaid\ngraph TD\n A[Start] --> B[End]\n```";
        // Default (mermaid on) → ASCII diagram, raw source hidden.
        let on = rendered_text(&render(src, 60, &PreviewOptions::default())).join("\n");
        assert!(on.contains("Start") && on.contains('┌'), "diagram: {on}");
        assert!(!on.contains("graph TD"), "source hidden: {on}");

        // Off → the raw mermaid source is shown as a normal code block.
        let opts = PreviewOptions { mermaid: false, ..Default::default() };
        let off = rendered_text(&render(src, 60, &opts)).join("\n");
        assert!(off.contains("graph TD"), "source shown: {off}");
    }

    #[test]
    fn wikilinks_off_keeps_literal_text() {
        // With wikilinks disabled, `[[Foo]]` must survive as literal text.
        let lines = render("see [[Foo Bar]] here", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("[[Foo Bar]]"), "got: {text}");
    }

    #[test]
    fn wikilink_live_renders_display_without_brackets() {
        let opts = PreviewOptions {
            wikilinks: true,
            ..Default::default()
        };
        let targets = link_targets(&["Foo Bar"]);
        let lines = render_full("see [[Foo Bar]] here", 80, &opts, &targets).lines;
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("see Foo Bar here"), "got: {text}");
        assert!(!text.contains("[["), "brackets should be consumed: {text}");
        // The display span carries the live (cyan) link style.
        let styled = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "Foo Bar")
            .expect("link span");
        assert_eq!(styled.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn wikilink_alias_and_dangling_marker() {
        let opts = PreviewOptions {
            wikilinks: true,
            ..Default::default()
        };
        // "Live" exists; "Ghost" does not.
        let targets = link_targets(&["Live"]);
        let lines = render_full("[[Live|shown]] and [[Ghost]]", 80, &opts, &targets).lines;
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("shown"), "alias display: {text}");
        assert!(!text.contains("Live"), "alias replaces target: {text}");
        assert!(text.contains("Ghost (?)"), "dangling marker: {text}");
    }

    #[test]
    fn focused_wikilink_is_reverse_highlighted() {
        let opts = PreviewOptions {
            wikilinks: true,
            focused_wikilink: Some(1),
            ..Default::default()
        };
        let targets = link_targets(&["One", "Two"]);
        let lines = render_full("[[One]] then [[Two]]", 80, &opts, &targets).lines;
        let spans: Vec<&Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        let one = spans.iter().find(|s| s.content.as_ref() == "One").unwrap();
        let two = spans.iter().find(|s| s.content.as_ref() == "Two").unwrap();
        assert!(
            !one.style.add_modifier.contains(Modifier::REVERSED),
            "unfocused link not highlighted"
        );
        assert!(
            two.style.add_modifier.contains(Modifier::REVERSED),
            "focused link reverse-highlighted"
        );
    }

    #[test]
    fn wikilink_in_table_cell_expands_to_display_text() {
        let opts = PreviewOptions {
            wikilinks: true,
            ..Default::default()
        };
        let targets = link_targets(&["Foo"]);
        let lines = render_full(
            "| a | b |\n|---|---|\n| [[Foo]] | [[Bar|baz]] |",
            80,
            &opts,
            &targets,
        )
        .lines;
        let text = rendered_text(&lines).join("\n");
        // No stray sentinel chars; display text shown in the cell.
        assert!(text.contains("Foo"), "got: {text}");
        assert!(text.contains("baz"), "alias in cell: {text}");
        assert!(!text.contains('\u{E000}'), "no leaked sentinel: {text:?}");
    }

    #[test]
    fn renders_headings_and_emphasis() {
        let lines = render(
            "# Heading\n\nPlain **bold** and *italic*.",
            80,
            &PreviewOptions::default(),
        );

        assert_eq!(rendered_text(&lines)[0], "Heading");
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));

        let bold = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));

        let italic = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "italic")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn renders_lists_tasks_quotes_rules_and_code() {
        let source = "- [x] done\n- [ ] todo\n\n> quoted\n\n---\n\n`inline`\n\n```\nblock\n```";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");

        assert!(text.contains("☑ done"));
        assert!(text.contains("☐ todo"));
        assert!(text.contains("▎ quoted"));
        assert!(text.contains("──"));
        assert!(text.contains(" inline "));
        assert!(text.contains("  block"));
    }

    #[test]
    fn raw_fallback_preserves_markdown_source() {
        let opts = PreviewOptions {
            render_markdown: false,
            ..Default::default()
        };
        let lines = render("# Raw **source**", 80, &opts);
        assert_eq!(rendered_text(&lines), vec!["# Raw **source**"]);
    }

    #[test]
    fn wraps_plain_fallback_with_shared_rows() {
        let opts = PreviewOptions {
            render_markdown: false,
            ..Default::default()
        };
        let lines = render("abcdef", 3, &opts);
        assert_eq!(rendered_text(&lines), vec!["abc", "def"]);
    }

    #[test]
    fn malformed_markdown_does_not_panic() {
        let lines = render("**unterminated", 80, &PreviewOptions::default());
        assert!(rendered_text(&lines).join("\n").contains("unterminated"));
    }

    #[test]
    fn link_inline_shows_url_in_parentheses() {
        let lines = render(
            "See [docs](https://example.com).",
            80,
            &PreviewOptions::default(),
        );
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("docs"));
        assert!(text.contains("(https://example.com)"));
    }

    #[test]
    fn link_footnote_numbers_and_lists_urls() {
        let opts = PreviewOptions {
            link_urls: LinkUrlMode::Footnote,
            ..Default::default()
        };
        let lines = render("[a](http://x) and [b](http://y)", 80, &opts);
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("a[1]"));
        assert!(text.contains("b[2]"));
        assert!(text.contains("[1] http://x"));
        assert!(text.contains("[2] http://y"));
    }

    #[test]
    fn link_hide_omits_url() {
        let opts = PreviewOptions {
            link_urls: LinkUrlMode::Hide,
            ..Default::default()
        };
        let lines = render("[docs](https://example.com)", 80, &opts);
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("docs"));
        assert!(!text.contains("example.com"));
    }

    #[test]
    fn fenced_code_block_shows_language_label() {
        let lines = render("```rust\nlet x = 1;\n```", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("rust"));
        assert!(text.contains("let x = 1;"));
    }

    #[test]
    fn table_renders_aligned_with_header_rule() {
        let source = "| Name | Qty |\n| --- | --- |\n| Apple | 3 |\n| Pear | 12 |";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines);
        // Header present and column-separated.
        assert!(text.iter().any(|l| l.contains("Name") && l.contains("Qty")));
        // A header rule line of box-drawing chars.
        assert!(text.iter().any(|l| l.contains("─┼─")));
        // Cells aligned to a common column width (Apple wider than Pear).
        assert!(text.iter().any(|l| l.contains("Apple")));
        assert!(text.iter().any(|l| l.contains("Pear ")));
    }

    #[test]
    fn wide_table_falls_back_to_stacked() {
        let source = "| Name | Description |\n| --- | --- |\n| A | a long description here |";
        // Narrow width forces the stacked fallback.
        let lines = render(source, 12, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("Name: A"));
        assert!(text.contains("Description:"));
    }

    #[test]
    fn image_renders_placeholder_with_alt() {
        let lines = render("![a cat](cat.png)", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("🖼"));
        assert!(text.contains("a cat"));
        // Alt text must not also leak as a separate bare paragraph.
        assert_eq!(text.matches("a cat").count(), 1);
    }

    #[test]
    fn emoji_shortcode_substitutes() {
        let lines = render("hello :smile:", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("😊"));
        assert!(!text.contains(":smile:"));
    }

    #[test]
    fn unknown_emoji_shortcode_stays_literal() {
        let lines = render("a :not_a_real_emoji: b", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains(":not_a_real_emoji:"));
    }

    #[test]
    fn mark_applies_reversed_modifier() {
        let lines = render("a ==highlighted== b", 80, &PreviewOptions::default());
        let span = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "highlighted")
            .expect("marked span");
        assert!(span.style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn ins_applies_underline_modifier() {
        let lines = render("a ++inserted++ b", 80, &PreviewOptions::default());
        let span = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "inserted")
            .expect("inserted span");
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn front_matter_is_suppressed() {
        let source = "---\ntitle: Secret\ntags: a\n---\n\nVisible body.";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("Visible body."));
        assert!(!text.contains("title"));
        assert!(!text.contains("Secret"));
    }

    #[test]
    fn custom_container_renders_callout() {
        let source = "::: warning\nWatch out here.\n:::";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("Warning"));
        assert!(text.contains("Watch out here."));
    }

    #[test]
    fn github_alert_renders_as_callout() {
        let source = "> [!WARNING]\n> Watch out here.\n> Second line.";
        let text = rendered_text(&render(source, 80, &PreviewOptions::default())).join("\n");
        assert!(text.contains("Warning"), "label: {text}");
        assert!(text.contains("Watch out here."), "body: {text}");
        assert!(text.contains("Second line."), "multi-line body: {text}");
        assert!(!text.contains("[!WARNING]"), "marker consumed: {text}");
    }

    #[test]
    fn plain_blockquote_is_not_a_callout() {
        let text = rendered_text(&render("> just a quote", 80, &PreviewOptions::default())).join("\n");
        assert!(text.contains("just a quote"));
        // No callout label/icon was injected.
        assert!(!text.contains("Note") && !text.contains('ℹ'), "got: {text}");
    }

    #[test]
    fn definition_list_renders_term_and_definition() {
        let source = "Coffee\n: A hot beverage.";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        // Term and definition both present; raw HTML tags must not leak.
        assert!(text.contains("Coffee"));
        assert!(text.contains("A hot beverage."));
        assert!(!text.contains("<dl>"));
        assert!(!text.contains("<dd>"));
    }

    #[test]
    fn footnotes_number_by_reference_order_and_list_definitions() {
        let source = "First[^a] then second[^b].\n\n[^b]: Second note.\n[^a]: First note.";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        // References render as superscripts in first-use order: a→1, b→2.
        assert!(text.contains("First¹"));
        assert!(text.contains("second²"));
        // Definitions are listed at the bottom, numbered to match the refs
        // (not source order), with their markup.
        assert!(text.contains("1. First note."));
        assert!(text.contains("2. Second note."));
        // Raw footnote syntax must not leak.
        assert!(!text.contains("[^a]"));
        assert!(!text.contains("[fn:"));
    }

    #[test]
    fn abbreviation_definitions_are_stripped() {
        let source = "The HTML spec.\n\n*[HTML]: HyperText Markup Language";
        let lines = render(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("The HTML spec."));
        assert!(!text.contains("HyperText Markup Language"));
    }

    #[test]
    fn linkify_styles_bare_urls_when_enabled() {
        let opts = PreviewOptions {
            linkify: true,
            ..Default::default()
        };
        let lines = render("visit https://example.com now", 80, &opts);
        let span = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == "https://example.com")
            .expect("linkified url span");
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn linkify_off_by_default_leaves_url_plain() {
        let lines = render("visit https://example.com now", 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("https://example.com"));
        // URL should remain part of a normal text span (no dedicated split).
        assert!(!lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.as_ref() == "https://example.com"
                && span.style.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[cfg(feature = "syntax-highlight")]
    #[test]
    fn syntax_highlight_tokens_rust() {
        let lines = render("```rust\nlet x = 1;\n```\n", 80, &PreviewOptions::default());

        // The code line should be tokenized into more than one span (keyword,
        // identifier, punctuation, etc.) and at least one span should have a
        // non-default foreground color.
        let code_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref().contains("let")))
            .expect("'let' keyword span should exist");

        let colored_spans: Vec<_> = code_line
            .spans
            .iter()
            .filter(|s| {
                !s.content.as_ref().trim().is_empty()
                    && s.style.fg.is_some()
                    && s.style.fg != Some(Color::default())
            })
            .collect();

        assert!(
            colored_spans.len() >= 2,
            "expected at least 2 colored token spans in highlighted rust code, got {}: {:?}",
            colored_spans.len(),
            code_line
                .spans
                .iter()
                .map(|s| format!("{:?}", s.content))
                .collect::<Vec<_>>()
        );
    }

    // ── code-block extraction & actions ──

    #[test]
    fn extract_code_blocks_captures_lang_and_code() {
        let source = "intro\n\n```bash\necho hi\nls\n```\n\ntail";
        let blocks = extract_code_blocks(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang.as_deref(), Some("bash"));
        assert_eq!(blocks[0].code, "echo hi\nls");
        // end_byte points just past the closing fence.
        assert!(source[..blocks[0].end_byte].ends_with("```"));
    }

    #[test]
    fn extract_code_blocks_multiple_and_langless() {
        let source = "```\nplain\n```\n\ntext\n\n```python\nprint(1)\n```";
        let blocks = extract_code_blocks(source);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lang, None);
        assert!(blocks[0].is_runnable()); // no lang → runnable
        assert_eq!(blocks[1].lang.as_deref(), Some("python"));
        assert!(!blocks[1].is_runnable());
    }

    #[test]
    fn extract_code_blocks_unterminated_does_not_panic() {
        let blocks = extract_code_blocks("```bash\necho hi\nno close fence");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].code.contains("echo hi"));
    }

    #[test]
    fn insert_output_when_absent() {
        let source = "```bash\necho hi\n```\n";
        let end = source.find("```\n").unwrap() + 3; // past closing fence
        let out = insert_or_replace_output(source, end, "hi");
        assert!(out.contains("```output\nhi\n```"));
        // The original block survives.
        assert!(out.contains("echo hi"));
    }

    #[test]
    fn insert_output_replaces_existing_adjacent_block() {
        let source = "```bash\necho hi\n```\n\n```output\nstale\n```\n\nafter";
        let end = source.find("```\n").unwrap() + 3;
        let out = insert_or_replace_output(source, end, "fresh");
        assert!(out.contains("```output\nfresh\n```"));
        assert!(!out.contains("stale"));
        // Content past the old output block is preserved exactly once.
        assert_eq!(out.matches("after").count(), 1);
    }

    #[test]
    fn focused_code_block_gets_distinct_gutter() {
        let source = "```bash\necho hi\n```";
        let opts = PreviewOptions {
            focused_code_block: Some(0),
            ..PreviewOptions::default()
        };
        let (lines, rows) = render_with_code_rows(source, 80, &opts);
        assert_eq!(rows.len(), 1);
        let text = rendered_text(&lines).join("\n");
        assert!(text.contains("┃"), "focused block should show the ┃ gutter: {text}");
    }

    #[test]
    fn unfocused_code_block_has_no_focus_gutter() {
        let source = "```bash\necho hi\n```";
        let (lines, _) = render_with_code_rows(source, 80, &PreviewOptions::default());
        let text = rendered_text(&lines).join("\n");
        assert!(!text.contains("┃"));
    }
}
