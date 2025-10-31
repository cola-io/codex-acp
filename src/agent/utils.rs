use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};

use agent_client_protocol::{
    ModelId, ModelInfo, SessionMode, SessionModeId, SessionModeState, ToolCallLocation, ToolKind,
};
use codex_common::approval_presets::{ApprovalPreset, builtin_approval_presets};
use codex_core::{
    config::{Config, profile::ConfigProfile},
    protocol::McpInvocation,
};
use codex_protocol::{config_types::ReasoningEffort, parse_command::ParsedCommand};

/// All available approval presets used to derive ACP session modes.
static APPROVAL_PRESETS: LazyLock<Vec<ApprovalPreset>> = LazyLock::new(builtin_approval_presets);

/// Formatted summary for a command/tool call used by ACP updates.
#[derive(Clone, Debug)]
pub struct FormatCommandCall {
    pub title: String,
    pub terminal_output: bool,
    pub locations: Vec<ToolCallLocation>,
    pub kind: ToolKind,
}

/// Metadata describing an FS tool call, including a display path and an
/// optional source location line for deep-linking in clients.
#[derive(Clone, Debug)]
pub struct FsToolMetadata {
    pub display_path: String,
    pub location_path: PathBuf,
    pub line: Option<u32>,
}

/// Format a tool/command call for display in the client, summarizing a
/// sequence of parsed commands into a single title, the kind, locations,
/// and whether terminal output should be rendered.
pub fn format_command_call(cwd: &Path, parsed_cmd: &[ParsedCommand]) -> FormatCommandCall {
    let mut titles = Vec::new();
    let mut locations = Vec::new();
    let mut terminal_output = false;
    let mut kind = ToolKind::Execute;

    for cmd in parsed_cmd {
        let mut cmd_path: Option<PathBuf> = None;
        match cmd {
            ParsedCommand::Read { cmd: _, name, path } => {
                titles.push(format!("Read {name}"));
                cmd_path = Some(path.clone());
                kind = ToolKind::Read;
            }
            ParsedCommand::ListFiles { cmd: _, path } => {
                let dir = if let Some(path) = path.as_ref() {
                    cwd.join(path)
                } else {
                    cwd.to_path_buf()
                };
                titles.push(format!("List {}", dir.display()));
                cmd_path = path.as_ref().map(PathBuf::from);
                kind = ToolKind::Search;
            }
            ParsedCommand::Search { cmd, query, path } => {
                let label = match (query, path.as_ref()) {
                    (Some(query), Some(path)) => format!("Search {query} in {path}"),
                    (Some(query), None) => format!("Search {query}"),
                    _ => format!("Search {}", cmd),
                };
                titles.push(label);
                cmd_path = path.as_ref().map(PathBuf::from);
                kind = ToolKind::Search;
            }
            ParsedCommand::Unknown { cmd } => {
                titles.push(format!("Run {cmd}"));
                terminal_output = true;
            }
        }

        if let Some(path) = cmd_path {
            locations.push(ToolCallLocation {
                path: if path.is_relative() {
                    cwd.join(&path)
                } else {
                    path
                },
                line: None,
                meta: None,
            });
        }
    }

    FormatCommandCall {
        title: titles.join(", "),
        terminal_output,
        locations,
        kind,
    }
}

/// Return a user-friendly display path for a raw path string.
/// If `raw_path` is within `cwd`, return a relative path; otherwise, fall back
/// to the file name or the original raw string.
pub fn display_fs_path(cwd: &Path, raw_path: &str) -> String {
    let path = Path::new(raw_path);
    if let Ok(relative) = path.strip_prefix(cwd) {
        let rel_display = relative.display().to_string();
        if !rel_display.is_empty() {
            return rel_display;
        }
    }

    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw_path.to_string())
}

/// Extract FS tool metadata from an MCP invocation, when applicable.
/// Only tools from the "acp_fs" server and supported tool names are considered.
pub fn fs_tool_metadata(invocation: &McpInvocation, cwd: &Path) -> Option<FsToolMetadata> {
    if invocation.server != "acp_fs" {
        return None;
    }

    match invocation.tool.as_str() {
        "read_text_file" | "write_text_file" | "edit_text_file" => {}
        _ => return None,
    }

    let args = invocation.arguments.as_ref()?.as_object()?;
    let path = args.get("path")?.as_str()?.to_string();
    let line = args
        .get("line")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32);
    let display_path = display_fs_path(cwd, &path);
    let location_path = PathBuf::from(&path);

    Some(FsToolMetadata {
        display_path,
        location_path,
        line,
    })
}

/// Describe an MCP tool call for ACP by creating a human-friendly title and
/// mapping to zero or more `ToolCallLocation`s. When the invocation is an
/// FS tool, the title includes the display path and a single location entry.
pub fn describe_mcp_tool(
    invocation: &McpInvocation,
    cwd: &Path,
) -> (String, Vec<ToolCallLocation>) {
    if let Some(metadata) = fs_tool_metadata(invocation, cwd) {
        let location = ToolCallLocation {
            path: metadata.location_path,
            line: metadata.line,
            meta: None,
        };
        (
            format!(
                "{}.{} ({})",
                invocation.server, invocation.tool, metadata.display_path
            ),
            vec![location],
        )
    } else {
        (
            format!("{}.{}", invocation.server, invocation.tool),
            Vec::new(),
        )
    }
}

/// Build the ACP `SessionModeState` (current + available) from a Codex `Config`.
pub fn session_modes_for_config(config: &Config) -> Option<SessionModeState> {
    let current_mode_id = current_mode_id_for_config(config)?;
    Some(SessionModeState {
        current_mode_id,
        available_modes: available_modes(),
        meta: None,
    })
}

/// Return the current ACP session mode id by matching the preset for the provided config.
pub fn current_mode_id_for_config(config: &Config) -> Option<SessionModeId> {
    APPROVAL_PRESETS
        .iter()
        .find(|preset| {
            preset.approval == config.approval_policy && preset.sandbox == config.sandbox_policy
        })
        .map(|preset| SessionModeId(preset.id.into()))
}

/// Find an approval preset by ACP session mode id.
pub fn find_preset_by_mode_id(mode_id: &SessionModeId) -> Option<&'static ApprovalPreset> {
    let target = mode_id.0.as_ref();
    APPROVAL_PRESETS.iter().find(|preset| preset.id == target)
}

/// Whether a mode id corresponds to a read-only mode.
pub fn is_read_only_mode(mode_id: &SessionModeId) -> bool {
    mode_id.0.as_ref() == "read-only"
}

/// Available modes derived from approval presets.
pub fn available_modes() -> Vec<SessionMode> {
    APPROVAL_PRESETS
        .iter()
        .map(|preset| SessionMode {
            id: SessionModeId(preset.id.into()),
            name: preset.label.to_string(),
            description: if preset.description.is_empty() {
                None
            } else {
                Some(preset.description.to_string())
            },
            meta: None,
        })
        .collect()
}

/// Check if a provider is a custom (non-builtin) provider.
pub fn is_custom_provider(provider_id: &str) -> bool {
    !matches!(provider_id, "openai")
}

/// Return the current model ID from config.
pub fn current_model_id_from_config(config: &Config) -> ModelId {
    ModelId(format!("{}@{}", config.model_provider_id, config.model).into())
}

/// Build a `ModelInfo` for display to the client.
fn build_model_info(config: &Config, provider_id: &str, model_name: &str) -> Option<ModelInfo> {
    let provider_info = config.model_providers.get(provider_id)?;
    let model_id = format!("{}@{}", provider_id, model_name);

    Some(ModelInfo {
        model_id: ModelId(model_id.into()),
        name: format!("{}@{}", provider_info.name, model_name),
        description: Some(format!(
            "Provider: {}, Model: {}",
            provider_info.name, model_name
        )),
        meta: None,
    })
}

/// Return the list of ACP `ModelInfo` entries derived from profiles (custom-only).
pub fn available_models_from_profiles(
    config: &Config,
    profiles: &HashMap<String, ConfigProfile>,
) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    // Add the current model from config first (only if it's a custom provider)
    if is_custom_provider(&config.model_provider_id)
        && let Some(model_info) = build_model_info(config, &config.model_provider_id, &config.model)
    {
        seen.insert(format!("{}@{}", &config.model_provider_id, &config.model));
        models.push(model_info);
    }

    // Extract unique model combinations from profiles (only custom providers)
    // Collect candidates first to allow deterministic sorting.
    let mut candidates = Vec::new();
    for profile in profiles.values() {
        if let (Some(model_name), Some(provider_id)) = (&profile.model, &profile.model_provider) {
            // Skip builtin providers
            if !is_custom_provider(provider_id) {
                continue;
            }

            candidates.push((
                provider_id.clone(),
                (
                    provider_id.clone(),
                    model_name.clone(),
                    profile.model_reasoning_effort,
                ),
            ));
        }
    }

    // Sort by provider id then model name for stable output.
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.1.cmp(&b.1.1)));

    for (_provider, (provider_id, model_name, _effort)) in candidates {
        let model_id = format!("{}@{}", provider_id, model_name);
        if seen.contains(&model_id) {
            continue;
        }
        if let Some(model_info) = build_model_info(config, &provider_id, &model_name) {
            seen.insert(model_id);
            models.push(model_info);
        }
    }

    models
}

/// Parse and validate a model id and return components (provider, model, effort).
pub fn parse_and_validate_model(
    config: &Config,
    profiles: &HashMap<String, ConfigProfile>,
    model_id: &ModelId,
) -> Option<(String, String, Option<ReasoningEffort>)> {
    let id_str = model_id.0.as_ref();
    let (provider_id, model_name) = id_str
        .split_once('@')
        .map(|(p, m)| (p.to_string(), m.to_string()))?;

    // Validate that the provider exists
    if !config.model_providers.contains_key(&provider_id) {
        return None;
    }

    // Check if this is the current config model
    if provider_id == config.model_provider_id && model_name == config.model {
        return Some((provider_id, model_name, config.model_reasoning_effort));
    }

    // Search in profiles for matching provider@model combination
    for profile in profiles.values() {
        if profile.model.as_ref() == Some(&model_name)
            && profile.model_provider.as_ref() == Some(&provider_id)
        {
            return Some((provider_id, model_name, profile.model_reasoning_effort));
        }
    }

    None
}
