//! The shim ↔ core socket protocol: length-prefixed JSON frames.
//!
//! 1. Shim sends [`Hello`]; the core answers [`HelloReply`] (the session
//!    must exist and the peer must be the same user).
//! 2. Shim sends [`CallRequest`]s; the core answers each with a
//!    [`CallReply`] carrying the same id. Calls may overlap (an approval can
//!    hold one open for minutes), so replies can arrive out of order.
//!
//! Internal and versioned with the app: the shim and app ship together.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRequest {
    pub id: u64,
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReply {
    pub id: u64,
    pub outcome: Outcome,
}

/// A tool's result as the agent will see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Outcome {
    /// `text` is what the model reads; `structured` mirrors it as JSON.
    Ok {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<serde_json::Value>,
    },
    /// A tool error the agent sees and can react to, e.g.
    /// `rejected_by_user`, `denied`, `invalid_arguments`.
    Error { code: String, message: String },
}

impl Outcome {
    pub fn json(value: serde_json::Value) -> Self {
        Outcome::Ok { text: value.to_string(), structured: Some(value) }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Outcome::Error { code: code.into(), message: message.into() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame of {0} bytes is too large")]
    TooLarge(usize),
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> Result<(), WireError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(WireError::TooLarge(bytes.len()));
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` at a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>, WireError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversize_is_refused() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let req = CallRequest { id: 7, tool: "mail_search".into(), arguments: serde_json::json!({"query": "x"}) };
        write_frame(&mut a, &req).await.unwrap();
        let got: CallRequest = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, req);
        drop(a);
        assert!(read_frame::<_, CallRequest>(&mut b).await.unwrap().is_none(), "clean EOF");

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        assert!(matches!(read_frame::<_, CallRequest>(&mut b).await, Err(WireError::TooLarge(_))));
    }

    #[test]
    fn outcomes_serialize_tagged() {
        let e = Outcome::error("rejected_by_user", "The user declined.");
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"status":"error","code":"rejected_by_user","message":"The user declined."}"#
        );
    }
}
