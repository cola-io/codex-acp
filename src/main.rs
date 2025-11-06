use codex_acp::{
    CodexAgent, FsBridge,
    agent::ClientOp::{ReadTextFile, RequestPermission, WriteTextFile},
};

use agent_client_protocol::{AgentSideConnection, Client, Error};
use anyhow::{Result, bail};
use codex_core::config::{self, Config, ConfigOverrides};
use std::env;
use tokio::{
    io,
    sync::mpsc,
    task::{self, LocalSet},
};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _logging = codex_acp::init_from_env()?;

    if env::args().nth(1).as_deref() == Some("--acp-fs-mcp") {
        return codex_acp::fs::run_mcp_server().await;
    }

    let outgoing = io::stdout().compat_write();
    let incoming = io::stdin().compat();

    let local_set = LocalSet::new();
    local_set.run_until(async move {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();

        // Config loading strategy:
        // 1. Load Config first (contains resolved runtime settings)
        // 2. Load ConfigToml to extract profiles (needed for model/mode switching)
        //
        // NOTE: This results in two file reads/parses. Optimizations:
        // - We reuse config.codex_home instead of calling find_codex_home() twice
        // - We take ownership of profiles instead of cloning
        // - Future: codex_core could expose profiles from Config to eliminate second load
        let config = Config::load_with_cli_overrides(vec![], ConfigOverrides::default()).await?;
        let config_toml = config::load_config_as_toml_with_cli_overrides(
            &config.codex_home,
            vec![],
        ).await?;
        let profiles = config_toml.profiles;
        let fs_bridge = FsBridge::start(client_tx.clone(), config.cwd.clone()).await?;
        let agent = CodexAgent::with_config(tx, client_tx, config, profiles, Some(fs_bridge));
        let session_manager = agent.session_manager().clone();
        let (conn, handle_io) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
            task::spawn_local(fut);
        });

        task::spawn_local(async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some((session_notification, tx)) => {
                                let result = conn.session_notification(session_notification).await;
                                if let Err(e) = result { error!(error = ?e, "failed to send session notification"); break; }
                                let _ = tx.send(());
                            }
                            None => break,
                        }
                    }
                    op = client_rx.recv() => {
                        match op {
                            Some(RequestPermission { request: req, response_tx: tx }) => {
                                let res = conn.request_permission(req).await;
                                let _ = tx.send(res);
                            }
                            Some(ReadTextFile { request: mut req, response_tx: tx }) => {
                                match session_manager.resolve_acp_session_id(&req.session_id) {
                                    Some(resolved_id) => {
                                        req.session_id = resolved_id;
                                        let res = conn.read_text_file(req).await;
                                        let _ = tx.send(res);
                                    }
                                    None => {
                                        let err = Error::invalid_params()
                                            .with_data("unknown session for read_text_file");
                                        let _ = tx.send(Err(err));
                                    }
                                }
                            }
                            Some(WriteTextFile { request: mut req, response_tx: tx }) => {
                                match session_manager.resolve_acp_session_id(&req.session_id) {
                                    Some(resolved_id) => {
                                        req.session_id = resolved_id.clone();
                                        if session_manager.is_read_only(&resolved_id) {
                                            let err = Error::invalid_params()
                                                .with_data("write_text_file is disabled while session mode is read-only");
                                            let _ = tx.send(Err(err));
                                        } else {
                                            let res = conn.write_text_file(req).await;
                                            let _ = tx.send(res);
                                        }
                                    }
                                    None => {
                                        let err = Error::invalid_params()
                                            .with_data("unknown session for write_text_file");
                                        let _ = tx.send(Err(err));
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        match handle_io.await {
            Ok(()) => Ok(()),
            Err(e) => bail!(e),
        }
    }).await
}
