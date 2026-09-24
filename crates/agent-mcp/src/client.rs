//! The shim's end of the socket.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, oneshot};

use crate::wire::{
    CallReply, CallRequest, Hello, HelloReply, Outcome, PROTOCOL_VERSION, WireError, read_frame, write_frame,
};

type Pending = Arc<std::sync::Mutex<Option<HashMap<u64, oneshot::Sender<Outcome>>>>>;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach OpenAGC: {0}")]
    Connect(#[from] WireError),
    #[error("OpenAGC refused the session: {0}")]
    Refused(String),
}

/// A connection bound to one agent session. Calls may run concurrently.
pub struct ShimClient {
    writer: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
}

impl ShimClient {
    pub async fn connect(socket: &Path, session: &str) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await.map_err(WireError::from)?;
        let (mut reader, mut writer) = stream.into_split();
        write_frame(&mut writer, &Hello { protocol: PROTOCOL_VERSION, session: session.to_owned() }).await?;
        let reply: HelloReply = read_frame(&mut reader)
            .await?
            .ok_or_else(|| ClientError::Refused("the app closed the connection".into()))?;
        if !reply.ok {
            return Err(ClientError::Refused(reply.error.unwrap_or_default()));
        }
        let pending: Pending = Arc::new(std::sync::Mutex::new(Some(HashMap::new())));
        let routes = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(reply)) = read_frame::<_, CallReply>(&mut reader).await {
                let waiter =
                    routes.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(&reply.id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(reply.outcome);
                }
            }
            // The app went away: fail everything still waiting, and any
            // later call, instead of hanging the agent.
            routes.lock().unwrap_or_else(|e| e.into_inner()).take();
        });
        Ok(Self { writer: Mutex::new(writer), pending, next_id: AtomicU64::new(1) })
    }

    pub async fn call(&self, tool: &str, arguments: serde_json::Value) -> Outcome {
        let unavailable = || Outcome::error("app_unavailable", "OpenAGC is not running or closed this session");
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            match pending.as_mut() {
                Some(map) => {
                    map.insert(id, tx);
                }
                None => return unavailable(),
            }
        }
        let request = CallRequest { id, tool: tool.to_owned(), arguments };
        if write_frame(&mut *self.writer.lock().await, &request).await.is_err() {
            return unavailable();
        }
        rx.await.unwrap_or_else(|_| unavailable())
    }
}
