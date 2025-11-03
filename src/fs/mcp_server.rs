use std::{
    collections::HashMap,
    env,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use diffy::{PatchFormatter, create_patch};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        AnnotateAble, CallToolResult, Content, Implementation, Meta, ProtocolVersion, RawContent,
        RawTextContent, ServerCapabilities, ServerInfo,
    },
    service, tool, tool_handler, tool_router,
    transport::io,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::Mutex,
    time::{Duration, timeout},
};
use tracing::info;

use super::bridge;
use crate::init_from_env;

const DEFAULT_READ_LINE_LIMIT: u32 = 1000;
const MAX_READ_BYTES: usize = 50 * 1024;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineRange {
    start: u32,
    end: u32,
}

#[derive(Debug)]
struct ReadSnippet {
    text: String,
    lines_returned: u32,
    end_line: u32,
    truncated_by_line_limit: bool,
    truncated_by_bytes: bool,
    additional_lines_available: bool,
    bytes_returned: usize,
}

pub async fn run() -> Result<()> {
    let _logging = init_from_env()?;
    // Capture required env to talk to our local bridge and session.
    let bridge_addr = env::var("ACP_FS_BRIDGE_ADDR")
        .context("ACP_FS_BRIDGE_ADDR environment variable is required")?;
    let session_id = env::var("ACP_FS_SESSION_ID")
        .context("ACP_FS_SESSION_ID environment variable is required")?;

    // Build an rmcp server over stdio with our tools.
    let server = FsTools::new(bridge_addr, session_id);
    let transport = io::stdio();
    // Serve and wait until the client closes the connection.
    let running = service::serve_server(server, transport).await?;
    let _ = running.waiting().await; // ignore quit reason
    Ok(())
}

// In-memory staging of edits to allow applying multi-step changes coherently.
#[derive(Default, Clone)]
struct StagedEdits {
    entries: Arc<Mutex<HashMap<String, StagedFile>>>,
}

#[derive(Clone)]
struct StagedFile {
    content: String,
}

impl StagedEdits {
    async fn stage(&self, path: String, content: String) {
        let mut map = self.entries.lock().await;
        map.insert(path, StagedFile { content });
    }
    async fn get(&self, path: &str) -> Option<StagedFile> {
        let map = self.entries.lock().await;
        map.get(path).cloned()
    }
}

#[derive(Clone)]
struct FsTools {
    bridge_addr: String,
    session_id: String,
    staged_edits: StagedEdits,
    tool_router: ToolRouter<Self>,
}

impl FsTools {
    fn new(bridge_addr: String, session_id: String) -> Self {
        Self {
            bridge_addr,
            session_id,
            staged_edits: Default::default(),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl FsTools {
    /// Read workspace files via ACP bridge (paged to ~1000 lines/50KB; use line/limit to continue).
    #[tool(
        description = "Read workspace files via ACP bridge (paged to ~1000 lines/50KB; use line/limit to continue)."
    )]
    async fn read_text_file(
        &self,
        Parameters(ReadTextFileArgs { path, line, limit }): Parameters<ReadTextFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let start_line = line.unwrap_or(1).max(1);
        let requested_limit = limit
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_READ_LINE_LIMIT);
        let bridge_limit = requested_limit.saturating_add(1);
        let response = perform_bridge_request(
            &self.bridge_addr,
            &self.session_id,
            bridge::BridgeOp::Read,
            &path,
            line,
            Some(bridge_limit),
            None,
        )
        .await
        .map_err(|e| {
            McpError::internal_error("bridge read failed", Some(json!({"reason": e.to_string()})))
        })?;

        let mut snippet =
            prepare_read_snippet(&response, start_line, requested_limit, MAX_READ_BYTES);

        if let Some(hint) =
            build_file_read_hint(&snippet, start_line, requested_limit, MAX_READ_BYTES)
        {
            if !snippet.text.is_empty() {
                snippet.text.push_str("\n\n");
            }
            snippet.text.push_str(&hint);
        }

        let ReadSnippet {
            text,
            lines_returned,
            end_line,
            truncated_by_line_limit,
            truncated_by_bytes,
            additional_lines_available,
            bytes_returned,
        } = snippet;

        let truncated = truncated_by_line_limit || truncated_by_bytes || additional_lines_available;
        let mut meta = json!({
            "path": path,
            "start_line": start_line,
            "end_line": end_line,
            "lines_returned": lines_returned,
            "line_limit": requested_limit,
            "bytes_returned": bytes_returned,
            "truncated": truncated,
            "truncated_by_line_limit": truncated_by_line_limit,
            "truncated_by_bytes": truncated_by_bytes,
            "additional_lines_available": additional_lines_available,
        });

        if truncated && let Some(obj) = meta.as_object_mut() {
            obj.insert("next_line".to_string(), json!(end_line.saturating_add(1)));
        }

        if truncated_by_bytes && let Some(obj) = meta.as_object_mut() {
            obj.insert("max_bytes".to_string(), json!(MAX_READ_BYTES));
        }

        let mut meta_obj = Meta::new();
        meta_obj.insert("codex_fs_read".to_string(), meta);
        let content = RawContent::Text(RawTextContent {
            text,
            meta: Some(meta_obj),
        })
        .no_annotation();
        Ok(CallToolResult::success(vec![content]))
    }

    /// Write workspace files via ACP bridge.
    #[tool(description = "Write workspace files via ACP bridge.")]
    async fn write_text_file(
        &self,
        Parameters(WriteTextFileArgs { path, content }): Parameters<WriteTextFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut final_content = content;
        let mut staged_applied = false;
        if let Some(entry) = self
            .staged_edits
            .get(&path)
            .await
            .filter(|entry| final_content.is_empty() || final_content == entry.content)
        {
            final_content = entry.content.clone();
            staged_applied = true;
        }

        perform_bridge_request(
            &self.bridge_addr,
            &self.session_id,
            bridge::BridgeOp::Write,
            &path,
            None,
            None,
            Some(final_content.clone()),
        )
        .await
        .map_err(|e| {
            McpError::internal_error(
                "bridge write failed",
                Some(json!({"reason": e.to_string()})),
            )
        })?;

        self.staged_edits.stage(path.clone(), final_content).await;

        let response_text = if staged_applied {
            "write completed (applied staged edits)"
        } else {
            "write completed"
        };
        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Apply a focused replacement in a file and persist the result.
    #[tool(description = "Apply a focused replacement in a file and persist the result.")]
    async fn edit_text_file(
        &self,
        Parameters(EditTextFileArgs {
            path,
            old_string,
            new_string,
        }): Parameters<EditTextFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let instructions = vec![EditInstruction {
            old_text: old_string,
            new_text: new_string,
            replace_all: false,
        }];
        stage_edits(
            &self.bridge_addr,
            &self.session_id,
            &path,
            instructions,
            &self.staged_edits,
        )
        .await
    }

    /// Apply multiple sequential replacements in a file and persist the result.
    #[tool(
        description = "Apply multiple sequential replacements in a file and persist the result."
    )]
    async fn multi_edit_text_file(
        &self,
        Parameters(MultiEditTextFileArgs { path, edits }): Parameters<MultiEditTextFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        if edits.is_empty() {
            return Err(McpError::invalid_params(
                "edits array must not be empty",
                None,
            ));
        }
        let instructions = edits
            .into_iter()
            .map(|edit| EditInstruction {
                old_text: edit.old_string,
                new_text: edit.new_string,
                replace_all: edit.replace_all,
            })
            .collect::<Vec<_>>();

        stage_edits(
            &self.bridge_addr,
            &self.session_id,
            &path,
            instructions,
            &self.staged_edits,
        )
        .await
    }
}

#[tool_handler]
impl ServerHandler for FsTools {
    fn get_info(&self) -> ServerInfo {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: caps,
            server_info: Implementation {
                name: "codex-acp-fs".to_string(),
                title: Some("Codex ACP Filesystem".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
            instructions: None,
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema, Clone)]
struct EditEntry {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct ReadTextFileArgs {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct WriteTextFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct EditTextFileArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct MultiEditTextFileArgs {
    path: String,
    edits: Vec<EditEntry>,
}

struct EditInstruction {
    old_text: String,
    new_text: String,
    replace_all: bool,
}

async fn stage_edits(
    bridge_addr: &str,
    session_id: &str,
    path: &str,
    instructions: Vec<EditInstruction>,
    staged_edits: &StagedEdits,
) -> Result<CallToolResult, McpError> {
    let base_content = if let Some(entry) = staged_edits.get(path).await {
        entry.content.clone()
    } else {
        match perform_bridge_request(
            bridge_addr,
            session_id,
            bridge::BridgeOp::Read,
            path,
            None,
            None,
            None,
        )
        .await
        {
            Ok(content) => content,
            Err(err) => {
                let message = err.to_string();
                if is_missing_file_error(&message) {
                    String::new()
                } else {
                    return Err(McpError::internal_error(
                        "failed to read current file content",
                        Some(json!({"reason": err.to_string()})),
                    ));
                }
            }
        }
    };

    let new_content = apply_edits(&base_content, &instructions)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

    if new_content == base_content {
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "No changes detected for {path}."
        ))]));
    }

    let diff_text = format_diff_for_path(path, &base_content, &new_content);

    let write_content = new_content.clone();
    let staged_bytes = write_content.len();
    let _ = perform_bridge_request(
        bridge_addr,
        session_id,
        bridge::BridgeOp::Write,
        path,
        None,
        None,
        Some(write_content.clone()),
    )
    .await
    .map_err(|e| {
        McpError::internal_error(
            "bridge write failed",
            Some(json!({"reason": e.to_string()})),
        )
    })?;

    staged_edits.stage(path.to_string(), write_content).await;
    info!(file = %path, bytes = staged_bytes, "Staged edits committed");

    let (new_ranges, old_ranges) = parse_diff_line_ranges(&diff_text);
    let diff_meta = json!({
        "path": path,
        "new_ranges": line_ranges_to_json(&new_ranges),
        "old_ranges": line_ranges_to_json(&old_ranges),
    });

    let mut meta_obj = Meta::new();
    meta_obj.insert("codex_fs_diff".to_string(), diff_meta);
    let diff_content = RawContent::Text(RawTextContent {
        text: diff_text,
        meta: Some(meta_obj),
    })
    .no_annotation();
    Ok(CallToolResult::success(vec![
        diff_content,
        Content::text(format!("Write completed for {path}.")),
    ]))
}

fn apply_edits(base: &str, edits: &[EditInstruction]) -> Result<String> {
    let mut content = base.to_string();
    for edit in edits {
        if edit.old_text.is_empty() {
            return Err(anyhow!(
                "the provided `old_string` is empty. No edits were applied."
            ));
        }

        if edit.replace_all {
            let replaced = content.replace(&edit.old_text, &edit.new_text);
            if replaced == content {
                return Err(anyhow!(
                    "The provided `old_string` does not appear in the file. No edits were applied."
                ));
            }
            content = replaced;
        } else {
            let Some(index) = content.find(&edit.old_text) else {
                return Err(anyhow!(
                    "The provided `old_string` does not appear in the file. No edits were applied."
                ));
            };
            let end = index + edit.old_text.len();
            content.replace_range(index..end, &edit.new_text);
        }
    }
    Ok(content)
}

fn format_diff_for_path(path: &str, before: &str, after: &str) -> String {
    let patch = create_patch(before, after);
    let formatter = PatchFormatter::new();
    let diff_body = formatter.fmt_patch(&patch).to_string();
    if diff_body.trim().is_empty() {
        format!("No textual differences for {path}.")
    } else {
        format!("--- {path}\n+++ {path}\n{diff_body}")
    }
}

fn is_missing_file_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no such file") || lower.contains("not found")
}

fn prepare_read_snippet(
    raw: &str,
    start_line: u32,
    requested_limit: u32,
    max_bytes: usize,
) -> ReadSnippet {
    if raw.is_empty() || requested_limit == 0 {
        return ReadSnippet {
            text: String::new(),
            lines_returned: 0,
            end_line: start_line.saturating_sub(1),
            truncated_by_line_limit: false,
            truncated_by_bytes: false,
            additional_lines_available: false,
            bytes_returned: 0,
        };
    }

    let mut text = String::new();
    let mut lines_taken: u32 = 0;
    let mut truncated_by_line_limit = false;
    let mut truncated_by_bytes = false;
    let mut bytes_used: usize = 0;

    for segment in raw.split_inclusive('\n') {
        if lines_taken >= requested_limit {
            truncated_by_line_limit = true;
            break;
        }

        let segment_bytes = segment.len();
        if bytes_used + segment_bytes > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes_used);
            if remaining > 0 {
                let cut = truncate_to_char_boundary(segment, remaining);
                text.push_str(&segment[..cut]);
                bytes_used += cut;
            }
            truncated_by_bytes = true;
            lines_taken += 1;
            break;
        }

        text.push_str(segment);
        bytes_used += segment_bytes;
        lines_taken += 1;
    }

    let raw_line_count = if raw.is_empty() {
        0
    } else {
        raw.lines().count() as u32
    };

    let additional_lines_available =
        truncated_by_bytes || raw_line_count > lines_taken || bytes_used < raw.len();

    let end_line = if lines_taken == 0 {
        start_line.saturating_sub(1)
    } else {
        start_line.saturating_add(lines_taken.saturating_sub(1))
    };

    ReadSnippet {
        text,
        lines_returned: lines_taken,
        end_line,
        truncated_by_line_limit,
        truncated_by_bytes,
        additional_lines_available,
        bytes_returned: bytes_used,
    }
}

fn truncate_to_char_boundary(segment: &str, max_bytes: usize) -> usize {
    if max_bytes >= segment.len() {
        return segment.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !segment.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn build_file_read_hint(
    snippet: &ReadSnippet,
    start_line: u32,
    requested_limit: u32,
    max_bytes: usize,
) -> Option<String> {
    if !(snippet.truncated_by_line_limit
        || snippet.truncated_by_bytes
        || snippet.additional_lines_available)
    {
        return None;
    }

    let effective_end = if snippet.lines_returned == 0 {
        start_line
    } else {
        snippet.end_line
    };
    let mut description = format!("Read lines {}-{}", start_line, effective_end);

    if snippet.truncated_by_bytes {
        description.push_str(&format!(" (hit {} byte cap)", max_bytes));
    } else if snippet.truncated_by_line_limit || snippet.additional_lines_available {
        description.push_str(&format!(" (showing up to {} lines)", requested_limit));
    }

    let mut hint = format!("<file-read-info>{}", description);
    let next_line = snippet.end_line.saturating_add(1).max(start_line);
    hint.push_str(&format!(
        " Continue with line={} limit={}.",
        next_line, requested_limit
    ));
    hint.push_str("</file-read-info>");

    Some(hint)
}

fn parse_diff_line_ranges(diff_text: &str) -> (Vec<LineRange>, Vec<LineRange>) {
    let mut new_ranges = Vec::new();
    let mut old_ranges = Vec::new();

    for line in diff_text.lines() {
        let Some(stripped) = line.strip_prefix("@@") else {
            continue;
        };
        let Some((body, _)) = stripped.split_once("@@") else {
            continue;
        };
        for token in body.split_whitespace() {
            if let Some(range) = token.strip_prefix('+').and_then(parse_range_token) {
                new_ranges.push(range);
            } else if let Some(range) = token.strip_prefix('-').and_then(parse_range_token) {
                old_ranges.push(range);
            }
        }
    }

    (new_ranges, old_ranges)
}

fn parse_range_token(token: &str) -> Option<LineRange> {
    if token.is_empty() {
        return None;
    }

    let mut parts = token.split(',');
    let start = parts.next()?.parse::<i64>().ok()?;
    let count = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);

    if count <= 0 {
        return None;
    }

    let start = start.max(1) as u32;
    let end = start.saturating_add((count as u32).saturating_sub(1));
    Some(LineRange { start, end })
}

fn line_ranges_to_json(ranges: &[LineRange]) -> Vec<serde_json::Value> {
    ranges
        .iter()
        .map(|range| {
            json!({
                "start": range.start,
                "end": range.end,
            })
        })
        .collect()
}

async fn perform_bridge_request(
    bridge_addr: &str,
    session_id: &str,
    op: bridge::BridgeOp,
    path: &str,
    line: Option<u32>,
    limit: Option<u32>,
    content: Option<String>,
) -> Result<String> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut stream = TcpStream::connect(bridge_addr)
        .await
        .with_context(|| format!("failed to connect to bridge at {bridge_addr}"))?;
    let (reader_half, mut writer_half) = stream.split();
    let mut reader = BufReader::new(reader_half).lines();

    let payload = serde_json::to_string(&json!({
        "id": request_id,
        "session_id": session_id,
        "op": match op {
            bridge::BridgeOp::Read => "read",
            bridge::BridgeOp::Write => "write",
        },
        "path": path,
        "line": line,
        "limit": limit,
        "content": content,
    }))?;

    writer_half.write_all(payload.as_bytes()).await?;
    writer_half.write_all(b"\n").await?;
    writer_half.flush().await?;

    let line = timeout(Duration::from_secs(5), reader.next_line())
        .await
        .map_err(|_| anyhow!("bridge request timed out"))??
        .ok_or_else(|| anyhow!("bridge closed connection"))?;

    let response: serde_json::Value = serde_json::from_str(&line)?;
    let success = response
        .get("success")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if success {
        Ok(response
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string())
    } else {
        let message = response
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("bridge error");
        Err(anyhow!(message.to_string()))
    }
}
