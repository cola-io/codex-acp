use std::{path::Path, sync::LazyLock};

use crate::CodexAgent;
use agent_client_protocol::{AvailableCommand, SessionId};
use codex_core::protocol::{AskForApproval, Op, ReviewRequest, ReviewTarget, SandboxPolicy};
use codex_protocol::user_input::UserInput;

pub static AVAILABLE_COMMANDS: LazyLock<Vec<AvailableCommand>> = LazyLock::new(built_in_commands);

impl CodexAgent {
    pub async fn handle_slash_command(&self, session_id: &SessionId, name: &str) -> Option<Op> {
        let mut msg = String::default();
        let op = match name {
            "init" => {
                let prompt = include_str!("../../prompt_init_command.md");

                msg = "📝 Creating AGENTS.md file with initial instructions...\n\n".into();
                Some(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: prompt.into(),
                    }],
                })
            }
            "status" => {
                let status_text = self.render_status(session_id).await;
                drop(
                    self.session_manager
                        .send_message_chunk(session_id, status_text.into())
                        .await,
                );
                None
            }
            "compact" => {
                self.session_manager
                    .with_session_state_mut(session_id, |state| {
                        state.token_usage = None;
                    });
                msg = "🧠 Compacting conversation to reduce context size...\n\n".into();
                Some(Op::Compact)
            }
            "review" => {
                msg = "🔍 Asking Codex to review current changes...\n\n".into();
                Some(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::UncommittedChanges,
                        user_facing_hint: Some("current changes".to_string()),
                    },
                })
            }
            _ => None,
        };

        if !msg.is_empty() {
            drop(
                self.session_manager
                    .send_message_chunk(session_id, msg.into())
                    .await,
            );
        }
        op
    }

    async fn render_status(&self, session_id: &SessionId) -> String {
        let sid_str = session_id.0.as_ref();
        // Session snapshot
        let (approval_mode, sandbox_mode, token_usage) = {
            if let Some(state) = self.session_manager.sessions().borrow().get(sid_str) {
                (
                    state.current_approval,
                    state.current_sandbox.clone(),
                    state.token_usage.clone(),
                )
            } else {
                (
                    AskForApproval::OnRequest,
                    SandboxPolicy::new_workspace_write_policy(),
                    None,
                )
            }
        };

        // Workspace
        let cwd = self.shorten_home(&self.config.cwd);

        // Account
        let (auth_mode, email, plan): (String, String, String) =
            match self.auth_manager.read().ok().and_then(|am| am.auth()) {
                Some(auth) => match auth.get_token_data().await {
                    Ok(td) => {
                        let email = td
                            .id_token
                            .email
                            .clone()
                            .unwrap_or_else(|| "(none)".to_string());
                        let plan = td
                            .id_token
                            .get_chatgpt_plan_type()
                            .unwrap_or_else(|| "(unknown)".to_string());
                        ("ChatGPT".to_string(), email, plan)
                    }
                    Err(_) => (
                        "API key".to_string(),
                        "(none)".to_string(),
                        "(unknown)".to_string(),
                    ),
                },
                None => (
                    "Not signed in".to_string(),
                    "(none)".to_string(),
                    "(unknown)".to_string(),
                ),
            };

        // Model
        let model = self.config.model.clone().unwrap_or_default();
        let provider = self.title_case(&self.config.model_provider_id);
        let effort = self.title_case(
            format!("{}", self.config.model_reasoning_effort.unwrap_or_default()).as_str(),
        );
        let summary = self.title_case(format!("{}", self.config.model_reasoning_summary).as_str());

        // Tokens
        let (input, output, total) = match token_usage {
            Some(u) => (
                u.input_tokens.to_string(),
                u.output_tokens.to_string(),
                u.total_tokens.to_string(),
            ),
            None => ("0".to_string(), "0".to_string(), "0".to_string()),
        };

        let status = format!(
            r#"
📂 Workspace

    Path:          {cwd}
    Approval Mode: {approval}
    Sandbox:       {sandbox}

👤 Account

    Signed in with: {auth_mode}
    Login:          {email}
    Plan:           {plan}

🧠 Model

    Name:                {model}
    Provider:            {provider}
    Reasoning Effort:    {effort}
    Reasoning Summaries: {summary}

📊 Token Usage

    Session ID:     {sid}
    Input:          {input}
    Output:         {output}
    Total:          {total}
"#,
            cwd = cwd,
            approval = approval_mode,
            sandbox = sandbox_mode,
            auth_mode = auth_mode,
            email = email,
            plan = plan,
            model = model,
            provider = provider,
            effort = effort,
            summary = summary,
            sid = sid_str,
            input = input,
            output = output,
            total = total,
        );
        status
    }

    fn shorten_home(&self, p: &Path) -> String {
        let s = p.display().to_string();
        if let Ok(home) = std::env::var("HOME")
            && s.starts_with(&home)
        {
            return s.replacen(&home, "~", 1);
        }
        s
    }

    fn title_case(&self, s: &str) -> String {
        if s.is_empty() {
            return s.to_string();
        }
        let mut chars = s.chars();
        let first = chars.next().unwrap().to_uppercase().to_string();
        let rest = chars.as_str();
        format!("{}{}", first, rest)
    }
}

fn built_in_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new(
            "init",
            "create an AGENTS.md file with instructions for Codex",
        ),
        AvailableCommand::new(
            "compact",
            "summarize conversation to prevent hitting the context limit",
        ),
        AvailableCommand::new("review", "review my current changes and find issues"),
        AvailableCommand::new(
            "status",
            "show current session configuration and token usage",
        ),
    ]
}
