use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
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
        render_editor(frame, main_layout[0], state);
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
        } else {
            // Row offset of the markdown body within `lines` (title + blank above).
            let body_offset = lines.len();
            let (body_lines, code_rows) = markdown::render_full(
                &state.preview_body,
                inner_width,
                &PreviewOptions {
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
                    focused_code_block: state.focused_code_index(),
                    focused_wikilink: state.focused_link_index(),
                },
                &state.link_targets,
            );
            lines.extend(body_lines);

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
        let title = preview_title(scroll, max_scroll, words, chars);
        let block = Block::default().title(title).borders(Borders::ALL);
        let paragraph = Paragraph::new(lines)
            .block(block)
            .scroll((scroll_u16, 0))
            .style(Style::default());
        frame.render_widget(paragraph, area);
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

fn preview_title(scroll: usize, max_scroll: usize, words: usize, chars: usize) -> String {
    let counts = format!(
        "{words} word{} · {chars} char{}",
        plural(words),
        plural(chars)
    );
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
    fn body_counts_words_and_chars() {
        assert_eq!(body_counts(""), (0, 0));
        assert_eq!(body_counts("hello"), (1, 5));
        assert_eq!(body_counts("  hello   world  "), (2, 17));
        // Unicode scalar values, not bytes.
        assert_eq!(body_counts("café 🦀"), (2, 6));
    }

    #[test]
    fn preview_title_shows_counts_and_scroll() {
        // Everything fits → no percentage, pluralized counts.
        assert_eq!(preview_title(0, 0, 2, 9), " Preview · 2 words · 9 chars ");
        // Singular for exactly one.
        assert_eq!(preview_title(0, 0, 1, 1), " Preview · 1 word · 1 char ");
        // Scrollable → percentage appended, clamped to 100.
        assert_eq!(
            preview_title(5, 10, 3, 12),
            " Preview · 3 words · 12 chars · 50% "
        );
        assert_eq!(
            preview_title(99, 10, 3, 12),
            " Preview · 3 words · 12 chars · 100% "
        );
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
