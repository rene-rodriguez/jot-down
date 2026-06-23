//! Daily-note rollups — carry unfinished tasks forward.
//!
//! When today's daily note is first created, the unchecked `- [ ]` items from
//! the most recent prior daily note are copied (not moved) into a clearly
//! marked section so nothing falls through the cracks day to day. The storage
//! layer owns the "find the prior note" query; everything here is pure string
//! manipulation over note bodies.

/// Heading under which carried-over tasks are appended in a new daily note.
pub const CARRIED_HEADING: &str = "## Carried over";

/// Extract the unchecked task lines (`- [ ] …`, any bullet, any indent) from a
/// note body, preserving each line verbatim (indentation and marker included).
/// Checked items (`- [x] …`) and everything else are dropped.
pub fn unfinished_tasks(body: &str) -> Vec<String> {
    body.lines()
        .filter(|line| is_unchecked_task(line))
        .map(|line| line.to_string())
        .collect()
}

/// True when `line` is an unchecked task item: optional leading whitespace, a
/// `-`/`*`/`+` bullet, a space, then `[ ] ` (a space inside the brackets marks
/// it incomplete). Mirrors the editor's task-marker parsing.
fn is_unchecked_task(line: &str) -> bool {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 6 || !matches!(bytes[0], b'-' | b'*' | b'+') || bytes[1] != b' ' {
        return false;
    }
    let after = &bytes[2..];
    after[0] == b'[' && after[1] == b' ' && after[2] == b']' && after[3] == b' '
}

/// Append a `## Carried over` section listing `tasks` after `body`. Returns the
/// body unchanged when there are no tasks, so a daily note with nothing to carry
/// stays pristine. Ensures exactly one blank line separates the existing body
/// from the new heading.
pub fn append_carried_over(body: &str, tasks: &[String]) -> String {
    if tasks.is_empty() {
        return body.to_string();
    }

    let mut out = String::new();
    let trimmed = body.trim_end_matches('\n');
    if !trimmed.is_empty() {
        out.push_str(trimmed);
        out.push_str("\n\n");
    }
    out.push_str(CARRIED_HEADING);
    out.push('\n');
    for task in tasks {
        out.push('\n');
        out.push_str(task);
    }
    out.push('\n');
    out
}

/// For an ISO `YYYY-MM-DD` `today` and a set of day `offsets`, the prior dates
/// to surface as "On this day" — each as `(offset, ISO date)`, in the offset
/// order given. Offsets of 0 (or that underflow the calendar) are skipped.
/// Returns empty if `today` doesn't parse. Pure; chrono handles month/year and
/// leap-day arithmetic.
pub fn on_this_day_targets(today: &str, offsets: &[u32]) -> Vec<(u32, String)> {
    let Ok(today) = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &offset in offsets {
        if offset == 0 {
            continue;
        }
        if let Some(date) = today.checked_sub_days(chrono::Days::new(offset as u64)) {
            out.push((offset, date.format("%Y-%m-%d").to_string()));
        }
    }
    out
}

/// A friendly label for a day offset (`7` → "a week ago", etc.).
pub fn offset_label(days: u32) -> String {
    match days {
        7 => "a week ago".to_string(),
        14 => "two weeks ago".to_string(),
        30 => "a month ago".to_string(),
        365 => "a year ago".to_string(),
        n => format!("{n} days ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_this_day_targets_handles_boundaries() {
        // Simple week/month/year offsets.
        let t = on_this_day_targets("2026-06-20", &[7, 30, 365]);
        assert_eq!(
            t,
            vec![
                (7, "2026-06-13".to_string()),
                (30, "2026-05-21".to_string()),
                (365, "2025-06-20".to_string()),
            ]
        );
        // Crossing a month boundary.
        assert_eq!(
            on_this_day_targets("2026-03-03", &[7]),
            vec![(7, "2026-02-24".to_string())]
        );
        // Leap day: 365 days before 2024-02-29 lands in 2023.
        assert_eq!(
            on_this_day_targets("2024-02-29", &[365]),
            vec![(365, "2023-03-01".to_string())]
        );
        // Zero and bad input.
        assert!(on_this_day_targets("2026-06-20", &[0]).is_empty());
        assert!(on_this_day_targets("not-a-date", &[7]).is_empty());
    }

    #[test]
    fn offset_label_is_friendly() {
        assert_eq!(offset_label(7), "a week ago");
        assert_eq!(offset_label(365), "a year ago");
        assert_eq!(offset_label(3), "3 days ago");
    }

    #[test]
    fn unfinished_tasks_keeps_only_unchecked() {
        let body = "\
# Today
- [ ] open one
- [x] done one
- [ ]   open with extra space
  - [ ] nested open
- [X] done caps
* [ ] star bullet
+ [ ] plus bullet
- not a task
plain line
- [] malformed (no inner space)";

        let tasks = unfinished_tasks(body);
        assert_eq!(
            tasks,
            vec![
                "- [ ] open one".to_string(),
                "- [ ]   open with extra space".to_string(),
                "  - [ ] nested open".to_string(),
                "* [ ] star bullet".to_string(),
                "+ [ ] plus bullet".to_string(),
            ]
        );
    }

    #[test]
    fn unfinished_tasks_empty_when_none() {
        assert!(unfinished_tasks("just prose\n- [x] all done").is_empty());
        assert!(unfinished_tasks("").is_empty());
    }

    #[test]
    fn append_carried_over_inserts_section() {
        let body = "Daily template\n- ";
        let tasks = vec!["- [ ] a".to_string(), "  - [ ] b".to_string()];
        let out = append_carried_over(body, &tasks);
        assert_eq!(
            out,
            "Daily template\n- \n\n## Carried over\n\n- [ ] a\n  - [ ] b\n"
        );
    }

    #[test]
    fn append_carried_over_handles_empty_body() {
        let tasks = vec!["- [ ] a".to_string()];
        assert_eq!(
            append_carried_over("", &tasks),
            "## Carried over\n\n- [ ] a\n"
        );
        // Trailing newlines in the source collapse to a single separating blank.
        assert_eq!(
            append_carried_over("x\n\n\n", &tasks),
            "x\n\n## Carried over\n\n- [ ] a\n"
        );
    }

    #[test]
    fn append_carried_over_noop_without_tasks() {
        assert_eq!(append_carried_over("body", &[]), "body");
    }
}
