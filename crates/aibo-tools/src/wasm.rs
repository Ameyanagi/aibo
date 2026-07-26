//! Tier 1: CPython on WASI via wasmtime, **downloaded on demand** (§11).
//!
//! This is the optional half of tier 1 and it is deliberately unimplemented.
//! Three facts from the plan shape the interface rather than the code:
//!
//! * §11: "Optional CPython-on-WASI via `wasmtime` 47, **downloaded on
//!   demand** — do not bundle Pyodide."
//! * §15/S8: wasmtime is linked in whether or not the feature is used, so it is
//!   the honest lever on binary size. The `python` cargo feature exists so the
//!   cost is opt-in, and S8's fallback is "drop the Python tier" outright.
//! * §11 again: this path has fuel metering and
//!   epoch interruption. That is the reason to keep it: deterministic step
//!   accounting, and therefore reproducible tool calls.
//!
//! # SPIKE: S8 — what must be validated before this is written
//!
//! 1. Binary size with `wasmtime` 47 linked, on both platforms. If the answer
//!    invalidates §15's budget, this module moves into a downloadable helper
//!    process and the in-process API below becomes an IPC client.
//! 2. Where the CPython-on-WASI build comes from, its signature, and where it
//!    is cached. An "download on demand" runtime is an update channel, and §19
//!    already has one with ed25519 signing — this must reuse it, not invent a
//!    second unsigned one.
//! 3. Whether fuel *and* epoch interruption are both needed, or whether fuel
//!    alone bounds a CPython guest usefully (a blocking WASI call consumes no
//!    fuel, which is exactly the case epoch deadlines cover).
//! 4. What the WASI preopen set is. The default must be **empty**: no preopened
//!    directory, no inherited stdio, no sockets. §11's "default-deny network in
//!    tier 1" is a property of the store configuration, not of the guest.
//!
//! Nothing here touches `wasmtime` yet, on purpose: an interface that compiles
//! against a runtime whose configuration has not been validated would be a
//! guess wearing the costume of a decision.

use std::path::PathBuf;
use std::time::Duration;

use aibo_core::types::{ToolSchema, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolError, ToolOutput, ToolResult};

/// Resource ceilings for one Python evaluation.
///
/// `fuel` and
/// `epoch_deadline` exist here and cannot exist there. Keeping the two structs
/// separate is the point — a shared "sandbox limits" type would imply the two
/// tiers offer the same guarantees, which is the exact confusion §11 corrects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmLimits {
    /// Deterministic step budget. Unlike a wall clock this is reproducible:
    /// the same script over the same input consumes the same fuel.
    pub fuel: u64,
    /// Wall-clock backstop for guest time that consumes no fuel (a blocking
    /// WASI call). Implemented with epoch interruption, not a thread kill.
    pub epoch_deadline: Duration,
    /// Linear-memory ceiling in bytes.
    pub memory_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            // SPIKE: S8 — these three numbers are placeholders. Fuel in
            // particular has no meaning until measured against a real CPython
            // build; do not treat it as a tuned value.
            fuel: 5_000_000_000,
            epoch_deadline: Duration::from_secs(10),
            memory_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Where the on-demand CPython runtime lives once fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAsset {
    /// Cache path of the `.wasm` module.
    pub module_path: PathBuf,
    /// Version string of the CPython build.
    pub version: String,
}

/// Why the Python tier is unavailable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PythonUnavailable {
    /// aibo was built without the `python` cargo feature (the default — §15).
    #[error("this build does not include the Python tier")]
    FeatureDisabled,

    /// The runtime has not been downloaded yet.
    #[error("the Python runtime has not been downloaded")]
    NotDownloaded,

    /// Downloading or verifying the runtime failed.
    #[error("could not provision the Python runtime: {0}")]
    ProvisionFailed(String),
}

impl From<PythonUnavailable> for ToolError {
    fn from(e: PythonUnavailable) -> Self {
        ToolError::Failed {
            tool: "python".to_owned(),
            message: e.to_string(),
        }
    }
}

/// Fetches and verifies the CPython-on-WASI module.
///
/// Separate from [`PythonSandbox`] because provisioning is a network operation
/// with a signature check and a progress UI, while evaluation must be neither.
#[async_trait]
pub trait PythonProvisioner: Send + Sync {
    /// The already-cached runtime, if any. Must not hit the network.
    fn cached(&self) -> Option<RuntimeAsset>;

    /// Download and verify the runtime, reporting progress out of band.
    ///
    /// Implementations must verify an ed25519 signature over the module before
    /// it is cached — §19's release signing key, not a new trust root.
    async fn provision(&self, cancel: CancellationToken)
    -> Result<RuntimeAsset, PythonUnavailable>;
}

/// The Python sandbox.
///
/// Constructed but not usable: every entry point returns
/// [`PythonUnavailable`] until S8 is resolved. This exists so the agent loop,
/// the settings UI and the tool registry can be written against the real shape
/// now, and so that "Python is off" is a value rather than a missing symbol.
#[derive(Debug, Clone)]
pub struct PythonSandbox {
    limits: WasmLimits,
    asset: Option<RuntimeAsset>,
}

impl PythonSandbox {
    /// A sandbox with no runtime provisioned.
    pub const fn new(limits: WasmLimits) -> Self {
        Self {
            limits,
            asset: None,
        }
    }

    /// A sandbox bound to a provisioned runtime.
    pub const fn with_asset(limits: WasmLimits, asset: RuntimeAsset) -> Self {
        Self {
            limits,
            asset: Some(asset),
        }
    }

    /// The limits in force.
    pub const fn limits(&self) -> WasmLimits {
        self.limits
    }

    /// Whether this build and this installation can run Python at all.
    pub fn availability(&self) -> Result<&RuntimeAsset, PythonUnavailable> {
        if !cfg!(feature = "python") {
            return Err(PythonUnavailable::FeatureDisabled);
        }
        self.asset.as_ref().ok_or(PythonUnavailable::NotDownloaded)
    }

    /// Evaluate a Python script.
    ///
    /// # Errors
    ///
    /// Always, today: [`PythonUnavailable::FeatureDisabled`] or
    /// [`PythonUnavailable::NotDownloaded`]. When S8 lands, the body becomes an
    /// `Engine` with `consume_fuel(true)` and `epoch_interruption(true)`, a
    /// `Store` with [`WasmLimits`] applied, and a WASI context with **no**
    /// preopens and no inherited stdio.
    pub async fn eval(&self, _source: &str, _cancel: CancellationToken) -> ToolResult<ToolOutput> {
        let _asset = self.availability()?;
        // SPIKE: S8 — wasmtime 47 Engine/Store/WASI wiring, fuel and epoch
        // configuration, and the preopen policy are all unvalidated. Returning
        // a typed "unavailable" is correct behaviour for a build that has not
        // provisioned a runtime; it is *not* a stand-in for the missing engine
        // code, which must not be guessed at.
        Err(PythonUnavailable::ProvisionFailed(
            "the Python tier is not implemented yet (spike S8)".to_owned(),
        )
        .into())
    }
}

/// The Python sandbox exposed as a tier-1 tool.
///
/// Register it only when [`PythonSandbox::availability`] succeeds: advertising
/// a tool that always fails teaches the model to retry it, which burns steps
/// against the §14 ceilings.
#[derive(Debug, Clone)]
pub struct PythonTool {
    sandbox: PythonSandbox,
}

impl PythonTool {
    /// Wrap a sandbox.
    pub const fn new(sandbox: PythonSandbox) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl Tool for PythonTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "python".to_owned(),
            description: "Evaluate a Python script in a WASI sandbox with no filesystem and no \
                          network. Requires the downloadable Python runtime."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string" }
                },
                "required": ["source"]
            }),
            tier: 1,
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Sandboxed
    }

    async fn call(
        &self,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput> {
        let source = crate::args::str_arg(&args, "python", "source")?;
        self.sandbox.eval(source, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_feature_says_so_rather_than_failing_obscurely() {
        let sandbox = PythonSandbox::new(WasmLimits::default());
        let err = sandbox.availability().unwrap_err();
        if cfg!(feature = "python") {
            assert_eq!(err, PythonUnavailable::NotDownloaded);
        } else {
            assert_eq!(err, PythonUnavailable::FeatureDisabled);
        }
    }

    #[tokio::test]
    async fn evaluation_is_unavailable_not_silently_wrong() {
        let sandbox = PythonSandbox::new(WasmLimits::default());
        let err = sandbox
            .eval("print(1)", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
    }

    #[test]
    fn fuel_metering_is_real_here() {
        // §11's correction, kept after the QuickJS tier was removed: fuel and
        // epoch interruption are wasmtime mechanisms. The plan once attributed
        // them to rquickjs, which never had them — this asserts the surviving
        // sandbox genuinely does.
        let wasm = WasmLimits::default();
        assert!(wasm.fuel > 0, "wasmtime fuel metering must be configured");
        assert!(wasm.epoch_deadline > Duration::ZERO, "epoch deadline must be set");
    }
}
