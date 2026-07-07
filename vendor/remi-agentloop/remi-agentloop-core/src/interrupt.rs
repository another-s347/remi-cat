use std::future::Future;
use std::pin::Pin;

use crate::error::AgentError;
use crate::types::{InterruptInfo, ResumePayload};

// ── InterruptHandler trait ────────────────────────────────────────────────────

/// Automatic handler for a specific kind of tool interrupt.
///
/// Implement this trait for each kind of interrupt that can be automatically
/// resolved (e.g., auto-approving small payments, waiting on rate limits).
pub trait InterruptHandler {
    /// Whether this handler can process the given interrupt `kind`.
    fn can_handle(&self, kind: &str) -> bool;

    /// Handle the interrupt and produce a resume payload.
    ///
    /// Return `Err` if the interrupt cannot be resolved automatically
    /// (e.g., amount too large for auto-approval).
    fn handle(
        &self,
        info: &InterruptInfo,
    ) -> impl Future<Output = Result<serde_json::Value, AgentError>>;
}

// ── DynInterruptHandler ──────────────────────────────────────────────────────

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Object-safe version of [`InterruptHandler`] (internal).
pub(crate) trait DynInterruptHandler: Send + Sync {
    fn can_handle(&self, kind: &str) -> bool;
    fn handle<'a>(
        &'a self,
        info: &'a InterruptInfo,
    ) -> BoxFuture<'a, Result<serde_json::Value, AgentError>>;
}

impl<T: InterruptHandler + Send + Sync> DynInterruptHandler for T {
    fn can_handle(&self, kind: &str) -> bool {
        InterruptHandler::can_handle(self, kind)
    }
    fn handle<'a>(
        &'a self,
        info: &'a InterruptInfo,
    ) -> BoxFuture<'a, Result<serde_json::Value, AgentError>> {
        Box::pin(InterruptHandler::handle(self, info))
    }
}

// ── InterruptRouter ──────────────────────────────────────────────────────────

/// Routes tool interrupts to registered handlers by `kind`.
///
/// Usage:
/// ```ignore
/// let router = InterruptRouter::new()
///     .register(AutoApprovalPolicy)
///     .register(RateLimitWaitHandler);
///
/// let (auto_payloads, unhandled) = router.try_handle_all(&interrupts).await;
/// ```
pub struct InterruptRouter {
    handlers: Vec<Box<dyn DynInterruptHandler>>,
}

impl InterruptRouter {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Register a handler. Handlers are tried in registration order.
    pub fn register(mut self, handler: impl InterruptHandler + Send + Sync + 'static) -> Self {
        self.handlers.push(Box::new(handler));
        self
    }

    /// Try to automatically handle all interrupts.
    ///
    /// Returns `(handled_payloads, unhandled_interrupts)`.
    ///
    /// - Successfully handled interrupts produce [`ResumePayload`] entries.
    /// - Interrupts with no matching handler, or whose handler returns `Err`,
    ///   are returned in the unhandled list (e.g., for manual human review).
    pub async fn try_handle_all(
        &self,
        interrupts: &[InterruptInfo],
    ) -> (Vec<ResumePayload>, Vec<InterruptInfo>) {
        let mut payloads = Vec::new();
        let mut unhandled = Vec::new();

        for info in interrupts {
            if let Some(handler) = self.handlers.iter().find(|h| h.can_handle(&info.kind)) {
                match handler.handle(info).await {
                    Ok(result) => payloads.push(ResumePayload {
                        interrupt_id: info.interrupt_id.clone(),
                        result,
                    }),
                    Err(_) => unhandled.push(info.clone()),
                }
            } else {
                unhandled.push(info.clone());
            }
        }

        (payloads, unhandled)
    }

    /// Returns `true` when no handlers are registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl Default for InterruptRouter {
    fn default() -> Self {
        Self::new()
    }
}
