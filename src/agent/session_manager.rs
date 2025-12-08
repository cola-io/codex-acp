use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use agent_client_protocol::{
    ClientCapabilities, ContentBlock, ContentChunk, Error, SessionId, SessionModeId,
    SessionNotification, SessionUpdate,
};
use codex_core::{
    CodexConversation, ConversationManager,
    config::Config,
    protocol::{AskForApproval, Op, SandboxPolicy, TokenUsage},
};
use codex_protocol::{ConversationId, openai_models::ReasoningEffort};
use tokio::sync::{
    mpsc::UnboundedSender,
    oneshot::{self, Sender},
};

use crate::agent::utils;

/// Per-session state shared across the agent runtime.
///
/// Notes:
/// - `fs_session_id` is the session id used by the FS bridge. It may differ
///   from the ACP session id (which is the key in the `sessions` map).
/// - `conversation` is lazily loaded on demand; `None` until first use.
/// - Reasoning text is aggregated across streaming events.
#[derive(Clone)]
pub struct SessionState {
    pub fs_session_id: String,
    pub conversation: Option<Arc<CodexConversation>>,
    pub current_approval: AskForApproval,
    pub current_sandbox: SandboxPolicy,
    pub current_mode: SessionModeId,
    pub current_model: Option<String>,
    pub current_effort: Option<ReasoningEffort>,
    pub token_usage: Option<TokenUsage>,
}

impl SessionState {
    /// Create a new SessionState initialized from config.
    pub fn new(
        fs_session_id: String,
        conversation: Option<Arc<CodexConversation>>,
        config: &Config,
        current_mode: SessionModeId,
    ) -> Self {
        let provider_id = &config.model_provider_id;
        let model_name = config.model.as_deref().unwrap_or("unknown");
        Self {
            fs_session_id,
            conversation,
            current_approval: config.approval_policy,
            current_sandbox: config.sandbox_policy.clone(),
            current_mode,
            current_model: Some(format!("{}@{}", provider_id, model_name)),
            current_effort: config.model_reasoning_effort,
            token_usage: None,
        }
    }
}

/// Manages session state, conversations, and client communication.
///
/// This struct centralizes all session-related operations including:
/// - Session state storage and mutation
/// - Conversation loading and caching
/// - Client update notifications
/// - Context override operations
pub struct SessionManager {
    sessions: Rc<RefCell<HashMap<String, SessionState>>>,
    session_update_tx: UnboundedSender<(SessionNotification, Sender<()>)>,
    conversation_manager: Arc<ConversationManager>,
    client_capabilities: RefCell<ClientCapabilities>,
}

impl SessionManager {
    /// Create a new SessionManager.
    pub fn new(
        session_update_tx: UnboundedSender<(SessionNotification, Sender<()>)>,
        conversation_manager: Arc<ConversationManager>,
    ) -> Self {
        Self {
            sessions: Rc::new(RefCell::new(HashMap::new())),
            session_update_tx,
            conversation_manager,
            client_capabilities: RefCell::new(Default::default()),
        }
    }

    /// Get a reference to the sessions store for external access.
    pub fn sessions(&self) -> Rc<RefCell<HashMap<String, SessionState>>> {
        self.sessions.clone()
    }

    /// Mutate session state with a function.
    ///
    /// Returns `None` if the session is not found.
    pub fn with_session_state_mut<R, F>(&self, session_id: &SessionId, f: F) -> Option<R>
    where
        F: FnOnce(&mut SessionState) -> R,
    {
        let mut sessions = self.sessions.borrow_mut();
        let key: &str = session_id.0.as_ref();
        sessions.get_mut(key).map(f)
    }

    /// Shared internal helper to resolve a session state by ACP id or FS id.
    fn resolve_state<'a>(
        sessions: &'a HashMap<String, SessionState>,
        session_id: &SessionId,
    ) -> Option<&'a SessionState> {
        let key: &str = session_id.0.as_ref();
        sessions
            .get(key)
            .or_else(|| sessions.values().find(|s| s.fs_session_id == key))
    }

    /// Return the current mode for the given ACP session id.
    ///
    /// This will also resolve when the provided id matches an FS session id
    /// held inside a `SessionState`.
    pub fn current_mode(&self, session_id: &SessionId) -> Option<SessionModeId> {
        let sessions = self.sessions.borrow();
        Self::resolve_state(&sessions, session_id).map(|s| s.current_mode.clone())
    }

    /// Whether the resolved session is currently read-only.
    pub fn is_read_only(&self, session_id: &SessionId) -> bool {
        self.current_mode(session_id)
            .map(|mode| utils::is_read_only_mode(&mode))
            .unwrap_or(false)
    }

    /// If the provided `session_id` refers to an FS session id, return the
    /// corresponding ACP session id. Otherwise, return the original ACP id.
    pub fn resolve_acp_session_id(&self, session_id: &SessionId) -> Option<SessionId> {
        let sessions = self.sessions.borrow();
        if sessions.contains_key(session_id.0.as_ref()) {
            return Some(session_id.clone());
        }

        sessions.iter().find_map(|(key, state)| {
            if state.fs_session_id == session_id.0.as_ref() {
                Some(SessionId::new(key.clone()))
            } else {
                None
            }
        })
    }

    /// Get a reference to the conversation manager.
    pub fn conversation_manager(&self) -> Arc<ConversationManager> {
        self.conversation_manager.clone()
    }

    /// Get or load the conversation for a session.
    ///
    /// This will reuse a cached conversation if available, otherwise load it
    /// from the conversation manager and cache it in the session state.
    pub async fn get_conversation(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<CodexConversation>, Error> {
        let conversation_opt = {
            let sessions = self.sessions.borrow();
            let state = sessions
                .get(session_id.0.as_ref())
                .ok_or_else(|| Error::invalid_params().data("session not found"))?;
            state.conversation.clone()
        };

        if let Some(conversation) = conversation_opt {
            return Ok(conversation);
        }

        let conversation_id = ConversationId::from_string(session_id.0.as_ref())
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        let conversation = self
            .conversation_manager
            .get_conversation(conversation_id)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.with_session_state_mut(session_id, |state| {
            state.conversation = Some(conversation.clone());
        });
        Ok(conversation)
    }

    /// Set client capabilities.
    pub fn set_client_capabilities(&self, capabilities: ClientCapabilities) {
        self.client_capabilities.replace(capabilities);
    }

    /// Get a reference to the client capabilities.
    pub fn client_capabilities(&self) -> std::cell::Ref<'_, ClientCapabilities> {
        self.client_capabilities.borrow()
    }

    /// Check if the client supports terminal operations.
    pub fn support_terminal(&self) -> bool {
        self.client_capabilities.borrow().terminal
    }

    /// Send a session update notification to the client.
    pub async fn send_session_update(
        &self,
        session_id: &SessionId,
        update: SessionUpdate,
    ) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        let notification = SessionNotification::new(session_id.clone(), update);
        self.session_update_tx
            .send((notification, tx))
            .map_err(Error::into_internal_error)?;
        rx.await.map_err(Error::into_internal_error)
    }

    /// Send a message content chunk to the client.
    pub async fn send_message_chunk(
        &self,
        session_id: &SessionId,
        content: ContentBlock,
    ) -> Result<(), Error> {
        let chunk = SessionUpdate::AgentMessageChunk(ContentChunk::new(content));
        self.send_session_update(session_id, chunk).await
    }

    /// Send a thought content chunk to the client.
    pub async fn send_thought_chunk(
        &self,
        session_id: &SessionId,
        content: ContentBlock,
    ) -> Result<(), Error> {
        let chunk = SessionUpdate::AgentThoughtChunk(ContentChunk::new(content));
        self.send_session_update(session_id, chunk).await
    }

    /// Helper to apply turn context overrides while preserving session state.
    ///
    /// This encapsulates the common pattern of:
    /// 1. Reading current session state to get context (approval, sandbox, model, effort)
    /// 2. Applying an `Op::OverrideTurnContext` with selective overrides
    /// 3. Updating session state with the new values
    ///
    /// Returns an error if the session is not found or if the operation fails.
    pub async fn apply_context_override<F>(
        &self,
        session_id: &SessionId,
        build_override: F,
        update_state: impl FnOnce(&mut SessionState),
    ) -> Result<(), Error>
    where
        F: FnOnce(&SessionState) -> Op,
    {
        // Build the override operation using the current session state
        let op = {
            let sessions = self.sessions.borrow();
            let state = sessions
                .get(session_id.0.as_ref())
                .ok_or_else(|| Error::invalid_params().data("session not found"))?;
            build_override(state)
        };
        self.get_conversation(session_id)
            .await?
            .submit(op)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        // Update session state
        self.with_session_state_mut(session_id, update_state);

        Ok(())
    }
}

impl Clone for SessionManager {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
            session_update_tx: self.session_update_tx.clone(),
            conversation_manager: self.conversation_manager.clone(),
            client_capabilities: self.client_capabilities.clone(),
        }
    }
}
