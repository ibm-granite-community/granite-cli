//! Session-scoped localhost model proxy: multi-target routing (for
//! sub-agents) and usage tracking (enabled by `--usage-tracking` on
//! `granite-cli launch`) share one running proxy per launch session. See
//! `docs/specs/0020-usage-tracking-proxy.md` and
//! `docs/specs/0021-sub-agent-capability.md`.

mod model_wrapper;
pub use model_wrapper::ProxiedModel;

mod server;
pub use server::{ProxyHandle, ProxyServer, UpstreamAuth, UpstreamTarget};

mod usage;
pub use usage::{UsageStats, UsageTracker};
