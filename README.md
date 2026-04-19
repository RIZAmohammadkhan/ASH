# ash

`ash` is a fast, cross-platform AI shell built in Rust. It turns natural language into shell commands, keeps context small, retries when commands fail, and stays focused on terminal workflows instead of chat.

## What is implemented

- Ratatui + Crossterm TUI with:
  - scrollable history
  - inline input bar
  - spinner + active model status
  - searchable model picker (`Alt+M` or `F2`)
- Dynamic OpenRouter model loading from `https://openrouter.ai/api/v1/models`
- Daily model cache at `~/.config/ash/models_cache.json`
- Free-only mode when using the embedded/build-time key path
- Full catalog mode when the user provides their own OpenRouter key
- Minimal prompt context injection:
  - current working directory
  - shell
  - OS/arch
  - PATH tool hints
  - last command result on retry
- Clarification/retry loop with max depth `3`
- Long output spilling to `/tmp/ash-out-<timestamp>.txt`
- `Ctrl+O` support for opening the latest saved output file or URL

## Build

```bash
cargo build
```

To compile with an embedded OpenRouter key:

```bash
ASH_EMBEDDED_OPENROUTER_KEY=sk-or-v1-... cargo build --release
```

If you do not embed a key, add one later in config.

## Run

```bash
cargo run
```

Or launch with an initial request:

```bash
cargo run -- "find the 20 largest files here"
```

Useful flags:

```bash
ash --model <model-id>
ash --refresh-models
ash --config /path/to/config.toml
```

## Config

Config lives at `~/.config/ash/config.toml`.

Example:

```toml
openrouter_api_key = "sk-or-v1-..."
default_model = "openrouter/auto"
shell = "/bin/zsh"
```

## Keys

- `Enter`: submit
- `Alt+M` or `F2`: open model picker
- `Ctrl+R`: refresh model list
- `Ctrl+O`: open latest saved output file or URL
- `Up` / `Down`: scroll history
- `Ctrl+C`: quit

## Notes

- Model discovery is dynamic; the code does not ship a hardcoded model catalog.
- The current implementation prefers the first available visible model unless `default_model` or `--model` selects one explicitly.
- The embedded-key path is wired for build-time injection, but no secret is committed in this repository.
