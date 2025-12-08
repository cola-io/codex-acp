use std::{
    collections::HashMap,
    env,
    sync::{Arc, RwLock},
};

use agent_client_protocol::{
    AgentCapabilities, AuthMethod, AuthMethodId, AuthenticateRequest, AuthenticateResponse,
    AvailableCommandsUpdate, Error, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, ModelId, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, ProtocolVersion, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionModeId, SessionModeState, SessionModelState, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, SetSessionModelRequest, SetSessionModelResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use codex_app_server_protocol::AuthMode;
use codex_core::{
    AuthManager, ConversationManager, NewConversation,
    config::{Config, profile::ConfigProfile},
    protocol::{Op, SessionSource},
};
use tokio::{
    sync::{mpsc::UnboundedSender, oneshot},
    task,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{agent::utils, fs::FsBridge};

use super::{
    commands,
    session_manager::{SessionManager, SessionState},
};

/// Operations that require client interaction.
///
/// These operations are sent to the client handler to request permissions,
/// read files, or write files based on client capabilities.
pub enum ClientOp {
    RequestPermission {
        request: RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<RequestPermissionResponse, Error>>,
    },
    ReadTextFile {
        request: ReadTextFileRequest,
        response_tx: oneshot::Sender<Result<ReadTextFileResponse, Error>>,
    },
    WriteTextFile {
        request: WriteTextFileRequest,
        response_tx: oneshot::Sender<Result<WriteTextFileResponse, Error>>,
    },
}

/// The main ACP agent implementation.
///
/// This struct manages sessions, conversations, and coordinates between
/// the client, Codex conversation engine, and filesystem bridge.
pub struct CodexAgent {
    pub(super) session_manager: SessionManager,
    pub(super) config: Config,
    pub(super) profiles: HashMap<String, ConfigProfile>,
    pub(super) auth_manager: Arc<RwLock<Arc<AuthManager>>>,
    pub(super) client_tx: UnboundedSender<ClientOp>,
    pub(super) fs_bridge: Option<Arc<FsBridge>>,
}

impl CodexAgent {
    /// Get a reference to the session manager.
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    /// Create a new CodexAgent with the provided configuration.
    pub fn with_config(
        session_update_tx: UnboundedSender<(SessionNotification, oneshot::Sender<()>)>,
        client_tx: UnboundedSender<ClientOp>,
        config: Config,
        profiles: HashMap<String, ConfigProfile>,
        fs_bridge: Option<Arc<FsBridge>>,
    ) -> Self {
        let auth = AuthManager::shared(
            config.codex_home.clone(),
            false,
            config.cli_auth_credentials_store_mode,
        );
        let conversation_manager = ConversationManager::new(auth.clone(), SessionSource::Unknown);

        let session_manager =
            SessionManager::new(session_update_tx, Arc::new(conversation_manager));

        Self {
            session_manager,
            config,
            profiles,
            auth_manager: Arc::new(RwLock::new(auth)),
            client_tx,
            fs_bridge,
        }
    }

    /// Initialize the agent and return supported capabilities and authentication methods.
    pub(super) async fn initialize(
        &self,
        args: InitializeRequest,
    ) -> Result<InitializeResponse, Error> {
        info!(?args, "Received initialize request");

        // Advertise supported auth methods based on the configured provider
        let mut auth_methods = vec![
            AuthMethod::new(AuthMethodId::new("chatgpt"), "ChatGPT")
                .description("Sign in with ChatGPT to use your plan"),
            AuthMethod::new(AuthMethodId::new("apikey"), "OpenAI API Key")
                .description("Use OPENAI_API_KEY from environment or auth.json"),
        ];

        // Add custom provider auth method if using a custom provider
        if utils::is_custom_provider(&self.config.model_provider_id) {
            auth_methods.push(
                AuthMethod::new(
                    AuthMethodId::new(self.config.model_provider_id.clone()),
                    self.config.model_provider.name.clone(),
                )
                .description(format!(
                    "Authenticate with custom provider: {}",
                    self.config.model_provider_id
                )),
            );
        }

        self.session_manager
            .set_client_capabilities(args.client_capabilities);

        let agent_capabilities = AgentCapabilities::new()
            .load_session(false)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .audio(false)
                    .embedded_context(true),
            )
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true));

        Ok(InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(agent_capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("codex-acp", env!("CARGO_PKG_VERSION")).title("Codex ACP"),
            ))
    }

    /// Authenticate the client using the specified authentication method.
    pub(super) async fn authenticate(
        &self,
        args: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, Error> {
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
                Err(Error::auth_required().data("Failed to load API key auth"))
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
                    .data("ChatGPT login not found. Run `codex login` to connect your plan."))
            }
            "custom_provider" => {
                // For custom providers, check if the provider is configured
                if !utils::is_custom_provider(&self.config.model_provider_id) {
                    return Err(Error::invalid_params().data(
                        "Custom provider auth method is only available for custom providers",
                    ));
                }

                // Verify the custom provider is properly configured in model_providers
                if !self
                    .config
                    .model_providers
                    .contains_key(&self.config.model_provider_id)
                {
                    return Err(Error::auth_required().data(format!(
                        "Custom provider '{}' is not configured in model_providers",
                        self.config.model_provider_id
                    )));
                }

                // For custom providers, we assume authentication is handled via the provider's
                // configuration (e.g., API keys in the provider settings). If auth_manager
                // has valid auth, accept it; otherwise require configuration.
                if let Ok(am) = self.auth_manager.write() {
                    am.reload();
                    if am.auth().is_some() {
                        return Ok(Default::default());
                    }
                }

                Err(Error::auth_required().data(format!(
                    "Custom provider '{}' requires authentication. Please configure API credentials in your Codex config.",
                    self.config.model_provider_id
                )))
            }
            other => Err(Error::invalid_params().data(format!("unknown auth method: {}", other))),
        }
    }

    /// Create a new session with the given configuration.
    ///
    /// This initializes a new Codex conversation, sets up the session state,
    /// and advertises available commands and models to the client.
    pub(super) async fn new_session(
        &self,
        args: NewSessionRequest,
    ) -> Result<NewSessionResponse, Error> {
        info!(?args, "Received new session request");
        let fs_session_id = Uuid::new_v4().to_string();

        let modes = utils::session_modes_for_config(&self.config);
        let current_mode = modes
            .as_ref()
            .map(|m| m.current_mode_id.clone())
            .unwrap_or_else(|| SessionModeId::new("auto"));

        let session_config = self.build_session_config(&fs_session_id, args.mcp_servers)?;

        let new_conv = self
            .session_manager
            .conversation_manager()
            .new_conversation(session_config)
            .await;

        let (conversation, conversation_id) = match new_conv {
            Ok(NewConversation {
                conversation,
                conversation_id,
                ..
            }) => (conversation, conversation_id),
            Err(e) => {
                warn!(error = %e, "Failed to create Codex conversation");
                return Err(Error::into_internal_error(e));
            }
        };

        // Use the Codex conversation id as ACP session id.
        let acp_session_id = conversation_id.to_string();

        // Initialize session state from config
        self.session_manager.sessions().borrow_mut().insert(
            acp_session_id.clone(),
            SessionState::new(
                fs_session_id.clone(),
                Some(conversation.clone()),
                &self.config,
                current_mode.clone(),
            ),
        );

        // Advertise available slash commands to the client right after
        // the session is created. Send it asynchronously to avoid racing
        // with the NewSessionResponse delivery.
        {
            let session_id = acp_session_id.clone();
            let available_commands = commands::AVAILABLE_COMMANDS.to_vec();
            let session_manager = self.session_manager.clone();
            task::spawn_local(async move {
                let _ = session_manager
                    .send_session_update(
                        &SessionId::new(session_id),
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            available_commands,
                        )),
                    )
                    .await;
            });
        }

        // Build models response only for custom providers
        let models = if utils::is_custom_provider(&self.config.model_provider_id) {
            Some(SessionModelState::new(
                utils::current_model_id_from_config(&self.config),
                utils::available_models_from_profiles(&self.config, &self.profiles),
            ))
        } else {
            None
        };

        Ok(
            NewSessionResponse::new(SessionId::new(acp_session_id.clone()))
                .modes(modes)
                .models(models),
        )
    }

    /// Load an existing session and return its current state.
    pub(super) async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, Error> {
        info!(?args, "Received load session request");
        let sessions = self.session_manager.sessions();
        let (current_mode, _current_model) = {
            let sessions = sessions.borrow();
            let state = sessions
                .get(args.session_id.0.as_ref())
                .ok_or_else(|| Error::invalid_params().data("session not found"))?;
            (state.current_mode.clone(), state.current_model.clone())
        };

        // Use stored model or derive from config
        let current_model_id = if let Some(ref stored_model) = _current_model {
            // If model was set via set_session_model, it's already in "model@provider" format
            ModelId::new(stored_model.clone())
        } else {
            // Otherwise, construct from current config
            utils::current_model_id_from_config(&self.config)
        };

        // Build models response only for custom providers
        let models = if utils::is_custom_provider(&self.config.model_provider_id) {
            Some(SessionModelState::new(
                current_model_id,
                utils::available_models_from_profiles(&self.config, &self.profiles),
            ))
        } else {
            None
        };

        Ok(LoadSessionResponse::new()
            .modes(SessionModeState::new(
                current_mode,
                utils::available_modes(),
            ))
            .models(models))
    }

    /// Change the approval and sandbox mode for a session.
    ///
    /// This preserves the current model and effort settings while updating
    /// the approval policy and sandbox policy based on the selected preset.
    pub(super) async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, Error> {
        info!(?args, "Received set session mode request");
        let preset = utils::find_preset_by_mode_id(&args.mode_id)
            .ok_or_else(|| Error::invalid_params().data("invalid mode id"))?;

        self.session_manager
            .apply_context_override(
                &args.session_id,
                |state| Op::OverrideTurnContext {
                    approval_policy: Some(preset.approval),
                    sandbox_policy: Some(preset.sandbox.clone()),
                    model: state.current_model.clone(),
                    effort: Some(state.current_effort),
                    cwd: None,
                    summary: None,
                },
                |state| {
                    state.current_approval = preset.approval;
                    state.current_sandbox = preset.sandbox.clone();
                    state.current_mode = args.mode_id.clone();
                },
            )
            .await?;

        Ok(SetSessionModeResponse::default())
    }

    /// Change the model for a session.
    ///
    /// This preserves the current approval and sandbox settings while updating
    /// the model and its associated reasoning effort level.
    ///
    /// This method is only available when using a custom (non-builtin) provider.
    pub(super) async fn set_session_model(
        &self,
        args: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        info!(?args, "Received set session model request");

        // Check if current provider is custom
        if !utils::is_custom_provider(&self.config.model_provider_id) {
            return Err(Error::invalid_params().data(
                "set_session_model is only available when using a custom provider. Current provider is a builtin provider.",
            ));
        }

        // Parse and validate the model_id, extracting provider, model name, and effort
        let (provider_id, model_name, effort) =
            utils::parse_and_validate_model(&self.config, &self.profiles, &args.model_id)
                .ok_or_else(|| {
                    Error::invalid_params()
                        .data("invalid model id format or provider/model not found")
                })?;

        // Ensure the requested model is also from a custom provider
        if !utils::is_custom_provider(&provider_id) {
            return Err(Error::invalid_params().data(
                "Cannot switch to a builtin provider model. Only custom provider models are allowed.",
            ));
        }

        self.session_manager
            .apply_context_override(
                &args.session_id,
                |state| Op::OverrideTurnContext {
                    cwd: None,
                    approval_policy: Some(state.current_approval),
                    sandbox_policy: Some(state.current_sandbox.clone()),
                    model: Some(format!("{}@{}", provider_id, model_name)),
                    effort: Some(effort),
                    summary: None,
                },
                |state| {
                    state.current_model = Some(format!("{}@{}", provider_id, model_name));
                    state.current_effort = effort;
                },
            )
            .await?;

        Ok(SetSessionModelResponse::default())
    }
}
