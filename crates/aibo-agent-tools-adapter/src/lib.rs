//! The narrow dependency-direction adapter between `aibo-agent` and
//! `aibo-tools`.
//!
//! `aibo-agent` owns [`ToolExecutor`] because intent must be known before its
//! permission gate runs. `aibo-tools` owns [`ToolRegistry`] and the concrete
//! tools. Neither lower-level crate should depend on the other, so this leaf
//! crate owns the local [`ToolRegistryExecutor`] type and implements the
//! foreign executor trait for it.
//!
//! The initial surface is intentionally closed to
//! [`ToolRegistry::with_builtins`]: six pure tier-0 tools, no filesystem,
//! process, network, MCP, or sandbox authority. Expanding that surface requires
//! a separate adapter that can bind canonical
//! [`AuthorizedToolInvocation::resolved_paths`] to concrete operations.

#![forbid(unsafe_code)]

mod workspace;
pub use workspace::{WorkspaceError, WorkspaceExecutor};

use aibo_agent::{
    AuthorizedToolInvocation, ToolExecutor, ToolIntent, ToolInvocation,
    ToolOutput as AgentToolOutput,
};
use aibo_core::error::{AiboError, Result};
use aibo_core::types::{ApprovalKind, ToolSchema, ToolTier};
use aibo_tools::{ToolError, ToolOutput as RegistryToolOutput, ToolRegistry};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A closed, tier-0-only adapter around [`ToolRegistry`].
///
/// Construct it with [`Self::tier_zero_builtins`]. There is deliberately no
/// public arbitrary-registry constructor: accepting a registry containing
/// shell/fs tools would claim filesystem authority without any mechanism for
/// binding the gate's canonical paths.
#[derive(Clone)]
pub struct ToolRegistryExecutor {
    registry: ToolRegistry,
}

impl std::fmt::Debug for ToolRegistryExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistryExecutor")
            .field("surface", &"tier-zero-builtins")
            .field("tool_count", &self.registry.len())
            .finish()
    }
}

/// A fail-closed adapter invariant was violated.
///
/// Values intentionally contain counts/classes only, never tool arguments,
/// output, paths, or underlying error strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// The registry contained a tool whose runtime tier was not builtin.
    #[error("tier-zero adapter refused a non-builtin tool")]
    NonBuiltinTool,
    /// The advertised numeric schema tier disagreed with tier 0.
    #[error("tier-zero adapter refused a schema outside tier 0")]
    NonBuiltinSchema,
    /// A builtin invocation arrived carrying canonical filesystem paths.
    #[error("builtin invocation unexpectedly carried {count} resolved path(s)")]
    UnexpectedResolvedPaths {
        /// Number of paths, without disclosing their contents.
        count: usize,
    },
}

impl ToolRegistryExecutor {
    /// Register the audited pure builtins and nothing else.
    pub fn tier_zero_builtins() -> std::result::Result<Self, AdapterError> {
        Self::from_registry(ToolRegistry::with_builtins())
    }

    fn from_registry(registry: ToolRegistry) -> std::result::Result<Self, AdapterError> {
        for schema in registry.schemas() {
            let Some(tool) = registry.get(&schema.name) else {
                return Err(AdapterError::NonBuiltinSchema);
            };
            if tool.tier() != ToolTier::Builtin {
                return Err(AdapterError::NonBuiltinTool);
            }
            if schema.tier != 0 {
                return Err(AdapterError::NonBuiltinSchema);
            }
        }
        Ok(Self { registry })
    }

    fn intent_for(&self, call: &ToolInvocation) -> Option<ToolIntent> {
        let tool = self.registry.get(&call.name)?;
        let schema = tool.schema();
        // Recheck at use time. Tool::schema is a method, so a stateful or
        // dishonest implementation must fail closed even after construction.
        if tool.tier() != ToolTier::Builtin || schema.tier != 0 || schema.name != call.name {
            return None;
        }
        Some(ToolIntent {
            tier: ToolTier::Builtin,
            kind: ApprovalKind::Builtin,
            summary: format!("Run the pure built-in `{}`", schema.name),
            command: None,
            paths: Vec::new(),
        })
    }

    async fn execute_parts(
        &self,
        invocation: ToolInvocation,
        resolved_paths: Vec<std::path::PathBuf>,
        cancel: CancellationToken,
    ) -> Result<AgentToolOutput> {
        if !resolved_paths.is_empty() {
            return Err(AiboError::Internal(Box::new(
                AdapterError::UnexpectedResolvedPaths {
                    count: resolved_paths.len(),
                },
            )));
        }

        // Race dispatch as well as forwarding the same token. The Tool trait
        // requires cooperative cancellation, but dropping a pure tier-0 future
        // is safe and keeps a buggy implementation from delaying Escape.
        let dispatch_cancel = cancel.clone();
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ToolError::Cancelled),
            result = self.registry.call(
                &invocation.name,
                invocation.args,
                dispatch_cancel,
            ) => result,
        };
        map_result(result)
    }
}

#[async_trait]
impl ToolExecutor for ToolRegistryExecutor {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.registry.schemas()
    }

    fn intent(&self, call: &ToolInvocation) -> Option<ToolIntent> {
        self.intent_for(call)
    }

    async fn execute(
        &self,
        call: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<AgentToolOutput> {
        let (invocation, resolved_paths) = call.into_parts();
        self.execute_parts(invocation, resolved_paths, cancel).await
    }
}

fn map_result(
    result: std::result::Result<RegistryToolOutput, ToolError>,
) -> Result<AgentToolOutput> {
    match result {
        Ok(output) => Ok(AgentToolOutput {
            content: output.text,
            is_error: output.is_error,
            // Tier-0 builtins cannot touch files.
            diffs: Vec::new(),
        }),
        Err(ToolError::Sandbox { tier, reason }) => Err(AiboError::Sandbox { tier, reason }),
        Err(error) => Ok(AgentToolOutput {
            content: public_error_message(&error).to_owned(),
            is_error: true,
            diffs: Vec::new(),
        }),
    }
}

fn public_error_message(error: &ToolError) -> &'static str {
    match error {
        ToolError::UnknownTool(_) => "tool is unavailable",
        ToolError::InvalidArguments { .. } => "tool arguments were invalid",
        ToolError::Failed { .. } | ToolError::Io(_) => "tool execution failed",
        ToolError::Denied(_) => "tool execution was denied",
        ToolError::Cancelled => "tool call cancelled",
        ToolError::Sandbox { .. } => "sandboxed tool execution failed",
        _ => "tool execution failed",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use aibo_agent::permission_gate::DenyAll;
    use aibo_agent::{Authorisation, GatedCall, PermissionGate};
    use aibo_core::types::ContentOrigin;
    use aibo_tools::{Tool, ToolResult};
    use serde_json::json;

    use super::*;

    const CANARY: &str = "sk-live-CANARY-must-never-appear";

    fn invocation(name: &str, args: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            id: "call-1".to_owned(),
            name: name.to_owned(),
            args,
        }
    }

    #[test]
    fn construction_exposes_only_stable_tier_zero_schemas() {
        let adapter = ToolRegistryExecutor::tier_zero_builtins().expect("builtins");
        let schemas = adapter.schemas();
        assert_eq!(schemas.len(), 6);
        assert!(schemas.iter().all(|schema| schema.tier == 0));
        let names: Vec<_> = schemas.iter().map(|schema| schema.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn intent_is_honest_and_has_no_filesystem_authority() {
        let adapter = ToolRegistryExecutor::tier_zero_builtins().expect("builtins");
        for schema in adapter.schemas() {
            let intent = adapter
                .intent(&invocation(&schema.name, json!({})))
                .expect("known builtin");
            assert_eq!(intent.tier, ToolTier::Builtin);
            assert_eq!(intent.kind, ApprovalKind::Builtin);
            assert!(intent.command.is_none());
            assert!(intent.paths.is_empty());
            assert!(intent.summary.contains(&schema.name));
        }
        assert!(
            adapter
                .intent(&invocation("read_file", json!({})))
                .is_none()
        );
    }

    #[tokio::test]
    async fn builtin_intent_passes_the_gate_without_prompt_or_paths() {
        let adapter = ToolRegistryExecutor::tier_zero_builtins().expect("builtins");
        let invocation = invocation("hash", json!({"text": "hello"}));
        let intent = adapter.intent(&invocation).expect("intent");
        let gated = GatedCall {
            call_id: invocation.id,
            tool: invocation.name,
            tier: intent.tier,
            kind: intent.kind,
            command: intent.command,
            paths: intent.paths,
            origin: ContentOrigin::UserInstruction,
            instruction: "hash this text".to_owned(),
            summary: intent.summary,
        };
        // DenyAll proves no UI approval was consulted: tier 0 must be allowed
        // by policy before the request seam is reached.
        let gate = PermissionGate::new(Arc::new(DenyAll), Vec::<PathBuf>::new());
        let authorization = gate.authorise(&gated).await.expect("gate");
        assert!(matches!(
            authorization,
            Authorisation::Allowed {
                resolved_paths,
                remembered: false,
            } if resolved_paths.is_empty()
        ));
        assert!(gate.revalidate(&gated).expect("revalidate").is_empty());
    }

    #[tokio::test]
    async fn builtin_output_maps_without_claiming_diffs() {
        let adapter = ToolRegistryExecutor::tier_zero_builtins().expect("builtins");
        let output = adapter
            .execute_parts(
                invocation(
                    "hash",
                    json!({
                        "text": "hello",
                        "algorithm": "sha256",
                    }),
                ),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("hash");
        assert!(!output.is_error);
        assert!(!output.content.is_empty());
        assert!(output.diffs.is_empty());
    }

    #[tokio::test]
    async fn resolved_paths_fail_closed_without_disclosing_them() {
        let adapter = ToolRegistryExecutor::tier_zero_builtins().expect("builtins");
        let error = adapter
            .execute_parts(
                invocation("hash", json!({"text": "hello"})),
                vec![PathBuf::from(format!("/private/{CANARY}"))],
                CancellationToken::new(),
            )
            .await
            .expect_err("paths are impossible for builtins");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(CANARY));
        let AiboError::Internal(source) = error else {
            panic!("unexpected error variant");
        };
        assert_eq!(
            source.downcast_ref::<AdapterError>(),
            Some(&AdapterError::UnexpectedResolvedPaths { count: 1 })
        );
    }

    #[derive(Debug)]
    struct SecretErrorTool;

    #[async_trait]
    impl Tool for SecretErrorTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "secret_error".to_owned(),
                description: "test".to_owned(),
                parameters: json!({"type": "object"}),
                tier: 0,
            }
        }

        fn tier(&self) -> ToolTier {
            ToolTier::Builtin
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolResult<RegistryToolOutput> {
            Err(ToolError::InvalidArguments {
                tool: CANARY.to_owned(),
                reason: CANARY.to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn underlying_error_details_are_not_exposed() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SecretErrorTool));
        let adapter = ToolRegistryExecutor::from_registry(registry).expect("test registry");
        let output = adapter
            .execute_parts(
                invocation("secret_error", json!({"secret": CANARY})),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("domain error output");
        assert!(output.is_error);
        assert_eq!(output.content, "tool arguments were invalid");
        assert!(!format!("{output:?}").contains(CANARY));
    }

    #[derive(Debug)]
    struct IgnoresCancellationTool {
        started: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for IgnoresCancellationTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "wait".to_owned(),
                description: "test".to_owned(),
                parameters: json!({"type": "object"}),
                tier: 0,
            }
        }

        fn tier(&self) -> ToolTier {
            ToolTier::Builtin
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolResult<RegistryToolOutput> {
            self.started.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_is_forwarded_and_enforced_at_the_adapter_boundary() {
        let started = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(IgnoresCancellationTool {
            started: Arc::clone(&started),
        }));
        let adapter = ToolRegistryExecutor::from_registry(registry).expect("test registry");
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                adapter
                    .execute_parts(invocation("wait", json!({})), Vec::new(), cancel)
                    .await
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tool started");
        cancel.cancel();
        let output = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("adapter cancelled promptly")
            .expect("task joined")
            .expect("cancellation maps to output");
        assert!(output.is_error);
        assert_eq!(output.content, "tool call cancelled");
    }

    #[derive(Debug)]
    struct FilesystemClaimingTool;

    #[async_trait]
    impl Tool for FilesystemClaimingTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "read_file".to_owned(),
                description: "test".to_owned(),
                parameters: json!({"type": "object"}),
                tier: 3,
            }
        }

        fn tier(&self) -> ToolTier {
            ToolTier::ShellFs
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolResult<RegistryToolOutput> {
            unreachable!("rejected during construction")
        }
    }

    #[test]
    fn a_registry_with_filesystem_authority_is_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FilesystemClaimingTool));
        assert!(matches!(
            ToolRegistryExecutor::from_registry(registry),
            Err(AdapterError::NonBuiltinTool)
        ));
    }
}
