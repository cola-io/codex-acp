# Codex ACP Agent

[![MSRV](https://img.shields.io/badge/MSRV-1.91%2B-blue.svg)](rust-toolchain.toml)
[![Edition](https://img.shields.io/badge/Edition-2024-blueviolet.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)

> Most of this repository code is implemented and reviewed by `codex` agents.

An Agent Client Protocol (ACP)–compatible agent that bridges the OpenAI Codex runtime with ACP clients over stdio. This project is under active development — features are evolving and breaking changes are likely.

## Architecture

The agent is structured around several key modules:

- **`agent/core.rs`** — Core ACP request handlers (`initialize`, `authenticate`, `new_session`, `load_session`, `prompt`, etc.) and `CodexAgent` implementation
- **`agent/session_manager.rs`** — Unified session management including:
  - Session state storage and mutation
  - Session mode/model queries (`current_mode()`, `is_read_only()`, `resolve_acp_session_id()`)
  - Conversation loading and caching
  - Client update notifications
  - Context override operations
- **`agent/commands.rs`** — Slash command handlers (`/init`, `/status`, `/compact`, `/review`)
- **`agent/events.rs`** — Codex Event → ACP update conversion; reasoning aggregation
- **`agent/config_builder.rs`** — Session/conversation config construction (cwd, MCP servers, etc.)
- **`fs/`** — Filesystem bridge and `acp_fs` MCP server implementation

Key design principles:
- `SessionManager` centralizes all session operations — both mutations and queries
- `SessionState` is a pure data structure representing per-session state
- The agent runs on Tokio's current-thread runtime with `LocalSet` for single-threaded async

## Highlights

- Agent Client Protocol (ACP) over stdio using `agent-client-protocol`.
- Integrates with the Codex Rust workspace for conversation management and event streaming.
- Slash commands with ACP AvailableCommands updates (advertised to clients on session start).
- Status output tailored for IDEs (workspace, account, model, token usage).
- Supports ACP session modes: `read-only`, `auto` (default), and `full-access`.
- Automatically launches an internal MCP filesystem server (`acp_fs`) built with `rmcp`, so Codex reads/writes files through ACP tooling instead of shell commands.

## Features

- **ACP Agent implementation**
  - Handles `initialize`, `authenticate`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, `session/setMode`, `session/setModel`.
  - Authentication support for OpenAI (ChatGPT/API key) and custom model providers.
  - Streams Codex events (assistant messages, reasoning, token counts, tool calls) as `session/update` notifications.
  - Event aggregation: reasoning deltas are accumulated and sent as complete blocks.

- **Slash commands** (advertised via `AvailableCommandsUpdate`)
  - `/init` — Create an `AGENTS.md` with repository contributor guidance. Uses a bundled prompt (`src/agent/prompt_init_command.md`).
  - `/status` — Rich status output (workspace, account, model, token usage).
  - `/compact` — Request Codex to compact/summarize the conversation to reduce context size.
  - `/review` — Ask Codex to review current changes, highlight issues, and suggest fixes.
  - Commands are dynamically advertised to clients on session start.

- **Session modes**
  - Three preset modes: `read-only`, `auto` (default), and `full-access`.
  - Modes control approval policy and sandbox restrictions.
  - Clients switch modes via `session/setMode`; agent emits `CurrentModeUpdate`.
  - `SessionManager` provides `is_read_only()` to check mode restrictions.

- **Custom model provider support**
  - Dynamic model listing and switching for custom (non-OpenAI) providers.
  - Model format: `{provider_id}@{model_name}` (e.g., `OpenRouter@anthropic/claude-3-opus`).
  - Configure providers and models via Codex config profiles.
  - Dedicated `custom_provider` authentication method for non-builtin providers.
  - Model switching enforces custom→custom provider transitions only.

- **Session management**
  - `SessionManager` provides unified interface for all session operations:
    - State queries: `current_mode()`, `is_read_only()`, `resolve_acp_session_id()`
    - Conversation management: lazy loading with caching
    - Client notifications: `send_session_update()`, `send_message_chunk()`, `send_thought_chunk()`
    - Context overrides: `apply_context_override()` for approval/sandbox/model changes
  - Access via `agent.session_manager()` for read-only operations or internal mutation.

## Build

### Requirements

- Rust (Rust 2024 edition; rustc 1.91+ as pinned in `rust-toolchain.toml`).
- Network access for building Git dependencies (Codex workspace, ACP crate).

```bash
make build
```

> Tip: use `make release` (or `cargo build --release`) when shipping the binary to an IDE like Zed. The release build lives at `target/release/codex-acp`.

### Configuration in [Zed](https://zed.dev)

> Add this configuration to zed settings.
```json
"agent_servers": {
  "Codex": {
    "command": "/path/to/codex-acp",
    "args": [],
    "env": {
      "RUST_LOG": "info"
    }
  }
}
```

## Filesystem tooling

When a session starts, `codex-acp` spins up an in-process TCP bridge and registers an MCP server named `acp_fs` using `rmcp`. Codex then calls structured tools:

- `read_text_file` — reads workspace files via ACP `client.read_text_file`, falling back to local disk if the client lacks FS support.
- `write_text_file` — writes workspace files via ACP `client.write_text_file`, with a local fallback.
- `edit_text_file` — apply a focused replace in a file and persist.
- `multi_edit_text_file` — apply multiple sequential replacements and persist.

`codex-acp` also injects a default instruction reminding the model to use these tools rather than shelling out with `cat`/`tee`. If your client exposes filesystem capabilities, file access stays within ACP.

**Dynamic tool availability:**
- Tools are enabled/disabled based on client filesystem capabilities.
- Read-only sessions disable write tools (`write_text_file`, `edit_text_file`, `multi_edit_text_file`).
- If the client lacks FS support, tools fall back to local disk I/O.
- The FS bridge uses a dedicated bridge address and session ID for MCP server communication.

## Status Output (`/status`)

The `/status` command prints a human-friendly summary, e.g.:

```
📂 Workspace
  • Path: ~/path/to/workspace
  • Approval Mode: on-request
  • Sandbox: workspace-write

👤 Account
  • Signed in with ChatGPT (or API key / Not signed in)
  • Login: user@example.com
  • Plan: Plus

🧠 Model
  • Name: gpt-5
  • Provider: OpenAI
  • Reasoning Effort: Medium
  • Reasoning Summaries: Auto

📊 Token Usage
  • Session ID: <uuid>
  • Input: 0
  • Output: 0
  • Total: 0
```

Notes
- Some fields may be unknown depending on your auth mode and environment.
- Token counts are aggregated from Codex `EventMsg::TokenCount` when available.

## Authentication

`codex-acp` supports multiple authentication methods:

### OpenAI (Builtin Provider)
- **ChatGPT**: Sign in with your ChatGPT account via `codex login`
- **API Key**: Use `OPENAI_API_KEY` from environment or `auth.json`

### Custom Providers
For custom model providers (e.g., Anthropic, custom LLMs):
1. Configure the provider in your Codex config:
   ```toml
   model_provider_id = "anthropic"
   model = "claude-3-opus"

   [model_providers.anthropic]
   name = "Anthropic"
   # ... provider-specific configuration

   [profiles.custom-fast]
   model = "claude-3-haiku"
   model_provider = "anthropic"
   ```

2. The agent will advertise a `custom_provider` authentication method during initialization.

3. Authenticate with provider-specific credentials configured in your Codex setup.

### Provider-Specific Features
- **OpenAI**: Standard authentication, no model switching (uses config defaults)
- **Custom Providers**:
  - Model listing via `available_models` in session responses
  - Model switching via `session/setModel` with `{provider}@{model}` format
  - Multiple model profiles for easy switching

Example model switching (custom providers only):
```json
{
  "method": "session/setModel",
  "params": {
    "session_id": "...",
    "model_id": "anthropic@claude-3-haiku"
  }
}
```

## Logging

`codex-acp` uses `tracing` + `tracing-subscriber` and can log to stderr and/or a file. Configure it via environment variables:

Environment variables (highest precedence first):
- `CODEX_LOG_FILE` — Path to append logs (non-rotating). Parent directories are created automatically. ANSI is disabled for file logs.
- `CODEX_LOG_DIR` — Directory for daily-rotated logs (file name: `acp.log`). Directory is created automatically. ANSI is disabled for file logs.
- `CODEX_LOG_STDERR` — Set to `0`, `false`, `off`, or `no` to disable stderr logging. Enabled by default.
- `RUST_LOG` — Standard filtering directives (defaults to `info` if unset/invalid). Examples: `info`, `debug`, `codex_acp=trace,rmcp=info`.

Behavior:
- If `CODEX_LOG_FILE` is set, logs go to stderr (unless disabled) and the specified file.
- Else if `CODEX_LOG_DIR` is set, logs go to stderr (unless disabled) and a daily-rotated file in that directory.
- Else logs go to stderr only (unless disabled).

Examples:
```bash
# Console only
RUST_LOG=info cargo run --quiet

# Console + append to file (non-rotating)
RUST_LOG=debug CODEX_LOG_FILE=./logs/codex-acp.log cargo run --quiet

# Console + daily rotation under logs directory
RUST_LOG=info CODEX_LOG_DIR=./logs cargo run --quiet

# File only (disable stderr)
CODEX_LOG_STDERR=0 CODEX_LOG_FILE=./logs/codex-acp.log cargo run --quiet

# MCP filesystem server also honors logging env:
RUST_LOG=debug CODEX_LOG_DIR=./logs cargo run --quiet -- --acp-fs-mcp
```

## Related Projects

- Zed ACP example (Claude): https://github.com/zed-industries/claude-code-acp
- Agent Client Protocol (Rust): https://crates.io/crates/agent-client-protocol
- OpenAI Codex (Rust workspace): https://github.com/openai/codex
- rmcp (Rust MCP): https://github.com/domdomegg/rmcp
