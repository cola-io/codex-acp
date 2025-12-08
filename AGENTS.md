# Repository Guidelines

This document describes how to work in this repo using idiomatic Rust patterns and the current module layout.

## Project Structure

- src/
  - lib.rs — library crate root; exposes `agent`, `fs`, `logging`; re-exports `CodexAgent`, `SessionManager`, `FsBridge`, and `init_from_env` for embedders.
  - main.rs — binary entrypoint; initializes tracing, loads config + profiles, boots the filesystem bridge, and wires the ACP runtime. Pass `--acp-fs-mcp` to run the standalone filesystem MCP server.
  - logging.rs — tracing init helpers driven by env variables (`init_from_env`), including optional file logging and daily rotation.
  - agent/
    - mod.rs — Agent trait façade; wires ACP trait methods into `CodexAgent` and re-exports `ClientOp`.
    - core.rs — defines `CodexAgent` and `ClientOp`, handles initialize/auth/new/load session, and applies session mode/model mutations while coordinating the auth/conversation managers.
    - prompt.rs — streaming prompt pipeline, slash-command detection, tool + reasoning updates, cancel handling, and extension method stubs.
    - commands.rs — slash command registry (`AVAILABLE_COMMANDS`) plus helpers like `handle_slash_command` and `/status` rendering.
    - events.rs — Codex Event → ACP update formatting (`EventHandler`), approval option helpers, and `ReasoningAggregator` for thought text.
    - config_builder.rs — builds per-session `Config` instances, injects filesystem guidance, and produces MCP server configs (`prepare_fs_mcp_server_config`, `build_mcp_server`).
    - session_manager.rs — session state store (`SessionState`, `SessionManager`), conversation caching, capability tracking, notification helpers, and `apply_context_override`.
    - utils.rs — shared helpers for session modes/models, provider detection, tool formatting (`format_command_call`, `describe_mcp_tool`, `is_custom_provider`, etc.).
  - fs/
    - mod.rs, bridge.rs, mcp_server.rs — filesystem bridge runtime (`FsBridge::start`) and MCP server entry (`run_mcp_server`) communicating via `ACP_FS_BRIDGE_ADDR`/`ACP_FS_SESSION_ID`.
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
- Session mode/model helpers now live in `agent::utils` (`session_modes_for_config`, `available_modes`, `find_preset_by_mode_id`, `is_custom_provider`, etc.).
- `SessionManager` is available via `codex_acp::SessionManager` and can be accessed from `CodexAgent` using the `session_manager()` method; it exposes `current_mode()`, `is_read_only()`, `resolve_acp_session_id()`, `support_terminal()`, and `apply_context_override()`.
- Queue client-side interactions through `ClientOp` (`RequestPermission`, `ReadTextFile`, `WriteTextFile`) rather than calling transport code directly.
- Prefer the session manager and filesystem bridge abstractions for capability checks and FS requests.

## Custom Provider Support

### Authentication
The agent supports custom (non-builtin) model providers through a dedicated authentication flow:

- Builtin providers: `openai` (uses existing ChatGPT or API key auth)
- Custom providers: any other provider configured in `model_providers`

When a custom provider is configured:
- Initialize advertises a provider-specific auth method:
  - id: `{provider_id}` (from `config.model_provider_id`)
  - name: provider display name (`config.model_provider.name`)
- Authenticate supports `apikey`, `chatgpt`, and a custom provider branch. The custom branch currently matches the method id `"custom_provider"`; ensure clients line up with that identifier.

### Model Management
Model listing and switching are only available for custom providers:

- `new_session` and `load_session` return `models: Some(...)` only for custom providers.
- `set_session_model` requires both current and target models to be custom providers.
- `utils::available_models_from_profiles` filters out builtin provider models.

Model ID format: `{provider_id}@{model_name}` (e.g., `anthropic@claude-3`, `custom-llm@my-model`).

### Implementation Details
- `utils::is_custom_provider(provider_id)` determines if a provider is custom (`!matches!(provider_id, "openai")`).
- `utils::available_models_from_profiles(...)` builds model lists from profiles (custom-only) plus the active config.
- `utils::parse_and_validate_model(...)` validates requested model ids and returns provider/model/effort metadata.
- `core::new_session`/`core::load_session` include `models` only for custom providers and use `utils::current_model_id_from_config`.
- `core::set_session_model` parses, validates, and enforces custom→custom switching before applying context overrides.

## FS Bridge & MCP

- The FS MCP server (`codex-acp --acp-fs-mcp`) reads `ACP_FS_BRIDGE_ADDR` and `ACP_FS_SESSION_ID` to communicate with the local bridge.
- `FsBridge::start` boots the bridge and hands its address into session config via `config_builder::prepare_fs_mcp_server_config`.
- The bridge exposes read/write ops; large reads are paged (~1000 lines/50KB) and advertise pagination metadata.

## ACP Schema Types & Builder Patterns

The `agent-client-protocol` (ACP) and `codex_core` crates use `#[non_exhaustive]` structs and builder patterns extensively. This section documents the required construction patterns.

### ID Types (Newtype Wrappers)

All ID types have private inner fields and must be created with `::new(...)`:

```rust
// Correct
SessionId::new("session-123")
ModelId::new("anthropic@claude-3")
ToolCallId::new("call-456")
AuthMethodId::new("apikey")
TerminalId::new("term-789")
PermissionOptionId::new("allow-once")
SessionModeId::new("fast-apply")

// Wrong — will not compile
SessionId("session-123")
```

### Response & State Types

Use `.new(...)` constructors followed by builder methods:

```rust
// InitializeResponse
InitializeResponse::new(ProtocolVersion::V1, agent_info)
    .auth(auth_methods)
    .meta(meta)

// AgentCapabilities
AgentCapabilities::new()
    .modes(true)
    .models(true)

// AuthMethod
AuthMethod::new(AuthMethodId::new("apikey"), "API Key".to_string())

// NewSessionResponse / LoadSessionResponse
NewSessionResponse::new(session_id)
    .modes(session_modes)
    .models(session_models)
    .meta(meta)

// SessionModelState
SessionModelState::new(models_vec)
    .current(ModelId::new("provider@model"))

// SessionModeState / SessionMode
SessionModeState::new(modes_vec)
    .current(SessionModeId::new("default"))

SessionMode::new(SessionModeId::new("fast-apply"), "Fast Apply".to_string())
    .description("Quick edits without confirmation")

// ModelInfo
ModelInfo::new(ModelId::new("provider@model"), "Model Display Name".to_string())
```

### ToolCall & ToolCallUpdate

```rust
// Creating a new ToolCall
ToolCall::new(ToolCallId::new("call-id"), "tool_name".to_string())
    .kind(ToolCallKind::Function)
    .status(ToolCallStatus::Running)

// Creating a ToolCallUpdate
ToolCallUpdate::new(ToolCallId::new("call-id"))
    .fields(
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .output("result text".to_string())
    )

// ToolCallLocation
ToolCallLocation::new("file/path.rs".to_string())
    .line(42)
```

### Content Types

```rust
// ContentChunk
ContentChunk::new(content_string)

// Terminal (for ToolCallContent)
ToolCallContent::Terminal(Terminal::new(TerminalId::new("term-id")))

// Diff
Diff::new("file/path.rs".to_string(), "new content".to_string())
    .old_text("old content".to_string())
```

### Permission Types

```rust
// PermissionOption
PermissionOption::new(
    PermissionOptionId::new("allow-once"),
    "Allow Once".to_string()
)

// RequestPermissionRequest
RequestPermissionRequest::new(
    ToolCallId::new("call-id"),
    permission_options_vec
)

// Handling RequestPermissionOutcome (tuple variant)
match outcome {
    RequestPermissionOutcome::Selected(selected) => {
        // selected.option_id, selected.metadata, etc.
    }
    RequestPermissionOutcome::Cancelled => { /* ... */ }
    _ => { /* handle future variants */ }
}
```

### Plan Types

```rust
Plan::new(plan_entries_vec)

PlanEntry::new("Step description".to_string())
```

### MCP Server Variants

`McpServer` enum uses tuple variants with dedicated structs:

```rust
match server {
    McpServer::Http(http) => { /* http.url, http.headers, etc. */ }
    McpServer::Sse(sse) => { /* sse.url, sse.headers, etc. */ }
    McpServer::Stdio(stdio) => { /* stdio.cmd, stdio.args, stdio.env, etc. */ }
    _ => { /* handle future variants */ }
}
```

### Error Handling

Use `Error::data(...)` as a chainable builder (not `Error::with_data(...)`):

```rust
// Correct
Err(Error::invalid_params("message").data(serde_json::json!({"key": "value"})))

// Wrong — method removed
Err(Error::with_data(ErrorCode::InvalidParams, "message", data))
```

### ExtResponse for Extension Methods

```rust
use std::sync::Arc;
use serde_json::value::RawValue;

let raw = RawValue::from_string(serde_json::to_string(&payload)?)?;
ExtResponse::new(Arc::new(raw))
```

### Config Model Field

`codex_core::Config.model` is `Option<String>`:

```rust
// Reading model
let model_name = config.model.as_deref().unwrap_or("unknown");

// Format string usage
format!("Model: {}", config.model.clone().unwrap_or_default())
```

### Non-Exhaustive Enum Handling

Always add wildcard arms for `#[non_exhaustive]` enums:

```rust
match content_block {
    ContentBlock::Text(t) => { /* ... */ }
    ContentBlock::Thinking(t) => { /* ... */ }
    ContentBlock::ToolUse(t) => { /* ... */ }
    _ => { /* ignore or log unknown variants */ }
}
```

### AvailableCommand

```rust
AvailableCommand::new("/status".to_string())
    .description("Show current session status")
```

### SessionNotification

```rust
SessionNotification::new(message_string)
```

### ReadTextFileRequest / WriteTextFileRequest

```rust
// Read with optional line range and limit
ReadTextFileRequest::new(path_string)
    .line(start_line)
    .limit(max_lines)

// Write
WriteTextFileRequest::new(path_string, content_string)
```

## Migration Notes

When upgrading `agent-client-protocol` or `codex_core`:

1. **Check for `#[non_exhaustive]` additions** — struct literals will stop compiling; switch to builders.
2. **Check for newtype ID changes** — tuple construction `Type(val)` becomes `Type::new(val)`.
3. **Check enum variant shapes** — struct variants may become tuple variants or vice versa.
4. **Check renamed fields** — e.g., `id` → `tool_call_id`, `id` → `option_id`.
5. **Run `cargo build`** — address errors incrementally; the compiler will guide you.
6. **Run `cargo clippy -- -D warnings`** — catch any new lints from API changes.
