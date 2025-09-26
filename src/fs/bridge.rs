use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task;
use tracing::{debug, error, warn};

use crate::agent::ClientOp;

#[derive(Clone)]
pub struct FsBridge {
    address: SocketAddr,
    _inner: Arc<FsBridgeInner>,
}

impl FsBridge {
    pub async fn start(
        client_tx: tokio::sync::mpsc::UnboundedSender<ClientOp>,
        workspace_root: PathBuf,
    ) -> anyhow::Result<Arc<FsBridge>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let inner = Arc::new(FsBridgeInner {
            client_tx,
            workspace_root,
        });
        let accept_inner = inner.clone();
        task::spawn_local(async move {
            let listener = listener;
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let connection_inner = accept_inner.clone();
                        task::spawn_local(async move {
                            if let Err(err) = handle_connection(stream, connection_inner).await {
                                warn!(error = %err, remote = %addr, "fs bridge connection errored");
                            }
                        });
                    }
                    Err(err) => {
                        error!(error = %err, "fs bridge listener failed");
                        break;
                    }
                }
            }
        });

        Ok(Arc::new(FsBridge {
            address,
            _inner: inner,
        }))
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum BridgeOp {
    Read,
    Write,
}

#[derive(Debug, serde::Deserialize)]
struct BridgeRequest {
    id: u64,
    session_id: String,
    op: BridgeOp,
    path: String,
    line: Option<u32>,
    limit: Option<u32>,
    content: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct BridgeResponse {
    id: u64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct FsBridgeInner {
    client_tx: tokio::sync::mpsc::UnboundedSender<ClientOp>,
    workspace_root: PathBuf,
}

async fn handle_connection(stream: TcpStream, inner: Arc<FsBridgeInner>) -> anyhow::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: BridgeRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                warn!(error = %err, "fs bridge received malformed request");
                continue;
            }
        };

        let response = inner.handle_request(request).await;
        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

impl FsBridgeInner {
    async fn handle_request(&self, request: BridgeRequest) -> BridgeResponse {
        let BridgeRequest {
            id,
            session_id,
            op,
            path,
            line,
            limit,
            content,
        } = request;

        let resolved_path = match self.resolve_path(&path) {
            Ok(p) => p,
            Err(err) => {
                return BridgeResponse {
                    id,
                    success: false,
                    content: None,
                    error: Some(err),
                };
            }
        };

        let session_id = acp::SessionId(session_id.into());

        match op {
            BridgeOp::Read => {
                match self
                    .read_with_fallback(&session_id, &resolved_path, line, limit)
                    .await
                {
                    Ok(text) => BridgeResponse {
                        id,
                        success: true,
                        content: Some(text),
                        error: None,
                    },
                    Err(err) => BridgeResponse {
                        id,
                        success: false,
                        content: None,
                        error: Some(err),
                    },
                }
            }
            BridgeOp::Write => {
                let Some(content) = content else {
                    return BridgeResponse {
                        id,
                        success: false,
                        content: None,
                        error: Some("missing content for write".to_string()),
                    };
                };

                match self
                    .write_with_fallback(&session_id, &resolved_path, content)
                    .await
                {
                    Ok(()) => BridgeResponse {
                        id,
                        success: true,
                        content: None,
                        error: None,
                    },
                    Err(err) => BridgeResponse {
                        id,
                        success: false,
                        content: None,
                        error: Some(err),
                    },
                }
            }
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            return Ok(candidate);
        }

        let mut resolved = self.workspace_root.clone();
        for component in Path::new(&candidate).components() {
            use std::path::Component;
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !resolved.pop() {
                        return Err("path escapes workspace root".to_string());
                    }
                }
                Component::Normal(part) => resolved.push(part),
                Component::RootDir => {}
                Component::Prefix(_) => {}
            }
        }

        Ok(resolved)
    }

    async fn read_with_fallback(
        &self,
        session_id: &acp::SessionId,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, String> {
        match self
            .read_via_client(session_id.clone(), path.to_path_buf(), line, limit)
            .await
        {
            Ok(content) => Ok(content),
            Err(err) => {
                debug!(error = %err, path = %path.display(), "client read failed, falling back to local read");
                self.read_locally(path, line, limit).await
            }
        }
    }

    async fn read_via_client(
        &self,
        session_id: acp::SessionId,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        let request = acp::ReadTextFileRequest {
            session_id,
            path,
            line,
            limit,
            meta: None,
        };
        self.client_tx
            .send(ClientOp::ReadTextFile(request, tx))
            .map_err(|_| "client read_text_file channel closed".to_string())?;

        match rx.await {
            Ok(Ok(resp)) => Ok(resp.content),
            Ok(Err(err)) => Err(err.message),
            Err(_) => Err("client read_text_file response dropped".to_string()),
        }
    }

    async fn read_locally(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, String> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

        match line {
            Some(start_line) => {
                let start = start_line.saturating_sub(1) as usize;
                let count = limit.unwrap_or(u32::MAX) as usize;
                let lines: Vec<&str> = content.lines().collect();
                if start >= lines.len() {
                    Ok(String::new())
                } else {
                    let end = (start + count).min(lines.len());
                    Ok(lines[start..end].join("\n"))
                }
            }
            None => Ok(content),
        }
    }

    async fn write_with_fallback(
        &self,
        session_id: &acp::SessionId,
        path: &Path,
        content: String,
    ) -> Result<(), String> {
        match self
            .write_via_client(session_id.clone(), path.to_path_buf(), content.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                debug!(error = %err, path = %path.display(), "client write failed, falling back to local write");
                self.write_locally(path, content).await
            }
        }
    }

    async fn write_via_client(
        &self,
        session_id: acp::SessionId,
        path: PathBuf,
        content: String,
    ) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let request = acp::WriteTextFileRequest {
            session_id,
            path,
            content,
            meta: None,
        };
        self.client_tx
            .send(ClientOp::WriteTextFile(request, tx))
            .map_err(|_| "client write_text_file channel closed".to_string())?;

        match rx.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(err.message),
            Err(_) => Err("client write_text_file response dropped".to_string()),
        }
    }

    async fn write_locally(&self, path: &Path, content: String) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| {
                format!(
                    "failed to create parent directories {}: {err}",
                    parent.display()
                )
            })?;
        }
        tokio::fs::write(path, content)
            .await
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
    }
}
