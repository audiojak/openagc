//! The core's end of the socket (spec §10.1): a per-launch Unix socket,
//! mode 0600, that only accepts the same user and only known sessions.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::catalog;
use crate::wire::{CallReply, CallRequest, Hello, HelloReply, Outcome, PROTOCOL_VERSION, read_frame, write_frame};

/// Runs tool calls for agent sessions. Implemented by the core, where the
/// permission engine and the store live.
#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    /// Whether `session` is a live agent session the shim may bind to.
    fn has_session(&self, session: &str) -> bool;
    /// Run one call. May take minutes (a pending approval).
    async fn call(&self, session: &str, tool: permissions::Tool, arguments: serde_json::Value) -> Outcome;
}

/// The listening socket; removed when dropped.
pub struct McpSocket {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl McpSocket {
    /// Listen on `<dir>/mcp-<pid>-<n>.sock`. `dir` is created 0700 if needed; a
    /// stale socket from a crashed launch is replaced. Must be called inside
    /// a Tokio runtime.
    pub fn bind(dir: &Path, handler: Arc<dyn ToolHandler>) -> std::io::Result<Self> {
        static BOUND: AtomicU32 = AtomicU32::new(0);
        let dir = socket_dir(dir)?;
        let n = BOUND.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("mcp-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        // The socket file is ours, so its owner is the uid peers must match.
        let uid = std::fs::metadata(&path)?.uid();
        let task = tokio::spawn(accept_loop(listener, handler, uid));
        Ok(Self { path, task })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for McpSocket {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// macOS limits a socket path to 104 bytes including the terminator.
const MAX_SOCKET_PATH: usize = 103;

/// `preferred` (created 0700), or — when a socket path there would be too
/// long, as with a long home directory — `/tmp/openagc-<uid>`, used only
/// if it is a real directory owned by us.
fn socket_dir(preferred: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(preferred)?;
    std::fs::set_permissions(preferred, std::fs::Permissions::from_mode(0o700))?;
    let longest = preferred.join(format!("mcp-{}-{}.sock", u32::MAX, u32::MAX));
    if longest.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(preferred.to_owned());
    }
    let uid = std::fs::metadata(preferred)?.uid();
    let short = PathBuf::from(format!("/tmp/openagc-{uid}"));
    match std::fs::create_dir(&short) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(&short)?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by this user", short.display()),
        ));
    }
    std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o700))?;
    Ok(short)
}

async fn accept_loop(listener: UnixListener, handler: Arc<dyn ToolHandler>, uid: u32) {
    // Owned here so that stopping the server (dropping `McpSocket`) also
    // ends every open connection and the calls inside it.
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "mcp socket accept failed");
                continue;
            }
        };
        let same_user = stream.peer_cred().map(|c| c.uid() == uid).unwrap_or(false);
        if !same_user {
            tracing::warn!("mcp socket: refused a connection from another user");
            continue;
        }
        connections.spawn(connection(stream, handler.clone()));
    }
}

async fn connection(stream: UnixStream, handler: Arc<dyn ToolHandler>) {
    let (mut reader, mut writer) = stream.into_split();
    let hello: Hello = match read_frame(&mut reader).await {
        Ok(Some(h)) => h,
        _ => return,
    };
    let refusal = if hello.protocol != PROTOCOL_VERSION {
        Some(format!("protocol {} is not {PROTOCOL_VERSION}; the shim and app must match", hello.protocol))
    } else if !handler.has_session(&hello.session) {
        Some("unknown agent session".to_owned())
    } else {
        None
    };
    let ok = refusal.is_none();
    if write_frame(&mut writer, &HelloReply { ok, error: refusal }).await.is_err() || !ok {
        return;
    }

    let (replies, mut outbox) = mpsc::unbounded_channel::<CallReply>();
    let write_task = tokio::spawn(async move {
        while let Some(reply) = outbox.recv().await {
            if write_frame(&mut writer, &reply).await.is_err() {
                break;
            }
        }
    });
    // Calls in flight; dropped (aborted) when the shim disconnects, which
    // cancels any that are waiting on the user.
    let mut calls = JoinSet::new();
    let session: Arc<str> = hello.session.into();
    while let Ok(Some(req)) = read_frame::<_, CallRequest>(&mut reader).await {
        let handler = handler.clone();
        let replies = replies.clone();
        let session = session.clone();
        calls.spawn(async move {
            let outcome = match catalog::tool(&req.tool) {
                Some(tool) => handler.call(&session, tool, req.arguments).await,
                None => Outcome::error("unknown_tool", format!("there is no tool named {}", req.tool)),
            };
            let _ = replies.send(CallReply { id: req.id, outcome });
        });
        // Reap finished calls so the set does not grow for a long session.
        while calls.try_join_next().is_some() {}
    }
    calls.abort_all();
    drop(replies);
    let _ = write_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_directories_fall_back_to_a_private_tmp_dir() {
        let base = std::env::temp_dir().join("x".repeat(80));
        let dir = socket_dir(&base).unwrap();
        assert!(dir.to_string_lossy().starts_with("/tmp/openagc-"), "{dir:?}");
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        let short = std::env::temp_dir().join("s");
        let short = if short.as_os_str().len() < 60 { short } else { PathBuf::from("/tmp/oagc-s") };
        assert_eq!(socket_dir(&short).unwrap(), short);
    }
}
