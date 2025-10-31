# Repository Guidelines

This document describes how to work in this repo using idiomatic Rust patterns and the current module layout.

## Project Structure

- src/
  - lib.rs — library crate root; exports `agent`, `fs`, `logging` (forbid unsafe). Re‑exports `CodexAgent`, `SessionModeLookup`, and `FsBridge`.
  - main.rs — binary entrypoint; stdio wiring and runtime setup. Pass `--acp-fs-mcp` to run the standalone `acp_fs` MCP server.
  - logging.rs — tracing/logging init helpers (`init_from_env`, `LoggingGuard`).
  - agent/
    - mod.rs — Agent trait façade; ACP Agent impl delegating into submodules.
    - core.rs — ACP request handlers (initialize, authenticate, new/load session, set_session_mode, set_session_model, prompt, ext); client I/O wiring.
    - session_manager.rs — session state + modes/models helpers: `SessionState`, `SessionModeLookup`, approval presets → ACP `SessionMode`s, model parsing/validation, custom provider helpers.
    - commands.rs — slash command handlers and helpers; `AVAILABLE_COMMANDS` advertised to clients.
    - events.rs — Codex Event → ACP updates; `EventHandler`, `ReasoningAggregator`.
    - prompt.rs — prompt text and authoring helpers used by the agent.
    - config_builder.rs — builds session/conversation config (cwd, MCP servers, etc.).
  - fs/
    - mod.rs, bridge.rs, mcp_server.rs — filesystem bridge + `acp_fs` MCP server. `mcp_server` uses `ACP_FS_BRIDGE_ADDR` and `ACP_FS_SESSION_ID` to talk to the bridge.
- Cargo.toml, rust-toolchain.toml
- README.md, AGENTS.md
- Makefile, scripts/stdio-smoke.sh

## Build, Test, Run

- `cargo check` — fast type pass.
- `cargo build` — compile.
- `cargo fmt --all` — format with rustfmt.
- `cargo clippy -- -D warnings` — lint and deny warnings.
- `cargo test` — run unit tests.
- `RUST_LOG=info cargo run --quiet` — run the agent over stdio.
- `make smoke` — run a simple stdio JSON-RPC smoke test.

## Coding Style & Conventions

- Rust 2024 edition; 4-space indentation; rustfmt enforced.
- Unsafe: forbidden at crate root (`#![forbid(unsafe_code)]`).
- Naming: snake_case (fns/vars), CamelCase (types/traits), SCREAMING_SNAKE_CASE (consts).
- Imports: group `std`, external crates, then local modules; avoid unused imports.
- Visibility: default to private; prefer `pub(crate)` over `pub` unless part of the public API.
- Errors: convert external errors early; map to ACP `Error` at boundaries. Use `anyhow` internally where appropriate.
- Logging: `tracing`; control via `RUST_LOG`.

## Testing Guidelines

- Keep tests deterministic (avoid timing races); prefer current-thread executors (`LocalSet`) for async tests when needed.
- Name tests by behavior in snake_case (e.g., `is_read_only_detection`).
- Place small unit tests inline with `#[cfg(test)]` in the same module, or create a dedicated tests module under `src/agent/` if grouping makes sense.

## Pull Requests & Commits

- Conventional Commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, etc.
- PRs include: problem statement, approach, linked issues, and a test plan (commands run, expected output). Include brief `RUST_LOG` snippets when relevant.

## Security & Configuration

- Auth: use `codex login` or `OPENAI_API_KEY`.
- Do not commit secrets (API keys, auth.json); rely on env/OS keychain.
- First build fetches git dependencies; subsequent builds are cached.

## Agent-Specific Notes

- Add/extend slash commands in `src/agent/commands.rs` (advertised via `AVAILABLE_COMMANDS`).
- Use `events::EventHandler` to construct ACP updates; aggregate reasoning with `ReasoningAggregator`.
- Use `session_manager::{session_modes_for_config, find_preset_by_mode_id, available_modes}` to manage session modes.
- `SessionManager` is available via `codex_acp::SessionManager` and can be accessed from `CodexAgent` using the `session_manager()` method.
- `SessionManager` provides both state management and query methods (`current_mode()`, `is_read_only()`, `resolve_acp_session_id()`).
- Prefer the session manager and client ops exposed by the agent for capability checks and FS requests.

## Custom Provider Support

### Authentication
The agent supports custom (non-builtin) model providers through a dedicated authentication flow:

- Builtin providers: `openai` (uses existing ChatGPT or API key auth)
- Custom providers: any other provider configured in `model_providers`

When a custom provider is configured:
- Initialize advertises a provider-specific auth method:
  - id: `{provider_id}` (from `config.model_provider_id`)
  - name: provider display name (`config.model_provider.name`)
- Authenticate supports `apikey`, `chatgpt`, and a custom provider branch. Note: the custom branch currently matches the method id `"custom_provider"` in code; clients should either send that id or the server should be updated to accept both. Keep this in mind when wiring clients.

### Model Management
Model listing and switching are only available for custom providers:

- `new_session` and `load_session` return `models: Some(...)` only for custom providers.
- `set_session_model` requires both current and target models to be custom providers.
- `available_models_from_profiles` filters out builtin provider models.

Model ID format: `{provider_id}@{model_name}` (e.g., `anthropic@claude-3`, `custom-llm@my-model`).

### Implementation Details
- `session_manager::is_custom_provider(provider_id)` determines if a provider is custom (`!matches!(provider_id, "openai")`).
- `session_manager::available_models_from_profiles(...)` builds model lists from profiles (custom-only).
- `core::new_session`/`core::load_session` include `models` only for custom providers.
- `core::set_session_model` parses and validates model ids and enforces custom→custom switching.

## FS Bridge & MCP

- The FS MCP server (`codex-acp --acp-fs-mcp`) reads `ACP_FS_BRIDGE_ADDR` and `ACP_FS_SESSION_ID` to communicate with the local bridge.
- The bridge exposes read/write ops; large reads are paged (~1000 lines/50KB) and advertise pagination metadata.

