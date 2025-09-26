use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use agent_client_protocol::{self as acp, Agent, Error, V1};
use codex_core::{
    AuthManager, CodexConversation, ConversationManager, NewConversation,
    config::Config as CodexConfig,
    config_types::McpServerConfig,
    protocol::{
        AskForApproval, EventMsg, InputItem, McpInvocation, Op, ReviewDecision, SandboxPolicy,
        TokenUsage,
    },
};
use codex_protocol::mcp_protocol::AuthMode;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, oneshot::Sender};
use tokio::task;
use tracing::{info, warn};

use crate::fs::FsBridge;

mod commands;
// Placeholder for per-session state. Holds the Codex conversation
// handle, its id (for status/reporting), and bookkeeping for streaming.
#[derive(Clone)]
struct SessionState {
    #[allow(dead_code)]
    created: SystemTime,
    // Conversation id string for display/logging purposes.
    conversation_id: String,
    conversation: Option<Arc<CodexConversation>>,
    current_approval: AskForApproval,
    current_sandbox: SandboxPolicy,
    current_mode: acp::SessionModeId,
    token_usage: Option<TokenUsage>,
    reasoning_sections: Vec<String>,
    current_reasoning_chunk: String,
}

#[derive(Clone)]
pub struct SessionModeLookup {
    inner: Rc<RefCell<HashMap<String, SessionState>>>,
}

impl SessionModeLookup {
    pub fn current_mode(&self, session_id: &acp::SessionId) -> Option<acp::SessionModeId> {
        self.inner
            .borrow()
            .get(session_id.0.as_ref())
            .map(|state| state.current_mode.clone())
    }

    pub fn is_read_only(&self, session_id: &acp::SessionId) -> bool {
        self.current_mode(session_id)
            .map(|mode| mode.0.as_ref() == "read-only")
            .unwrap_or(false)
    }
}

pub struct CodexAgent {
    session_update_tx: mpsc::UnboundedSender<(acp::SessionNotification, Sender<()>)>,
    sessions: Rc<RefCell<HashMap<String, SessionState>>>,
    config: CodexConfig,
    conversation_manager: ConversationManager,
    auth_manager: Arc<RwLock<Arc<AuthManager>>>,
    available_commands: Vec<acp::AvailableCommand>,
    client_tx: mpsc::UnboundedSender<ClientOp>,
    client_capabilities: RefCell<acp::ClientCapabilities>,
    next_session_id: RefCell<u64>,
    fs_bridge: Option<Arc<FsBridge>>,
}

impl CodexAgent {
    fn prepare_fs_mcp_server_config(
        &self,
        session_id: &str,
        bridge: &FsBridge,
    ) -> Result<McpServerConfig, Error> {
        let exe_path = std::env::current_exe().map_err(|err| {
            Error::internal_error().with_data(format!("failed to locate agent binary: {err}"))
        })?;

        let mut env = HashMap::new();
        env.insert(
            "ACP_FS_BRIDGE_ADDR".to_string(),
            bridge.address().to_string(),
        );
        env.insert("ACP_FS_SESSION_ID".to_string(), session_id.to_string());

        Ok(McpServerConfig {
            command: exe_path.to_string_lossy().into_owned(),
            args: vec!["--acp-fs-mcp".to_string()],
            env: Some(env),
            startup_timeout_sec: Some(Duration::from_secs(5)),
            tool_timeout_sec: Some(Duration::from_secs(30)),
        })
    }

    pub fn with_config(
        session_update_tx: mpsc::UnboundedSender<(acp::SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
        config: CodexConfig,
        fs_bridge: Option<Arc<FsBridge>>,
    ) -> Self {
        let auth = AuthManager::shared(config.codex_home.clone());
        let conversation_manager = ConversationManager::new(auth.clone());

        Self {
            session_update_tx,
            sessions: Rc::new(RefCell::new(HashMap::new())),
            config,
            conversation_manager,
            auth_manager: Arc::new(RwLock::new(auth)),
            available_commands: commands::built_in_commands(),
            client_tx,
            client_capabilities: RefCell::new(Default::default()),
            next_session_id: RefCell::new(1),
            fs_bridge,
        }
    }

    pub fn session_mode_lookup(&self) -> SessionModeLookup {
        SessionModeLookup {
            inner: self.sessions.clone(),
        }
    }

    pub fn send_message_chunk(
        &self,
        session_id: &acp::SessionId,
        content: acp::ContentBlock,
        tx: Sender<()>,
    ) -> Result<(), Error> {
        self.session_update_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::AgentMessageChunk { content },
                    meta: None,
                },
                tx,
            ))
            .map_err(Error::into_internal_error)?;
        Ok(())
    }

    pub fn send_thought_chunk(
        &self,
        session_id: &acp::SessionId,
        content: acp::ContentBlock,
        tx: Sender<()>,
    ) -> Result<(), Error> {
        self.session_update_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::AgentThoughtChunk { content },
                    meta: None,
                },
                tx,
            ))
            .map_err(Error::into_internal_error)?;
        Ok(())
    }

    fn handle_response_outcome(&self, resp: acp::RequestPermissionResponse) -> ReviewDecision {
        match resp.outcome {
            acp::RequestPermissionOutcome::Selected { option_id } => {
                if option_id.0.as_ref() == "approve" {
                    ReviewDecision::Approved
                } else if option_id.0.as_ref() == "approve_for_session" {
                    ReviewDecision::ApprovedForSession
                } else {
                    ReviewDecision::Denied
                }
            }
            acp::RequestPermissionOutcome::Cancelled => ReviewDecision::Abort,
        }
    }

    fn with_session_state_mut<R, F>(&self, session_id: &acp::SessionId, f: F) -> Option<R>
    where
        F: FnOnce(&mut SessionState) -> R,
    {
        let mut sessions = self.sessions.borrow_mut();
        let key: &str = session_id.0.as_ref();
        sessions.get_mut(key).map(f)
    }

    fn reset_reasoning_tracking(&self, session_id: &acp::SessionId) {
        self.with_session_state_mut(session_id, |state| {
            state.reasoning_sections.clear();
            state.current_reasoning_chunk.clear();
        });
    }

    fn append_reasoning_delta(&self, session_id: &acp::SessionId, delta: &str) {
        self.with_session_state_mut(session_id, |state| {
            state.current_reasoning_chunk.push_str(delta);
        });
    }

    fn finish_current_reasoning_section(&self, session_id: &acp::SessionId) {
        self.with_session_state_mut(session_id, |state| {
            if !state.current_reasoning_chunk.is_empty() {
                let chunk = std::mem::take(&mut state.current_reasoning_chunk);
                state.reasoning_sections.push(chunk);
            }
        });
    }

    fn take_reasoning_text(&self, session_id: &acp::SessionId) -> Option<String> {
        self.with_session_state_mut(session_id, |state| {
            let mut combined = String::new();
            let mut first = true;

            for section in state.reasoning_sections.drain(..) {
                if section.trim().is_empty() {
                    continue;
                }
                if !first {
                    combined.push_str("\n\n");
                }
                combined.push_str(section.trim_end());
                first = false;
            }

            if !state.current_reasoning_chunk.trim().is_empty() {
                if !first {
                    combined.push_str("\n\n");
                }
                combined.push_str(state.current_reasoning_chunk.trim_end());
            }

            state.current_reasoning_chunk.clear();

            if combined.is_empty() {
                None
            } else {
                Some(combined)
            }
        })
        .flatten()
    }

    fn describe_mcp_tool(
        &self,
        invocation: &McpInvocation,
    ) -> (String, Vec<acp::ToolCallLocation>) {
        if let Some(metadata) = self.fs_tool_metadata(invocation) {
            let FsToolMetadata {
                display_path,
                location_path,
                line,
            } = metadata;
            let location = acp::ToolCallLocation {
                path: location_path,
                line,
                meta: None,
            };
            (
                format!("{}.{} ({display_path})", invocation.server, invocation.tool),
                vec![location],
            )
        } else {
            (
                format!("{}.{}", invocation.server, invocation.tool),
                Vec::new(),
            )
        }
    }

    fn fs_tool_metadata(&self, invocation: &McpInvocation) -> Option<FsToolMetadata> {
        if invocation.server != "acp_fs" {
            return None;
        }

        match invocation.tool.as_str() {
            "read_text_file" | "write_text_file" => {}
            _ => return None,
        }

        let args = invocation.arguments.as_ref()?.as_object()?;
        let path = args.get("path")?.as_str()?.to_string();
        let line = args
            .get("line")
            .and_then(|value| value.as_u64())
            .map(|value| value as u32);
        let display_path = self.display_fs_path(&path);
        let location_path = PathBuf::from(&path);

        Some(FsToolMetadata {
            display_path,
            location_path,
            line,
        })
    }

    fn display_fs_path(&self, raw_path: &str) -> String {
        let path = Path::new(raw_path);
        if let Ok(relative) = path.strip_prefix(&self.config.cwd) {
            let rel_display = relative.display().to_string();
            if !rel_display.is_empty() {
                return rel_display;
            }
        }

        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw_path.to_string())
    }
}

struct FsToolMetadata {
    display_path: String,
    location_path: PathBuf,
    line: Option<u32>,
}

#[derive(Debug)]
pub enum ClientOp {
    RequestPermission(
        acp::RequestPermissionRequest,
        Sender<Result<acp::RequestPermissionResponse, Error>>,
    ),
    ReadTextFile(
        acp::ReadTextFileRequest,
        Sender<Result<acp::ReadTextFileResponse, Error>>,
    ),
    WriteTextFile(
        acp::WriteTextFileRequest,
        Sender<Result<acp::WriteTextFileResponse, Error>>,
    ),
}

#[async_trait::async_trait(?Send)]
impl Agent for CodexAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, Error> {
        info!(?args, "Received initialize request");
        // Advertise supported auth methods. We surface both ChatGPT and API key.
        let auth_methods = vec![
            acp::AuthMethod {
                id: acp::AuthMethodId("chatgpt".into()),
                name: "ChatGPT".into(),
                description: Some("Sign in with ChatGPT to use your plan".into()),
                meta: None,
            },
            acp::AuthMethod {
                id: acp::AuthMethodId("apikey".into()),
                name: "OpenAI API Key".into(),
                description: Some("Use OPENAI_API_KEY from environment or auth.json".into()),
                meta: None,
            },
        ];
        self.client_capabilities.replace(args.client_capabilities);
        let capacities = acp::AgentCapabilities {
            load_session: true,
            prompt_capabilities: acp::PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: true,
                meta: None,
            },
            mcp_capabilities: acp::McpCapabilities {
                http: true,
                sse: true,
                meta: None,
            },
            meta: None,
        };
        Ok(acp::InitializeResponse {
            protocol_version: V1,
            agent_capabilities: capacities,
            auth_methods,
            meta: None,
        })
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, Error> {
        info!(?args, "Received authenticate request");
        let method = args.method_id.0.as_ref();
        match method {
            "apikey" => {
                if let Ok(am) = self.auth_manager.write() {
                    // Persisting the API key is handled by Codex core when reloading;
                    // here we simply reload and check.
                    am.reload();
                    if am.auth().is_some() {
                        return Ok(Default::default());
                    }
                }
                Err(Error::auth_required().with_data("Failed to load API key auth"))
            }
            "chatgpt" => {
                if let Ok(am) = self.auth_manager.write() {
                    am.reload();
                    if let Some(auth) = am.auth()
                        && auth.mode == AuthMode::ChatGPT
                    {
                        return Ok(Default::default());
                    }
                }
                Err(Error::auth_required()
                    .with_data("ChatGPT login not found. Run `codex login` to connect your plan."))
            }
            other => {
                Err(Error::invalid_params().with_data(format!("unknown auth method: {}", other)))
            }
        }
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, Error> {
        info!(?args, "Received new session request");
        let session_id = {
            let mut next_id = self.next_session_id.borrow_mut();
            let session_id = next_id.to_string();
            *next_id += 1;
            session_id
        };

        // Populate a placeholder session entry prior to spawning Codex so that
        // pipelined requests (like an immediate `/status`) see the session.
        self.sessions.borrow_mut().insert(
            session_id.clone(),
            SessionState {
                created: SystemTime::now(),
                conversation_id: String::new(),
                conversation: None,
                current_approval: AskForApproval::OnRequest,
                current_sandbox: SandboxPolicy::new_workspace_write_policy(),
                current_mode: acp::SessionModeId("auto".into()),
                token_usage: None,
                reasoning_sections: Vec::new(),
                current_reasoning_chunk: String::new(),
            },
        );

        // Start a new Codex conversation for this session
        let mut session_config = self.config.clone();
        let fs_guidance = "For workspace file I/O, use the acp_fs MCP tools.
Follow this workflow:
1. Call read_text_file to capture the current content (and a hash if helpful).
2. Plan edits locally instead of mutating files via shell commands.
3. Stage replacements with edit_text_file (or multi_edit_text_file for multiple sequential edits) so the bridge can validate them and surface unified diffs.
4. Call write_text_file once to persist the staged content. If the client rejects because the file changed, read again to refresh your view and restage.";

        if let Some(mut base) = session_config.base_instructions.take() {
            if !base.contains("acp_fs") {
                if !base.trim_end().is_empty() {
                    base.push_str("\n\n");
                }
                base.push_str(fs_guidance);
            }
            session_config.base_instructions = Some(base);
        } else {
            session_config.user_instructions = match session_config.user_instructions.take() {
                Some(mut existing) => {
                    if !existing.contains("acp_fs") {
                        if !existing.trim_end().is_empty() {
                            existing.push_str("\n\n");
                        }
                        existing.push_str(fs_guidance);
                    }
                    Some(existing)
                }
                None => Some(fs_guidance.to_string()),
            };
        }

        if let Some(bridge) = &self.fs_bridge {
            match self.prepare_fs_mcp_server_config(&session_id, bridge.as_ref()) {
                Ok(server_config) => {
                    session_config
                        .mcp_servers
                        .insert("acp_fs".to_string(), server_config);
                }
                Err(err) => {
                    self.sessions.borrow_mut().remove(&session_id);
                    return Err(err);
                }
            }
        }

        let (conversation, session_configured) = match self
            .conversation_manager
            .new_conversation(session_config)
            .await
        {
            Ok(NewConversation {
                conversation,
                session_configured,
                ..
            }) => (conversation, session_configured),
            Err(e) => {
                warn!(error = %e, "Failed to create Codex conversation");
                self.sessions.borrow_mut().remove(&session_id);
                return Err(Error::into_internal_error(e));
            }
        };

        if let Ok(mut sessions) = self.sessions.try_borrow_mut()
            && let Some(state) = sessions.get_mut(&session_id)
        {
            state.conversation_id = session_configured.session_id.to_string();
            state.conversation = Some(conversation.clone());
        }

        // Advertise available slash commands to the client right after
        // the session is created. Send it asynchronously to avoid racing
        // with the NewSessionResponse delivery.
        {
            let session_id = session_id.clone();
            let available_commands = self.available_commands.clone();
            let tx_updates = self.session_update_tx.clone();
            task::spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = tx_updates.send((
                    acp::SessionNotification {
                        session_id: acp::SessionId(session_id.clone().into()),
                        update: acp::SessionUpdate::AvailableCommandsUpdate { available_commands },
                        meta: None,
                    },
                    tx,
                ));
                let _ = rx.await;
            });
        }

        Ok(acp::NewSessionResponse {
            session_id: acp::SessionId(session_id.into()),
            modes: Some(acp::SessionModeState {
                current_mode_id: acp::SessionModeId("auto".into()),
                available_modes: vec![
                    acp::SessionMode {
                        id: acp::SessionModeId("read-only".into()),
                        name: "Read Only".to_string(),
                        description: Some("Codex can read files and answer questions. Codex requires approval to make edits, run commands, or access network".to_string()),
                        meta: None,
                    },
                    acp::SessionMode {
                        id: acp::SessionModeId("auto".into()),
                        name: "Auto".to_string(),
                        description: Some("Codex can read files, make edits, and run commands in the workspace. Codex requires approval to work outside the workspace or access network".to_string()),
                        meta: None,
                    },
                    acp::SessionMode {
                        id: acp::SessionModeId("full-access".into()),
                        name: "Full Access".to_string(),
                        description: Some("Codex can read files, make edits, and run commands with network access, without approval. Exercise caution".to_string()),
                        meta: None,
                    },
                ],
                meta: None,
            }),
            meta: None,
        })
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, Error> {
        info!(?args, "Received load session request");
        Ok(acp::LoadSessionResponse {
            modes: None,
            meta: None,
        })
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, Error> {
        info!(?args, "Received set session mode request");
        // Validate session exists
        let session_id_key = args.session_id.0.to_string();
        if !self.sessions.borrow().contains_key(&session_id_key) {
            return Err(Error::invalid_params());
        }

        let session_id = args.session_id.clone();
        let mode_id = args.mode_id.clone();
        self.with_session_state_mut(&session_id, |state| {
            state.current_mode = mode_id.clone();
        });

        let tx_updates = self.session_update_tx.clone();
        task::spawn_local(async move {
            let (tx, rx) = oneshot::channel();
            let _ = tx_updates.send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::CurrentModeUpdate {
                        current_mode_id: mode_id,
                    },
                    meta: None,
                },
                tx,
            ));
            let _ = rx.await;
        });
        // Notify client about the new current mode immediately.
        Ok(acp::SetSessionModeResponse { meta: None })
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, Error> {
        info!(?args, "Received prompt request");
        let session_id = args.session_id.0.to_string();
        let session = match self.sessions.borrow().get(&session_id) {
            Some(s) => s.clone(),
            None => return Err(Error::invalid_params().with_data("Session not found")),
        };

        // Handle slash commands (e.g., "/status") when the first block is text starting with '/'
        if let Some(acp::ContentBlock::Text(t)) = args.prompt.first() {
            let line = t.text.trim();
            if let Some(cmd) = line.strip_prefix('/') {
                let mut parts = cmd.split_whitespace();
                let name = parts.next().unwrap_or("").to_lowercase();
                let rest = parts.collect::<Vec<_>>().join(" ");
                if self
                    .handle_slash_command(&args.session_id, &name, &rest)
                    .await?
                {
                    return Ok(acp::PromptResponse {
                        stop_reason: acp::StopReason::EndTurn,
                        meta: None,
                    });
                }
            }
        }

        // Ensure we have a Codex conversation for non-slash content.
        let conversation = match session.conversation.as_ref() {
            Some(conv) => conv.clone(),
            None => {
                let msg = "No Codex backend available. Use slash commands like /status";
                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(&args.session_id, msg.into(), tx)?;
                let _ = rx.await;
                return Ok(acp::PromptResponse {
                    stop_reason: acp::StopReason::EndTurn,
                    meta: None,
                });
            }
        };

        self.reset_reasoning_tracking(&args.session_id);

        // Build user input submission items from prompt content blocks.
        let mut items: Vec<InputItem> = Vec::new();
        for block in &args.prompt {
            match block {
                acp::ContentBlock::Text(t) => {
                    items.push(InputItem::Text {
                        text: t.text.clone(),
                    });
                }
                acp::ContentBlock::Image(img) => {
                    let url = format!("data:{};base64,{}", img.mime_type, img.data);
                    items.push(InputItem::Image { image_url: url });
                }
                acp::ContentBlock::Audio(_a) => {
                    // Not supported by Codex input yet; skip.
                }
                acp::ContentBlock::Resource(res) => {
                    if let acp::EmbeddedResourceResource::TextResourceContents(trc) = &res.resource
                    {
                        items.push(InputItem::Text {
                            text: trc.text.clone(),
                        });
                    }
                }
                acp::ContentBlock::ResourceLink(link) => {
                    items.push(InputItem::Text {
                        text: format!("Resource: {}", link.uri),
                    });
                }
            }
        }

        // Enqueue work and then stream corresponding events back as ACP updates.
        let submit_id = conversation
            .submit(Op::UserInput { items })
            .await
            .map_err(Error::into_internal_error)?;

        let pos = Arc::new(vec![
            acp::PermissionOption {
                id: acp::PermissionOptionId("approve_for_session".into()),
                name: "Approve for Session".into(),
                kind: acp::PermissionOptionKind::AllowAlways,
                meta: None,
            },
            acp::PermissionOption {
                id: acp::PermissionOptionId("approve".into()),
                name: "Approve".into(),
                kind: acp::PermissionOptionKind::AllowOnce,
                meta: None,
            },
            acp::PermissionOption {
                id: acp::PermissionOptionId("deny".into()),
                name: "Deny".into(),
                kind: acp::PermissionOptionKind::RejectOnce,
                meta: None,
            },
        ]);

        let mut saw_message_delta = false;
        let stop_reason = loop {
            let event = conversation
                .next_event()
                .await
                .map_err(Error::into_internal_error)?;
            if event.id != submit_id {
                continue;
            }

            match event.msg {
                EventMsg::AgentMessageDelta(delta) => {
                    saw_message_delta = true;
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, delta.delta.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                EventMsg::AgentMessage(msg) => {
                    if saw_message_delta {
                        continue;
                    }
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, msg.message.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                EventMsg::AgentReasoningDelta(delta) => {
                    self.append_reasoning_delta(&args.session_id, &delta.delta);
                }
                EventMsg::AgentReasoningRawContentDelta(delta) => {
                    self.append_reasoning_delta(&args.session_id, &delta.delta);
                }
                EventMsg::AgentReasoning(reason) => {
                    self.finish_current_reasoning_section(&args.session_id);
                    let aggregated = self.take_reasoning_text(&args.session_id);
                    let normalized_final = if reason.text.trim().is_empty() {
                        None
                    } else {
                        Some(reason.text)
                    };

                    let content = match (aggregated, normalized_final) {
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
                    };
                    if let Some(text) = content
                        && !text.trim().is_empty()
                    {
                        let (tx, rx) = oneshot::channel();
                        self.send_thought_chunk(&args.session_id, text.into(), tx)?;
                        rx.await.map_err(Error::into_internal_error)?;
                    }
                }
                EventMsg::AgentReasoningRawContent(reason) => {
                    self.finish_current_reasoning_section(&args.session_id);
                    if !reason.text.trim().is_empty() {
                        self.append_reasoning_delta(&args.session_id, &reason.text);
                    }
                }
                EventMsg::AgentReasoningSectionBreak(_) => {
                    self.finish_current_reasoning_section(&args.session_id);
                }
                // MCP tool calls → ACP ToolCall/ToolCallUpdate
                EventMsg::McpToolCallBegin(begin) => {
                    let (title, locations) = self.describe_mcp_tool(&begin.invocation);
                    let tool = acp::ToolCall {
                        id: acp::ToolCallId(begin.call_id.clone().into()),
                        title,
                        kind: acp::ToolKind::Fetch,
                        status: acp::ToolCallStatus::InProgress,
                        content: Vec::new(),
                        locations,
                        raw_input: begin.invocation.arguments,
                        raw_output: None,
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            acp::SessionNotification {
                                session_id: args.session_id.clone(),
                                update: acp::SessionUpdate::ToolCall(tool),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::McpToolCallEnd(end) => {
                    // status and optional output
                    let status = if end.is_success() {
                        acp::ToolCallStatus::Completed
                    } else {
                        acp::ToolCallStatus::Failed
                    };
                    let raw_output = serde_json::to_value(&end.result).ok();
                    let (title, locations) = self.describe_mcp_tool(&end.invocation);
                    let update = acp::ToolCallUpdate {
                        id: acp::ToolCallId(end.call_id.clone().into()),
                        fields: acp::ToolCallUpdateFields {
                            status: Some(status),
                            title: Some(title),
                            locations: if locations.is_empty() {
                                None
                            } else {
                                Some(locations)
                            },
                            raw_output,
                            ..Default::default()
                        },
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            acp::SessionNotification {
                                session_id: args.session_id.clone(),
                                update: acp::SessionUpdate::ToolCallUpdate(update),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                // Exec command begin/end → ACP ToolCall/ToolCallUpdate
                EventMsg::ExecCommandBegin(beg) => {
                    let tool = acp::ToolCall {
                        id: acp::ToolCallId(beg.call_id.clone().into()),
                        title: beg.command.join(" "),
                        kind: acp::ToolKind::Execute,
                        status: acp::ToolCallStatus::InProgress,
                        content: Vec::new(),
                        locations: vec![acp::ToolCallLocation {
                            path: beg.cwd.clone(),
                            line: None,
                            meta: None,
                        }],
                        raw_input: Some(json!({"command": beg.command, "cwd": beg.cwd})),
                        raw_output: None,
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            acp::SessionNotification {
                                session_id: args.session_id.clone(),
                                update: acp::SessionUpdate::ToolCall(tool),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::ExecCommandEnd(end) => {
                    let status = if end.exit_code == 0 {
                        acp::ToolCallStatus::Completed
                    } else {
                        acp::ToolCallStatus::Failed
                    };

                    let mut content: Vec<acp::ToolCallContent> = Vec::new();
                    if !end.aggregated_output.is_empty() {
                        content.push(acp::ToolCallContent::from(end.aggregated_output.clone()));
                    } else if !end.stdout.is_empty() || !end.stderr.is_empty() {
                        let merged = if !end.stderr.is_empty() {
                            format!("{}\n{}", end.stdout, end.stderr)
                        } else {
                            end.stdout.clone()
                        };
                        if !merged.is_empty() {
                            content.push(acp::ToolCallContent::from(merged));
                        }
                    }

                    let update = acp::ToolCallUpdate {
                        id: acp::ToolCallId(end.call_id.clone().into()),
                        fields: acp::ToolCallUpdateFields {
                            status: Some(status),
                            content: if content.is_empty() {
                                None
                            } else {
                                Some(content)
                            },
                            raw_output: Some(json!({
                                "exit_code": end.exit_code,
                                "duration_ms": end.duration.as_millis(),
                                "formatted_output": end.formatted_output,
                            })),
                            ..Default::default()
                        },
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            acp::SessionNotification {
                                session_id: args.session_id.clone(),
                                update: acp::SessionUpdate::ToolCallUpdate(update),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::ExecApprovalRequest(req) => {
                    // Build a ToolCallUpdate describing the pending exec
                    let title = format!("`{}`", req.command.join(" "));
                    let update = acp::ToolCallUpdate {
                        id: acp::ToolCallId(req.call_id.clone().into()),
                        fields: acp::ToolCallUpdateFields {
                            kind: Some(acp::ToolKind::Execute),
                            status: Some(acp::ToolCallStatus::Pending),
                            title: Some(title),
                            locations: Some(vec![acp::ToolCallLocation {
                                path: req.cwd.clone(),
                                line: None,
                                meta: None,
                            }]),
                            ..Default::default()
                        },
                        meta: None,
                    };

                    let permission_req = acp::RequestPermissionRequest {
                        session_id: args.session_id.clone(),
                        tool_call: update,
                        options: pos.as_ref().clone(),
                        meta: None,
                    };

                    let (txp, rxp) = oneshot::channel();
                    let _ = self
                        .client_tx
                        .send(ClientOp::RequestPermission(permission_req, txp));
                    let outcome = rxp.await.map_err(|_| Error::internal_error())?;
                    if let Ok(resp) = outcome {
                        let decision = self.handle_response_outcome(resp);
                        // Send ExecApproval back to Codex; refer to current event.id
                        conversation
                            .submit(Op::ExecApproval {
                                id: event.id.clone(),
                                decision,
                            })
                            .await
                            .map_err(Error::into_internal_error)?;
                    }
                }
                EventMsg::ApplyPatchApprovalRequest(req) => {
                    // Summarize patch as content lines
                    let mut contents = Vec::new();
                    for (path, change) in req.changes.iter() {
                        use codex_core::protocol::FileChange as FC;
                        match change {
                            FC::Add { content } => {
                                contents.push(acp::ToolCallContent::from(acp::Diff {
                                    path: path.clone(),
                                    old_text: Some("".into()),
                                    new_text: content.clone(),
                                    meta: None,
                                }));
                            }
                            FC::Delete { content } => {
                                contents.push(acp::ToolCallContent::from(acp::Diff {
                                    path: path.clone(),
                                    old_text: Some(content.clone()),
                                    new_text: "".into(),
                                    meta: None,
                                }));
                            }
                            FC::Update { unified_diff, .. } => {
                                contents.push(acp::ToolCallContent::from(acp::Diff {
                                    path: path.clone(),
                                    old_text: Some(unified_diff.into()),
                                    new_text: unified_diff.clone(),
                                    meta: None,
                                }));
                            }
                        };
                    }

                    let title = if req.changes.len() == 1 {
                        "Apply changes".to_string()
                    } else {
                        format!("Edit {} files", req.changes.len())
                    };
                    let update = acp::ToolCallUpdate {
                        id: acp::ToolCallId(req.call_id.clone().into()),
                        fields: acp::ToolCallUpdateFields {
                            kind: Some(acp::ToolKind::Edit),
                            status: Some(acp::ToolCallStatus::Pending),
                            title: Some(title),
                            content: if contents.is_empty() {
                                None
                            } else {
                                Some(contents)
                            },
                            ..Default::default()
                        },
                        meta: None,
                    };

                    let permission_req = acp::RequestPermissionRequest {
                        session_id: args.session_id.clone(),
                        tool_call: update,
                        options: pos.as_ref().clone(),
                        meta: None,
                    };
                    let (txp, rxp) = oneshot::channel();
                    let _ = self
                        .client_tx
                        .send(ClientOp::RequestPermission(permission_req, txp));
                    if let Ok(resp) = rxp.await.map_err(Error::into_internal_error)? {
                        let decision = self.handle_response_outcome(resp);
                        conversation
                            .submit(Op::PatchApproval {
                                id: event.id.clone(),
                                decision,
                            })
                            .await
                            .map_err(Error::into_internal_error)?;
                    }
                }
                EventMsg::TokenCount(tc) => {
                    if let Some(info) = tc.info
                        && let Ok(mut map) = self.sessions.try_borrow_mut()
                        && let Some(state) = map.get_mut(&session_id)
                    {
                        state.token_usage = Some(info.total_token_usage.clone());
                    }
                }
                EventMsg::TaskComplete(_) => {
                    break acp::StopReason::EndTurn;
                }
                EventMsg::Error(err) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, err.message.into(), tx)?;
                    let _ = rx.await;
                    break acp::StopReason::EndTurn;
                }
                EventMsg::TurnAborted(_) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, "".into(), tx)?;
                    let _ = rx.await;
                    break acp::StopReason::Cancelled;
                }
                // Ignore other events for now.
                _ => {}
            }
        };

        if let Some(text) = self.take_reasoning_text(&args.session_id)
            && !text.trim().is_empty()
        {
            let (tx, rx) = oneshot::channel();
            self.send_thought_chunk(&args.session_id, text.into(), tx)?;
            rx.await.map_err(Error::into_internal_error)?;
        }

        Ok(acp::PromptResponse {
            stop_reason,
            meta: None,
        })
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), Error> {
        info!(?args, "Received cancel request");
        let session_id = args.session_id.0.to_string();
        // Scope borrow to avoid RefCell issues across await
        let conv_opt = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&session_id)
                .and_then(|s| s.conversation.clone())
        };
        match conv_opt {
            Some(conv) => {
                // Best-effort: we don't need the submission id here.
                let _ = conv.submit(Op::Interrupt).await;
                // Remove session from cache after interrupt
                self.sessions.borrow_mut().remove(&session_id);
                Ok(())
            }
            None => {
                warn!(
                    session_id,
                    "Cancel called but no active Codex conversation found"
                );
                Err(Error::invalid_params().with_data("No active Codex backend for cancel"))
            }
        }
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, Error> {
        info!(method = %args.method, params = ?args.params, "Received extension method call");
        Ok(serde_json::value::to_raw_value(&json!({"example": "response"}))?.into())
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), Error> {
        info!(method = %args.method, params = ?args.params, "Received extension notification call");
        Ok(())
    }
}
