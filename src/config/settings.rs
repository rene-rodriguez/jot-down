use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,

    #[serde(default)]
    pub editor: EditorConfig,

    #[serde(default)]
    pub preview: PreviewConfig,

    #[serde(default)]
    pub sync: SyncConfig,

    #[serde(default)]
    pub ai: AiConfig,

    #[serde(default)]
    pub notes: NotesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorConfig {
    #[serde(default = "default_autosave_seconds")]
    pub autosave_seconds: u64,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            autosave_seconds: default_autosave_seconds(),
        }
    }
}

/// How to surface a link's URL alongside its text in the preview pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkUrlMode {
    /// Append the URL in parentheses after the link text.
    #[default]
    Inline,
    /// Number links and list the URLs at the bottom of the note.
    Footnote,
    /// Show only the link text.
    Hide,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewConfig {
    /// Render markdown in the preview pane (false → raw source).
    #[serde(default = "default_preview_render_markdown")]
    pub render_markdown: bool,

    /// How to display link URLs (inline / footnote / hide).
    #[serde(default = "default_link_urls")]
    pub show_link_urls: LinkUrlMode,

    /// Enable smart quotes, dashes, and ellipsis.
    #[serde(default = "default_typographer")]
    pub typographer: bool,

    /// Enable :shortcode: emoji substitution.
    #[serde(default = "default_emoji")]
    pub emoji: bool,

    /// Enable emoticon substitution (:-) → 😊, etc.) — false-positive prone.
    #[serde(default = "default_emoji_emoticons")]
    pub emoji_emoticons: bool,

    /// Enable ==mark== highlighting.
    #[serde(default = "default_mark")]
    pub mark: bool,

    /// Enable ++ins++ as underlined text.
    #[serde(default = "default_ins")]
    pub ins: bool,

    /// Enable ^super^script and ~sub~script.
    #[serde(default = "default_sup_sub")]
    pub sup_sub: bool,

    /// Strip `*[ABBR]: expansion` lines from source.
    #[serde(default = "default_abbreviations")]
    pub abbreviations: bool,

    /// Render definition lists (`term\n:   def`).
    #[serde(default = "default_definition_lists")]
    pub definition_lists: bool,

    /// Render `::: type` custom container callouts.
    #[serde(default = "default_custom_containers")]
    pub custom_containers: bool,

    /// Style bare URLs in running text.
    #[serde(default = "default_linkify")]
    pub linkify: bool,

    /// Render `[[wikilinks]]` as styled internal links + a backlinks panel.
    #[serde(default = "default_wikilinks")]
    pub wikilinks: bool,

    /// Enable code-block actions in the preview (focus + copy).
    #[serde(default = "default_code_actions")]
    pub code_actions: bool,

    /// Allow running shell code blocks from the preview (always behind a y/n
    /// confirm). When false, the run key is inert.
    #[serde(default = "default_allow_run")]
    pub allow_run: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            render_markdown: default_preview_render_markdown(),
            show_link_urls: default_link_urls(),
            typographer: default_typographer(),
            emoji: default_emoji(),
            emoji_emoticons: default_emoji_emoticons(),
            mark: default_mark(),
            ins: default_ins(),
            sup_sub: default_sup_sub(),
            abbreviations: default_abbreviations(),
            definition_lists: default_definition_lists(),
            custom_containers: default_custom_containers(),
            linkify: default_linkify(),
            wikilinks: default_wikilinks(),
            code_actions: default_code_actions(),
            allow_run: default_allow_run(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Whether sync is enabled at all.
    #[serde(default = "default_sync_enabled")]
    pub enabled: bool,

    /// PostgreSQL connection string. Prefer JOT_DATABASE_URL env var.
    #[serde(default)]
    pub database_url: Option<String>,

    /// How often to poll for remote changes (seconds).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: default_sync_enabled(),
            database_url: None,
            poll_interval_seconds: default_poll_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    /// Whether local AI storage/indexing support is enabled.
    #[serde(default = "default_ai_enabled")]
    pub enabled: bool,

    /// Default search strategy: "keyword" or "semantic".
    #[serde(default = "default_search_default")]
    pub search_default: String,

    /// "Ask your notes" chat backend configuration.
    #[serde(default)]
    pub chat: ChatConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: default_ai_enabled(),
            search_default: default_search_default(),
            chat: ChatConfig::default(),
        }
    }
}

/// OpenAI-compatible chat endpoint for "ask your notes". Defaults to a local
/// LLM runtime; remote endpoints require `allow_remote = true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    /// Base URL of the OpenAI-compatible API (e.g. Ollama's `/v1`).
    #[serde(default = "default_chat_base_url")]
    pub base_url: String,

    /// Model name to request.
    #[serde(default = "default_chat_model")]
    pub model: String,

    /// Environment variable holding the API key (only needed for remote
    /// endpoints; local runtimes ignore it).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Must be true to use a non-localhost `base_url`.
    #[serde(default)]
    pub allow_remote: bool,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            base_url: default_chat_base_url(),
            model: default_chat_model(),
            api_key_env: None,
            allow_remote: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesConfig {
    #[serde(default = "default_daily_format")]
    pub daily_format: String,
    #[serde(default)]
    pub daily_template: String,

    /// Carry unfinished `- [ ]` tasks from the most recent prior daily note
    /// into a new daily note (under a `## Carried over` heading).
    #[serde(default = "default_rollup_tasks")]
    pub rollup_tasks: bool,

    /// Surface an "On this day" list of daily notes from prior periods when
    /// previewing a daily note.
    #[serde(default = "default_on_this_day")]
    pub on_this_day: bool,

    /// Day offsets used for "On this day" matches (default a week/month/year).
    #[serde(default = "default_on_this_day_offsets")]
    pub on_this_day_offsets: Vec<u32>,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            daily_format: default_daily_format(),
            daily_template: String::new(),
            rollup_tasks: default_rollup_tasks(),
            on_this_day: default_on_this_day(),
            on_this_day_offsets: default_on_this_day_offsets(),
        }
    }
}

impl NotesConfig {
    /// Today's daily-note calendar date as ISO `YYYY-MM-DD` (local time) — the
    /// stored `daily_date` marker, independent of `daily_format`.
    pub fn daily_date(&self) -> String {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    }
}

impl NotesConfig {
    /// Today's daily-note title from `daily_format` (local calendar day).
    pub fn daily_title(&self) -> String {
        format_or_iso(chrono::Local::now().naive_local(), &self.daily_format)
    }
}

/// Format `now` with a strftime `fmt`, falling back to the ISO date when `fmt`
/// is empty or contains an invalid specifier.
///
/// `daily_format` comes from user config, and chrono *panics* when
/// `.format(bad).to_string()` is called on an invalid format. Formatting via
/// `write!` instead surfaces that as an `Err` we can recover from, so a typo in
/// the config can never crash the app.
fn format_or_iso(now: chrono::NaiveDateTime, fmt: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if write!(out, "{}", now.format(fmt)).is_err() || out.is_empty() {
        out.clear();
        let _ = write!(out, "{}", now.format("%Y-%m-%d"));
    }
    out
}

fn default_autosave_seconds() -> u64 {
    5
}

fn default_preview_render_markdown() -> bool {
    true
}

fn default_link_urls() -> LinkUrlMode {
    LinkUrlMode::default()
}

fn default_typographer() -> bool {
    true
}

fn default_emoji() -> bool {
    true
}

fn default_emoji_emoticons() -> bool {
    false
}

fn default_mark() -> bool {
    true
}

fn default_ins() -> bool {
    true
}

fn default_sup_sub() -> bool {
    true
}

fn default_abbreviations() -> bool {
    true
}

fn default_definition_lists() -> bool {
    true
}

fn default_custom_containers() -> bool {
    true
}

fn default_linkify() -> bool {
    false
}

fn default_wikilinks() -> bool {
    true
}

fn default_code_actions() -> bool {
    true
}

fn default_allow_run() -> bool {
    true
}

fn default_sync_enabled() -> bool {
    false
}

fn default_poll_interval() -> u64 {
    30
}

fn default_ai_enabled() -> bool {
    cfg!(feature = "ai")
}

fn default_search_default() -> String {
    "semantic".to_string()
}

fn default_chat_base_url() -> String {
    // Ollama's OpenAI-compatible endpoint; also matches a llama.cpp server.
    "http://localhost:11434/v1".to_string()
}

fn default_chat_model() -> String {
    "llama3.1:8b".to_string()
}

fn default_daily_format() -> String {
    "%Y-%m-%d".to_string()
}

fn default_rollup_tasks() -> bool {
    true
}

fn default_on_this_day() -> bool {
    true
}

fn default_on_this_day_offsets() -> Vec<u32> {
    vec![7, 30, 365]
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "jot-down")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".jot-down"))
}

fn default_db_path() -> PathBuf {
    default_data_dir().join("jot-down.db")
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            db_path: default_db_path(),
            editor: EditorConfig::default(),
            preview: PreviewConfig::default(),
            sync: SyncConfig::default(),
            ai: AiConfig::default(),
            notes: NotesConfig::default(),
        }
    }
}

impl Settings {
    /// The directory where the config file lives (`~/.config/jot-down`).
    pub fn config_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "jot-down")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".jot-down"))
    }

    /// The full path to the config file (`~/.config/jot-down/config.toml`).
    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// Whether a config file already exists on disk. Used to decide whether to
    /// show the first-run setup screen.
    pub fn config_exists() -> bool {
        Self::config_path().exists()
    }

    /// Persist the settings to `~/.config/jot-down/config.toml`, creating the config
    /// directory if needed.
    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;
        let toml = toml::to_string_pretty(self)?;
        std::fs::write(Self::config_path(), toml)?;
        Ok(())
    }

    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        let mut settings = if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            toml::from_str(&contents)?
        } else {
            Settings::default()
        };

        // Environment variable overrides
        if let Ok(url) = std::env::var("JOT_DATABASE_URL") {
            settings.sync.database_url = Some(url);
            settings.sync.enabled = true;
        }

        settings.resolve_paths();
        Ok(settings)
    }

    fn resolve_paths(&mut self) {
        if self.data_dir.as_os_str().is_empty() {
            self.data_dir = default_data_dir();
        }
        if self.db_path.as_os_str().is_empty() {
            self.db_path = self.data_dir.join("jot-down.db");
        }
    }

    /// Ensure the data directory exists.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_toml_roundtrips() {
        // A settings struct serialized to TOML and parsed back should be equal.
        let mut s = Settings::default();
        s.sync.enabled = true;
        s.sync.database_url = Some("postgres://user:pw@localhost:5432/jot".to_string());
        s.sync.poll_interval_seconds = 45;
        s.editor.autosave_seconds = 10;
        s.ai.enabled = true;

        let toml = toml::to_string_pretty(&s).expect("serialize");
        let parsed: Settings = toml::from_str(&toml).expect("deserialize");

        assert!(parsed.sync.enabled);
        assert_eq!(
            parsed.sync.database_url.as_deref(),
            Some("postgres://user:pw@localhost:5432/jot")
        );
        assert_eq!(parsed.sync.poll_interval_seconds, 45);
        assert_eq!(parsed.editor.autosave_seconds, 10);
        assert!(parsed.ai.enabled);
        assert_eq!(parsed.data_dir, s.data_dir);
        assert_eq!(parsed.db_path, s.db_path);
    }

    #[test]
    fn settings_accepts_ai_enabled_flag() {
        let toml = r#"
data_dir = ""
db_path = ""

[ai]
enabled = false
"#;

        let parsed: Settings = toml::from_str(toml).expect("deserialize");
        assert!(!parsed.ai.enabled);
    }

    #[test]
    fn settings_accepts_daily_format() {
        let toml = r#"
data_dir = ""
db_path = ""

[notes]
daily_format = "%Y/%m/%d"
"#;

        let parsed: Settings = toml::from_str(toml).expect("deserialize");
        assert_eq!(parsed.notes.daily_format, "%Y/%m/%d");
        assert!(parsed.notes.daily_template.is_empty());
    }

    #[test]
    fn daily_title_formats_and_falls_back_safely() {
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 6, 17)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();

        assert_eq!(format_or_iso(dt, "%Y-%m-%d"), "2026-06-17");
        assert_eq!(format_or_iso(dt, "%Y/%m/%d"), "2026/06/17");
        // Invalid specifier would panic via `to_string()`; we fall back instead.
        assert_eq!(format_or_iso(dt, "%Q-nope"), "2026-06-17");
        // Empty format also falls back rather than yielding an empty title.
        assert_eq!(format_or_iso(dt, ""), "2026-06-17");
    }
}
