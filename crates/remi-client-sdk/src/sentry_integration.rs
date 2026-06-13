//! Sentry integration for the Remi client SDK.
//!
//! Call [`init`] once at process start (before any other SDK work) and hold
//! onto the returned guard for the lifetime of the process.  When the guard
//! is dropped Sentry flushes pending events.
//!
//! # Environment separation
//!
//! Pass `environment` as `"dev"` or `"prod"` – this is forwarded as-is to
//! the Sentry event envelope so you can filter in the Sentry dashboard.
//!
//! # Example
//!
//! ```rust,ignore
//! let _guard = remi_client_sdk::sentry_integration::init(
//!     "https://examplePublicKey@o0.ingest.sentry.io/0",
//!     "dev",
//!     "1.0.0-nightly",
//! );
//! ```

use sentry::ClientInitGuard;

/// Initialise the Sentry SDK for the on-device runtime.
///
/// * `dsn` – Sentry DSN string.  Pass an empty string to disable.
/// * `environment` – `"dev"` or `"prod"`.
/// * `release` – human-readable version string (e.g. `"1.2.3-nightly+42"`).
///
/// Returns a guard that **must** be kept alive; dropping it triggers a flush.
pub fn init(dsn: &str, environment: &str, release: &str) -> Option<ClientInitGuard> {
    if dsn.is_empty() {
        tracing::warn!("Sentry DSN is empty – crash reporting disabled for SDK");
        return None;
    }

    let guard = sentry::init(sentry::ClientOptions {
        dsn: dsn.parse().ok(),
        environment: Some(environment.to_owned().into()),
        release: Some(release.to_owned().into()),
        sample_rate: 1.0,
        // Attach useful device context but avoid PII.
        send_default_pii: false,
        ..Default::default()
    });

    if guard.is_enabled() {
        tracing::info!(environment, release, "Sentry SDK initialised");
    } else {
        tracing::warn!("Sentry guard created but client is not enabled (bad DSN?)");
    }

    Some(guard)
}
