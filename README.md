# Codex ACP Agent

[![MSRV](https://img.shields.io/badge/MSRV-1.90%2B-blue.svg)](rust-toolchain.toml)
[![Edition](https://img.shields.io/badge/Edition-2024-blueviolet.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)

> This repository is implemented by the `codex` agent.

An Agent Client Protocol (ACP)–compatible agent that bridges the OpenAI Codex runtime with ACP clients over stdio. This project is under active development — features are evolving and breaking changes are likely.

## Highlights

- Agent Client Protocol (ACP) over stdio using `agent-client-protocol`.
- Integrates with the Codex Rust workspace for conversation management and event streaming.
- Slash commands with ACP AvailableCommands updates (advertised to clients on session start).
- Status output tailored for IDEs (workspace, account, model, token usage).
- Supports ACP session modes: `read-only`, `auto` (default), and `full-access`.
- Automatically launches an internal MCP filesystem server (`acp_fs`) so Codex reads/writes files through ACP tooling instead of shell commands.

## Requirements

- Rust (Rust 2024 edition; rustc 1.90+ as pinned in `rust-toolchain.toml`).
- Network access for building Git dependencies (Codex workspace, ACP crate).

## Build

```bash
make build
```

## Run

The agent communicates over stdin/stdout using ACP JSON-RPC. Launch it and connect from an ACP client (e.g., an IDE integration or a CLI client implementing ACP):

```bash
# With tracing logs
RUST_LOG=info cargo run --quiet
```

Because this agent speaks on stdio, it is intended to be spawned by your client. For manual testing, you can pipe ACP JSON-RPC messages to stdin and read replies from stdout.

> Tip: use `make release` (or `cargo build --release`) when shipping the binary to an IDE like Zed. The release build lives at `target/release/codex-acp`.

Example JSON-RPC (initialize → new session → /status):

```
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"v1","clientName":"cli","capabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/absolute/path","mcpServers":[]}}
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"1","prompt":[{"type":"text","text":"/status"}]}}
```

## Usage (ACP over stdio)

Minimal smoke test from a shell piping JSON-RPC over stdio:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"v1","clientName":"cli","capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"'"$PWD"'","mcpServers":[]}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"1","prompt":[{"type":"text","text":"/status"}]}}' \
| RUST_LOG=info cargo run --quiet
```

Or use the included script and Makefile target:

```bash
chmod +x scripts/stdio-smoke.sh
make smoke
```

### Configuration in [Zed](https://zed.dev)

> Add this configuration to zed settings.
```json
"agent_servers": {
  "Codex": {
    "command": "codex-acp",
    "args": [],
    "env": {
      "RUST_LOG": "info"
    }
  }
}
```

The agent automatically boots an MCP filesystem bridge. No extra configuration (or AGENTS.md edits) are required—Codex will discover the `acp_fs` server on every session.

## Filesystem tooling

When a session starts, `codex-acp` spins up an in-process TCP bridge and registers an MCP server named `acp_fs`. Codex then calls two structured tools:

- `read_text_file` — reads workspace files via ACP `client.read_text_file`, falling back to local disk if the client lacks FS support.
- `write_text_file` — writes workspace files via ACP `client.write_text_file`, with a local fallback.

`codex-acp` also injects a default instruction reminding the model to use these tools rather than shelling out with `cat`/`tee`. If your client exposes filesystem capabilities, file access stays within ACP.

## Features

- ACP Agent implementation
  - Handles `initialize`, `authenticate` (API key), `session/new`, `session/prompt`, `session/cancel`.
  - Streams Codex events (assistant text and deltas, reasoning deltas, token counts) as `session/update` notifications.

- Slash commands (advertised via `AvailableCommandsUpdate`)
  - Implemented today:
    - `/init` - Create an AGENTS.md file with instructions for Codex
    - `/model` — Show or set the current model (uses `Op::OverrideTurnContext`).
    - `/approvals` — Set approval mode (`untrusted | on-request | on-failure | never`).
    - `/status` — Rich status (workspace, account, model, token usage).
    - `/compact` — Summarize conversation to prevent hitting the context limit.
    - `/review` — Review current changes and find issues.
    - `/new` — Start a new chat during a conversation.
    - `/quit` — Exit Codex agent. Shows a goodbye message and requests backend shutdown if available.

- Session modes
  - Advertises `read-only`, `auto` (current), and `full-access` on new session.
  - Clients may switch modes via ACP `session/setMode`; the agent emits `CurrentModeUpdate`.

## Status Output (`/status`)

The `/status` command prints a human-friendly summary, e.g.:

```
📂 Workspace
  • Path: ~/path/to/workspace
  • Approval Mode: on-request
  • Sandbox: workspace-write
  • AGENTS files: (none)

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

## Development

- Branching: prefer topic branches; small, focused commits.
- Lint/test locally using `cargo check`, `cargo fmt`, `cargo clippy`, and `cargo test`.
- Logging uses `tracing` + `tracing-subscriber`; use `RUST_LOG=info` during development.

## Related Projects

- Zed ACP example (Claude): https://github.com/zed-industries/claude-code-acp
- Agent Client Protocol (Rust): https://crates.io/crates/agent-client-protocol
- OpenAI Codex (Rust workspace): https://github.com/openai/codex
