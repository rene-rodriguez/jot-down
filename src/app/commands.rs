//! The command registry: a single source of truth for the actions a user can
//! invoke from the note list.
//!
//! The key dispatcher (`run_command` in `main.rs`), the help overlay, and the
//! command palette all read from [`commands()`], so a command's name, key, and
//! behavior can't drift apart. AI-only commands are present only when the `ai`
//! feature is compiled in.

/// Every invocable command. Navigation (arrows, `j`/`k`, scrolling, applying a
/// numbered tag) is intentionally *not* here — those are contextual, not
/// launchable actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandId {
    NewNote,
    RenameNote,
    EditBody,
    DailyNote,
    DeleteNote,
    OpenTrash,
    AddTag,
    RemoveTag,
    FilterByTag,
    Search,
    Ask,
    /// AI-only; never constructed in `--no-default-features` builds.
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    Reindex,
    Export,
    Import,
    SyncNow,
    ReviewConflicts,
    Settings,
    Help,
    Quit,
}

/// Display grouping for the help overlay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Notes,
    Tags,
    SearchAi,
    Data,
    App,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Notes => "Notes",
            Category::Tags => "Tags",
            Category::SearchAi => "Search & AI",
            Category::Data => "Data",
            Category::App => "App",
        }
    }

    /// Categories in display order.
    pub const ALL: [Category; 5] = [
        Category::Notes,
        Category::Tags,
        Category::SearchAi,
        Category::Data,
        Category::App,
    ];
}

/// One command's static metadata. `title` is the palette/help label and the
/// fuzzy-match target; `key` is its single-character shortcut.
#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub id: CommandId,
    pub title: &'static str,
    pub key: &'static str,
    pub category: Category,
}

/// The full registry, in display order.
pub fn commands() -> Vec<CommandSpec> {
    use Category::*;
    use CommandId::*;
    let spec = |id, title, key, category| CommandSpec {
        id,
        title,
        key,
        category,
    };

    // `mut` is only used under the `ai` cfg below.
    #[cfg_attr(not(feature = "ai"), allow(unused_mut))]
    let mut v = vec![
        spec(NewNote, "New note", "n", Notes),
        spec(RenameNote, "Rename note", "e", Notes),
        spec(EditBody, "Edit note body", "i", Notes),
        spec(DailyNote, "Daily note", "o", Notes),
        spec(DeleteNote, "Delete note (to trash)", "d", Notes),
        spec(OpenTrash, "Open trash", "D", Notes),
        spec(AddTag, "Add tag", "t", Tags),
        spec(RemoveTag, "Remove tag", "R", Tags),
        spec(FilterByTag, "Filter by tag", "T", Tags),
        spec(Search, "Search notes", "/", SearchAi),
        spec(Ask, "Ask your notes", "a", SearchAi),
        spec(Export, "Export notes to Markdown", "E", Data),
        spec(Import, "Import notes from Markdown", "I", Data),
        spec(SyncNow, "Sync now", "s", Data),
        spec(ReviewConflicts, "Review sync conflicts", "C", Data),
        spec(Settings, "Settings", ",", App),
        spec(Help, "Help", "?", App),
        spec(Quit, "Quit", "q", App),
    ];

    // Reindex needs the vector index, so it only exists in `ai` builds. Pushed
    // after Ask so it still lands last within the Search & AI group.
    #[cfg(feature = "ai")]
    v.push(spec(Reindex, "Rebuild embedding index", "X", SearchAi));

    v
}

/// The metadata for one command (for rendering its title/key).
pub fn spec_for(id: CommandId) -> Option<CommandSpec> {
    commands().into_iter().find(|s| s.id == id)
}

/// The command bound to a single-character key, if any. AI-only keys resolve to
/// `None` in non-`ai` builds, so the key is inert there.
pub fn command_for_key(c: char) -> Option<CommandId> {
    let key = c.to_string();
    commands()
        .into_iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.id)
}

/// Commands matching `query`, best match first. An empty query returns the
/// whole registry in display order.
pub fn filter_commands(query: &str) -> Vec<CommandId> {
    let cmds = commands();
    if query.is_empty() {
        return cmds.iter().map(|c| c.id).collect();
    }
    let mut scored: Vec<(i32, usize, CommandId)> = cmds
        .iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_score(c.title, query).map(|s| (s, i, c.id)))
        .collect();
    // Higher score first; ties keep registry order.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, id)| id).collect()
}

/// Fuzzy subsequence score of `needle` against `haystack` (case-insensitive).
/// Returns `None` when `needle` isn't a subsequence. Higher is better, rewarding
/// contiguous runs and matches at word boundaries.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    let h: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let n: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if n.is_empty() {
        return Some(0);
    }

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut prev_idx: Option<usize> = None;

    for &nc in &n {
        let mut matched = None;
        while hi < h.len() {
            if h[hi] == nc {
                matched = Some(hi);
                hi += 1;
                break;
            }
            hi += 1;
        }
        let idx = matched?; // not a subsequence
        score += 1;
        if let Some(p) = prev_idx {
            if idx == p + 1 {
                score += 5; // contiguous
            }
        }
        if idx == 0 || !h[idx - 1].is_alphanumeric() {
            score += 3; // word boundary
        }
        prev_idx = Some(idx);
    }

    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicate_keys_or_titles() {
        let cmds = commands();
        for (i, a) in cmds.iter().enumerate() {
            for b in &cmds[i + 1..] {
                assert_ne!(a.key, b.key, "duplicate key {}", a.key);
                assert_ne!(a.title, b.title, "duplicate title {}", a.title);
                assert_ne!(a.id, b.id, "duplicate id {:?}", a.id);
            }
        }
    }

    #[test]
    fn command_for_key_maps_known_keys() {
        assert_eq!(command_for_key('n'), Some(CommandId::NewNote));
        assert_eq!(command_for_key('?'), Some(CommandId::Help));
        assert_eq!(command_for_key('z'), None);
    }

    #[test]
    fn command_for_key_o_is_daily_note() {
        assert_eq!(command_for_key('o'), Some(CommandId::DailyNote));
    }

    #[test]
    fn fuzzy_rejects_non_subsequence() {
        assert!(fuzzy_score("New note", "xyz").is_none());
        assert!(fuzzy_score("New note", "nn").is_some());
    }

    #[test]
    fn fuzzy_prefers_contiguous_and_boundary_matches() {
        // "new" runs contiguously from the start; should beat scattered "nt".
        let contiguous = fuzzy_score("New note", "new").unwrap();
        let scattered = fuzzy_score("New note", "nt").unwrap();
        assert!(contiguous > scattered, "{contiguous} !> {scattered}");
    }

    #[test]
    fn filter_ranks_best_match_first() {
        let matches = filter_commands("note");
        // "New note" / "Rename note" etc. all contain "note"; the empty-query
        // path is bypassed, and every result must actually match.
        assert!(!matches.is_empty());
        // A query of a full title puts that command at the top.
        let top = filter_commands("sync now");
        assert_eq!(top.first(), Some(&CommandId::SyncNow));
    }

    #[test]
    fn empty_query_returns_full_registry_in_order() {
        let all = filter_commands("");
        assert_eq!(all.first(), Some(&CommandId::NewNote));
        assert_eq!(all.len(), commands().len());
    }
}
