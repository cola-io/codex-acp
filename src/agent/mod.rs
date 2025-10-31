use agent_client_protocol::{self as acp, Agent};

// Submodules
mod commands;
mod config_builder;
mod core;
mod events;
mod prompt;
mod session_manager;
mod utils;

// Public exports
pub use core::{ClientOp, CodexAgent};
pub use session_manager::SessionManager;

// Agent trait implementation - delegates to submodule methods
#[async_trait::async_trait(?Send)]
impl Agent for CodexAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        self.initialize(args).await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        self.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        self.new_session(args).await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        self.load_session(args).await
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        self.set_session_mode(args).await
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        self.set_session_model(args).await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        self.prompt(args).await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        self.cancel(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        self.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        self.ext_notification(args).await
    }
}
