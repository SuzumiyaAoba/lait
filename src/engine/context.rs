//! Shared services and per-invocation execution state.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, bail};

use crate::{config::ConfigFile, mcp, secret, skill, subagent, usage};

/// Shared services for one application configuration.
///
/// Registries and caches are resources of the loaded configuration, rather
/// than state belonging to an individual operation. Keeping them behind one
/// Arc lets nested workflows, subagents, and concurrent branches share lazy
/// caches and one MCP connection pool. Shutdown is guarded so a service set
/// can safely be referenced by more than one run context.
pub(crate) struct AppServices {
    pub(crate) file_config: Arc<ConfigFile>,
    pub(crate) registry: mcp::McpRegistry,
    pub(crate) skill_cache: skill::SkillCache,
    pub(crate) agent_registry: subagent::AgentRegistry,
    pub(crate) secret_resolver: secret::SecretResolver,
    shutdown: tokio::sync::OnceCell<()>,
}

impl AppServices {
    pub(crate) fn new(file_config: Arc<ConfigFile>) -> Self {
        Self {
            registry: mcp::McpRegistry::new(Arc::new(file_config.mcp_servers.clone())),
            skill_cache: skill::SkillCache::new(Arc::new(file_config.skills.clone())),
            agent_registry: subagent::AgentRegistry::new(Arc::new(file_config.agents.clone())),
            secret_resolver: secret::SecretResolver::new(),
            file_config,
            shutdown: tokio::sync::OnceCell::const_new(),
        }
    }

    async fn shutdown(&self) {
        self.shutdown
            .get_or_init(|| async {
                self.registry.shutdown().await;
            })
            .await;
    }

    /// Runs the top-level operation, then closes shared services. The owner
    /// of the service Arc calls this once after every context using it has
    /// finished; individual RunContext values deliberately cannot shut down
    /// shared registries behind another context's back.
    pub(crate) async fn finish<T>(self: Arc<Self>, fut: impl std::future::Future<Output = T>) -> T {
        let result = fut.await;
        self.shutdown().await;
        result
    }
}

/// The process-wide cancellation source for one invocation. It is kept
/// separate from operation tokens: a command may create a child token for one
/// request without losing the fact that the root source belongs to the whole
/// invocation (Ctrl-C or a workflow deadline).
#[derive(Clone)]
pub(crate) struct CancellationSource {
    root: tokio_util::sync::CancellationToken,
}

impl CancellationSource {
    pub(crate) fn new(root: tokio_util::sync::CancellationToken) -> Self {
        Self { root }
    }

    pub(crate) fn root_token(&self) -> tokio_util::sync::CancellationToken {
        self.root.clone()
    }

    pub(crate) fn operation_token(&self) -> tokio_util::sync::CancellationToken {
        self.root.child_token()
    }
}

/// Response and tool execution policy for one invocation. CassettePolicy is
/// an enum so record and replay cannot be enabled at the same time, even when
/// a caller constructs a context outside the CLI parser.
#[derive(Clone, Debug, Default)]
pub(crate) enum CachePolicy {
    #[default]
    Disabled,
    Enabled {
        ttl: Option<u64>,
    },
}

impl CachePolicy {
    pub(crate) fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    pub(crate) fn ttl(&self) -> Option<u64> {
        match self {
            Self::Disabled => None,
            Self::Enabled { ttl } => *ttl,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) enum CassettePolicy {
    #[default]
    Live,
    Record(PathBuf),
    Replay(PathBuf),
}

impl CassettePolicy {
    pub(crate) fn record_dir(&self) -> Option<&Path> {
        match self {
            Self::Record(path) => Some(path),
            Self::Live | Self::Replay(_) => None,
        }
    }

    pub(crate) fn replay_dir(&self) -> Option<&Path> {
        match self {
            Self::Replay(path) => Some(path),
            Self::Live | Self::Record(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RunPolicy {
    pub(crate) cache: CachePolicy,
    pub(crate) cassette: CassettePolicy,
    pub(crate) approve_tools: bool,
}

/// Per-invocation mutable state. It references shared services but owns
/// cancellation, variables, usage accounting, and policy, so those values do
/// not accidentally leak between separate commands or evaluation runs.
pub(crate) struct RunContext {
    pub(crate) services: Arc<AppServices>,
    cancellation: CancellationSource,
    pub(crate) usage: usage::UsageTally,
    pub(crate) vars: serde_json::Map<String, serde_json::Value>,
    pub(super) policy: RunPolicy,
    pub(crate) always_approved_tools: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl RunContext {
    pub(crate) fn new(
        services: Arc<AppServices>,
        root_cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            services,
            cancellation: CancellationSource::new(root_cancel),
            usage: usage::UsageTally::default(),
            vars: serde_json::Map::new(),
            policy: RunPolicy::default(),
            always_approved_tools: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub(crate) fn with_vars(mut self, vars: serde_json::Map<String, serde_json::Value>) -> Self {
        self.vars = vars;
        self
    }

    pub(crate) fn with_cache(mut self, enabled: bool, ttl: Option<u64>) -> Self {
        self.policy.cache = if enabled {
            CachePolicy::Enabled { ttl }
        } else {
            CachePolicy::Disabled
        };
        self
    }

    pub(crate) fn with_approve_tools(mut self, approve_tools: bool) -> Self {
        self.policy.approve_tools = approve_tools;
        self
    }

    pub(crate) fn with_record_replay(
        mut self,
        record_dir: Option<PathBuf>,
        replay_dir: Option<PathBuf>,
    ) -> Result<Self> {
        self.policy.cassette = match (record_dir, replay_dir) {
            (Some(_), Some(_)) => {
                bail!("record and replay modes cannot be enabled together")
            }
            (Some(path), None) => CassettePolicy::Record(path),
            (None, Some(path)) => CassettePolicy::Replay(path),
            (None, None) => CassettePolicy::Live,
        };
        Ok(self)
    }

    pub(crate) fn root_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancellation.root_token()
    }

    pub(crate) fn operation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancellation.operation_token()
    }
}
