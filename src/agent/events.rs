use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_client_protocol::{
    Diff, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionUpdate, Terminal,
    TerminalId, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use codex_core::protocol::{FileChange, McpInvocation, ReviewDecision};
use codex_protocol::parse_command::ParsedCommand;
use serde_json::json;

use super::utils;

/// Arguments for "Exec Command End" update generation.
pub struct ExecEndArgs {
    pub call_id: String,
    pub exit_code: i32,
    pub aggregated_output: String,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub formatted_output: String,
}

/// Centralized helpers to translate Codex Event data into ACP updates and requests.
///
/// This module does not send updates itself; instead, it produces ACP model
/// structures (`SessionUpdate`, `RequestPermissionRequest`, etc.) that the
/// caller can pass to their transport layer. This makes it easier to unit test
/// the formatting logic and to keep the agent's event loop focused.
pub struct EventHandler {
    cwd: PathBuf,
    support_terminal: bool,
    permission_options: Arc<Vec<PermissionOption>>,
}

impl EventHandler {
    /// Create a new handler with the workspace `cwd` and whether the client supports terminals.
    pub fn new(cwd: PathBuf, support_terminal: bool) -> Self {
        Self {
            cwd,
            support_terminal,
            permission_options: default_permission_options(),
        }
    }

    /// Build a ToolCall update for "MCP Tool Call Begin".
    pub fn on_mcp_tool_call_begin(
        &self,
        call_id: &str,
        invocation: &McpInvocation,
    ) -> SessionUpdate {
        let (title, locations) = utils::describe_mcp_tool(invocation, &self.cwd);
        let tool = ToolCall::new(ToolCallId::new(call_id), title)
            .kind(ToolKind::Fetch)
            .status(ToolCallStatus::InProgress)
            .locations(locations)
            .raw_input(invocation.arguments.clone());
        SessionUpdate::ToolCall(tool)
    }

    /// Build a ToolCallUpdate for "MCP Tool Call End".
    pub fn on_mcp_tool_call_end(
        &self,
        call_id: &str,
        invocation: &McpInvocation,
        result: &serde_json::Value,
        success: bool,
    ) -> SessionUpdate {
        let status = if success {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        };
        let (title, locations) = utils::describe_mcp_tool(invocation, &self.cwd);
        let fields = ToolCallUpdateFields::new()
            .status(status)
            .title(title)
            .locations(if locations.is_empty() {
                None
            } else {
                Some(locations)
            })
            .raw_output(result.clone());
        let update = ToolCallUpdate::new(ToolCallId::new(call_id), fields);
        SessionUpdate::ToolCallUpdate(update)
    }

    /// Build a ToolCall for "Exec Command Begin".
    pub fn on_exec_command_begin(
        &self,
        call_id: &str,
        cwd: &Path,
        command: &[String],
        parsed_cmd: &[ParsedCommand],
    ) -> SessionUpdate {
        let utils::FormatCommandCall {
            title,
            locations,
            terminal_output,
            kind,
        } = utils::format_command_call(cwd, parsed_cmd);

        let (content, meta) = if self.support_terminal && terminal_output {
            let content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
                call_id,
            )))];
            let mut meta_map = serde_json::Map::new();
            meta_map.insert(
                "terminal_info".to_string(),
                json!({
                    "terminal_id": call_id,
                    "cwd": cwd
                }),
            );
            (content, Some(meta_map))
        } else {
            (vec![], None)
        };

        let mut tool = ToolCall::new(ToolCallId::new(call_id), title)
            .kind(kind)
            .status(ToolCallStatus::InProgress)
            .content(content)
            .locations(locations)
            .raw_input(json!({
                "command": command,
                "command_string": command.join(" "),
                "cwd": cwd
            }));
        if let Some(m) = meta {
            tool = tool.meta(m);
        }
        SessionUpdate::ToolCall(tool)
    }

    /// Arguments for "Exec Command End" update generation.
    /// Build a ToolCallUpdate for "Exec Command End".
    pub fn on_exec_command_end(&self, end: ExecEndArgs) -> SessionUpdate {
        let status = if end.exit_code == 0 {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        };

        let mut content: Vec<ToolCallContent> = Vec::new();
        if !end.aggregated_output.is_empty() {
            content.push(ToolCallContent::from(end.aggregated_output.clone()));
        } else if !end.stdout.is_empty() || !end.stderr.is_empty() {
            let merged = if !end.stderr.is_empty() {
                format!("{}\n{}", end.stdout, end.stderr)
            } else {
                end.stdout.clone()
            };
            if !merged.is_empty() {
                content.push(ToolCallContent::from(merged));
            }
        }

        let fields = ToolCallUpdateFields::new()
            .status(status)
            .content(if content.is_empty() {
                None
            } else {
                Some(content)
            })
            .raw_output(json!({
                "exit_code": end.exit_code,
                "duration_ms": end.duration_ms,
                "formatted_output": end.formatted_output,
            }));
        let update = ToolCallUpdate::new(ToolCallId::new(end.call_id), fields);

        SessionUpdate::ToolCallUpdate(update)
    }

    /// Build a permission request for an exec approval.
    pub fn on_exec_approval_request(
        &self,
        session_id: &SessionId,
        call_id: &str,
        cwd: &Path,
        parsed_cmd: &[ParsedCommand],
    ) -> RequestPermissionRequest {
        let utils::FormatCommandCall {
            title,
            locations,
            terminal_output: _,
            kind,
        } = utils::format_command_call(cwd, parsed_cmd);

        let fields = ToolCallUpdateFields::new()
            .kind(kind)
            .status(ToolCallStatus::Pending)
            .title(title)
            .locations(if locations.is_empty() {
                None
            } else {
                Some(locations)
            });
        let update = ToolCallUpdate::new(ToolCallId::new(call_id), fields);

        RequestPermissionRequest::new(
            session_id.clone(),
            update,
            self.permission_options.as_ref().clone(),
        )
    }

    // ---- Patch approval ----

    /// Build a permission request for "Apply Patch Approval Request".
    pub fn on_apply_patch_approval_request(
        &self,
        session_id: &SessionId,
        call_id: &str,
        changes: &[(String, FileChange)],
    ) -> RequestPermissionRequest {
        let mut contents: Vec<ToolCallContent> = Vec::new();
        for (path, change) in changes.iter() {
            match change {
                FileChange::Add { content } => {
                    contents.push(ToolCallContent::from(
                        Diff::new(PathBuf::from(path), content.clone()).old_text(None),
                    ));
                }
                FileChange::Delete { content } => {
                    contents.push(ToolCallContent::from(
                        Diff::new(PathBuf::from(path), "".to_string()).old_text(content.clone()),
                    ));
                }
                FileChange::Update { unified_diff, .. } => {
                    contents.push(ToolCallContent::from(
                        Diff::new(PathBuf::from(path), unified_diff.clone())
                            .old_text(unified_diff.clone()),
                    ));
                }
            }
        }

        let title = if changes.len() == 1 {
            "Apply changes".to_string()
        } else {
            format!("Edit {} files", changes.len())
        };

        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::Pending)
            .title(title)
            .content(if contents.is_empty() {
                None
            } else {
                Some(contents)
            });
        let update = ToolCallUpdate::new(ToolCallId::new(call_id), fields);

        RequestPermissionRequest::new(
            session_id.clone(),
            update,
            self.permission_options.as_ref().clone(),
        )
    }

    /// Build a ToolCallUpdate for "Patch Apply End".
    pub fn on_patch_apply_end(
        &self,
        call_id: &str,
        success: bool,
        raw_event_json: serde_json::Value,
    ) -> SessionUpdate {
        let fields = ToolCallUpdateFields::new()
            .status(if success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            })
            .raw_output(raw_event_json);
        let update = ToolCallUpdate::new(ToolCallId::new(call_id), fields);

        SessionUpdate::ToolCallUpdate(update)
    }
}

/// Map an approval response to the `ReviewDecision` used by Codex operations.
pub fn handle_response_outcome(resp: RequestPermissionResponse) -> ReviewDecision {
    match resp.outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            "approved" => ReviewDecision::Approved,
            "approved-for-session" => ReviewDecision::ApprovedForSession,
            _ => ReviewDecision::Abort,
        },
        RequestPermissionOutcome::Cancelled => ReviewDecision::Abort,
        // Handle any future RequestPermissionOutcome variants
        _ => ReviewDecision::Abort,
    }
}

/// Build the default permission options set for approval requests.
pub fn default_permission_options() -> Arc<Vec<PermissionOption>> {
    Arc::new(vec![
        PermissionOption::new(
            "approved-for-session",
            "Approved Always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("approved", "Approved", PermissionOptionKind::AllowOnce),
        PermissionOption::new("abort", "Reject", PermissionOptionKind::RejectOnce),
    ])
}

/// Aggregates reasoning deltas and sections to produce a compact text output.
///
/// This mirrors the logic used by the agent to collate streaming reasoning.
/// It can be used to decouple reasoning accumulation from the main event loop.
pub struct ReasoningAggregator {
    sections: Vec<String>,
    current: String,
}

impl ReasoningAggregator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
            current: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.sections.clear();
        self.current.clear();
    }

    pub fn append_delta(&mut self, delta: &str) {
        self.current.push_str(delta);
    }

    pub fn section_break(&mut self) {
        if !self.current.is_empty() {
            let chunk = std::mem::take(&mut self.current);
            self.sections.push(chunk);
        }
    }

    /// Returns combined text with double newlines between sections, trimming trailing whitespace.
    pub fn take_text(&mut self) -> Option<String> {
        let mut combined = String::new();
        let mut first = true;

        for section in self.sections.drain(..) {
            if section.trim().is_empty() {
                continue;
            }
            if !first {
                combined.push_str("\n\n");
            }
            combined.push_str(section.trim_end());
            first = false;
        }

        if !self.current.trim().is_empty() {
            if !first {
                combined.push_str("\n\n");
            }
            combined.push_str(self.current.trim_end());
        }

        self.current.clear();

        if combined.is_empty() {
            None
        } else {
            Some(combined)
        }
    }

    /// Given a final reasoning text (if any), choose the longer, non-empty variant
    /// between the aggregated text and the final text.
    pub fn choose_final_text(&mut self, final_text: Option<String>) -> Option<String> {
        let aggregated = self.take_text();
        match (aggregated, final_text) {
            (Some(agg), Some(final_text)) => {
                if final_text.trim().len() > agg.trim().len() {
                    Some(final_text)
                } else {
                    Some(agg)
                }
            }
            (Some(agg), None) => Some(agg),
            (None, Some(final_text)) => Some(final_text),
            (None, None) => None,
        }
    }
}
