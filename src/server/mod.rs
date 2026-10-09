mod openai_compat;
mod oauth_handlers;

use crate::cli::AppConfig;
use crate::models::AnthropicRequest;
use crate::router::Router;
use crate::providers::ProviderRegistry;
use crate::auth::TokenStore;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        Html, IntoResponse, Response, sse::{Event, Sse},
    },
    routing::{get, post},
    Form, Json, Router as AxumRouter,
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use futures::stream::StreamExt;

/// Application state shared across handlers (always used behind `Arc`)
pub struct AppState {
    /// Current runtime configuration. Held behind a lock so the admin UI can
    /// hot-reload tier mappings without restarting the process.
    pub config: std::sync::RwLock<AppConfig>,
    /// Router rebuilt together with `config` (it embeds router settings).
    pub router: std::sync::RwLock<Router>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub token_store: TokenStore,
    pub config_path: std::path::PathBuf,
}

/// Start the HTTP server
pub async fn start_server(config: AppConfig, config_path: std::path::PathBuf) -> anyhow::Result<()> {
    // Initialize OAuth token store FIRST (needed by provider registry)
    let token_store = TokenStore::default()
        .map_err(|e| anyhow::anyhow!("Failed to initialize token store: {}", e))?;

    let existing_tokens = token_store.list_providers();
    if !existing_tokens.is_empty() {
        info!("🔐 Loaded {} OAuth tokens from storage", existing_tokens.len());
    }

    // Initialize provider registry from config (with token store)
    let provider_registry = Arc::new(
        ProviderRegistry::from_configs(&config.providers, Some(token_store.clone()))
            .map_err(|e| anyhow::anyhow!("Failed to initialize provider registry: {}", e))?
    );

    // Build router with the set of exactly-registered model names so that
    // auto-mapping does not rewrite models that providers actually expose.
    let registered_models: std::collections::HashSet<String> = provider_registry
        .list_models()
        .into_iter()
        .collect();
    let router = Router::new(config.clone(), registered_models);

    info!("📦 Loaded {} providers with {} models",
        provider_registry.list_providers().len(),
        provider_registry.list_models().len()
    );

    let state = Arc::new(AppState {
        config: std::sync::RwLock::new(config.clone()),
        router: std::sync::RwLock::new(router),
        provider_registry,
        token_store,
        config_path,
    });

    // Build router
    let app = AxumRouter::new()
        .route("/", get(serve_admin))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/chat/completions", post(handle_openai_chat_completions))
        .route("/v1/models", get(list_gateway_models))
        .route("/health", get(health_check))
        .route("/api/models", get(get_models))
        .route("/api/providers", get(get_providers))
        .route("/api/models-config", get(get_models_config))
        .route(
            "/api/tier-mapping",
            get(get_tier_mapping).post(save_tier_mapping),
        )
        .route(
            "/api/model-visibility",
            get(get_model_visibility).post(save_model_visibility),
        )
        .route("/api/config", get(get_config))
        .route("/api/config", post(update_config))
        .route("/api/config/json", get(get_config_json))
        .route("/api/config/json", post(update_config_json))
        .route("/api/restart", post(restart_server))
        // OAuth endpoints
        .route("/api/oauth/authorize", post(oauth_handlers::oauth_authorize))
        .route("/api/oauth/exchange", post(oauth_handlers::oauth_exchange))
        .route("/api/oauth/callback", get(oauth_handlers::oauth_callback))
        .route("/auth/callback", get(oauth_handlers::oauth_callback))  // OpenAI Codex uses this path
        .route("/api/oauth/tokens", get(oauth_handlers::oauth_list_tokens))
        .route("/api/oauth/tokens/delete", post(oauth_handlers::oauth_delete_token))
        .route("/api/oauth/tokens/refresh", post(oauth_handlers::oauth_refresh_token));

    // Clone state before moving it
    let oauth_state = state.clone();
    let app = app.with_state(state);

    // Bind to main address
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;

    info!("🚀 Server listening on {}", addr);

    // Start OAuth callback server on port 1455 (required for OpenAI Codex)
    // This is necessary because OpenAI's OAuth app only allows localhost:1455/auth/callback
    tokio::spawn(async move {
        let oauth_callback_app = AxumRouter::new()
            .route("/auth/callback", get(oauth_handlers::oauth_callback))
            .with_state(oauth_state);

        let oauth_addr = "127.0.0.1:1455";
        match TcpListener::bind(oauth_addr).await {
            Ok(oauth_listener) => {
                info!("🔐 OAuth callback server listening on {}", oauth_addr);
                if let Err(e) = axum::serve(oauth_listener, oauth_callback_app).await {
                    error!("OAuth callback server error: {}", e);
                }
            }
            Err(e) => {
                // Don't fail if port 1455 is already in use - just warn
                error!("⚠️  Failed to bind OAuth callback server on {}: {}", oauth_addr, e);
                error!("⚠️  OpenAI Codex OAuth will not work. Port 1455 must be available.");
            }
        }
    });

    // Start main server
    axum::serve(listener, app).await?;

    Ok(())
}

/// Serve Admin UI
async fn serve_admin() -> impl IntoResponse {
    Html(include_str!("admin.html"))
}

/// Health check endpoint
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "claude-code-mux"
    }))
}

/// REMOVED: This endpoint was for LiteLLM integration which has been removed.
/// Models are now managed through the provider registry and config.
async fn get_models(State(_state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::ParseError("This endpoint has been removed. Use /api/models-config instead.".to_string()))
}

/// Get current routing configuration
async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.read().unwrap();
    Json(serde_json::json!({
        "server": {
            "host": config.server.host,
            "port": config.server.port,
        },
        "router": {
            "default": config.router.default,
            "background": config.router.background,
            "think": config.router.think,
            "websearch": config.router.websearch,
        }
    }))
}

/// Update configuration
#[derive(serde::Deserialize)]
struct ConfigUpdate {
    // Router models
    default_model: String,
    background_model: Option<String>,
    think_model: Option<String>,
    websearch_model: Option<String>,
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    Form(update): Form<ConfigUpdate>,
) -> Result<Html<String>, AppError> {
    // Read current config
    let config_path = &state.config_path;
    let config_str = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;

    let mut config: toml::Value = toml::from_str(&config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to parse config: {}", e)))?;

    // Update router section
    if let Some(router) = config.get_mut("router").and_then(|v| v.as_table_mut()) {
        router.insert("default".to_string(), toml::Value::String(update.default_model));

        if let Some(bg) = update.background_model {
            router.insert("background".to_string(), toml::Value::String(bg));
        }

        if let Some(think) = update.think_model {
            router.insert("think".to_string(), toml::Value::String(think));
        }

        if let Some(ws) = update.websearch_model {
            router.insert("websearch".to_string(), toml::Value::String(ws));
        }
    }

    // Write back to file
    let new_config_str = toml::to_string_pretty(&config)
        .map_err(|e| AppError::ParseError(format!("Failed to serialize config: {}", e)))?;

    std::fs::write(config_path, new_config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    info!("✅ Configuration updated successfully");

    Ok(Html("<div class='px-4 py-3 rounded-xl bg-primary/20 border border-primary/50 text-foreground text-sm'>✅ Configuration saved successfully! Please restart the server to apply changes.</div>".to_string()))
}

/// Get providers configuration
async fn get_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.config.read().unwrap().providers.clone())
}

/// Get models configuration
async fn get_models_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.config.read().unwrap().models.clone())
}

/// Get full configuration as JSON (for admin UI)
async fn get_config_json(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.read().unwrap();
    Json(serde_json::json!({
        "server": {
            "host": config.server.host,
            "port": config.server.port,
        },
        "router": {
            "default": config.router.default,
            "background": config.router.background,
            "think": config.router.think,
            "websearch": config.router.websearch,
        },
        "providers": config.providers,
        "models": config.models,
    }))
}

// ---------------------------------------------------------------------------
// Tier mapping (admin UI): pick the backend model for each Claude Code tier
// (Opus / Sonnet / Haiku buttons). Persisted in ccm.toml and hot-reloaded.
// ---------------------------------------------------------------------------

/// Wire model name emitted by Claude Code's Opus tier button.
const TIER_OPUS_WIRE_NAME: &str = "claude-opus-5-5";
/// Wire model name emitted by Claude Code's Sonnet tier button.
const TIER_SONNET_WIRE_NAME: &str = "claude-sonnet-5-5";

#[derive(serde::Deserialize)]
struct TierMappingUpdate {
    opus: String,
    sonnet: String,
    haiku: String,
}

/// First enabled provider whose model list contains `model`.
fn find_provider_for_model(config: &AppConfig, model: &str) -> Option<String> {
    config
        .providers
        .iter()
        .filter(|p| p.is_enabled())
        .find(|p| p.models.iter().any(|m| m == model))
        .map(|p| p.name.clone())
}

/// Currently effective model for a tier wire name (mapping or the name itself).
fn tier_current_model(config: &AppConfig, wire_name: &str) -> String {
    config
        .models
        .iter()
        .find(|m| m.name == wire_name)
        .and_then(|m| m.mappings.iter().min_by_key(|mp| mp.priority))
        .map(|mp| mp.actual_model.clone())
        .unwrap_or_else(|| wire_name.to_string())
}

async fn get_tier_mapping(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let config = state.config.read().unwrap();
    let mut models = state.provider_registry.list_models();
    models.sort();
    Ok(Json(serde_json::json!({
        "opus": tier_current_model(&config, TIER_OPUS_WIRE_NAME),
        "sonnet": tier_current_model(&config, TIER_SONNET_WIRE_NAME),
        "haiku": config.router.background.clone(),
        "models": models,
    })))
}

/// Render a `[[models]]` entry text block for a tier wire name.
fn models_entry_text(name: &str, provider: &str, actual_model: &str) -> String {
    format!(
        "[[models]]\nname = \"{}\"\n\n[[models.mappings]]\npriority = 1\nprovider = \"{}\"\nactual_model = \"{}\"\n",
        name, provider, actual_model
    )
}

/// Byte range (line indices, end-exclusive) of the `[[models]]` block whose
/// `name` field equals `name`, or `None` if absent.
fn find_models_block_range(lines: &[String], name: &str) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[models]]" {
            let start = i;
            let mut j = i + 1;
            let mut found = false;
            while j < lines.len() {
                let trimmed = lines[j].trim_start();
                if !trimmed.starts_with('[') {
                    // Ordinary key line inside the block.
                    if let Some(v) = lines[j].trim().strip_prefix("name = ") {
                        if v.trim().trim_matches('"') == name {
                            found = true;
                        }
                    }
                } else if trimmed.starts_with("[[models.") {
                    // Sub-table of this entry (e.g. [[models.mappings]]) —
                    // still part of the block.
                } else {
                    // Any other table header ends this block.
                    break;
                }
                j += 1;
            }
            if found {
                return Some((start, j));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Line index of `key = ...` inside the `[router]` section, if present.
/// Exact key match (`background` does not match `background_default`).
fn find_router_line(lines: &[String], key: &str) -> Option<usize> {
    let mut in_router = false;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "[router]" {
            in_router = true;
            continue;
        }
        if in_router {
            if trimmed.starts_with('[') {
                return None;
            }
            if let Some(rest) = trimmed.strip_prefix(key) {
                if rest.trim_start().starts_with('=') {
                    return Some(i);
                }
            }
        }
    }
    None
}

async fn save_tier_mapping(
    State(state): State<Arc<AppState>>,
    Json(update): Json<TierMappingUpdate>,
) -> Result<Json<serde_json::Value>, AppError> {
    // 1. Resolve and validate selections ("": clear tier override).
    let (opus_selection, sonnet_selection, haiku_selection) = {
        let config = state.config.read().unwrap();

        let resolve_mapping = |model: &str| -> Result<Option<(String, String)>, AppError> {
            if model.is_empty() {
                return Ok(None);
            }
            let provider = find_provider_for_model(&config, model).ok_or_else(|| {
                AppError::ParseError(format!("Model '{}' is not registered on any enabled provider", model))
            })?;
            Ok(Some((provider, model.to_string())))
        };

        let opus = resolve_mapping(&update.opus)?;
        let sonnet = resolve_mapping(&update.sonnet)?;

        let haiku = if update.haiku.is_empty() {
            None
        } else if state.provider_registry.get_provider_for_model(&update.haiku).is_err() {
            return Err(AppError::ParseError(format!(
                "Model '{}' is not registered on any enabled provider",
                update.haiku
            )));
        } else {
            Some(update.haiku.clone())
        };

        (opus, sonnet, haiku)
    };

    // 2. Text-edit ccm.toml (preserves comments and untouched sections).
    let config_path = &state.config_path;
    let doc = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;
    let mut lines: Vec<String> = doc.lines().map(str::to_string).collect();

    for (wire_name, selection) in [
        (TIER_OPUS_WIRE_NAME, &opus_selection),
        (TIER_SONNET_WIRE_NAME, &sonnet_selection),
    ] {
        match selection {
            Some((provider, model)) => {
                let entry = models_entry_text(wire_name, provider, model);
                match find_models_block_range(&lines, wire_name) {
                    Some((start, end)) => {
                        lines.splice(
                            start..end,
                            entry.lines().map(str::to_string).collect::<Vec<_>>(),
                        );
                    }
                    None => {
                        if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                            lines.push(String::new());
                        }
                        lines.extend(entry.lines().map(str::to_string));
                    }
                }
            }
            None => {
                if let Some((start, end)) = find_models_block_range(&lines, wire_name) {
                    lines.drain(start..end);
                }
            }
        }
    }

    match haiku_selection {
        Some(model) => {
            // Remember the original background once, so "default" can restore it.
            if find_router_line(&lines, "background_default").is_none() {
                if let Some(i) = find_router_line(&lines, "background") {
                    let original = lines[i].replacen("background", "background_default", 1);
                    lines.insert(i + 1, original);
                }
            }
            let line = format!("background = \"{}\"", model);
            if let Some(i) = find_router_line(&lines, "background") {
                lines[i] = line;
            } else if let Some(i) = lines.iter().position(|l| l.trim() == "[router]") {
                lines.insert(i + 1, line);
            }
        }
        None => {
            // Restore the remembered original background, if one was captured.
            if let Some(i) = find_router_line(&lines, "background_default") {
                let original = lines[i].replacen("background_default", "background", 1);
                if let Some(bi) = find_router_line(&lines, "background") {
                    lines[bi] = original;
                } else if let Some(ri) = lines.iter().position(|l| l.trim() == "[router]") {
                    lines.insert(ri + 1, original);
                }
                lines.remove(i);
            }
        }
    }

    let mut new_doc = lines.join("\n");
    if !new_doc.ends_with('\n') {
        new_doc.push('\n');
    }

    // 3. The edited document must parse as a valid AppConfig.
    let new_config: AppConfig = toml::from_str(&new_doc)
        .map_err(|e| AppError::ParseError(format!("Edited config failed to parse: {}", e)))?;

    // 4. Persist.
    std::fs::write(config_path, new_doc)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    // 5. Hot reload: rebuild the router (it embeds router settings) and swap
    // the runtime config in place — no process restart needed.
    let registered_models: std::collections::HashSet<String> =
        state.provider_registry.list_models().into_iter().collect();
    let new_router = Router::new(new_config.clone(), registered_models);
    *state.config.write().unwrap() = new_config;
    *state.router.write().unwrap() = new_router;

    info!("✅ Tier mapping saved and hot-reloaded (opus={:?}, sonnet={:?}, haiku={:?})",
        update.opus, update.sonnet, update.haiku);

    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "Tier mapping saved and applied (hot reload)",
    })))
}

// ---------------------------------------------------------------------------
// Model visibility (admin UI): which models show up in gateway discovery
// (GET /v1/models). Persisted as an optional top-level `hidden_models = [...]`
// key in ccm.toml. Visibility does NOT affect routing — hidden models stay
// callable and remain selectable in tier mappings.
// ---------------------------------------------------------------------------

/// Read the `hidden_models` list from the raw config document (top-level key).
fn read_hidden_models(doc: &str) -> Vec<String> {
    toml::from_str::<toml::Value>(doc)
        .ok()
        .and_then(|value| {
            value
                .get("hidden_models")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// Line index of a top-level `hidden_models = ...` assignment (before the
/// first `[section]` header), if present.
fn find_hidden_models_line(lines: &[String]) -> Option<usize> {
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            return None; // Reached a section header — key is absent.
        }
        if let Some(rest) = trimmed.strip_prefix("hidden_models") {
            if rest.trim_start().starts_with('=') {
                return Some(i);
            }
        }
    }
    None
}

/// GET /api/model-visibility — all registered models grouped by provider,
/// with their current hidden state.
async fn get_model_visibility(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let doc = std::fs::read_to_string(&state.config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;
    let hidden = read_hidden_models(&doc);

    let config = state.config.read().unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for provider_config in config.providers.iter().filter(|p| p.is_enabled()) {
        for model in &provider_config.models {
            if !seen.insert(model.clone()) {
                continue;
            }
            models.push(serde_json::json!({
                "id": model,
                "provider": provider_config.name,
                "hidden": hidden.contains(model),
            }));
        }
    }

    let mut hidden_sorted = hidden;
    hidden_sorted.sort();
    Ok(Json(serde_json::json!({ "models": models, "hidden": hidden_sorted })))
}

#[derive(serde::Deserialize)]
struct ModelVisibilityUpdate {
    hidden: Vec<String>,
}

/// POST /api/model-visibility — full replacement of the hidden list.
async fn save_model_visibility(
    State(state): State<Arc<AppState>>,
    Json(update): Json<ModelVisibilityUpdate>,
) -> Result<Json<serde_json::Value>, AppError> {
    let registered: std::collections::HashSet<String> =
        state.provider_registry.list_models().into_iter().collect();
    for model in &update.hidden {
        if !registered.contains(model) {
            return Err(AppError::ParseError(format!(
                "Model '{}' is not registered on any enabled provider",
                model
            )));
        }
    }

    let config_path = &state.config_path;
    let doc = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;
    let mut lines: Vec<String> = doc.lines().map(str::to_string).collect();

    let kv = format!(
        "hidden_models = [{}]",
        update
            .hidden
            .iter()
            .map(|m| format!("\"{}\"", m))
            .collect::<Vec<_>>()
            .join(", ")
    );
    match find_hidden_models_line(&lines) {
        Some(i) => lines[i] = kv,
        None => {
            let insert_at = lines
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .unwrap_or(lines.len());
            lines.insert(insert_at, kv);
        }
    }

    let mut new_doc = lines.join("\n");
    if !new_doc.ends_with('\n') {
        new_doc.push('\n');
    }
    std::fs::write(config_path, new_doc)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    info!(
        "✅ Model visibility updated: {} hidden",
        update.hidden.len()
    );

    // No hot reload needed: visibility is consulted from disk on each
    // gateway discovery request and does not affect routing state.
    Ok(Json(serde_json::json!({
        "status": "ok",
        "hidden": update.hidden,
    })))
}

/// Remove null values from JSON (TOML doesn't support null)
fn remove_null_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for (_, v) in map.iter_mut() {
                remove_null_values(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                remove_null_values(item);
            }
        }
        _ => {}
    }
}

/// Update configuration via JSON (for admin UI)
async fn update_config_json(
    State(state): State<Arc<AppState>>,
    Json(mut new_config): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Remove null values (TOML doesn't support null)
    remove_null_values(&mut new_config);

    // Write back to config file
    let config_path = &state.config_path;

    // Read current config
    let config_str = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;

    let mut config: toml::Value = toml::from_str(&config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to parse config: {}", e)))?;

    // Update providers section
    if let Some(providers) = new_config.get("providers") {
        // Convert from serde_json::Value to toml::Value
        let providers_toml: toml::Value = serde_json::from_str(&providers.to_string())
            .map_err(|e| AppError::ParseError(format!("Failed to convert providers: {}", e)))?;

        if let Some(table) = config.as_table_mut() {
            table.insert("providers".to_string(), providers_toml);
        }
    }

    // Update models section
    if let Some(models) = new_config.get("models") {
        // Convert from serde_json::Value to toml::Value
        let models_toml: toml::Value = serde_json::from_str(&models.to_string())
            .map_err(|e| AppError::ParseError(format!("Failed to convert models: {}", e)))?;

        if let Some(table) = config.as_table_mut() {
            table.insert("models".to_string(), models_toml);
        }
    }

    // Update router section if provided
    if let Some(router) = new_config.get("router") {
        if let Some(router_table) = config.get_mut("router").and_then(|v| v.as_table_mut()) {
            if let Some(default) = router.get("default") {
                if let Some(s) = default.as_str() {
                    router_table.insert("default".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(think) = router.get("think") {
                if let Some(s) = think.as_str() {
                    router_table.insert("think".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(ws) = router.get("websearch") {
                if let Some(s) = ws.as_str() {
                    router_table.insert("websearch".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(bg) = router.get("background") {
                if let Some(s) = bg.as_str() {
                    router_table.insert("background".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(auto_map) = router.get("auto_map_regex") {
                if let Some(s) = auto_map.as_str() {
                    router_table.insert("auto_map_regex".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(bg_regex) = router.get("background_regex") {
                if let Some(s) = bg_regex.as_str() {
                    router_table.insert("background_regex".to_string(), toml::Value::String(s.to_string()));
                }
            }
        }
    }

    // Write back to file
    let new_config_str = toml::to_string_pretty(&config)
        .map_err(|e| AppError::ParseError(format!("Failed to serialize config: {}", e)))?;

    std::fs::write(config_path, new_config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    info!("✅ Configuration updated successfully via admin UI");

    Ok(Json(serde_json::json!({
        "status": "success",
        "message": "Configuration saved successfully"
    })))
}

/// Restart server automatically using shell script
async fn restart_server(State(state): State<Arc<AppState>>) -> Response {
    info!("🔄 Server restart requested via UI");

    let port = state.config.read().unwrap().server.port;

    // Create a shell script to handle restart
    match create_and_execute_restart_script(port) {
        Ok(_) => {
            info!("✅ Restart script initiated");

            let response = Html("<div class='px-4 py-3 rounded-xl bg-green-500/20 border border-green-500/50 text-foreground text-sm'><strong>✅ Server restarting...</strong><br/>Shutting down current instance and starting new one.</div>").into_response();

            // Shutdown current process after a short delay
            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                info!("Shutting down for restart...");
                std::process::exit(0);
            });

            response
        }
        Err(e) => {
            error!("Failed to initiate restart: {}", e);
            Html(format!("<div class='px-4 py-3 rounded-xl bg-red-500/20 border border-red-500/50 text-foreground text-sm'><strong>❌ Restart failed</strong><br/>Error: {}</div>", e)).into_response()
        }
    }
}

/// Create and execute a shell script that waits for shutdown and restarts
fn create_and_execute_restart_script(port: u16) -> std::io::Result<()> {
    use std::process::Command;
    use std::fs;

    // Get current executable path and PID
    let exe_path = std::env::current_exe()?;
    let current_pid = std::process::id();

    info!("Creating restart script for PID: {} on port: {}", current_pid, port);

    #[cfg(unix)]
    {
        // Create shell script
        let script_content = format!(
            r#"#!/bin/bash
# Wait for old process to exit
while kill -0 {} 2>/dev/null; do
    sleep 0.1
done
# Start new server
{} start --port {} > /dev/null 2>&1 &
"#,
            current_pid,
            exe_path.display(),
            port
        );

        let script_path = "/tmp/ccm_restart.sh";
        fs::write(script_path, script_content)?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(script_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(script_path, perms)?;
        }

        // Execute script in background
        Command::new("sh")
            .arg(script_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        info!("Restart script started");
    }

    #[cfg(windows)]
    {
        // Create batch script for Windows
        let script_content = format!(
            r#"@echo off
:wait
tasklist /FI "PID eq {}" 2>NUL | find /I /N "ccm.exe">NUL
if "%ERRORLEVEL%"=="0" (
    timeout /t 1 /nobreak > nul
    goto wait
)
start "" "{}" start --port {}
"#,
            current_pid,
            exe_path.display(),
            port
        );

        let script_path = std::env::temp_dir().join("ccm_restart.bat");
        fs::write(&script_path, script_content)?;

        // Execute batch file
        Command::new("cmd")
            .args(&["/C", "start", "/B", script_path.to_str().unwrap()])
            .spawn()?;
    }

    Ok(())
}

/// Handle /v1/chat/completions requests (OpenAI-compatible endpoint)
async fn handle_openai_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(openai_request): Json<openai_compat::OpenAIRequest>,
) -> Result<Response, AppError> {
    let model = openai_request.model.clone();
    info!("Received OpenAI-compatible request for model: {}", model);

    // 1. Transform OpenAI request to Anthropic format
    let mut anthropic_request = openai_compat::transform_openai_to_anthropic(openai_request)
        .map_err(|e| AppError::ParseError(format!("Failed to transform OpenAI request: {}", e)))?;

    info!("Transformed OpenAI request to Anthropic format");

    // 2. Route the request (may modify system prompt to remove CCM-SUBAGENT-MODEL tag)
    let decision = state
        .router
        .read()
        .unwrap()
        .route(&mut anthropic_request)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🎯 Routed to: {} ({})",
        decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    let runtime_models = state.config.read().unwrap().models.clone();
    if let Some(model_config) = runtime_models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for model: {}", model_config.mappings.len(), decision.model_name);

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!("🎯 Using forced provider from X-Provider header: {}", provider_name);
        }

        // Sort mappings by priority (or filter by forced provider)
        let mut sorted_mappings = model_config.mappings.clone();

        if let Some(ref provider_name) = forced_provider {
            // Filter to only the specified provider
            sorted_mappings.retain(|m| m.provider == *provider_name);
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(format!(
                    "Provider '{}' not found in mappings for model '{}'",
                    provider_name, decision.model_name
                )));
            }
        } else {
            // Use priority ordering
            sorted_mappings.sort_by_key(|m| m.priority);
        }

        // Try each mapping in priority order (or just the forced one)
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

                // Check if streaming is requested
                let is_streaming = anthropic_request.stream == Some(true);

                if is_streaming {
                    // Streaming not fully implemented for OpenAI format yet
                    info!("⚠️ Streaming requested but not fully supported for OpenAI format, falling back to non-streaming");
                }

                // Non-streaming request
                match provider.send_message(anthropic_request.clone()).await {
                    Ok(anthropic_response) => {
                        info!("✅ Request succeeded with provider: {}", mapping.provider);

                        // Transform Anthropic response to OpenAI format
                        let openai_response = openai_compat::transform_anthropic_to_openai(
                            anthropic_response,
                            model.clone(),
                        );

                        return Ok(Json(openai_response).into_response());
                    }
                    Err(e) => {
                        info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                        continue;
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup): {}", decision.model_name);

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Call provider
            let anthropic_response = provider.send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Transform to OpenAI format
            let openai_response = openai_compat::transform_anthropic_to_openai(
                anthropic_response,
                model,
            );

            return Ok(Json(openai_response).into_response());
        }

        error!("❌ No model mapping or provider found for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for model: {}",
            decision.model_name
        )));
    }
}

/// Convert a provider byte stream (raw SSE) into an axum SSE response.
///
/// Chunks are re-framed through a stateful buffer (handles events split
/// across TCP chunks) and re-emitted with proper `event:` / `data:` fields.
/// This is required because axum's `Event::data` panics on strings that
/// contain newlines, so raw multi-line SSE frames cannot be passed through.
fn sse_response_from_provider_stream(
    stream: std::pin::Pin<
        Box<
            dyn futures::stream::Stream<
                    Item = Result<bytes::Bytes, crate::providers::error::ProviderError>,
                > + Send,
        >,
    >,
) -> Response {
    let reframer = crate::providers::sse_bridge::SseReframer::new(stream);
    let sse_stream = reframer.map(|result| {
        result
            .map(|ev| {
                let mut event = Event::default();
                if let Some(name) = ev.event {
                    event = event.event(name);
                }
                event.data(ev.data)
            })
            .map_err(|e| {
                error!("Stream error: {}", e);
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })
    });
    Sse::new(sse_stream).into_response()
}

/// Handle GET /v1/models (Anthropic gateway model discovery).
///
/// Lists the union of models exposed by all enabled providers (deduplicated,
/// config order preserved). Claude Code only offers non-`claude-*` gateway
/// models inside its picker if they are presented under a `claude-*` id, so
/// every non-Claude model is additionally exposed as a slot entry
/// `claude-<model>` (display name stays the real model name); the bare
/// non-Claude id is omitted to avoid duplicates.
async fn list_gateway_models(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut seen = std::collections::HashSet::new();
    let mut data = Vec::new();

    let provider_configs = state.config.read().unwrap().providers.clone();
    // Models hidden via the admin UI are excluded from discovery but stay
    // routable (direct calls keep working).
    let hidden: std::collections::HashSet<String> =
        read_hidden_models(&std::fs::read_to_string(&state.config_path).unwrap_or_default())
            .into_iter()
            .collect();
    for provider_config in provider_configs.iter().filter(|p| p.is_enabled()) {
        for model in &provider_config.models {
            if !seen.insert(model.clone()) {
                continue;
            }
            if hidden.contains(model) {
                continue;
            }
            if state.provider_registry.get_provider_for_model(model).is_err() {
                continue;
            }
            if model.starts_with("claude-") {
                data.push(serde_json::json!({
                    "type": "model",
                    "id": model,
                    "display_name": model,
                }));
            } else {
                // Claude Code only accepts ids made of lowercase letters,
                // digits and dashes — sanitize everything else to a dash
                // (e.g. gpt-6.1-sol -> claude-gpt-6-1-sol). The router
                // resolves these back to the real model via normalized
                // comparison.
                let slot_id: String = format!(
                    "claude-{}",
                    model
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() {
                            c.to_ascii_lowercase()
                        } else {
                            '-'
                        })
                        .collect::<String>()
                );
                data.push(serde_json::json!({
                    "type": "model",
                    "id": slot_id,
                    "display_name": model,
                }));
            }
        }
    }

    Ok(Json(serde_json::json!({ "data": data, "has_more": false })))
}

/// Handle /v1/messages requests (both streaming and non-streaming)
async fn handle_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request_json): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    let model = request_json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    info!("Received request for model: {}", model);

    // DEBUG: Log request body for debugging
    if let Ok(json_str) = serde_json::to_string_pretty(&request_json) {
        tracing::debug!("📥 Incoming request body:\n{}", json_str);
    }

    // 1. Parse request for routing decision (mutable for tag extraction)
    let mut request_for_routing: AnthropicRequest = serde_json::from_value(request_json.clone())
        .map_err(|e| {
            tracing::error!("❌ Failed to parse request: {}", e);
            AppError::ParseError(format!("Invalid request format: {}", e))
        })?;

    // 2. Route the request (may modify system prompt to remove CCM-SUBAGENT-MODEL tag)
    let decision = state
        .router
        .read()
        .unwrap()
        .route(&mut request_for_routing)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🎯 Routed to: {} ({})",
        decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    let runtime_models = state.config.read().unwrap().models.clone();
    if let Some(model_config) = runtime_models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for model: {}", model_config.mappings.len(), decision.model_name);

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())  // Ignore empty strings
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!("🎯 Using forced provider from X-Provider header: {}", provider_name);
        }

        // Sort mappings by priority (or filter by forced provider)
        let mut sorted_mappings = model_config.mappings.clone();

        if let Some(ref provider_name) = forced_provider {
            // Filter to only the specified provider
            sorted_mappings.retain(|m| m.provider == *provider_name);
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(format!(
                    "Provider '{}' not found in mappings for model '{}'",
                    provider_name, decision.model_name
                )));
            }
        } else {
            // Use priority ordering
            sorted_mappings.sort_by_key(|m| m.priority);
        }

        // Try each mapping in priority order (or just the forced one)
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Parse request as Anthropic format
                let mut anthropic_request: AnthropicRequest = serde_json::from_value(request_json.clone())
                    .map_err(|e| AppError::ParseError(format!("Invalid request format: {}", e)))?;

                // Save original model name for response
                let original_model = anthropic_request.model.clone();

                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

                // Update system if modified during routing
                anthropic_request.system = request_for_routing.system.clone();

                // Check if streaming is requested
                let is_streaming = anthropic_request.stream == Some(true);

                if is_streaming {
                    // Streaming request
                    info!("🌊 Streaming request to provider: {}", mapping.provider);

                    match provider.send_message_stream(anthropic_request).await {
                        Ok(stream) => {
                            info!("✅ Streaming request started with provider: {}", mapping.provider);

                            // Convert byte stream to SSE response
                            // Provider returns raw SSE bytes; re-frame them
                            // into axum Events via the stateful SSE bridge.
                            return Ok(sse_response_from_provider_stream(stream));
                        }
                        Err(e) => {
                            info!("⚠️ Provider {} streaming failed: {}, trying next fallback", mapping.provider, e);
                            continue;
                        }
                    }
                } else {
                    // Non-streaming request (original behavior)
                    match provider.send_message(anthropic_request).await {
                        Ok(mut response) => {
                            // Restore original model name in response
                            response.model = original_model;
                            info!("✅ Request succeeded with provider: {}, response model: {}", mapping.provider, response.model);
                            return Ok(Json(response).into_response());
                        }
                        Err(e) => {
                            info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                            continue;
                        }
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup): {}", decision.model_name);

            // Parse request as Anthropic format
            let mut anthropic_request: AnthropicRequest = serde_json::from_value(request_json.clone())
                .map_err(|e| AppError::ParseError(format!("Invalid request format: {}", e)))?;

            // Save original model name for response
            let original_model = anthropic_request.model.clone();

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Update system if modified during routing
            anthropic_request.system = request_for_routing.system.clone();

            // Honor streaming requests in the direct-lookup path too. This
            // branch previously always used the non-streaming call, which
            // forwarded `stream: true` upstream and then failed to parse the
            // SSE body as JSON.
            if anthropic_request.stream == Some(true) {
                match provider.send_message_stream(anthropic_request).await {
                    Ok(stream) => {
                        info!("🌊 Streaming request started via direct lookup for model: {}", decision.model_name);
                        return Ok(sse_response_from_provider_stream(stream));
                    }
                    Err(e) => {
                        error!("❌ Streaming request failed (direct lookup): {}", e);
                        return Err(AppError::ProviderError(e.to_string()));
                    }
                }
            }

            // Call provider
            let mut provider_response = provider.send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Restore original model name in response
            provider_response.model = original_model;

            // Return provider response
            return Ok(Json(provider_response).into_response());
        }

        error!("❌ No model mapping or provider found for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for model: {}",
            decision.model_name
        )));
    }
}

/// Handle /v1/messages/count_tokens requests
async fn handle_count_tokens(
    State(state): State<Arc<AppState>>,
    Json(request_json): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    let model = request_json.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    info!("Received count_tokens request for model: {}", model);

    // 1. Parse as CountTokensRequest first
    use crate::models::CountTokensRequest;
    let count_request: CountTokensRequest = serde_json::from_value(request_json.clone())
        .map_err(|e| AppError::ParseError(format!("Invalid count_tokens request format: {}", e)))?;

    // 2. Create a minimal AnthropicRequest for routing
    let mut routing_request = AnthropicRequest {
        model: count_request.model.clone(),
        messages: count_request.messages.clone(),
        max_tokens: 1024, // Dummy value for routing
        system: count_request.system.clone(),
        tools: count_request.tools.clone(),
        thinking: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        stream: None,
        metadata: None,
    };
    let decision = state
        .router
        .read()
        .unwrap()
        .route(&mut routing_request)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🧮 Routed count_tokens: {} → {} ({})",
        model, decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    let runtime_models = state.config.read().unwrap().models.clone();
    if let Some(model_config) = runtime_models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for token counting: {}", model_config.mappings.len(), decision.model_name);

        // Sort mappings by priority
        let mut sorted_mappings = model_config.mappings.clone();
        sorted_mappings.sort_by_key(|m| m.priority);

        // Try each mapping in priority order
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying token count mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Update model to actual model name
                let mut count_request_for_provider = count_request.clone();
                count_request_for_provider.model = mapping.actual_model.clone();

                // Call provider's count_tokens
                match provider.count_tokens(count_request_for_provider).await {
                    Ok(response) => {
                        info!("✅ Token count succeeded with provider: {}", mapping.provider);
                        return Ok(Json(response).into_response());
                    }
                    Err(e) => {
                        info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                        continue;
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for token counting: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for token counting: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup) for token counting: {}", decision.model_name);

            // Update model to routed model
            let mut count_request_for_provider = count_request.clone();
            count_request_for_provider.model = decision.model_name.clone();

            // Call provider's count_tokens
            let response = provider.count_tokens(count_request_for_provider)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            info!("✅ Token count completed via provider");
            return Ok(Json(response).into_response());
        }

        error!("❌ No model mapping or provider found for token counting: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for token counting: {}",
            decision.model_name
        )));
    }
}

/// Application error types
#[derive(Debug)]
pub enum AppError {
    RoutingError(String),
    ParseError(String),
    ProviderError(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::RoutingError(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::ParseError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::ProviderError(msg) => (StatusCode::BAD_GATEWAY, msg),
        };

        let body = Json(serde_json::json!({
            "error": {
                "type": "error",
                "message": message
            }
        }));

        (status, body).into_response()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::RoutingError(msg) => write!(f, "Routing error: {}", msg),
            AppError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            AppError::ProviderError(msg) => write!(f, "Provider error: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}
