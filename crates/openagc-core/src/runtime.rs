//! The core's own tokio runtime (spec §4.4).
//!
//! UniFFI drives exported futures on the caller's executor (Swift's
//! cooperative pool), and its optional tokio integration uses a
//! current-thread runtime that panics on `block_in_place`. So the core owns
//! a multi-thread runtime, and every exported `async fn` hops onto it with
//! [`run`]. Exported *sync* functions must never block or touch the runtime.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

use crate::{CoreError, ErrorKind};

const WORKER_THREADS: usize = 4;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub(crate) fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("openagc-core")
            .enable_all()
            .build()
            .expect("failed to start the core tokio runtime")
    })
}

/// Run `fut` on the core runtime and await its result from any executor.
pub(crate) async fn run<F, T>(fut: F) -> Result<T, CoreError>
where
    F: Future<Output = Result<T, CoreError>> + Send + 'static,
    T: Send + 'static,
{
    // Work scoped to an account stays scoped on the runtime (registry).
    let account = crate::registry::scoped_account();
    match runtime().spawn(crate::registry::scoped(account, fut)).await {
        Ok(result) => result,
        Err(e) if e.is_cancelled() => Err(CoreError::new(ErrorKind::Cancelled, "task was cancelled")),
        Err(e) => Err(CoreError::new(ErrorKind::Internal, format!("task panicked: {e}"))),
    }
}
