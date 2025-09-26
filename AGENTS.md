# Repository Guidelines

> This document guides contributors working on the Codex ACP Rust agent. Keep changes focused and incremental.

## Project Structure & Module Organization

- Source: `src/`
  - `src/main.rs` — process entry; wires runtime, agent, and filesystem bridge startup.
  - `src/agent.rs` — core ACP agent implementation (sessions, streaming, approvals).
  - `src/agent/commands.rs` — slash command handling and helpers.
  - `src/fs/mod.rs` — filesystem bridge module exports.
  - `src/fs/bridge.rs` — TCP bridge that proxies file reads/writes via the client.
  - `src/fs/mcp_server.rs` — standalone MCP server exposing `read_text_file`/`write_text_file` tools.
- Config/build: `Cargo.toml`, `rust-toolchain.toml`
- Scripts: `scripts/stdio-smoke.sh` — quick stdio smoke test harness.
- Docs: `README.md`, `AGENTS.md`

## Build, Test, and Development Commands

- `cargo build` — compile with dependencies.
- `RUST_LOG=info cargo run --quiet` — run the agent over stdio.
- `ACP_DEV_ALLOW_MOCK=1 RUST_LOG=info cargo run --quiet` — run without Codex backend (slash-commands only).
- `cargo check` — fast type/lint pass.
- `cargo fmt --all` — format code.
- `cargo clippy -- -D warnings` — lint and deny warnings.
- `cargo test` — run unit/integration tests (add as described below).

## Coding Style & Naming Conventions

- Language: Rust 2024 edition; 4-space indentation; `rustfmt` enforced.
- Naming: `snake_case` for functions/vars, `CamelCase` for types/traits, `SCREAMING_SNAKE_CASE` for consts.
- Imports: use explicit paths; group std/crate/local; keep unused imports out.
- Errors: prefer `anyhow::Result<T>` at boundaries; map external errors early.
- Logging: use `tracing`; gate verbosity with `RUST_LOG`.

## Testing Guidelines

- Unit tests: inline with modules using `#[cfg(test)] mod tests { ... }`.
- Integration tests: create `tests/` with files like `agent_status_test.rs`.
- Test names: describe behavior in `snake_case` (e.g., `advertises_available_commands`).
- Determinism: avoid timing races; prefer local `LocalSet` and channel fakes where possible.

## Commit & Pull Request Guidelines

- Commits: concise scope; prefer Conventional Commits (e.g., `feat:`, `fix:`, `refactor:`).
- PRs must include:
  - Problem statement and approach; link issues if any.
  - Test plan (commands run, expected output). Include relevant `RUST_LOG` snippets.
  - Screenshots/logs for ACP interactions when helpful.

## Security & Configuration Tips

- Auth: use `codex login` (ChatGPT) or `OPENAI_API_KEY` for API key mode.
- Secrets: never commit keys or `auth.json`; rely on env/OS keychain.
- Network: first build fetches git dependencies; subsequent builds are cached.
- Safety: default to on-request approvals; avoid destructive commands in examples.
