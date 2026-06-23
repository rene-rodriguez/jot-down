//! Markdown export / import — the local-first escape hatch.
//!
//! Export writes one `.md` per live note (`# title` then the body) into a
//! directory. Import reads every `.md` in a directory back in as new notes,
//! skipping any whose exact title+body already exists so re-imports are
//! idempotent. Together they let users get their notes out (and back) as plain
//! files, which is what makes "your notes live on your machine" credible.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::models::note::CreateNoteInput;
use crate::storage::SqliteStorage;

/// Outcome of an export run.
pub struct ExportSummary {
    pub dir: PathBuf,
    pub written: usize,
}

/// Outcome of an import run.
pub struct ImportSummary {
    pub dir: PathBuf,
    pub imported: usize,
    pub skipped: usize,
}

/// Export every live note into `dir` (created if needed), one Markdown file
/// each. Filenames are `<slug>-<id8>.md` so they're stable and collision-free.
pub async fn export_notes(storage: &SqliteStorage, dir: &Path) -> Result<ExportSummary> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating export dir {}", dir.display()))?;

    let mut written = 0usize;
    let mut used: HashSet<String> = HashSet::new();
    for summary in storage.list_notes().await? {
        let Some(note) = storage.get_note(summary.id).await? else {
            continue; // raced with a delete
        };
        let filename = unique_filename(&note.title, note.id, &mut used);
        let path = dir.join(filename);
        let contents = format!("# {}\n\n{}", note.title, note.body);
        std::fs::write(&path, contents)
            .with_context(|| format!("writing {}", path.display()))?;
        written += 1;
    }

    Ok(ExportSummary {
        dir: dir.to_path_buf(),
        written,
    })
}

/// Import every `*.md` file in `dir` (created if missing) as a new note,
/// skipping any whose exact title+body already exists.
pub async fn import_notes(storage: &SqliteStorage, dir: &Path) -> Result<ImportSummary> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating import dir {}", dir.display()))?;

    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading import dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .map(|x| x.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let (title, body) = parse_markdown(&raw, &path);
        if storage.live_note_exists_with_content(&title, &body).await? {
            skipped += 1;
            continue;
        }
        storage.create_note(CreateNoteInput { title, body }).await?;
        imported += 1;
    }

    Ok(ImportSummary {
        dir: dir.to_path_buf(),
        imported,
        skipped,
    })
}

/// Parse a note from Markdown: a leading `# Heading` becomes the title (stripped
/// from the body); otherwise the file stem is the title and the whole file is
/// the body. Round-trips with [`export_notes`]' `# title\n\n{body}` layout.
fn parse_markdown(raw: &str, path: &Path) -> (String, String) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw); // strip BOM

    let mut parts = raw.splitn(2, '\n');
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");

    if let Some(heading) = first.trim_end().strip_prefix("# ") {
        let title = heading.trim();
        let title = if title.is_empty() { stem } else { title.to_string() };
        let body = rest.trim_start_matches('\n').to_string();
        return (title, body);
    }

    (stem, raw.trim_end().to_string())
}

/// Build a stable, filesystem-safe filename for a note. The id suffix keeps it
/// unique even when titles collide or slugify to nothing.
fn unique_filename(title: &str, id: Uuid, used: &mut HashSet<String>) -> String {
    let slug = slugify(title);
    let short = &id.to_string()[..8];
    let base = if slug.is_empty() {
        format!("note-{short}")
    } else {
        format!("{slug}-{short}")
    };

    let mut name = format!("{base}.md");
    let mut n = 1;
    while !used.insert(name.clone()) {
        name = format!("{base}-{n}.md");
        n += 1;
    }
    name
}

/// Lowercase, replace runs of non-alphanumerics with a single dash, trim dashes.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_collapses_and_trims() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("***"), "");
        assert_eq!(slugify("Rust 🦀 notes"), "rust-notes");
    }

    #[test]
    fn parse_markdown_uses_heading_as_title() {
        let (title, body) = parse_markdown("# My Note\n\nLine one\nLine two", Path::new("x.md"));
        assert_eq!(title, "My Note");
        assert_eq!(body, "Line one\nLine two");
    }

    #[test]
    fn parse_markdown_falls_back_to_filename() {
        let (title, body) = parse_markdown("just body text", Path::new("/tmp/ideas.md"));
        assert_eq!(title, "ideas");
        assert_eq!(body, "just body text");
    }

    #[test]
    fn parse_round_trips_export_layout() {
        let exported = format!("# {}\n\n{}", "Title Here", "body\n\nwith blank line");
        let (title, body) = parse_markdown(&exported, Path::new("whatever.md"));
        assert_eq!(title, "Title Here");
        assert_eq!(body, "body\n\nwith blank line");
    }

    #[test]
    fn unique_filename_disambiguates_collisions() {
        let mut used = HashSet::new();
        let id = Uuid::nil();
        let a = unique_filename("Note", id, &mut used);
        let b = unique_filename("Note", id, &mut used);
        assert_ne!(a, b);
        assert!(a.ends_with(".md") && b.ends_with(".md"));
    }

    #[tokio::test]
    async fn export_then_import_round_trips_and_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("jot-io-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Seed a DB with two notes (AI off so no vector index is needed).
        let src = SqliteStorage::connect_with_ai(&tmp.join("a.db"), false)
            .await
            .expect("connect src");
        for (t, b) in [("First", "Body one"), ("Second", "Body two\n\nwith blank")] {
            src.create_note(CreateNoteInput {
                title: t.to_string(),
                body: b.to_string(),
            })
            .await
            .expect("create");
        }

        let out = tmp.join("out");
        let exported = export_notes(&src, &out).await.expect("export");
        assert_eq!(exported.written, 2);

        // Import into a fresh DB.
        let dst = SqliteStorage::connect_with_ai(&tmp.join("b.db"), false)
            .await
            .expect("connect dst");
        let first = import_notes(&dst, &out).await.expect("import");
        assert_eq!((first.imported, first.skipped), (2, 0));
        assert_eq!(dst.list_notes().await.expect("list").len(), 2);

        // Re-import skips everything (dedupe by exact title+body).
        let second = import_notes(&dst, &out).await.expect("reimport");
        assert_eq!((second.imported, second.skipped), (0, 2));
        assert_eq!(dst.list_notes().await.expect("list").len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
