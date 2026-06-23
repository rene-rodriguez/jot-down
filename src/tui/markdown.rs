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
#[derive(Debug, Clone, Copy)]
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
    render_full(source, width, opts, &std::collections::HashSet::new())
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
) -> (Vec<Line<'static>>, Vec<usize>) {
    let width = usize::from(width.max(1));
    if !opts.render_markdown {
        return (render_plain(source, width), Vec::new());
    }

    // Pre-passes (source-level transforms).
    let source = strip_abbreviations(source, opts.abbreviations);
    let source = strip_definition_lists(&source, opts.definition_lists);
    let source = convert_custom_containers(&source, opts.custom_containers);

    // Mask `[[wikilinks]]` into bracket-free sentinels before parsing, so they
    // survive pulldown's inline tokenizer; the renderer expands them back into
    // styled spans. Skipped (no allocation, no table) when wikilinks are off.
    let (source, wikilink_table) = if opts.wikilinks {
        let (masked, table) = crate::notes::wikilinks::mask_wikilinks(&source);
        (Cow::Owned(masked), table)
    } else {
        (source, Vec::new())
    };

    Renderer::new(width, *opts, link_targets.clone(), wikilink_table).render(&source)
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
    /// Normalized titles of existing notes, for resolving `[[wikilinks]]` as
    /// live vs dangling. Empty unless `opts.wikilinks` is on.
    link_targets: std::collections::HashSet<String>,
    /// Wikilinks masked out of the source, indexed by the sentinel number; the
    /// `add_text` pass expands `\u{E000}<i>\u{E001}` back into styled spans.
    wikilink_table: Vec<crate::notes::wikilinks::WikiLink>,
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
            link_targets,
            wikilink_table,
        }
    }

    fn render(mut self, source: &str) -> (Vec<Line<'static>>, Vec<usize>) {
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

        for event in Parser::new_ext(source, options) {
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

        self.flush_line();
        self.render_footnote_defs();
        self.render_footnotes();
        trim_trailing_blank_lines(&mut self.lines);
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        (self.lines, self.code_block_rows)
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
            Tag::Image { .. } => {
                // Buffer alt text from subsequent Event::Text; emit placeholder on close.
                self.image_alt = Some(String::new());
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
                if let Some(alt) = self.image_alt.take() {
                    if self.is_suppressed() {
                        return;
                    }
                    let label = if alt.is_empty() { "image" } else { &alt };
                    self.ensure_prefix();
                    self.current.push(Span::styled(
                        format!("🖼 {} ", label),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
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

            // Check for ^superscript^ (only if enabled)
            if self.opts.sup_sub && i + 2 <= len && chars[i] == '^' {
                if active_sup {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sup = false;
                } else {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sup = true;
                }
                i += 1;
                continue;
            }

            // Check for ~subscript~ (only if enabled)
            if self.opts.sup_sub && i + 2 <= len && chars[i] == '~' {
                if active_sub {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sub = false;
                } else {
                    flush_buf(&mut buf, &mut spans, base_style);
                    active_sub = true;
                }
                i += 1;
                continue;
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
            let (ss, theme) = highlighter();
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
fn highlighter() -> &'static (syntect::parsing::SyntaxSet, syntect::highlighting::Theme) {
    static H: OnceLock<(syntect::parsing::SyntaxSet, syntect::highlighting::Theme)> =
        OnceLock::new();
    H.get_or_init(|| {
        let ss = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let ts = syntect::highlighting::ThemeSet::load_defaults();
        let theme = ts.themes["base16-ocean.dark"].clone();
        (ss, theme)
    })
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
        let (lines, _) = render_full("see [[Foo Bar]] here", 80, &opts, &targets);
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
        let (lines, _) = render_full("[[Live|shown]] and [[Ghost]]", 80, &opts, &targets);
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
        let (lines, _) = render_full("[[One]] then [[Two]]", 80, &opts, &targets);
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
        let (lines, _) = render_full(
            "| a | b |\n|---|---|\n| [[Foo]] | [[Bar|baz]] |",
            80,
            &opts,
            &targets,
        );
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
