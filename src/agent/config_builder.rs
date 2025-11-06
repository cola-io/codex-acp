use std::{collections::HashMap, env, time::Duration};

use agent_client_protocol::{Error, HttpHeader, McpServer};
use codex_core::config::{
    Config,
    types::{McpServerConfig, McpServerTransportConfig},
};

use crate::fs::FsBridge;

use super::core::CodexAgent;

impl CodexAgent {
    /// Prepare the filesystem MCP server configuration for a session.
    ///
    /// This creates a stdio-based MCP server that communicates with the
    /// filesystem bridge, enabling file operations within the session.
    pub(super) fn prepare_fs_mcp_server_config(
        &self,
        session_id: &str,
        bridge: &FsBridge,
    ) -> Result<McpServerConfig, Error> {
        let exe_path = env::current_exe().map_err(|err| {
            Error::internal_error().with_data(format!("failed to locate agent binary: {err}"))
        })?;

        let mut env = HashMap::new();
        env.insert(
            "ACP_FS_BRIDGE_ADDR".to_string(),
            bridge.address().to_string(),
        );
        env.insert("ACP_FS_SESSION_ID".to_string(), session_id.to_string());

        Ok(McpServerConfig {
            transport: McpServerTransportConfig::Stdio {
                command: exe_path.to_string_lossy().into_owned(),
                args: vec!["--acp-fs-mcp".to_string()],
                env: Some(env),
                env_vars: vec![],
                cwd: None,
            },
            enabled: true,
            startup_timeout_sec: Some(Duration::from_secs(5)),
            tool_timeout_sec: Some(Duration::from_secs(30)),
            enabled_tools: None,
            disabled_tools: {
                let caps = self.session_manager.client_capabilities();
                let mut v: Vec<String> = Vec::new();
                if !caps.fs.read_text_file {
                    v.push("read_text_file".to_string());
                }
                if !caps.fs.write_text_file {
                    v.push("write_text_file".to_string());
                    v.push("edit_text_file".to_string());
                    v.push("multi_edit_text_file".to_string());
                }
                if v.is_empty() { None } else { Some(v) }
            },
        })
    }

    /// Build a streamable HTTP-based MCP server configuration.
    fn build_streamable_http_server(
        name: String,
        url: String,
        headers: Vec<HttpHeader>,
        startup_timeout: Option<Duration>,
        tool_timeout: Option<Duration>,
    ) -> (String, McpServerConfig) {
        let http_headers = headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect::<HashMap<_, _>>();
        (
            name,
            McpServerConfig {
                transport: McpServerTransportConfig::StreamableHttp {
                    url,
                    http_headers: Some(http_headers),
                    bearer_token_env_var: None,
                    env_http_headers: None,
                },
                enabled: true,
                startup_timeout_sec: startup_timeout,
                tool_timeout_sec: tool_timeout,
                enabled_tools: None,
                disabled_tools: None,
            },
        )
    }

    /// Build an MCP server configuration from an ACP McpServer specification.
    pub(super) fn build_mcp_server(
        &self,
        server: McpServer,
        startup_timeout: Option<Duration>,
        tool_timeout: Option<Duration>,
    ) -> Option<(String, McpServerConfig)> {
        match server {
            McpServer::Http { name, url, headers } | McpServer::Sse { name, url, headers } => {
                Some(Self::build_streamable_http_server(
                    name,
                    url.to_string(),
                    headers,
                    startup_timeout,
                    tool_timeout,
                ))
            }
            McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let env = if env.is_empty() {
                    None
                } else {
                    Some(
                        env.into_iter()
                            .map(|var| (var.name, var.value))
                            .collect::<HashMap<_, _>>(),
                    )
                };
                Some((
                    name,
                    McpServerConfig {
                        transport: McpServerTransportConfig::Stdio {
                            command: command.to_string_lossy().into_owned(),
                            args,
                            env,
                            env_vars: vec![],
                            cwd: None,
                        },
                        enabled: true,
                        startup_timeout_sec: startup_timeout,
                        tool_timeout_sec: tool_timeout,
                        enabled_tools: None,
                        disabled_tools: None,
                    },
                ))
            }
        }
    }

    /// Build a session-specific Codex configuration.
    ///
    /// This clones the base config and adds:
    /// - Filesystem guidance instructions
    /// - Session-specific MCP servers
    /// - The acp_fs MCP server if filesystem bridge is available
    pub(super) fn build_session_config(
        &self,
        session_id: &str,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Config, Error> {
        let mut session_config = self.config.clone();
        let fs_guidance = include_str!("../../prompt_fs_guidance.md");

        // Inject filesystem guidance into instructions
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

        let startup_timeout = Some(Duration::from_secs(5));
        let tool_timeout = Some(Duration::from_secs(30));

        // Add requested MCP servers
        session_config.mcp_servers.extend(
            mcp_servers
                .into_iter()
                .filter_map(|srv| self.build_mcp_server(srv, startup_timeout, tool_timeout)),
        );

        // Add acp_fs MCP server if bridge is available
        if let Some(bridge) = &self.fs_bridge {
            let server_config = self.prepare_fs_mcp_server_config(session_id, bridge.as_ref())?;
            session_config
                .mcp_servers
                .insert("acp_fs".to_string(), server_config);
        }

        Ok(session_config)
    }
}
