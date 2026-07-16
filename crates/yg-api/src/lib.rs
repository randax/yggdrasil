//! REST + MCP server: the transport over the Verb engine. Modules
//! follow the crate's seams — `config` (boot), `auth`, `error`, `wire`
//! (canonical serialization), `mcp`, `verbs` (thin encoders over
//! `yg_verbs::Engine`), `search`, `admin`, and `health`. The Verb
//! contract itself — cursors, validation, Shard resolution, blocking
//! execution — lives in yg-verbs; this crate holds no graph-connection
//! handling.

mod admin;
mod auth;
mod config;
mod error;
mod health;
mod mcp;
mod metrics;
mod rate_limit;
mod request_timeout;
mod search;
mod search_limit;
mod verbs;
mod wire;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use object_store::ObjectStore;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use yg_control::ControlPlane;

pub use config::{
    CacheCapacity, DEFAULT_MCP_BATCH_SIZE_LIMIT, DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_SEARCH_CONCURRENCY_LIMIT, DEFAULT_TOKEN_RATE_LIMIT_REQUESTS,
    DEFAULT_TOKEN_RATE_LIMIT_WINDOW, ObjectStoreConfig, ProtectionConfig, RunningServer,
    ServerConfig, TokenRateLimitConfig, probe_object_store,
};
pub use health::UPTIME_HEADER;
pub use metrics::Metrics;

use error::ApiError;
use verbs::ShardAccess;

pub(crate) struct AppState {
    control: ControlPlane,
    forge_registry: yg_sync::forge::ForgeRegistry,
    store: Arc<dyn ObjectStore>,
    engine: Arc<yg_verbs::Engine<ShardAccess>>,
    metrics: Metrics,
    rate_limiter: rate_limit::TokenRateLimiter,
    auth_failure_limiter: auth::AuthFailureLimiter,
    search_limiter: search_limit::SearchLimiter,
    mcp_batch_size_limit: usize,
    bootstrap_token: String,
    started: std::time::Instant,
}

impl AppState {
    /// The one transport-independent entry to the server-wide search gate.
    /// The timer starts before semaphore acquisition, so it includes permit
    /// wait. The request deadline drops and therefore truncates the timer even
    /// while detached search execution finishes or reaches its own bound.
    async fn search(
        &self,
        request: yg_verbs::SearchRequest,
    ) -> Result<yg_verbs::SearchResponse, yg_verbs::VerbError> {
        let _timer = self.engine.metrics().timer(yg_verbs::Verb::Search);
        let engine = self.engine.clone();
        self.search_limiter
            .run(move || async move { engine.search(request).await })
            .await
            .map_err(|_| {
                yg_verbs::VerbError::Unavailable("search execution timed out".to_owned())
            })?
    }
}

pub(crate) struct MetricsServerState {
    metrics: Metrics,
    bootstrap_token: String,
}

/// Boot the Index Server: connect to the control plane, verify object
/// storage, and start serving.
pub async fn serve(config: ServerConfig) -> anyhow::Result<RunningServer> {
    serve_with_registry(config, yg_sync::forge::ForgeRegistry::builtin()).await
}

/// Boot the Index Server with an injected Forge registry used by repository
/// registration and Forge administration.
pub async fn serve_with_registry(
    config: ServerConfig,
    forge_registry: yg_sync::forge::ForgeRegistry,
) -> anyhow::Result<RunningServer> {
    serve_with_metrics_and_registry(config, Metrics::new(), MetricsAccess::Admin, forge_registry)
        .await
}

/// Whether Prometheus exposition participates in the Admin auth policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsAccess {
    /// Require the bootstrap Admin bearer token.
    Admin,
    /// Expose without HTTP authentication for a network-restricted scraper.
    Unauthenticated,
}

/// Boot the Index Server with collectors supplied by the process composition
/// root, allowing an `all` role to expose worker and API observations together.
pub async fn serve_with_metrics(
    config: ServerConfig,
    metrics: Metrics,
    metrics_access: MetricsAccess,
) -> anyhow::Result<RunningServer> {
    serve_with_metrics_and_registry(
        config,
        metrics,
        metrics_access,
        yg_sync::forge::ForgeRegistry::builtin(),
    )
    .await
}

/// Boot the Index Server with explicit request-protection bounds.
pub async fn serve_with_metrics_and_protection(
    config: ServerConfig,
    metrics: Metrics,
    metrics_access: MetricsAccess,
    protection: ProtectionConfig,
) -> anyhow::Result<RunningServer> {
    serve_with_metrics_registry_and_protection(
        config,
        metrics,
        metrics_access,
        yg_sync::forge::ForgeRegistry::builtin(),
        protection,
    )
    .await
}

/// Boot the Index Server with supplied metrics and Forge adapters.
pub async fn serve_with_metrics_and_registry(
    config: ServerConfig,
    metrics: Metrics,
    metrics_access: MetricsAccess,
    forge_registry: yg_sync::forge::ForgeRegistry,
) -> anyhow::Result<RunningServer> {
    serve_with_metrics_registry_and_protection(
        config,
        metrics,
        metrics_access,
        forge_registry,
        ProtectionConfig::default(),
    )
    .await
}

async fn serve_with_metrics_registry_and_protection(
    config: ServerConfig,
    metrics: Metrics,
    metrics_access: MetricsAccess,
    forge_registry: yg_sync::forge::ForgeRegistry,
    protection: ProtectionConfig,
) -> anyhow::Result<RunningServer> {
    let control =
        ControlPlane::connect_and_migrate_with_metrics(&config.database_url, metrics.control())
            .await?;

    let store = config.object_store.connect()?;
    probe_object_store(store.as_ref())
        .await
        .context("object storage unreachable at boot")?;

    let shards = Arc::new(yg_shard::ShardCache::with_metrics_and_capacity(
        store.clone(),
        config.shard_cache,
        metrics.shard_cache(),
        config.shard_cache_capacity,
    ));
    let state = Arc::new(AppState {
        engine: Arc::new(yg_verbs::Engine::with_metrics(
            ShardAccess::new(control.clone(), shards.clone()),
            metrics.verbs(),
        )),
        control,
        forge_registry,
        store,
        metrics,
        rate_limiter: rate_limit::TokenRateLimiter::new(protection.token_rate_limit),
        auth_failure_limiter: auth::AuthFailureLimiter::default(),
        search_limiter: search_limit::SearchLimiter::new(protection.search_concurrency_limit),
        mcp_batch_size_limit: protection.mcp_batch_size_limit.get(),
        bootstrap_token: config.bootstrap_token,
        started: std::time::Instant::now(),
    });
    // The route table is the authorization policy (issue #38): everything
    // in `member_routes` is reachable with any valid token, everything
    // nested under `/admin` additionally passes `require_admin`. Adding a
    // route to a router grants its scope — no path allowlists anywhere.
    let admin_routes = Router::new()
        .route("/repos", post(admin::admin_repo_add))
        .route("/forges", post(admin::admin_forge_add))
        .route("/forges/discover", post(admin::admin_forge_discover))
        .route(
            "/rules",
            get(admin::admin_rules_list).post(admin::admin_rules_add),
        )
        .route(
            "/tokens",
            get(admin::admin_tokens_list).post(admin::admin_token_issue),
        )
        .route("/tokens/{id}/revoke", post(admin::admin_token_revoke))
        .route("/status", get(admin::admin_status))
        // Catch-alls so the scope gate covers the whole /admin subtree,
        // including its bare root (`{*rest}` cannot match an empty
        // remainder): a Member probing any admin path gets the same 403
        // as a real one, never a 404 that maps the admin surface. The
        // one exception is the trailing-slash root `/admin/`, which
        // matchit cannot route here; it falls through to the outer
        // fallback and answers exactly like every other unknown `/v1`
        // path, so it is non-differential too.
        .route(
            "/",
            axum::routing::any(async || ApiError::not_found("not found")),
        )
        .route(
            "/{*rest}",
            axum::routing::any(async || ApiError::not_found("not found")),
        )
        // Set per router: method_not_allowed_fallback only rewrites the
        // MethodRouters that exist in *this* router when called — it
        // does not reach routes nested later. Before the route_layer,
        // so a Member's wrong-method probe still answers the same 403
        // as every other admin path.
        .method_not_allowed_fallback(wire::method_not_allowed)
        .route_layer(middleware::from_fn(auth::require_admin));
    let member_routes = Router::new()
        .route("/status", get(health::status))
        .route("/verbs/node", post(verbs::verb_node))
        .route("/verbs/neighbors", post(verbs::verb_neighbors))
        .route("/verbs/history", post(verbs::verb_history))
        .route("/verbs/search", post(search::verb_search))
        .route("/mcp", post(mcp::mcp))
        // A wrong method on a live route keeps the one error shape
        // instead of axum's empty-body 405.
        .method_not_allowed_fallback(wire::method_not_allowed);
    // The auth layer wraps everything routed so far — including the
    // fallback, so even nonexistent paths answer 401 to unauthenticated
    // callers. `/healthz` is added *after* the layer: its exemption is
    // structural, not a path comparison inside the middleware.
    let authenticated = Router::new()
        .nest("/v1", member_routes.nest("/admin", admin_routes))
        // Unknown paths leave in the same `{"error": …}` shape as every
        // other error, and — being registered before the auth layer —
        // still answer 401 to unauthenticated callers.
        .fallback(async || ApiError::not_found("not found"));
    let authenticated = match metrics_access {
        MetricsAccess::Admin => authenticated.merge(
            Router::new()
                .route("/metrics", get(metrics::metrics))
                .method_not_allowed_fallback(wire::method_not_allowed)
                .route_layer(middleware::from_fn(auth::require_admin)),
        ),
        MetricsAccess::Unauthenticated => authenticated,
    };
    let app = authenticated
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ))
        .route("/healthz", get(health::healthz))
        // For /healthz only: the /v1 routers carry their own (auth-
        // wrapped) method fallbacks, set before nesting.
        .method_not_allowed_fallback(wire::method_not_allowed);
    let app = match metrics_access {
        MetricsAccess::Admin => app,
        MetricsAccess::Unauthenticated => app.merge(
            Router::new()
                .route("/metrics", get(metrics::metrics))
                .method_not_allowed_fallback(wire::method_not_allowed),
        ),
    }
    .with_state(state)
    .layer(middleware::from_fn_with_state(
        request_timeout::RequestTimeout::new(protection.request_timeout),
        request_timeout::enforce,
    ));

    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_requested) = oneshot::channel::<config::ServerShutdown>();
    let handle = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_requested.await;
            })
            .await;
        if let Err(e) = &result {
            tracing::error!("server exited: {e}");
        }
        result
    });

    Ok(RunningServer {
        local_addr,
        handle,
        shutdown: Some(shutdown),
    })
}

/// Bind a worker-only Prometheus listener with the same access-policy choice
/// as the API server. This server owns no database or application routes: a
/// scrape only encodes the supplied process-local registry.
pub async fn serve_metrics(
    listen: std::net::SocketAddr,
    metrics: Metrics,
    metrics_access: MetricsAccess,
    bootstrap_token: Option<String>,
) -> anyhow::Result<RunningServer> {
    let bootstrap_token = match metrics_access {
        MetricsAccess::Admin => bootstrap_token
            .context("a bootstrap Admin token is required for an authenticated metrics listener")?,
        MetricsAccess::Unauthenticated => String::new(),
    };
    let state = Arc::new(MetricsServerState {
        metrics,
        bootstrap_token,
    });
    let app = Router::new()
        .route("/metrics", get(metrics::standalone_metrics))
        .method_not_allowed_fallback(wire::method_not_allowed);
    let app = match metrics_access {
        MetricsAccess::Admin => app.route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate_metrics_admin,
        )),
        MetricsAccess::Unauthenticated => app,
    }
    .with_state(state);

    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding worker metrics listener {listen}"))?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_requested) = oneshot::channel::<config::ServerShutdown>();
    let handle = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_requested.await;
            })
            .await;
        if let Err(error) = &result {
            tracing::error!("worker metrics server exited: {error}");
        }
        result
    });
    Ok(RunningServer {
        local_addr,
        handle,
        shutdown: Some(shutdown),
    })
}

#[cfg(test)]
mod tests {
    //! Cross-crate drift guard: the id grammar (yg-verbs) duplicates
    //! vocabulary that the Shard writer (yg-shard) owns, on purpose —
    //! the grammar is a wire contract, not a schema read. yg-api is the
    //! crate that assembles the whole read path, so this is the test
    //! that catches a node kind added to yg-shard but never taught to
    //! the id grammar. (The edge-kind filter vocabulary and the graph
    //! table/column names no longer need a guard: yg-verbs reads both
    //! straight from yg-shard, so drift there is a compile error.)

    /// Every node kind the Shard writer mints must produce an external
    /// id the read path can parse and round-trip — otherwise a node is
    /// stored and counted but its `node`/`neighbors` ids 400 as
    /// malformed (this is exactly how `pkg:` would have regressed).
    #[test]
    fn every_node_kind_prefix_round_trips_through_the_id_grammar() {
        let repo = "github.com/acme/widgets";
        for kind in yg_shard::NodeKind::ALL {
            // A representative local part per prefix; the grammar cares
            // only about the prefix and a non-empty local part.
            let local = match kind.id_prefix() {
                "sym" => "cmd/main.go#Hello".to_string(),
                other => format!("{other}-local/part"),
            };
            let external = format!("{}:{repo}:{local}", kind.id_prefix());
            let parsed = yg_verbs::VerbId::parse(&external)
                .unwrap_or_else(|e| panic!("{kind:?} id {external:?} must parse: {e}"));
            assert_eq!(parsed.repo, repo, "{kind:?}");
            assert_eq!(parsed.external(), external, "{kind:?} must round-trip");
        }
    }
}
