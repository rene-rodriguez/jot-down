use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::commands;
use crate::app::state::{AppState, AppView};
use crate::tui::markdown::{self, PreviewOptions};
use crate::tui::wrap;

/// Render the full application UI into the given frame.
pub fn render(frame: &mut Frame, state: &AppState) {
    // Split screen into main area and status bar
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    if state.view == AppView::Editor || state.view == AppView::EditorSearch {
        if state.editor_preview_split {
            // Editor on the left, a live markdown preview of the in-progress
            // buffer on the right (toggled with Ctrl+P).
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(main_layout[0]);
            render_editor(frame, cols[0], state);
            render_editor_preview(frame, cols[1], state);
        } else {
            render_editor(frame, main_layout[0], state);
        }
    } else if state.view == AppView::Settings {
        render_settings(frame, main_layout[0], state);
    } else if state.view == AppView::ConflictReview {
        render_conflict_list(frame, main_layout[0], state);
    } else if state.view == AppView::ConflictDetail {
        render_conflict_detail(frame, main_layout[0], state);
    } else if state.view == AppView::Ask {
        render_ask(frame, main_layout[0], state);
    } else if state.view == AppView::Help {
        render_help(frame, main_layout[0], state);
    } else if state.view == AppView::CommandPalette {
        render_palette(frame, main_layout[0], state);
    } else if state.view == AppView::Trash || state.view == AppView::ConfirmPurge {
        render_trash(frame, main_layout[0], state);
    } else {
        render_main_area(frame, main_layout[0], state);
    }
    render_status_bar(frame, main_layout[1], state);
}

/// Render the main area with sidebar (notes list) and preview pane.
fn render_main_area(frame: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3), Constraint::Ratio(2, 3)])
        .split(area);

    render_note_list(frame, chunks[0], state);
    render_preview(frame, chunks[1], state);
}

/// Render the note list sidebar.
fn render_note_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let title = if state.view == AppView::Search {
        format!(
            " Search [{}]: {} ",
            state.search_kind.label(),
            state.search_query
        )
    } else if let Some(tag) = &state.tag_filter {
        format!(" Notes (tag: {}) ", tag)
    } else {
        " Notes ".to_string()
    };

    let items: Vec<ListItem> = state
        .notes
        .iter()
        .map(|note| {
            let tags = if note.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", note.tags.join(", "))
            };
            let content = Line::from(vec![
                Span::styled(&note.title, Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(tags, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(content)
        })
        .collect();

    let notes_list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        // High-contrast selection: black text on a light background reads well in
        // both light and dark terminals (the previous blue-on-white was muddy).
        .highlight_style(
            Style::default()
                .bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");

    frame.render_stateful_widget(notes_list, area, &mut state.list_state.clone());
}

/// Render the note preview pane.
fn render_preview(frame: &mut Frame, area: Rect, state: &AppState) {
    if let Some(note) = state.selected_note() {
        let inner_width = area.width.saturating_sub(2).max(1);
        // Zen mode renders the body into a narrower, centered column.
        let render_width = if state.zen_mode {
            inner_width.min(state.settings.preview.zen_width.max(1) as u16)
        } else {
            inner_width
        };
        let zen_margin = usize::from((inner_width - render_width) / 2);
        let viewport_height = usize::from(area.height.saturating_sub(2));
        state
            .preview_viewport_height
            .set(area.height.saturating_sub(2).max(1));

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("# {}", note.title),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        if state.preview_body.is_empty() {
            lines.push(Line::from(Span::styled(
                "(empty note — press 'i' to write the body, Ctrl+S to save)",
                Style::default().fg(Color::DarkGray),
            )));
            state.preview_headings.borrow_mut().clear();
        } else {
            // Row offset of the markdown body within `lines` (title + blank above).
            let body_offset = lines.len();
            let out = markdown::render_full(
                &state.preview_body,
                render_width,
                &preview_options(state, state.focused_code_index(), state.focused_link_index()),
                &state.link_targets,
            );
            // Move out the headings and code rows, then collapse any folded
            // sections; `remap` carries original rows into folded coordinates.
            let unfolded_headings = out.headings;
            let unfolded_code_rows = out.code_rows;
            let (folded_body, remap) = {
                let folds = state.folded_headings.borrow();
                apply_folds(out.lines, &unfolded_headings, &folds)
            };
            let code_rows: Vec<usize> = unfolded_code_rows
                .iter()
                .map(|&r| remap_row(&remap, r))
                .collect();
            // Stash headings at their folded, absolute preview rows for the
            // outline overlay, jump-to-heading, and scrollbar ticks.
            *state.preview_headings.borrow_mut() = unfolded_headings
                .iter()
                .map(|h| markdown::Heading {
                    level: h.level,
                    text: h.text.clone(),
                    row: body_offset + remap_row(&remap, h.row),
                })
                .collect();
            lines.extend(folded_body);

            // When focus just moved, scroll the focused block's first row near
            // the top of the viewport (one-line margin). Honored once.
            if state.preview_focus_scroll.replace(false) {
                if let Some(row) = state
                    .focused_code_index()
                    .and_then(|i| code_rows.get(i))
                    .map(|r| body_offset + r)
                {
                    state
                        .preview_scroll
                        .set(row.saturating_sub(1).min(usize::from(u16::MAX)) as u16);
                }
            }
        }

        // Related notes (AI feature).
        if !state.related_notes.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── Related ──────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            for rn in &state.related_notes {
                let tags = if rn.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", rn.tags.join(", "))
                };
                lines.push(Line::from(vec![
                    Span::styled("▸ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&rn.title, Style::default().fg(Color::Cyan)),
                    Span::styled(tags, Style::default().fg(Color::DarkGray)),
                ]));
            }
        }

        // Backlinks — notes that link *to* this one via a resolved [[wikilink]].
        // Explicit links, shown separately from the semantic "Related" panel.
        // Panel entries are focusable (backlinks first, then "On this day"); the
        // focused one is reverse-highlighted and opens with Enter.
        let focused_panel = state.focused_panel_index();
        if !state.backlinks.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("── Linked from ({}) ──────────────────────", state.backlinks.len()),
                Style::default().fg(Color::DarkGray),
            )));
            for (i, bl) in state.backlinks.iter().enumerate() {
                let tags = if bl.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", bl.tags.join(", "))
                };
                let mut title_style = Style::default().fg(Color::Cyan);
                if focused_panel == Some(i) {
                    title_style = title_style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::from(vec![
                    Span::styled("◂ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&bl.title, title_style),
                    Span::styled(tags, Style::default().fg(Color::DarkGray)),
                ]));
            }
        }

        // "On this day" — daily notes from prior periods on the same calendar
        // day. Focus indices continue after the backlinks.
        if !state.on_this_day.is_empty() {
            let base = state.backlinks.len();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── On this day ───────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            for (k, (offset, note)) in state.on_this_day.iter().enumerate() {
                let mut title_style = Style::default().fg(Color::Cyan);
                if focused_panel == Some(base + k) {
                    title_style = title_style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::from(vec![
                    Span::styled("◷ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&note.title, title_style),
                    Span::styled(
                        format!("  ({})", crate::notes::daily::offset_label(*offset)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }

        // Tag suggestions (AI feature). Each is numbered so the user can apply
        // it by pressing the matching digit (see handle_command_event).
        if !state.suggested_tags.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── Suggested tags (press number to add) ──",
                Style::default().fg(Color::DarkGray),
            )));
            let suggestion_line = Line::from(
                state
                    .suggested_tags
                    .iter()
                    .take(9)
                    .enumerate()
                    .flat_map(|(i, t)| {
                        [
                            Span::styled(
                                format!("{}:", i + 1),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("#{}  ", t),
                                Style::default().fg(Color::Green),
                            ),
                        ]
                    })
                    .collect::<Vec<_>>(),
            );
            lines.push(suggestion_line);
        }

        // Find-in-preview: match against the rendered text, highlight the hits
        // (current match brighter), and scroll the current match into view once.
        if !state.preview_search_query.is_empty() {
            let matches = find_preview_matches(&lines, &state.preview_search_query);
            let idx = state.preview_search_idx.min(matches.len().saturating_sub(1));
            if state.preview_search_scroll.replace(false) {
                if let Some(&(row, _, _)) = matches.get(idx) {
                    let new = wrap::scroll_to_cursor(
                        usize::from(state.preview_scroll.get()),
                        row,
                        lines.len(),
                        viewport_height,
                    );
                    state.preview_scroll.set(new.min(usize::from(u16::MAX)) as u16);
                }
            }
            highlight_preview_matches(&mut lines, &matches, idx);
            *state.preview_search_matches.borrow_mut() = matches;
        } else {
            state.preview_search_matches.borrow_mut().clear();
        }

        // Zen mode: center the column by left-padding every line.
        if zen_margin > 0 {
            let pad = " ".repeat(zen_margin);
            for line in &mut lines {
                line.spans.insert(0, Span::raw(pad.clone()));
            }
        }

        let scroll = wrap::clamp_scroll(
            usize::from(state.preview_scroll.get()),
            lines.len(),
            viewport_height,
        );
        let scroll_u16 = scroll.min(usize::from(u16::MAX)) as u16;
        state.preview_scroll.set(scroll_u16);
        let max_scroll = if viewport_height == 0 {
            0
        } else {
            lines.len().saturating_sub(viewport_height)
        };
        let (words, chars) = body_counts(&state.preview_body);
        let title = if !state.preview_search_query.is_empty() {
            let total = state.preview_search_matches.borrow().len();
            let cur = if total == 0 {
                0
            } else {
                state.preview_search_idx.min(total - 1) + 1
            };
            format!(
                " Find \"{}\" — {cur}/{total} · n/N · Esc ",
                state.preview_search_query
            )
        } else {
            preview_title(
                scroll,
                max_scroll,
                words,
                chars,
                task_counts(&state.preview_body),
            )
        };
        let content_len = lines.len();
        let block = Block::default().title(title).borders(Borders::ALL);
        let paragraph = Paragraph::new(lines)
            .block(block)
            .scroll((scroll_u16, 0))
            .style(Style::default());
        frame.render_widget(paragraph, area);

        render_heading_scrollbar(
            frame,
            area,
            content_len,
            viewport_height,
            scroll,
            &state.preview_headings.borrow(),
        );

        if state.outline_open {
            render_outline_overlay(frame, area, state);
        }
    } else if state.view == AppView::Search && !state.search_query.is_empty() {
        let block = Block::default().title(" Preview ").borders(Borders::ALL);
        let paragraph = Paragraph::new("No notes found matching your search.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));

        frame.render_widget(paragraph, area);
    } else {
        let block = Block::default().title(" Preview ").borders(Borders::ALL);
        let paragraph = Paragraph::new("No notes yet. Press 'n' to create one.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));

        frame.render_widget(paragraph, area);
    }
}

/// Build `PreviewOptions` from the user's settings with the given focus indices,
/// shared by the standalone preview and the editor's live preview.
fn preview_options(
    state: &AppState,
    focused_code_block: Option<usize>,
    focused_wikilink: Option<usize>,
) -> PreviewOptions {
    PreviewOptions {
        render_markdown: state.preview_render_markdown,
        link_urls: state.settings.preview.show_link_urls,
        typographer: state.settings.preview.typographer,
        emoji: state.settings.preview.emoji,
        mark: state.settings.preview.mark,
        ins: state.settings.preview.ins,
        sup_sub: state.settings.preview.sup_sub,
        abbreviations: state.settings.preview.abbreviations,
        definition_lists: state.settings.preview.definition_lists,
        custom_containers: state.settings.preview.custom_containers,
        linkify: state.settings.preview.linkify,
        wikilinks: state.settings.preview.wikilinks,
        math: state.settings.preview.math,
        syntax_theme: state.settings.preview.syntax_theme.clone(),
        images: state.settings.preview.images,
        mermaid: state.settings.preview.mermaid,
        focused_code_block,
        focused_wikilink,
    }
}

fn hash_body(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Resolve the editor cursor's section for the live preview. Returns the body
/// output row to keep visible (the cursor's source line mapped through
/// `source_line_rows`) and the row of the nearest heading at or above it, both
/// in body-local coordinates (the caller adds the title offset). Pure, so the
/// cursor-follow + section-highlight behaviour is unit-testable without a Frame.
fn editor_preview_focus(
    source_line_rows: &[usize],
    headings: &[markdown::Heading],
    cursor_line: usize,
) -> (usize, Option<usize>) {
    let body_row = source_line_rows.get(cursor_line).copied().unwrap_or(0);
    let heading_row = headings.iter().rfind(|h| h.row <= body_row).map(|h| h.row);
    (body_row, heading_row)
}

/// Render the body editor's live markdown preview (the right pane of the split).
/// Unlike the standalone preview, this renders the *in-progress* `body_buffer`
/// (not the saved note) so it tracks every keystroke, follows the editor cursor's
/// section, and reverse-highlights the heading the cursor sits under. The parse
/// is memoized on (body hash, width) so typing stays responsive on large notes.
fn render_editor_preview(frame: &mut Frame, area: Rect, state: &AppState) {
    let inner_width = area.width.saturating_sub(2).max(1);
    let viewport_height = usize::from(area.height.saturating_sub(2).max(1));

    // Refresh the memoized render only when the body or pane width changed.
    let body_hash = hash_body(&state.body_buffer);
    {
        let mut cache = state.editor_preview_cache.borrow_mut();
        let fresh = cache
            .as_ref()
            .is_some_and(|(h, w, _)| *h == body_hash && *w == inner_width);
        if !fresh {
            let opts = preview_options(state, None, None);
            let out =
                markdown::render_full(&state.body_buffer, inner_width, &opts, &state.link_targets);
            *cache = Some((body_hash, inner_width, out));
        }
    }
    let cache = state.editor_preview_cache.borrow();
    let out = &cache.as_ref().expect("cache populated above").2;

    // Title line, mirroring the standalone preview's header.
    let title = state
        .selected_note()
        .map(|n| n.title.clone())
        .unwrap_or_default();
    let mut lines: Vec<Line> = Vec::with_capacity(out.lines.len() + 2);
    lines.push(Line::from(Span::styled(
        format!("# {title}"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    let body_offset = lines.len();

    if state.body_buffer.is_empty() {
        lines.push(Line::from(Span::styled(
            "(start typing — the preview updates live)",
            Style::default().fg(Color::DarkGray),
        )));
        state.editor_preview_scroll.set(0);
    } else {
        // Map the editor cursor to its source line, then to a body output row.
        let cursor = state.cursor_pos.min(state.body_buffer.len());
        let cursor_line = state.body_buffer.as_bytes()[..cursor]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        let (body_row, current_heading_row) =
            editor_preview_focus(&out.source_line_rows, &out.headings, cursor_line);

        lines.extend(out.lines.iter().cloned());

        if let Some(hr) = current_heading_row {
            if let Some(line) = lines.get_mut(body_offset + hr) {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
        }

        // Follow the cursor: keep its section's row visible with minimal movement.
        let target_row = body_offset + body_row;
        let scroll = wrap::scroll_to_cursor(
            usize::from(state.editor_preview_scroll.get()),
            target_row,
            lines.len(),
            viewport_height,
        );
        state
            .editor_preview_scroll
            .set(scroll.min(usize::from(u16::MAX)) as u16);
    }

    let scroll = state.editor_preview_scroll.get();
    let block = Block::default()
        .title(" Live preview ")
        .borders(Borders::ALL);
    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Collapse folded sections of the rendered body. Returns the folded body plus
/// a `remap` from each original body row to its row in the folded body (`None`
/// when the row is hidden inside a fold). Every heading survives — folded ones
/// gain a `▸` marker and a placeholder — so heading indices stay stable.
fn apply_folds(
    body: Vec<Line<'static>>,
    headings: &[markdown::Heading],
    folded: &std::collections::HashSet<usize>,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    use std::collections::HashMap;
    let n = body.len();
    if folded.is_empty() {
        return (body, (0..n).map(Some).collect());
    }

    // Hide each folded section: from the line after its heading to the next
    // heading of the same or higher level (or the end of the body).
    let mut hidden = vec![false; n];
    let mut hidden_count: HashMap<usize, usize> = HashMap::new();
    for (i, h) in headings.iter().enumerate() {
        if !folded.contains(&i) {
            continue;
        }
        let start = (h.row + 1).min(n);
        let end = headings[i + 1..]
            .iter()
            .find(|hn| hn.level <= h.level)
            .map(|hn| hn.row.min(n))
            .unwrap_or(n);
        for cell in hidden.iter_mut().take(end).skip(start) {
            *cell = true;
        }
        hidden_count.insert(h.row, end.saturating_sub(start));
    }

    let heading_at: HashMap<usize, usize> =
        headings.iter().enumerate().map(|(i, h)| (h.row, i)).collect();

    let mut folded_body: Vec<Line<'static>> = Vec::with_capacity(n);
    let mut remap = vec![None; n];
    for (r, line) in body.into_iter().enumerate() {
        if hidden[r] {
            continue;
        }
        remap[r] = Some(folded_body.len());
        match heading_at.get(&r) {
            Some(&i) if folded.contains(&i) => {
                folded_body.push(decorate_folded_heading(&line));
                folded_body.push(fold_placeholder(hidden_count.get(&r).copied().unwrap_or(0)));
            }
            _ => folded_body.push(line),
        }
    }
    (folded_body, remap)
}

/// Map an original body row to its folded-body row, falling back to the nearest
/// preceding visible row (a fold's heading) when the row itself is hidden.
fn remap_row(remap: &[Option<usize>], row: usize) -> usize {
    let row = row.min(remap.len().saturating_sub(1));
    (0..=row).rev().find_map(|k| remap[k]).unwrap_or(0)
}

/// Prefix a folded heading line with a `▸` collapse marker.
fn decorate_folded_heading(line: &Line<'static>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled("▸ ", Style::default().fg(Color::DarkGray)));
    spans.extend(line.spans.iter().cloned());
    Line::from(spans)
}

/// The placeholder shown in place of a folded section's body.
fn fold_placeholder(hidden_lines: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("   … {hidden_lines} hidden · z to expand"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// Find case-insensitive (ASCII) matches of `query` in the rendered preview
/// lines. Returns `(row, char_start, char_end)` per non-overlapping match, with
/// positions in character units relative to each line's concatenated text.
fn find_preview_matches(lines: &[Line], query: &str) -> Vec<(usize, usize, usize)> {
    let ql: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if ql.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
        let mut i = 0;
        while i + ql.len() <= chars.len() {
            let hit = chars[i..i + ql.len()]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .eq(ql.iter().copied());
            if hit {
                out.push((row, i, i + ql.len()));
                i += ql.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Highlight `matches` within `lines` in place, splitting styled spans at match
/// boundaries (the `current` match is rendered brighter) while preserving the
/// underlying markdown styling of unmatched text.
fn highlight_preview_matches(lines: &mut [Line], matches: &[(usize, usize, usize)], current: usize) {
    use std::collections::HashMap;
    let mut by_row: HashMap<usize, Vec<(usize, usize, bool)>> = HashMap::new();
    for (i, &(row, s, e)) in matches.iter().enumerate() {
        by_row.entry(row).or_default().push((s, e, i == current));
    }
    for (row, ranges) in by_row {
        if let Some(line) = lines.get_mut(row) {
            *line = highlight_matches_in_line(line, &ranges);
        }
    }
}

/// The highlight kind at character position `pos`: `Some(true)` for the current
/// match, `Some(false)` for another match, `None` for unmatched text.
fn range_kind(ranges: &[(usize, usize, bool)], pos: usize) -> Option<bool> {
    ranges
        .iter()
        .find(|(s, e, _)| pos >= *s && pos < *e)
        .map(|(_, _, cur)| *cur)
}

/// Rebuild a styled line with search-match highlights overlaid. Walks the
/// existing spans character by character, emitting sub-spans whenever the
/// highlight kind changes so original styling is kept outside the matches.
fn highlight_matches_in_line<'a>(line: &Line<'a>, ranges: &[(usize, usize, bool)]) -> Line<'a> {
    if ranges.is_empty() {
        return line.clone();
    }
    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut pos = 0usize;
    for span in &line.spans {
        let chars: Vec<char> = span.content.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let kind = range_kind(ranges, pos + i);
            let mut j = i + 1;
            while j < chars.len() && range_kind(ranges, pos + j) == kind {
                j += 1;
            }
            let text: String = chars[i..j].iter().collect();
            let style = match kind {
                Some(true) => span
                    .style
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                Some(false) => span.style.bg(Color::DarkGray).fg(Color::White),
                None => span.style,
            };
            spans.push(Span::styled(text, style));
            i = j;
        }
        pos += chars.len();
    }
    Line::from(spans)
}

/// Overlay a minimap-style scrollbar on the preview's right border: a bright
/// thumb for the visible window plus a cyan `◆` tick at every heading, giving
/// an at-a-glance sense of structure and position. No-op when nothing scrolls.
fn render_heading_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_len: usize,
    viewport: usize,
    scroll: usize,
    headings: &[markdown::Heading],
) {
    if area.width < 2 || area.height < 3 || content_len <= viewport {
        return;
    }
    let track_x = area.x + area.width - 1;
    let track_top = area.y + 1;
    let track_h = area.height - 2; // inside the top/bottom borders
    if track_h == 0 {
        return;
    }

    // Map a content row (0..content_len) onto a track row, via integer math.
    let denom = content_len.saturating_sub(1).max(1);
    let span = usize::from(track_h - 1);
    let map = |row: usize| -> u16 {
        let r = row.min(content_len - 1);
        track_top + (r * span / denom) as u16
    };

    let buf = frame.buffer_mut();

    // Thumb: the currently visible window, drawn first so ticks sit on top.
    let top = map(scroll);
    let bottom = map(scroll + viewport);
    for y in top..=bottom {
        let cell = &mut buf[(track_x, y)];
        cell.set_symbol("┃");
        cell.set_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Heading ticks on top of the thumb.
    for h in headings {
        let cell = &mut buf[(track_x, map(h.row))];
        cell.set_symbol("◆");
        cell.set_style(Style::default().fg(Color::Cyan));
    }
}

/// Draw the jump-to-heading outline as a centered popup over the preview pane.
/// Indents each entry by heading level and reverse-highlights the selection.
fn render_outline_overlay(frame: &mut Frame, area: Rect, state: &AppState) {
    use ratatui::widgets::Clear;
    let headings = state.preview_headings.borrow();
    if headings.is_empty() {
        return;
    }

    let longest = headings
        .iter()
        .map(|h| h.text.chars().count() + usize::from(h.level) + 2)
        .max()
        .unwrap_or(10) as u16;
    // Bound the popup to the pane; keep each clamp's lower bound <= upper bound
    // so a tiny pane can't invert them.
    let max_w = area.width.max(1);
    let max_h = area.height.max(1);
    let width = (longest + 4).clamp(20.min(max_w), max_w);
    let height = (headings.len() as u16 + 2).clamp(3.min(max_h), max_h);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    let items: Vec<ListItem> = headings
        .iter()
        .map(|h| {
            let indent = "  ".repeat(usize::from(h.level.saturating_sub(1)));
            ListItem::new(format!("{indent}{}", h.text))
        })
        .collect();

    let block = Block::default()
        .title(" Outline — ↑/↓ select · Enter jump · Esc close ")
        .borders(Borders::ALL);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut ls = ListState::default();
    ls.select(Some(state.outline_selected.min(headings.len() - 1)));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut ls);
}

fn preview_title(
    scroll: usize,
    max_scroll: usize,
    words: usize,
    chars: usize,
    tasks: (usize, usize),
) -> String {
    let mut counts = format!(
        "{words} word{} · {chars} char{}",
        plural(words),
        plural(chars)
    );
    if words > 0 {
        // ~200 wpm reading speed, rounded up.
        counts.push_str(&format!(" · ~{} min", words.div_ceil(200)));
    }
    let (done, total) = tasks;
    if total > 0 {
        counts.push_str(&format!(" · ☑ {done}/{total}"));
    }
    match (scroll * 100).checked_div(max_scroll) {
        // max_scroll == 0 means everything fits — nothing to scroll.
        None => format!(" Preview · {counts} "),
        Some(percent) => format!(" Preview · {counts} · {}% ", percent.min(100)),
    }
}

/// Word and character counts for a note body. Words are whitespace-separated;
/// chars count Unicode scalar values.
fn body_counts(body: &str) -> (usize, usize) {
    (body.split_whitespace().count(), body.chars().count())
}

/// Count completed and total markdown task items (`- [ ]` / `- [x]`) in a body.
fn task_counts(body: &str) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    for line in body.lines() {
        let rest = match line.trim_start() {
            t if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") => &t[2..],
            _ => continue,
        };
        if rest.starts_with("[x]") || rest.starts_with("[X]") {
            done += 1;
            total += 1;
        } else if rest.starts_with("[ ]") {
            total += 1;
        }
    }
    (done, total)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Whether the configured chat endpoint is local (drives the LOCAL/REMOTE tag).
#[cfg(feature = "ai")]
fn chat_locality(state: &AppState) -> &'static str {
    if crate::ai::chat::is_local(&state.settings.ai.chat.base_url) {
        "LOCAL"
    } else {
        "REMOTE"
    }
}

#[cfg(not(feature = "ai"))]
fn chat_locality(_state: &AppState) -> &'static str {
    "LOCAL"
}

/// Render the Ask-your-notes view: question input on top, answer + citations
/// below.
fn render_ask(frame: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Question input, tagged with the endpoint locality.
    let input_title = format!(" Ask [{}] ", chat_locality(state));
    let input = Paragraph::new(state.ask_input.as_str())
        .block(Block::default().title(input_title).borders(Borders::ALL));
    frame.render_widget(input, chunks[0]);

    // Answer + citations.
    let inner_width = chunks[1].width.saturating_sub(2).max(1);
    let mut lines: Vec<Line> = Vec::new();
    if state.ask_pending {
        lines.push(Line::from(Span::styled(
            "Thinking…",
            Style::default().fg(Color::DarkGray),
        )));
    } else if let Some(answer) = &state.ask_answer {
        lines.extend(markdown::render(
            answer,
            inner_width,
            &PreviewOptions::default(),
        ));
        if !state.ask_citations.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Sources (press a number to open):",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for citation in &state.ask_citations {
                lines.push(Line::from(Span::styled(
                    format!("[{}] {}", citation.index, citation.title),
                    Style::default().fg(Color::Cyan),
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Type a question and press Enter. Answers are grounded only in your notes.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let answer = Paragraph::new(lines)
        .block(Block::default().title(" Answer ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(answer, chunks[1]);

    // Cursor in the question input.
    let col = state.ask_input.chars().count() as u16;
    let x = chunks[0].x + 1 + col.min(chunks[0].width.saturating_sub(2));
    frame.set_cursor_position(ratatui::layout::Position {
        x,
        y: chunks[0].y + 1,
    });
}

/// Build the styled spans for one wrapped editor row, highlighting the portions
/// covered by search matches (the current match brighter than the rest).
///
/// `text` is the row's slice of the body; `row_start` is its byte offset within
/// the body; `matches` are body-relative match start offsets (ascending,
/// non-overlapping); `query_len` is the match length in bytes. Each match is
/// intersected with the row's byte range, so a match that straddles a wrapped
/// line is highlighted in every row it touches (head, middle, and tail). The
/// row's text is always reproduced in full.
fn highlight_row_spans<'a>(
    text: &'a str,
    row_start: usize,
    matches: &[usize],
    query_len: usize,
    current_idx: usize,
) -> Vec<Span<'a>> {
    let row_end = row_start + text.len();
    let mut spans: Vec<Span> = Vec::new();
    let mut last = 0usize; // byte index into `text` of the next unemitted char
    for (mi, &off) in matches.iter().enumerate() {
        let match_end = off + query_len;
        if off >= row_end {
            break; // matches are ascending — the rest start past this row
        }
        if match_end <= row_start {
            continue; // ends before this row — not visible here
        }
        // Visible intersection of the match with this row, in row-local bytes.
        // All four endpoints are char boundaries, so the slices below are safe.
        let local_start = off.max(row_start) - row_start;
        let local_end = match_end.min(row_end) - row_start;
        if local_start > last {
            spans.push(Span::raw(&text[last..local_start]));
        }
        let style = if mi == current_idx {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        };
        spans.push(Span::styled(&text[local_start..local_end], style));
        last = local_end;
    }
    if last < text.len() {
        spans.push(Span::raw(&text[last..]));
    }
    spans
}

/// Render the full-screen text editor for note body editing.
fn render_editor(frame: &mut Frame, area: Rect, state: &AppState) {
    let title = state
        .selected_note()
        .map(|n| format!(" Editing: {} ", n.title))
        .unwrap_or_else(|| " Editor ".to_string());

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .title_alignment(ratatui::layout::Alignment::Left);

    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;

    // Own the line layout: hard-wrap the buffer ourselves so the rendered lines
    // and the cursor's visual position use the same algorithm and can't drift.
    let cursor = state.cursor_pos.min(state.body_buffer.len());
    let rows = wrap::wrap(&state.body_buffer, inner_width.max(1));
    let (crow, ccol) = wrap::cursor_row_col(&rows, &state.body_buffer, cursor);

    // Cursor-follow scrolling: nudge the offset only when the cursor would fall
    // outside the viewport, then clamp so we never scroll past the last row.
    let scroll = wrap::scroll_to_cursor(
        state.editor_scroll.get() as usize,
        crow,
        rows.len(),
        inner_height,
    );
    state.editor_scroll.set(scroll as u16);

    let is_searching = state.view == AppView::EditorSearch;
    let matches = &state.editor_search_matches;
    let current_idx = state.editor_search_idx;
    let query_len = state.editor_search_query.len();
    let lines: Vec<Line> = rows
        .iter()
        .map(|r| {
            let text = &state.body_buffer[r.start..r.end];
            if is_searching && !matches.is_empty() {
                Line::from(highlight_row_spans(text, r.start, matches, query_len, current_idx))
            } else {
                Line::from(text.to_string())
            }
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((scroll as u16, 0))
        .style(Style::default());
    frame.render_widget(paragraph, area);

    // Place the terminal cursor at the visual position, accounting for scroll.
    // The +1 offsets account for the surrounding block border. `ccol` can equal
    // the width at the end of an exactly-full row, so clamp it on screen.
    if crow >= scroll && crow < scroll + inner_height {
        let col = ccol.min(inner_width.saturating_sub(1)) as u16;
        let x = area.x + 1 + col;
        let y = area.y + 1 + (crow - scroll) as u16;
        frame.set_cursor_position(ratatui::layout::Position { x, y });

        // The [[ autocomplete popup floats just under the cursor.
        if let Some(wc) = &state.wiki_complete {
            render_wiki_complete(frame, area, x, y, wc);
        }
    }
}

/// Draw the `[[` autocomplete popup near the editor cursor at `(cx, cy)`.
fn render_wiki_complete(
    frame: &mut Frame,
    area: Rect,
    cx: u16,
    cy: u16,
    wc: &crate::app::state::WikiComplete,
) {
    use ratatui::widgets::Clear;

    let longest = wc.matches.iter().map(|m| m.len()).max().unwrap_or(0) as u16;
    // Width fits the longest candidate (+2 border), bounded to the editor pane.
    // Built with min/max (not clamp) so a narrow pane can't invert the bounds.
    let max_w = area.width.saturating_sub(2).max(1);
    let width = (longest + 2).max(12).min(max_w).max(1);
    let height = (wc.matches.len() as u16 + 2)
        .min(area.height.saturating_sub(1).max(3))
        .max(3);

    // Prefer below the cursor; flip above if there isn't room.
    let below = cy + 1;
    let y = if below + height <= area.y + area.height {
        below
    } else {
        cy.saturating_sub(height)
    };
    // Keep the box inside the pane horizontally.
    let x = cx.min((area.x + area.width).saturating_sub(width));

    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    let items: Vec<ListItem> = wc
        .matches
        .iter()
        .enumerate()
        .map(|(i, title)| {
            let style = if i == wc.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            ListItem::new(Line::from(Span::styled(title.clone(), style)))
        })
        .collect();

    let title = if wc.query.is_empty() {
        " [[ link ".to_string()
    } else {
        format!(" [[{} ", wc.query)
    };
    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(Clear, popup);
    frame.render_widget(list, popup);
}

/// Render the conflict list screen.
fn render_conflict_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let items: Vec<ListItem> = state
        .conflicts
        .iter()
        .map(|c| {
            let note_id_short = &c.note_id.to_string()[..8];
            let detected = c.detected_at.format("%Y-%m-%d %H:%M");
            ListItem::new(Line::from(Span::raw(format!(
                "{} … — detected {}",
                note_id_short, detected
            ))))
        })
        .collect();

    let hint = if state.conflicts.is_empty() {
        " No conflicts found. Press Esc to return. "
    } else {
        " Conflicts — j/k navigate, Enter view detail, Esc back "
    };

    let list = List::new(items)
        .block(Block::default().title(hint).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    // Build a fake ListState for the conflict list
    let mut list_state = ratatui::widgets::ListState::default();
    if !state.conflicts.is_empty() {
        list_state.select(Some(state.conflict_index));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Render the scrollable keybinding reference. Reuses the preview scroll state
/// (`preview_scroll`/`preview_viewport_height`) since the two views are never
/// active at once.
fn render_help(frame: &mut Frame, area: Rect, state: &AppState) {
    let viewport_height = usize::from(area.height.saturating_sub(2));
    state
        .preview_viewport_height
        .set(area.height.saturating_sub(2).max(1));

    let mut lines: Vec<Line> = Vec::new();
    let section = |lines: &mut Vec<Line>, name: &str| {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            name.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    };
    let row = |lines: &mut Vec<Line>, keys: &str, desc: &str| {
        lines.push(Line::from(vec![
            Span::styled(format!("  {keys:<10}"), Style::default().fg(Color::Cyan)),
            Span::raw(desc.to_string()),
        ]));
    };

    // Navigation keys are contextual, not registry commands, so they stay here.
    section(&mut lines, "Navigation");
    row(&mut lines, "j / k", "Move down / up (↓ / ↑ also work)");
    row(&mut lines, "g / G", "Jump to top / bottom");
    row(&mut lines, "PgUp/PgDn", "Page up / down");
    row(&mut lines, "Enter", "Open preview · confirm");
    row(&mut lines, "Esc", "Back · cancel");
    row(&mut lines, ":", "Command palette");
    row(&mut lines, "1–9", "Apply that suggested tag (in preview)");

    // Code-block + wikilink focus are contextual Preview keys, not registry commands.
    section(&mut lines, "Preview focus (code & links)");
    row(&mut lines, "] / [ / Tab", "Focus next / previous code block or link");
    row(&mut lines, "Enter", "Open the focused [[wikilink]]");
    row(&mut lines, "y", "Copy focused code block to clipboard");
    row(&mut lines, "x", "Run focused shell block (confirm first)");

    // Everything else is generated from the command registry, so the help and
    // the real keybindings can't drift apart.
    let cmds = commands::commands();
    for category in commands::Category::ALL {
        let in_cat: Vec<&commands::CommandSpec> =
            cmds.iter().filter(|c| c.category == category).collect();
        if in_cat.is_empty() {
            continue;
        }
        section(&mut lines, category.label());
        for spec in in_cat {
            row(&mut lines, spec.key, spec.title);
        }
    }

    let scroll = wrap::clamp_scroll(
        usize::from(state.preview_scroll.get()),
        lines.len(),
        viewport_height,
    );
    let scroll_u16 = scroll.min(usize::from(u16::MAX)) as u16;
    state.preview_scroll.set(scroll_u16);

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Help — keys · j/k scroll · Esc/? close ")
                .borders(Borders::ALL),
        )
        .scroll((scroll_u16, 0));
    frame.render_widget(paragraph, area);
}

/// Render the fuzzy command palette: query input on top, ranked matches below.
fn render_palette(frame: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let input = Paragraph::new(format!(":{}", state.palette_query)).block(
        Block::default()
            .title(" Command palette ")
            .borders(Borders::ALL),
    );
    frame.render_widget(input, chunks[0]);

    // Title on the left, key hint right-aligned. Width budget = inner minus the
    // "> " highlight symbol.
    let row_width = chunks[1].width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = state
        .palette_matches
        .iter()
        .filter_map(|id| {
            let spec = commands::spec_for(*id)?;
            let key = format!("[{}]", spec.key);
            let pad = row_width.saturating_sub(spec.title.chars().count() + key.chars().count());
            Some(ListItem::new(Line::from(vec![
                Span::raw(spec.title.to_string()),
                Span::raw(" ".repeat(pad)),
                Span::styled(key, Style::default().fg(Color::DarkGray)),
            ])))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(
                    " {} match(es) — Enter run · Esc close ",
                    state.palette_matches.len()
                ))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut list_state = state.palette_state.clone();
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    // Cursor at the end of the query (after the leading ':').
    let col = state.palette_query.chars().count() as u16 + 1;
    frame.set_cursor_position(ratatui::layout::Position {
        x: chunks[0].x + 1 + col.min(chunks[0].width.saturating_sub(2)),
        y: chunks[0].y + 1,
    });
}

/// Render the trash: soft-deleted notes that can be restored or purged.
fn render_trash(frame: &mut Frame, area: Rect, state: &AppState) {
    let items: Vec<ListItem> = state
        .trash_notes
        .iter()
        .map(|n| {
            let when = n.updated_at.format("%Y-%m-%d %H:%M");
            ListItem::new(Line::from(vec![
                Span::raw(n.title.clone()),
                Span::styled(format!("  · {when}"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let title = if state.trash_notes.is_empty() {
        " Trash (empty) — Esc back ".to_string()
    } else {
        format!(
            " Trash ({}) — r restore · x purge · Esc back ",
            state.trash_notes.len()
        )
    };

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut list_state = state.trash_state.clone();
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Render a single conflict detail with keep-local / keep-remote / save-both options.
fn render_conflict_detail(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(conflict) = state.selected_conflict() else {
        let p = Paragraph::new("No conflict selected.")
            .block(Block::default().title(" Conflict ").borders(Borders::ALL))
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(p, area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Local version
    let local_title = format!(" Local (v{}) ", conflict.base_version,);
    let local_body = conflict
        .local_payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let local_para = Paragraph::new(local_body)
        .block(Block::default().title(local_title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .style(Style::default());
    frame.render_widget(local_para, chunks[0]);

    // Remote version
    let remote_title = format!(" Remote (v{}) ", conflict.base_version + 1,);
    let remote_body = conflict
        .remote_payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let remote_para = Paragraph::new(remote_body)
        .block(Block::default().title(remote_title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .style(Style::default());
    frame.render_widget(remote_para, chunks[1]);

    // Actions hint in status-like area — rendered as a third chunk
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(area);

    let actions = Paragraph::new(Line::from(vec![
        Span::styled(" [1] Keep local ", Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(" [2] Keep remote ", Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(" [3] Save both ", Style::default().fg(Color::Cyan)),
        Span::raw(" "),
        Span::styled(" [Esc] Back ", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL))
    .style(Style::default().bg(Color::Black).fg(Color::White));

    // Use the bottom 2 lines of the full area for the action bar
    frame.render_widget(actions, bottom[1]);
}

/// Render the first-run welcome / settings editor screen.
fn render_settings(frame: &mut Frame, area: Rect, state: &AppState) {
    use crate::app::state::SettingsForm;

    let form = &state.settings_form;

    let title = if form.first_run {
        " ✎ Welcome to Jot — First-time setup "
    } else {
        " Settings "
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Intro / explanation lines.
    let mut lines: Vec<Line> = Vec::new();
    if form.first_run {
        lines.push(Line::from(Span::styled(
            "Jot stores notes locally and works fully offline.",
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(Span::styled(
            "Optionally connect a PostgreSQL server to sync across devices.",
            Style::default().fg(Color::Gray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Storage, sync & AI configuration. ←/→ or Space toggles on/off fields.",
            Style::default().fg(Color::Gray),
        )));
    }
    lines.push(Line::from(""));

    // Editable fields.
    for i in 0..SettingsForm::FIELD_COUNT {
        let focused = i == form.field;
        let marker = if focused { "> " } else { "  " };
        let label = format!("{:<18}", SettingsForm::label(i));
        let value = form.value(i);

        let label_style = if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let value_style = if focused {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(label, label_style),
            Span::styled(value, value_style),
        ]));
    }

    lines.push(Line::from(""));

    // Inline status / validation / connection-test feedback.
    if !form.status.is_empty() {
        let status_color = if form.status.starts_with('✓') {
            Color::Green
        } else if form.status.starts_with('✗') {
            Color::Red
        } else {
            Color::Yellow
        };
        lines.push(Line::from(Span::styled(
            form.status.clone(),
            Style::default().fg(status_color),
        )));
        lines.push(Line::from(""));
    }

    // Key hints.
    let skip_label = if form.first_run {
        "[Esc] skip"
    } else {
        "[Esc] close"
    };
    lines.push(Line::from(Span::styled(
        format!(
            "[↑/↓] move field   [Space] toggle sync   [Enter] test connection   [Ctrl+S] save   {}",
            skip_label
        ),
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines).style(Style::default());
    frame.render_widget(paragraph, inner);
}

/// Render the status bar.
fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let mode_indicator = match state.view {
        AppView::List => "NORMAL",
        AppView::Preview => "PREVIEW",
        AppView::PreviewSearch => "FIND",
        AppView::Search => "SEARCH",
        AppView::Ask => "ASK",
        AppView::Edit => "EDIT TITLE",
        AppView::Tag => "ADD TAG",
        AppView::TagRemove => "REMOVE TAG",
        AppView::TagFilter => "FILTER TAG",
        AppView::Editor => "EDITOR",
        AppView::EditorSearch => "FIND",
        AppView::ConflictReview => "CONFLICTS",
        AppView::ConflictDetail => "CONFLICT DETAIL",
        AppView::Settings => "SETTINGS",
        AppView::ConfirmDelete => "CONFIRM DELETE",
        AppView::ConfirmReindex => "CONFIRM REINDEX",
        AppView::Help => "HELP",
        AppView::Trash => "TRASH",
        AppView::ConfirmPurge => "CONFIRM PURGE",
        AppView::ConfirmRunBlock => "CONFIRM RUN",
        AppView::CommandPalette => "PALETTE",
    };

    // Show the input buffer when in a text-input mode.
    let input_hint = match state.view {
        AppView::Search if !state.search_query.is_empty() => {
            format!(" /{} ", state.search_query)
        }
        AppView::EditorSearch => {
            if state.editor_search_matches.is_empty() && !state.editor_search_query.is_empty() {
                format!(" find: {} (no matches) ", state.editor_search_query)
            } else if !state.editor_search_matches.is_empty() {
                format!(
                    " find: {} ({}/{}) ",
                    state.editor_search_query,
                    state.editor_search_idx + 1,
                    state.editor_search_matches.len()
                )
            } else {
                format!(" find: {} ", state.editor_search_query)
            }
        }
        AppView::Edit => {
            format!(" title: {} ", state.edit_buffer)
        }
        AppView::Tag => {
            format!(" tag: {} ", state.edit_buffer)
        }
        AppView::TagFilter => {
            format!(" filter: {} ", state.edit_buffer)
        }
        _ => String::new(),
    };

    // Show active tag filter if set
    let filter_hint = match &state.tag_filter {
        Some(tag) => format!(" [tag: {}]", tag),
        None => String::new(),
    };

    let sync_label = if state.sync_enabled {
        format!(" [{}]", state.sync_status)
    } else {
        String::new()
    };

    let conflict_hint = if state.conflict_count > 0
        && state.view != AppView::ConflictReview
        && state.view != AppView::ConflictDetail
    {
        format!(" [{} conflict(s) — press C]", state.conflict_count)
    } else {
        String::new()
    };

    let index_hint = if state.embed_pending > 0 {
        format!(" [{} indexing]", state.embed_pending)
    } else {
        String::new()
    };

    let status_text = format!(
        " {}  |  {}{}{}{}{}{}  ",
        mode_indicator,
        state.status_message,
        input_hint,
        filter_hint,
        sync_label,
        conflict_hint,
        index_hint
    );

    let bar = Paragraph::new(Line::from(vec![
        Span::styled(
            " ✎ Jot ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            status_text,
            Style::default().fg(Color::White).bg(Color::Black),
        ),
    ]))
    .style(Style::default().bg(Color::Black).fg(Color::White));

    frame.render_widget(bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::WikiComplete;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the full UI with the editor + an active `[[` popup in a few
    /// viewport sizes — including a tiny one — to ensure popup geometry never
    /// panics (no inverted clamp bounds, no out-of-bounds draw).
    #[test]
    fn editor_autocomplete_popup_renders_without_panic() {
        for (w, h) in [(80u16, 24u16), (10, 6), (6, 4), (4, 3)] {
            let mut state = AppState::new();
            state.view = AppView::Editor;
            state.body_buffer = "see [[al".to_string();
            state.cursor_pos = state.body_buffer.len();
            state.wiki_complete = Some(WikiComplete {
                start: 6,
                query: "al".to_string(),
                matches: vec!["Alpha".to_string(), "Alpaca".to_string()],
                selected: 1,
            });
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|f| render(f, &state)).expect("draw");
        }
    }

    #[test]
    fn editor_preview_focus_tracks_cursor_section() {
        use crate::tui::markdown::Heading;
        let source_line_rows = vec![0, 2, 5];
        let headings = vec![
            Heading { level: 1, text: "A".into(), row: 0 },
            Heading { level: 2, text: "B".into(), row: 4 },
        ];
        // Cursor on source line 0 → body row 0, inside heading A (row 0).
        assert_eq!(editor_preview_focus(&source_line_rows, &headings, 0), (0, Some(0)));
        // Cursor on source line 1 → body row 2, still under heading A.
        assert_eq!(editor_preview_focus(&source_line_rows, &headings, 1), (2, Some(0)));
        // Cursor on source line 2 → body row 5, now under heading B (row 4).
        assert_eq!(editor_preview_focus(&source_line_rows, &headings, 2), (5, Some(4)));
        // A cursor line past the map clamps to the top.
        assert_eq!(editor_preview_focus(&source_line_rows, &headings, 99), (0, Some(0)));
        // No headings → no current section.
        assert_eq!(editor_preview_focus(&source_line_rows, &[], 1), (2, None));
    }

    /// Render the editor with the live preview split across several viewport
    /// sizes (including tiny ones) to ensure the split layout, cursor-synced
    /// scroll, section highlight, and render cache never panic.
    #[test]
    fn editor_live_preview_split_renders_without_panic() {
        for (w, h) in [(80u16, 24u16), (40, 12), (10, 6), (4, 3)] {
            let mut state = AppState::new();
            state.view = AppView::Editor;
            state.editor_preview_split = true;
            state.body_buffer =
                "# Title\n\nSome **bold** text.\n\n## Section\n\n- a\n- b\n".to_string();
            state.cursor_pos = state.body_buffer.len();
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|f| render(f, &state)).expect("draw");
        }
    }

    #[test]
    fn find_preview_matches_is_case_insensitive_and_nonoverlapping() {
        let lines = vec![
            Line::from("Hello World"),
            Line::from(vec![Span::raw("foo "), Span::raw("BAR baz")]),
        ];
        // Spans within a line are concatenated, so "BAR" is at chars 4..7.
        assert_eq!(find_preview_matches(&lines, "bar"), vec![(1, 4, 7)]);
        // 'l' occurs three times on row 0, none on row 1.
        assert_eq!(
            find_preview_matches(&lines, "l"),
            vec![(0, 2, 3), (0, 3, 4), (0, 9, 10)]
        );
    }

    #[test]
    fn highlight_matches_in_line_splits_and_marks_current() {
        let line = Line::from("abcdef");
        let out = highlight_matches_in_line(&line, &[(2, 4, true)]);
        let texts: Vec<String> = out.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
        assert_eq!(out.spans[1].style.bg, Some(Color::Yellow)); // current match
        assert_eq!(out.spans[0].style.bg, None); // untouched
    }

    /// Render the preview with an active find query across viewport sizes to
    /// ensure match highlighting + scrollbar never panic.
    #[test]
    fn preview_find_renders_without_panic() {
        for (w, h) in [(80u16, 24u16), (30, 8), (6, 4)] {
            let mut state = AppState::new();
            state.view = AppView::PreviewSearch;
            state.preview_body =
                "# Title\n\nalpha beta alpha\n\n## More\n\nbeta gamma\n".to_string();
            state.preview_search_query = "alpha".to_string();
            // selected_note() is None here, so render shows the empty-preview
            // path; drive the find path directly instead.
            let matches = find_preview_matches(
                &[Line::from("alpha beta alpha")],
                &state.preview_search_query,
            );
            assert_eq!(matches, vec![(0, 0, 5), (0, 11, 16)]);
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|f| render(f, &state)).expect("draw");
        }
    }

    #[test]
    fn apply_folds_hides_section_and_remaps_rows() {
        use crate::tui::markdown::Heading;
        let body: Vec<Line> = (0..6).map(|i| Line::from(format!("row{i}"))).collect();
        let headings = vec![
            Heading { level: 1, text: "A".into(), row: 0 },
            Heading { level: 1, text: "B".into(), row: 4 },
        ];
        let mut folded = std::collections::HashSet::new();
        folded.insert(0); // fold section A → rows 1..4 hidden

        let (out, remap) = apply_folds(body, &headings, &folded);
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        // Folded body: [▸ row0, placeholder, row4, row5].
        assert!(texts[0].starts_with('▸') && texts[0].contains("row0"));
        assert!(texts[1].contains("hidden"));
        assert!(texts[2].contains("row4"));
        assert!(texts[3].contains("row5"));

        assert_eq!(remap[0], Some(0));
        assert_eq!(remap[1], None); // hidden
        assert_eq!(remap[4], Some(2));
        assert_eq!(remap[5], Some(3));
        // A hidden row maps back to its (visible) fold heading.
        assert_eq!(remap_row(&remap, 2), 0);
    }

    /// Render the full preview with a selected note, headings, an active fold,
    /// the outline overlay, and a find query — across sizes — to ensure the
    /// whole Phase 2 surface composes without panicking.
    #[test]
    fn preview_render_with_headings_folds_and_find_no_panic() {
        use crate::models::note::NoteSummary;
        for (w, h) in [(80u16, 24u16), (24, 8), (5, 4)] {
            let mut state = AppState::new();
            state.notes = vec![NoteSummary {
                id: uuid::Uuid::new_v4(),
                title: "Doc".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                tags: Vec::new(),
            }];
            state.list_state.select(Some(0));
            state.view = AppView::Preview;
            state.preview_body =
                "# Alpha\n\nalpha body\nmore alpha\n\n## Beta\n\nbeta body\n".to_string();

            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            // First render populates the heading list.
            terminal.draw(|f| render(f, &state)).expect("draw 1");
            assert!(!state.preview_headings.borrow().is_empty());

            // Fold a section, open the outline, and run a find, then re-render.
            state.toggle_fold_at_scroll();
            state.outline_open = true;
            state.preview_search_query = "alpha".to_string();
            state.preview_search_scroll.set(true);
            terminal.draw(|f| render(f, &state)).expect("draw 2");
        }
    }

    /// Render the preview in zen mode across widths (wide → margin applied,
    /// narrow → margin collapses to zero) to ensure centering never panics.
    #[test]
    fn preview_zen_mode_renders_without_panic() {
        use crate::models::note::NoteSummary;
        for (w, h) in [(120u16, 24u16), (40, 10), (6, 4)] {
            let mut state = AppState::new();
            state.notes = vec![NoteSummary {
                id: uuid::Uuid::new_v4(),
                title: "Doc".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                tags: Vec::new(),
            }];
            state.list_state.select(Some(0));
            state.view = AppView::Preview;
            state.zen_mode = true;
            state.preview_body =
                "# Title\n\nA reasonably long paragraph of body text that should wrap.\n".to_string();
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|f| render(f, &state)).expect("draw");
        }
    }

    #[test]
    fn body_counts_words_and_chars() {
        assert_eq!(body_counts(""), (0, 0));
        assert_eq!(body_counts("hello"), (1, 5));
        assert_eq!(body_counts("  hello   world  "), (2, 17));
        // Unicode scalar values, not bytes.
        assert_eq!(body_counts("café 🦀"), (2, 6));
    }

    #[test]
    fn preview_title_shows_counts_reading_time_and_scroll() {
        // Everything fits → no percentage; counts + reading time, no tasks.
        assert_eq!(
            preview_title(0, 0, 2, 9, (0, 0)),
            " Preview · 2 words · 9 chars · ~1 min "
        );
        // Singular for exactly one; zero words → no reading time.
        assert_eq!(preview_title(0, 0, 0, 0, (0, 0)), " Preview · 0 words · 0 chars ");
        // Scrollable → percentage appended; 450 words → ~3 min; task progress.
        assert_eq!(
            preview_title(5, 10, 450, 12, (2, 5)),
            " Preview · 450 words · 12 chars · ~3 min · ☑ 2/5 · 50% "
        );
    }

    #[test]
    fn task_counts_counts_done_and_total() {
        let body = "- [ ] a\n- [x] b\n* [X] c\n+ [ ] d\n- not a task\nplain";
        assert_eq!(task_counts(body), (2, 4));
        assert_eq!(task_counts("no tasks here"), (0, 0));
    }

    // ── highlight_row_spans ──────────────────────────────────────────────────

    /// Concatenated span text must always reproduce the row exactly (no bytes
    /// dropped or duplicated, no panic on slicing).
    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }
    /// Count of styled (highlighted) spans.
    fn highlighted(spans: &[Span]) -> usize {
        spans.iter().filter(|s| s.style.bg.is_some()).count()
    }

    #[test]
    fn highlight_single_in_row_match() {
        // "foo" highlighted at offset 0, qlen 3.
        let spans = highlight_row_spans("foo bar", 0, &[0], 3, 0);
        assert_eq!(joined(&spans), "foo bar");
        assert_eq!(highlighted(&spans), 1);
        assert_eq!(spans[0].content.as_ref(), "foo"); // the match leads the row
    }

    #[test]
    fn highlight_two_matches_current_and_other() {
        // "foo … foo": both highlighted; reproduce the whole row.
        let spans = highlight_row_spans("foo x foo", 0, &[0, 6], 3, 1);
        assert_eq!(joined(&spans), "foo x foo");
        assert_eq!(highlighted(&spans), 2);
    }

    #[test]
    fn highlight_skips_match_ending_before_row() {
        // Match at offset 0 (len 3) ends at byte 3, before this row starts at 6,
        // so it isn't visible here; only the offset-6 match is highlighted.
        let spans = highlight_row_spans("more text", 6, &[0, 6], 3, 0);
        assert_eq!(joined(&spans), "more text");
        assert_eq!(highlighted(&spans), 1);
    }

    #[test]
    fn highlight_straddle_in_tail() {
        // A match at body offset 2, len 4 ([2,6)) straddles into a row that
        // starts at byte 4: the tail "cd" of "cdef" must be highlighted.
        let spans = highlight_row_spans("cdef", 4, &[2], 4, 0);
        assert_eq!(joined(&spans), "cdef");
        assert_eq!(highlighted(&spans), 1);
        assert_eq!(spans[0].content.as_ref(), "cd"); // tail leads the row
    }

    #[test]
    fn highlight_straddle_across_two_rows() {
        // Body "foobarx" with match "ooba" at offset 1, len 4 ([1,5)), wrapped
        // into rows "foo" [0,3) and "barx" [3,7). Each row highlights its part.
        let row1 = highlight_row_spans("foo", 0, &[1], 4, 0);
        assert_eq!(joined(&row1), "foo");
        assert_eq!(highlighted(&row1), 1);
        assert_eq!(row1.last().unwrap().content.as_ref(), "oo"); // head

        let row2 = highlight_row_spans("barx", 3, &[1], 4, 0);
        assert_eq!(joined(&row2), "barx");
        assert_eq!(highlighted(&row2), 1);
        assert_eq!(row2[0].content.as_ref(), "ba"); // tail
    }

    #[test]
    fn highlight_clamps_match_running_off_row() {
        // Match at offset 5 with qlen 10 would run past this 7-byte row; clamp.
        let spans = highlight_row_spans("abcdefg", 0, &[5], 10, 0);
        assert_eq!(joined(&spans), "abcdefg");
        // Trailing highlighted span is the clamped tail "fg".
        assert_eq!(spans.last().unwrap().content.as_ref(), "fg");
    }

    #[test]
    fn highlight_multibyte_is_boundary_safe() {
        // "café" then a match on "au" after it; must not panic on the 2-byte é.
        let body = "café au"; // bytes: c a f é(2) space a u
        let au = body.find("au").unwrap(); // byte offset 6
        let spans = highlight_row_spans(body, 0, &[au], 2, 0);
        assert_eq!(joined(&spans), body);
        assert_eq!(highlighted(&spans), 1);
    }

    #[test]
    fn highlight_no_matches_in_row_returns_whole_text() {
        let spans = highlight_row_spans("nothing here", 100, &[0, 3], 3, 0);
        assert_eq!(joined(&spans), "nothing here");
        assert_eq!(highlighted(&spans), 0);
    }
}
