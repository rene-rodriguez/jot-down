<div align="center">

<img src="assets/jot-down-logo.png?v=2" alt="jot-down — Terminal Markdown Notes" width="380">

<br>

**A fast, offline-first note-taking TUI for the terminal — built in Rust.**

Keyboard-driven Markdown notes, full-text and semantic search, and local AI Q&A —
all in a single binary. Notes live in a local SQLite database; an optional PostgreSQL
backend syncs them across machines. No cloud account, no subscription, no lock-in.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/rene-rodriguez/jot-down?sort=semver)](https://github.com/rene-rodriguez/jot-down/releases/latest)
![Platforms](https://img.shields.io/badge/platforms-macOS%20·%20Linux-lightgrey)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)

</div>

---

## Features

- **Markdown-native editor** with live preview, syntax highlighting, and scrolling
- **Full-text and semantic search** — find notes by keywords or meaning (`/`, then `Tab` to toggle)
- **Ask your notes** — question answering grounded in your own notes, with cited sources (`a`)
- **Daily notes** — `o` opens or creates today's note
- **Wikilinks** — `[[link]]` syntax with autocomplete and backlinks panel
- **Multi-device sync** via a shared PostgreSQL backend with conflict resolution
- **Command palette** — fuzzy-searchable launcher for every command (`:`)
- **Trash & restore** — deleted notes are soft-deleted and can be restored (`D`)
- **Export / import** — full round-trip to and from `.md` files (`E` / `I`)
- **Local AI, fully offline** — semantic search and note Q&A run on a bundled vector index with no API key required

---

## Installation

**Requirements:** Rust stable (1.75+). No other runtime dependencies for local-only mode.

### Makefile

```bash
git clone https://github.com/rene-rodriguez/jot-down.git
cd jot-down
make install                   # → /usr/local/bin/jot-down
make install PREFIX=~/.local   # → ~/.local/bin/jot-down
```

```bash
make uninstall
```

### Install script

```bash
git clone https://github.com/rene-rodriguez/jot-down.git
cd jot-down
./scripts/install.sh           # auto-detects /usr/local or ~/.local
./scripts/install.sh /opt/homebrew
```

### cargo install

```bash
cargo install --path .
```

Places the binary at `~/.cargo/bin/jot-down`, already on `PATH` if you use `rustup`.

---

## Quick start

```bash
cargo run
```

The database is created automatically at `~/.local/share/jot-down/jot-down.db` on first launch. On first run you'll see a setup screen where you can configure paths and an optional sync connection — or just press `Esc` to skip and use all defaults.

---

## Keyboard shortcuts

| Key          | Action                              |
| ------------ | ----------------------------------- |
| `n`          | New note (prompts for title)        |
| `o`          | Open today's daily note             |
| `i`          | Open body editor                    |
| `Ctrl+F`     | Find in note (editor)               |
| `Ctrl+S`     | Save and exit editor                |
| `e`          | Rename selected note                |
| `d`          | Move note to trash                  |
| `D`          | Browse trash (restore / purge)      |
| `t`          | Add tag                             |
| `1`–`9`      | Apply suggested tag                 |
| `R`          | Remove tag                          |
| `T`          | Filter by tag                       |
| `/`          | Search (keyword or semantic)        |
| `Tab`        | Toggle keyword ↔ semantic search    |
| `a`          | Ask your notes                      |
| `s`          | Sync now                            |
| `C`          | Review sync conflicts               |
| `X`          | Rebuild embedding index             |
| `E`          | Export all notes to Markdown        |
| `I`          | Import notes from Markdown          |
| `:`          | Command palette                     |
| `,`          | Settings                            |
| `?`          | Keybinding reference                |
| `j` / `k`   | Move down / up                      |
| `q`          | Quit                                |

Navigation keys act as commands in the note list only. Inside the editor, search box, and settings form they type literally.

---

## AI features

AI is local-first and on by default — no API key, no installation, no network.

### Semantic search

Press `/`, type a query, and `Tab` switches between keyword and semantic mode. Semantic mode ranks notes by meaning, surfacing notes that share no exact words with your query.

### Ask your notes

Press `a`, ask a question in plain language. `jot-down` retrieves the most relevant notes, generates an answer grounded in them, and shows numbered source citations you can open directly.

### How it works

Notes are embedded into a bundled vector index (`sqlite-vec`, compiled into the binary) on every save. No network is involved. The answer generation step calls an **OpenAI-compatible** chat endpoint — defaulting to a **local LLM** via [Ollama](https://ollama.com) or `llama.cpp`:

```sh
ollama serve && ollama pull llama3.1:8b
```

The index tracks which embedding model built it and rebuilds automatically if the model changes. Force a manual rebuild with `X`; run `jot-down doctor` to see index and endpoint status.

### Configuration

Set via the in-app settings (`,`) or `~/.config/jot-down/config.toml`:

```toml
[ai]
enabled = true
search_default = "semantic"    # or "keyword"

[ai.chat]
base_url = "http://localhost:11434/v1"  # any OpenAI-compatible endpoint
model    = "llama3.1:8b"
api_key_env  = "JOT_AI_API_KEY"        # env var name — only needed for remote endpoints
allow_remote = false                    # set true to use a non-local base_url
```

With defaults, note content never leaves your machine. The settings screen and Ask view both display a `LOCAL` / `REMOTE` indicator.

```sh
jot-down doctor   # storage, vector index, embedding model, and chat endpoint status
```

Build without AI: `cargo build --no-default-features`. Disable at runtime: `[ai].enabled = false`.

---

## Sync

`jot-down` syncs through a shared PostgreSQL database. Each machine keeps its own local SQLite cache; Postgres is the transport layer, not the source of truth.

### Setup

Point every machine at the same database URL — via the setup screen on first launch, the settings (`,`), `~/.config/jot-down/config.toml`, or the `JOT_DATABASE_URL` environment variable:

```toml
[sync]
enabled              = true
database_url         = "postgres://user:password@host:5432/jot-down"
poll_interval_seconds = 30
```

### How it works

- On launch, `jot-down` syncs once automatically.
- A background worker polls every `poll_interval_seconds`; press `s` to sync on demand.
- A new machine pulls the full note history on its first sync.
- Note create, edit, delete, and tag changes propagate in both directions.
- If the same note is edited on two machines before syncing, a conflict badge appears — press `C` to resolve (keep local / keep remote / save both).

**Note:** sync is poll-based, not push. The current implementation uses a single shared database with no per-user encryption; treat it as a trusted private backend.

---

## Configuration reference

`~/.config/jot-down/config.toml` (all fields optional — defaults work out of the box):

```toml
data_dir = "/home/you/.local/share/jot-down"
db_path  = "/home/you/.local/share/jot-down/jot-down.db"

[editor]
autosave_seconds = 5

[sync]
enabled              = true
database_url         = "postgres://user:password@localhost:5432/jot-down"
poll_interval_seconds = 30

[ai]
enabled        = true
search_default = "semantic"
```

Changes to `data_dir`, `db_path`, and sync settings take effect on the next launch.

---

## Data layout

```
~/.local/share/jot-down/
├── jot-down.db        # local note cache
├── exports/           # Markdown exports (E)
├── import/            # drop .md files here, then press I
└── logs/

~/.config/jot-down/
└── config.toml
```

---

## Development

```bash
cargo build --release
cargo test
cargo fmt
cargo clippy --all-targets --all-features
```

---

## Stack

| Layer          | Technology    |
| -------------- | ------------- |
| Language       | Rust          |
| TUI            | Ratatui       |
| Event loop     | Crossterm     |
| Local storage  | SQLite        |
| Remote storage | PostgreSQL    |
| Async runtime  | Tokio         |
| Database access| SQLx          |
| Config         | Serde + TOML  |
| Logging        | Tracing       |
| Vector index   | sqlite-vec    |

---

## License

MIT — see [LICENSE](LICENSE).
