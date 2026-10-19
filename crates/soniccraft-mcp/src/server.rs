//! The MCP server: rmcp [`ServerHandler`] with explicit `tools/list` and `tools/call`.
//!
//! rmcp provides the lifecycle (both protocol eras), request concurrency, cancellation tokens and the
//! progress channel; this module maps tool calls onto the engine and its reports onto MCP results.
//!
//! Safety: every engine the server builds has `allow_command` set from [`ServerOptions::allow_command`]
//! (or env `SONICCRAFT_ALLOW_COMMAND`), so a timeline runs local programs only when the user opted in,
//! and `confine_outputs_to` set to the call's base directory, so a timeline's own `output.path` cannot
//! write elsewhere (an explicit `output_path` argument is the caller's choice and stays free). Renders
//! run at most [`ServerOptions::max_concurrent_renders`] at a time.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CompleteRequestParams, CompleteResult,
    CustomRequest, CustomResult, ErrorCode, Implementation, JsonObject, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::json;
use soniccraft_engine::{
    CancelFlag, Engine, EngineError, EnvSource, ProgressCallback, ProgressEvent, RenderOverrides,
    ValidateOptions,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::args::{self, DryRunArgs, LoadedTimeline, RenderArgs, ValidateArgs, VoicesArgs};
use crate::paths::{self, BaseDirSource};
use crate::progress::ProgressReporter;
use crate::results::{self, Era, Shape};
use crate::tools::{self, TOOL_NAMES, ToolKind, ToolSpec};
use crate::{
    ALLOW_COMMAND_ENV, DEFAULT_MAX_CONCURRENT_RENDERS, MAX_RENDERS_ENV, SERVER_NAME, SERVER_TITLE,
    ServerOptions,
};

/// `_meta` key of the server's identity in 2026-07-28 results (SEP-2575).
const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";

/// How often a queued render repeats its "waiting" progress notification.
const QUEUE_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// The SonicCraft MCP server. Cheap to clone (rmcp shares one instance across concurrent requests).
#[derive(Clone)]
pub(crate) struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    /// The shared engine, `allow_command` already set from the server options.
    engine: Engine,
    base_dir: PathBuf,
    config: ServerConfig,
    renders: Arc<RenderRegistry>,
    /// One permit per render that may run at once.
    render_slots: Arc<Semaphore>,
    max_renders: usize,
}

impl Server {
    pub(crate) fn new(engine: Engine, options: &ServerOptions) -> Self {
        let project_env = std::env::var_os(paths::PROJECT_DIR_ENV);
        let (base_dir, source) =
            paths::default_base_dir(options.base_dir.as_deref(), project_env.as_deref());
        let allow_command = options.allow_command || env_flag(engine.env(), ALLOW_COMMAND_ENV);
        let max_renders = max_renders(options, engine.env());
        tracing::info!(
            base_dir = %base_dir.display(),
            from = source.describe(),
            allow_command,
            max_renders,
            "SonicCraft MCP server ready"
        );
        let config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation())
            .with_instructions(instructions(&base_dir, source));
        // Engines default to allowing `command` providers (the CLI renders the user's own files); tool
        // calls carry documents from anywhere, so the server decides.
        let mut engine_options = engine.options().clone();
        engine_options.allow_command = allow_command;
        let engine = engine.with_options(engine_options);
        Server {
            inner: Arc::new(Inner {
                engine,
                base_dir,
                config,
                renders: Arc::new(RenderRegistry::default()),
                render_slots: Arc::new(Semaphore::new(max_renders)),
                max_renders,
            }),
        }
    }

    /// The registry of running renders (cancelled and awaited at shutdown).
    pub(crate) fn renders(&self) -> Arc<RenderRegistry> {
        self.inner.renders.clone()
    }

    /// The engine for one call: a copy of the shared engine whose outputs from the timeline document
    /// must stay inside `base_dir`, with this call's progress callback chained after the configured one
    /// and/or the cache read switched off. The copy keeps the shared engine's environment
    /// ([`Engine::with_options`]) and its `allow_command`.
    fn engine_for(
        &self,
        progress: Option<ProgressCallback>,
        no_cache: bool,
        base_dir: &Path,
    ) -> Engine {
        let shared = &self.inner.engine;
        let mut options = shared.options().clone();
        options.confine_outputs_to = Some(base_dir.to_path_buf());
        if no_cache {
            options.read_cache = false;
        }
        if let Some(callback) = progress {
            options.progress = Some(match options.progress.take() {
                Some(outer) => Arc::new(move |event: &ProgressEvent| {
                    outer(event);
                    callback(event);
                }),
                None => callback,
            });
        }
        shared.clone().with_options(options)
    }

    async fn dispatch(
        &self,
        spec: &'static ToolSpec,
        args: JsonObject,
        ctx: RequestContext<RoleServer>,
        era: Era,
    ) -> CallToolResult {
        match spec.kind {
            ToolKind::Validate => self.validate(spec, &args, &ctx, era).await,
            ToolKind::DryRun => self.dry_run(spec, &args, &ctx, era).await,
            ToolKind::Render => self.render(spec, &args, &ctx, era).await,
            ToolKind::GetSchema => self.get_schema(spec, &args),
            ToolKind::ListProviders => self.list_providers(era).await,
            ToolKind::ListVoices => self.list_voices(spec, &args, &ctx, era).await,
        }
    }

    async fn validate(
        &self,
        spec: &ToolSpec,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
        era: Era,
    ) -> CallToolResult {
        let parsed: ValidateArgs = match args::parse(spec.name(), args) {
            Ok(parsed) => parsed,
            Err(error) => return results::error_message(error.0),
        };
        let detail = match args::detail(spec.name(), &parsed.detail) {
            Ok(detail) => detail,
            Err(error) => return results::error_message(error.0),
        };
        let loaded = match parsed.timeline.load(&self.inner.base_dir).await {
            Ok(loaded) => loaded,
            Err(error) => return results::error_message(error.0),
        };
        // `..Default::default()` keeps this compiling when the engine adds options.
        #[allow(clippy::needless_update)]
        let options = ValidateOptions {
            check_credentials_online: parsed.check_credentials.unwrap_or(false),
            ..Default::default()
        };
        let engine = self.engine_for(None, false, &loaded.base_dir);
        let report = tokio::select! {
            report = engine.validate(&loaded.source, &options) => report,
            () = ctx.ct.cancelled() => return cancelled(),
        };
        results::validation(&report, &display_name(&loaded), Shape { detail, era })
    }

    async fn dry_run(
        &self,
        spec: &ToolSpec,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
        era: Era,
    ) -> CallToolResult {
        let parsed: DryRunArgs = match args::parse(spec.name(), args) {
            Ok(parsed) => parsed,
            Err(error) => return results::error_message(error.0),
        };
        let detail = match args::detail(spec.name(), &parsed.detail) {
            Ok(detail) => detail,
            Err(error) => return results::error_message(error.0),
        };
        let loaded = match parsed.timeline.load(&self.inner.base_dir).await {
            Ok(loaded) => loaded,
            Err(error) => return results::error_message(error.0),
        };
        let engine = self.engine_for(None, false, &loaded.base_dir);
        let report = tokio::select! {
            report = engine.plan(&loaded.source) => report,
            () = ctx.ct.cancelled() => return cancelled(),
        };
        results::plan(&report, &display_name(&loaded), Shape { detail, era })
    }

    async fn render(
        &self,
        spec: &ToolSpec,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
        era: Era,
    ) -> CallToolResult {
        let parsed: RenderArgs = match args::parse(spec.name(), args) {
            Ok(parsed) => parsed,
            Err(error) => return results::error_message(error.0),
        };
        let detail = match args::detail(spec.name(), &parsed.detail) {
            Ok(detail) => detail,
            Err(error) => return results::error_message(error.0),
        };
        let format = match args::output_format(spec.name(), &parsed.format) {
            Ok(format) => format,
            Err(error) => return results::error_message(error.0),
        };
        let loaded = match parsed.timeline.load(&self.inner.base_dir).await {
            Ok(loaded) => loaded,
            Err(error) => return results::error_message(error.0),
        };
        let shape = Shape { detail, era };
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let overrides = RenderOverrides {
            output_path: args::output_path(&parsed.output_path, &loaded.base_dir),
            format,
            export_subtitles: parsed.export_subtitles,
            cancel: Some(cancel.clone()),
            ..Default::default()
        };
        let progress = ctx
            .meta
            .get_progress_token()
            .map(|token| ProgressReporter::start(ctx.peer.clone(), token));
        let _registration = self.inner.renders.register(cancel.clone());
        // At most `max_renders` renders hold audio in memory at once; the others wait here.
        let Some(_slot) = self.render_slot(progress.as_ref(), ctx, &cancel).await else {
            return cancelled();
        };
        let engine = self.engine_for(
            progress.as_ref().map(ProgressReporter::callback),
            parsed.no_cache.unwrap_or(false),
            &loaded.base_dir,
        );
        // notifications/cancelled (or server shutdown) cancels ctx.ct: tell the engine to stop at its next
        // checkpoint (it removes partial files) and send no more progress. rmcp drops the response of a
        // cancelled request, so the failure built below is never sent in that case.
        let _watcher = AbortOnDrop(tokio::spawn({
            let ct = ctx.ct.clone();
            let cancel = cancel.clone();
            let closer = progress.as_ref().map(ProgressReporter::closer);
            async move {
                ct.cancelled().await;
                cancel.store(true, Ordering::SeqCst);
                if let Some(closer) = closer {
                    closer.close();
                }
            }
        }));
        let outcome = engine.render(&loaded.source, &overrides).await;
        drop(engine);
        if let Some(progress) = progress {
            if ctx.ct.is_cancelled() {
                drop(progress);
            } else {
                progress.finish().await;
            }
        }
        match outcome {
            Ok(report) => results::render(report, &loaded.base_dir, shape),
            Err(failure) => results::render_failure(&failure, &display_name(&loaded), shape),
        }
    }

    /// Wait for a render slot. While the render waits it sends a "waiting" progress notification every
    /// [`QUEUE_PROGRESS_INTERVAL`]; `None` when the request is cancelled (or the server shuts down) first.
    async fn render_slot(
        &self,
        progress: Option<&ProgressReporter>,
        ctx: &RequestContext<RoleServer>,
        cancel: &CancelFlag,
    ) -> Option<OwnedSemaphorePermit> {
        let slots = self.inner.render_slots.clone();
        if let Ok(permit) = slots.clone().try_acquire_owned() {
            return Some(permit);
        }
        let max = self.inner.max_renders;
        tracing::info!(max, "render queued until another render finishes");
        let started = Instant::now();
        let acquire = slots.acquire_owned();
        tokio::pin!(acquire);
        let mut ticker = tokio::time::interval(QUEUE_PROGRESS_INTERVAL);
        loop {
            tokio::select! {
                permit = &mut acquire => {
                    // The server may have begun shutting down while this render waited.
                    return permit.ok().filter(|_| !cancel.load(Ordering::SeqCst));
                }
                () = ctx.ct.cancelled() => return None,
                () = self.inner.renders.shutting_down() => return None,
                _ = ticker.tick() => {
                    if let Some(progress) = progress {
                        progress.waiting(&queue_message(max, started.elapsed()));
                    }
                }
            }
        }
    }

    fn get_schema(&self, spec: &ToolSpec, args: &JsonObject) -> CallToolResult {
        let parsed: args::SchemaArgs = match args::parse(spec.name(), args) {
            Ok(parsed) => parsed,
            Err(error) => return results::error_message(error.0),
        };
        match args::detail(spec.name(), &parsed.detail) {
            Ok(detail) => schema_result(detail),
            Err(error) => results::error_message(error.0),
        }
    }

    async fn list_providers(&self, era: Era) -> CallToolResult {
        // `providers()` probes the environment (binaries on PATH, the OS); keep it off the async workers.
        let engine = self.inner.engine.clone();
        match tokio::task::spawn_blocking(move || engine.providers()).await {
            Ok(report) => results::providers(
                &report,
                Shape {
                    detail: args::Detail::Auto,
                    era,
                },
            ),
            Err(error) => results::internal_error(&format!("listing providers failed: {error}")),
        }
    }

    async fn list_voices(
        &self,
        spec: &ToolSpec,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
        era: Era,
    ) -> CallToolResult {
        let parsed: VoicesArgs = match args::parse(spec.name(), args) {
            Ok(parsed) => parsed,
            Err(error) => return results::error_message(error.0),
        };
        let provider = parsed.provider.trim();
        if provider.is_empty() {
            return results::error_message(format!(
                "`provider` is empty. Pass a built-in provider name: {}.",
                builtin_provider_names()
            ));
        }
        // Tool arguments are not documents: accept `ElevenLabs` or `macos-say` for the exact spelling.
        let provider = soniccraft_core::ir::ProviderType::spelling_of(provider).unwrap_or(provider);
        let engine = self.inner.engine.clone();
        let outcome = tokio::select! {
            outcome = engine.voices(provider) => outcome,
            () = ctx.ct.cancelled() => return cancelled(),
        };
        match outcome {
            Ok(report) => results::voices(
                &report,
                Shape {
                    detail: args::Detail::Auto,
                    era,
                },
            ),
            Err(EngineError::InvalidArgument(message)) => {
                results::error_message(unknown_provider_message(&message))
            }
            Err(error) => results::error_message(format!(
                "Listing the voices of `{provider}` failed: {error}"
            )),
        }
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        self.inner.config.clone()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if let Some(cursor) = request.and_then(|r| r.cursor).filter(|c| !c.is_empty()) {
            return Err(ErrorData::invalid_params(
                format!(
                    "Invalid cursor {cursor:?}: tools/list returns every tool in one page and never issues \
                     a cursor."
                ),
                None,
            ));
        }
        let tools: Vec<Tool> = tools::specs()
            .iter()
            .map(|spec| spec.tool.clone())
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        add_server_info(&ctx, &mut result.meta);
        Ok(result)
    }

    // The server has no prompts or resources; rmcp answers these lists with empty results, which in the
    // 2026-07-28 revision carry the server's identity like every other result.

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let mut result = ListPromptsResult::default();
        add_server_info(&ctx, &mut result.meta);
        Ok(result)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut result = ListResourcesResult::default();
        add_server_info(&ctx, &mut result.meta);
        Ok(result)
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let mut result = ListResourceTemplatesResult::default();
        add_server_info(&ctx, &mut result.meta);
        Ok(result)
    }

    async fn complete(
        &self,
        _request: CompleteRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        let mut result = CompleteResult::default();
        add_server_info(&ctx, &mut result.meta);
        Ok(result)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools::find(name).map(|spec| spec.tool.clone())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(spec) = tools::find(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!(
                    "Unknown tool `{}`. Available tools: {}.",
                    request.name,
                    TOOL_NAMES.join(", ")
                ),
                Some(json!({ "tool": request.name, "available": TOOL_NAMES })),
            ));
        };
        let args = request.arguments.unwrap_or_default();
        let started = Instant::now();
        let id = ctx.id.clone();
        let era = Era::of(ctx.protocol_version().as_ref());
        let modern = is_modern(&ctx);
        // Argument problems are tool execution errors the model can correct (SEP-1303), never -32602.
        let mut result = if let Some(error) = check_argument_names(spec, &args) {
            error
        } else if let Err(error) = args::check_values(spec.name(), &spec.tool.input_schema, &args) {
            results::error_message(error.0)
        } else {
            AssertUnwindSafe(self.dispatch(spec, args, ctx, era))
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| {
                    let detail = panic_message(panic.as_ref());
                    tracing::error!(tool = spec.name(), %detail, "tool handler panicked");
                    results::internal_error(&format!("`{}` panicked: {detail}", spec.name()))
                })
        };
        if modern {
            insert_server_info(&mut result.meta);
        }
        tracing::info!(
            tool = spec.name(),
            %id,
            is_error = result.is_error.unwrap_or(false),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "tool call finished"
        );
        Ok(result.into())
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        // rmcp parses a request whose params do not match its method's shape as a "custom" request; for
        // methods this server implements that means malformed params, not an unknown method.
        let expected = match request.method.as_str() {
            "tools/call" => Some("{name: string, arguments?: object, _meta?: object}"),
            "tools/list" => Some("{cursor?: string, _meta?: object} or no params"),
            "ping" => Some("no params (or {_meta: object})"),
            "server/discover" => Some("{_meta: object}"),
            _ => None,
        };
        Err(match expected {
            Some(shape) => ErrorData::invalid_params(
                format!("Invalid params for `{}`: expected {shape}.", request.method),
                None,
            ),
            None => ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!(
                    "Method not found: `{}`. This server implements initialize, ping, server/discover, \
                     tools/list and tools/call.",
                    request.method
                ),
                Some(json!({ "method": request.method })),
            ),
        })
    }
}

/// `serverInfo`: name, title, version, description and website.
pub(crate) fn implementation() -> Implementation {
    let mut info = Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .with_title(SERVER_TITLE)
        .with_description(
            "Compiles declarative audio timelines (TTS speech, music beds, sound effects) into ducked, \
             loudness-mastered audio with subtitles.",
        );
    let repository = env!("CARGO_PKG_REPOSITORY");
    if !repository.is_empty() {
        info = info.with_website_url(repository);
    }
    info
}

/// The server instructions (key words first; well under the 4096 characters clients keep).
pub(crate) fn instructions(base_dir: &Path, source: BaseDirSource) -> String {
    format!(
        "SonicCraft compiles declarative YAML/JSON audio timelines (TTS speech, music beds, sound effects) \
into sidechain-ducked, EBU R128 loudness-mastered audio (wav, mp3, flac, ogg, opus, m4a) with WebVTT/SRT \
subtitles and word timings.

Workflow: write a timeline -> soniccraft_validate_timeline -> soniccraft_dry_run -> soniccraft_render.
1. Once per session, call soniccraft_get_schema (the DSL quick reference; detail: \"full\" adds the JSON \
Schema) and soniccraft_list_providers (which TTS providers work here; `mock`, and `macos_say` on macOS, \
need no API key).
2. Validate after every edit. Each diagnostic has a stable `code`, a `path` (e.g. \
tracks[1].sequence[0].start_relative.after_cue), line/column and usually a `hint`: apply the hints \
mechanically, fix all errors at once and validate again until `valid` is true. An invalid timeline is a \
successful call with valid: false.
3. Dry-run (free; never calls paid TTS APIs) to check the total duration, cue order (`pushed_from_ms` marks \
cues the overlap policy moved), ducking and `characters_to_synthesize`; show the ASCII diagram when timing \
matters.
4. Render. The result gives the absolute output path, duration, loudness against its target and the side \
files. TTS audio is cached, so re-renders only pay for changed cues. A failed render returns isError: true \
with diagnostics: fix them and render again. Large reports come back compact (items_truncated, \
items_total); pass detail: \"full\" when you need every item.

Paths: pass `yaml_path` (absolute is safest) or the document itself as `yaml_content`. Relative tool paths \
(yaml_path, output_path, and paths inside yaml_content) resolve against `base_dir`, default {base} (from \
{origin}); paths inside a timeline file resolve against that file's directory. A timeline's own \
`output.path` must stay inside base_dir; pass `output_path` (and `format`, e.g. mp3 for an .mp3 file) to \
write elsewhere or in another format. Never put API keys in a timeline; providers read them from \
environment variables (ELEVENLABS_API_KEY, OPENAI_API_KEY, CARTESIA_API_KEY).",
        base = base_dir.display(),
        origin = source.describe(),
    )
}

/// The `soniccraft_get_schema` result: the quick reference as text, plus (for `detail: "full"`, or
/// `auto` while it fits the agent budget) the JSON Schema as text and in `structuredContent`.
fn schema_result(detail: args::Detail) -> CallToolResult {
    let schema = soniccraft_engine::timeline_schema();
    let schema_text = schema.to_string();
    let full = || {
        let mut result = CallToolResult::success(vec![
            rmcp::model::ContentBlock::text(crate::DSL_QUICK_REFERENCE),
            rmcp::model::ContentBlock::text(schema_text.clone()),
        ]);
        result.structured_content = Some(json!({
            "dsl_version": soniccraft_core::DSL_VERSION,
            "schema_id": soniccraft_core::schema::SCHEMA_ID,
            "quick_reference": crate::DSL_QUICK_REFERENCE,
            "schema": schema,
        }));
        result
    };
    match detail {
        args::Detail::Full => return full(),
        args::Detail::Auto => {
            let result = full();
            if results::result_len(&result) <= results::AGENT_RESULT_BUDGET {
                return result;
            }
        }
        args::Detail::Summary => {}
    }
    let note = format!(
        "The JSON Schema ({} characters) is left out to keep this result small; the quick reference \
         above covers every field. Call soniccraft_get_schema with detail: \"full\" to get it, or run \
         `soniccraft schema`.",
        schema_text.chars().count()
    );
    let mut result = CallToolResult::success(vec![
        rmcp::model::ContentBlock::text(crate::DSL_QUICK_REFERENCE),
        rmcp::model::ContentBlock::text(note.clone()),
    ]);
    result.structured_content = Some(json!({
        "dsl_version": soniccraft_core::DSL_VERSION,
        "schema_id": soniccraft_core::schema::SCHEMA_ID,
        "quick_reference": crate::DSL_QUICK_REFERENCE,
        "detail": "summary",
        "full_report": note,
    }));
    result
}

/// A placeholder result for a cancelled call; rmcp never sends it.
fn cancelled() -> CallToolResult {
    results::error_message("The request was cancelled.")
}

fn display_name(loaded: &LoadedTimeline) -> String {
    match &loaded.source.path {
        Some(path) => path.display().to_string(),
        None => "(inline yaml_content)".to_string(),
    }
}

fn builtin_provider_names() -> String {
    soniccraft_core::providers::all()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The engine's unknown-provider message as one sentence, naming the built-in providers once.
fn unknown_provider_message(message: &str) -> String {
    let mut text = message.trim().trim_end_matches('.').to_string();
    if let Some(first) = text.get(..1) {
        text.replace_range(..1, &first.to_ascii_uppercase());
    }
    if !text.to_ascii_lowercase().contains("built-in providers") {
        text.push_str(&format!(
            "; built-in providers: {}",
            builtin_provider_names()
        ));
    }
    text.push('.');
    text
}

/// True when `name` is set to `1`, `true`, `yes` or `on` (any case).
fn env_flag(env: &EnvSource, name: &str) -> bool {
    env.get(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// [`ServerOptions::max_concurrent_renders`], else env `SONICCRAFT_MCP_MAX_RENDERS`, else the default.
fn max_renders(options: &ServerOptions, env: &EnvSource) -> usize {
    if let Some(renders) = options.max_concurrent_renders {
        return renders.clamp(1, Semaphore::MAX_PERMITS);
    }
    let Some(value) = env.get(MAX_RENDERS_ENV) else {
        return DEFAULT_MAX_CONCURRENT_RENDERS;
    };
    match value.parse::<usize>() {
        Ok(renders) if renders >= 1 => renders.min(Semaphore::MAX_PERMITS),
        _ => {
            tracing::warn!(
                %value,
                "{MAX_RENDERS_ENV} must be a positive integer; using {DEFAULT_MAX_CONCURRENT_RENDERS}"
            );
            DEFAULT_MAX_CONCURRENT_RENDERS
        }
    }
}

/// The progress message of a render waiting for a slot.
fn queue_message(max: usize, waited: Duration) -> String {
    let at_once = if max == 1 {
        "one render runs at a time".to_string()
    } else {
        format!("{max} renders run at a time")
    };
    if waited < Duration::from_secs(1) {
        format!("Waiting for another render to finish ({at_once})")
    } else {
        format!(
            "Still waiting for another render to finish ({at_once}; queued for {} s)",
            waited.as_secs()
        )
    }
}

/// True when the request uses the 2026-07-28 revision (or later).
fn is_modern(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.protocol_version()
        .is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
}

/// [`implementation`] as JSON, for `_meta`.
fn server_info_value() -> serde_json::Value {
    serde_json::to_value(implementation()).unwrap_or_else(|_| json!({"name": SERVER_NAME}))
}

/// In the 2026-07-28 revision, every result carries the server's identity in `_meta` so stateless
/// clients can identify the server (SEP-2575); `server/discover` already does through rmcp.
fn add_server_info(ctx: &RequestContext<RoleServer>, meta: &mut Option<MetaObject>) {
    if is_modern(ctx) {
        insert_server_info(meta);
    }
}

fn insert_server_info(meta: &mut Option<MetaObject>) {
    meta.get_or_insert_with(MetaObject::new)
        .0
        .insert(SERVER_INFO_META_KEY.into(), server_info_value());
}

/// Argument names the tool's schema does not declare: an `isError` result naming the valid arguments
/// (with a did-you-mean), with `structuredContent` `{error: "unknown_argument", tool, unknown, valid,
/// suggestions}`. Unknown keys still satisfy the `tools/call` request schema (`arguments` is any object),
/// so this is a tool execution error, not a protocol error (MCP 2025-11-25, SEP-1303).
fn check_argument_names(spec: &ToolSpec, args: &JsonObject) -> Option<CallToolResult> {
    let unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !spec.arg_names.iter().any(|name| name == key))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    let tool = spec.name();
    let valid = &spec.arg_names;
    let quoted: Vec<String> = unknown.iter().map(|k| format!("`{k}`")).collect();
    let mut message = if valid.is_empty() {
        format!("`{tool}` takes no arguments; remove {}.", quoted.join(", "))
    } else {
        format!(
            "Unknown argument{} {} for `{tool}`.",
            if unknown.len() == 1 { "" } else { "s" },
            quoted.join(", ")
        )
    };
    let mut suggestions = serde_json::Map::new();
    for key in &unknown {
        if let Some(suggestion) = suggest_argument(key, valid) {
            message.push_str(&format!(" Did you mean `{suggestion}` instead of `{key}`?"));
            suggestions.insert((*key).to_string(), json!(suggestion));
        }
    }
    if !valid.is_empty() {
        message.push_str(&format!(" Valid arguments: {}.", valid.join(", ")));
    }
    Some(results::error_with_details(
        message,
        json!({
            "error": "unknown_argument",
            "tool": tool,
            "unknown": unknown,
            "valid": valid,
            "suggestions": suggestions,
        }),
    ))
}

/// The intended argument for a name agents commonly guess, or a close spelling.
fn suggest_argument<'a>(key: &str, valid: &'a [String]) -> Option<&'a str> {
    let normalized = key.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    let alias = match normalized.as_str() {
        "timeline_path" | "file" | "file_path" | "filepath" | "timeline_file" | "yaml_file"
        | "filename" | "json_path" | "input" | "input_path" => "yaml_path",
        "timeline" | "yaml" | "content" | "text" | "yaml_text" | "timeline_yaml" | "json"
        | "json_content" | "document" | "timeline_content" | "source" => "yaml_content",
        "output" | "out" | "output_file" | "out_path" | "outfile" | "output_filename" | "dest" => {
            "output_path"
        }
        "subtitles" | "export_subtitle" | "captions" | "subs" => "export_subtitles",
        "cwd" | "dir" | "directory" | "project_dir" | "root" | "base" | "basedir"
        | "working_dir" | "workdir" => "base_dir",
        "nocache" | "skip_cache" | "force" | "refresh" => "no_cache",
        "credentials" | "check_keys" | "verify_credentials" | "check_credential" => {
            "check_credentials"
        }
        "name" | "provider_name" | "tts_provider" | "type" => "provider",
        _ => "",
    };
    if let Some(name) = valid.iter().find(|name| name.as_str() == alias) {
        return Some(name.as_str());
    }
    soniccraft_core::diagnostics::did_you_mean(key, valid.iter().map(String::as_str))
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Aborts a task when dropped (the cancellation watcher must not outlive its render).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Renders in flight (running or waiting for a slot), so shutdown can cancel them and wait for their
/// cleanup.
#[derive(Default)]
pub(crate) struct RenderRegistry {
    state: Mutex<RegistryState>,
    idle: Notify,
    /// Set by [`RenderRegistry::cancel_all`]; wakes renders waiting for a slot.
    shutdown: AtomicBool,
    shutdown_notify: Notify,
}

#[derive(Default)]
struct RegistryState {
    next: u64,
    active: BTreeMap<u64, CancelFlag>,
}

impl RenderRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, flag: CancelFlag) -> RenderRegistration {
        let mut state = self.lock();
        let id = state.next;
        state.next += 1;
        state.active.insert(id, flag);
        RenderRegistration {
            registry: self.clone(),
            id,
        }
    }

    /// Ask every render to stop (running ones at their next checkpoint, queued ones at once) and let no
    /// queued render start; returns how many there were.
    pub(crate) fn cancel_all(&self) -> usize {
        let count = {
            let state = self.lock();
            for flag in state.active.values() {
                flag.store(true, Ordering::SeqCst);
            }
            state.active.len()
        };
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
        count
    }

    /// Resolves once [`RenderRegistry::cancel_all`] has been called.
    pub(crate) async fn shutting_down(&self) {
        loop {
            let notified = self.shutdown_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    /// Wait until no render is running, at most `timeout`; true when idle.
    pub(crate) async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().active.is_empty() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.lock().active.is_empty();
            }
        }
    }
}

/// Removes a render from the registry when the render ends (normally, by panic or by cancellation).
struct RenderRegistration {
    registry: Arc<RenderRegistry>,
    id: u64,
}

impl Drop for RenderRegistration {
    fn drop(&mut self) {
        let empty = {
            let mut state = self.registry.lock();
            state.active.remove(&self.id);
            state.active.is_empty()
        };
        if empty {
            self.registry.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static ToolSpec {
        tools::find(name).unwrap()
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true));
        result.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn unknown_arguments_are_tool_errors_naming_the_valid_ones() {
        let args: JsonObject = serde_json::from_value(json!({"timeline_path": "a.yaml"})).unwrap();
        let result = check_argument_names(spec(tools::RENDER_TOOL), &args).unwrap();
        let message = error_text(&result);
        assert!(message.contains("`timeline_path`"), "{message}");
        assert!(message.contains("Did you mean `yaml_path`"), "{message}");
        assert!(
            message.contains("Valid arguments: yaml_content, yaml_path, path"),
            "{message}"
        );
        let details = result.structured_content.unwrap();
        assert_eq!(details["error"], "unknown_argument");
        assert_eq!(details["unknown"], json!(["timeline_path"]));
        assert_eq!(
            details["suggestions"],
            json!({"timeline_path": "yaml_path"})
        );
        assert_eq!(details["valid"][0], "yaml_content");

        let args: JsonObject = serde_json::from_value(json!({"x": 1})).unwrap();
        let result = check_argument_names(spec(tools::PROVIDERS_TOOL), &args).unwrap();
        assert!(error_text(&result).contains("takes no arguments"));
        let result = check_argument_names(spec(tools::SCHEMA_TOOL), &args).unwrap();
        assert!(error_text(&result).ends_with("Valid arguments: detail."));

        let args: JsonObject = serde_json::from_value(json!({"yaml_contnet": "x"})).unwrap();
        let result = check_argument_names(spec(tools::VALIDATE_TOOL), &args).unwrap();
        assert!(error_text(&result).contains("Did you mean `yaml_content`"));

        let args: JsonObject = serde_json::from_value(
            json!({"path": "a", "base_dir": "/", "check_credentials": true, "detail": "full"}),
        )
        .unwrap();
        assert!(check_argument_names(spec(tools::VALIDATE_TOOL), &args).is_none());
    }

    #[test]
    fn unknown_provider_messages_name_the_providers_once() {
        let engine_message = "unknown provider `bogus`; built-in providers: elevenlabs, openai, cartesia, \
                              kokoro, piper, macos_say, command, mock";
        let message = unknown_provider_message(engine_message);
        assert_eq!(
            message,
            "Unknown provider `bogus`; built-in providers: elevenlabs, openai, cartesia, kokoro, piper, \
             macos_say, command, mock."
        );
        assert_eq!(message.matches("elevenlabs").count(), 1);
        let other = unknown_provider_message("unknown provider `x`.");
        assert!(
            other.starts_with("Unknown provider `x`; built-in providers: elevenlabs"),
            "{other}"
        );
        assert!(other.ends_with("mock.") && !other.contains(".;"), "{other}");
    }

    #[test]
    fn server_options_and_environment() {
        let env = |vars: &[(&str, &str)]| EnvSource::fixed(vars.iter().copied());
        for (value, allowed) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("", false),
            ("please", false),
        ] {
            assert_eq!(
                env_flag(&env(&[(ALLOW_COMMAND_ENV, value)]), ALLOW_COMMAND_ENV),
                allowed,
                "{value:?}"
            );
        }
        assert!(!env_flag(&env(&[]), ALLOW_COMMAND_ENV));

        let none = ServerOptions::default();
        assert_eq!(
            max_renders(&none, &env(&[])),
            DEFAULT_MAX_CONCURRENT_RENDERS
        );
        assert_eq!(max_renders(&none, &env(&[(MAX_RENDERS_ENV, "5")])), 5);
        for bad in ["0", "-1", "two", "1.5"] {
            assert_eq!(
                max_renders(&none, &env(&[(MAX_RENDERS_ENV, bad)])),
                DEFAULT_MAX_CONCURRENT_RENDERS,
                "{bad}"
            );
        }
        let one = ServerOptions::default().with_max_concurrent_renders(1);
        assert_eq!(max_renders(&one, &env(&[(MAX_RENDERS_ENV, "5")])), 1);

        assert_eq!(
            queue_message(2, Duration::ZERO),
            "Waiting for another render to finish (2 renders run at a time)"
        );
        assert_eq!(
            queue_message(1, Duration::from_secs(20)),
            "Still waiting for another render to finish (one render runs at a time; queued for 20 s)"
        );
    }

    #[test]
    fn engines_get_the_command_policy_and_the_output_confinement() {
        let engine = || {
            Engine::new(soniccraft_engine::EngineOptions::default())
                .with_env(EnvSource::fixed([("UNRELATED", "1")]))
        };
        let server = Server::new(engine(), &ServerOptions::with_base_dir("/project"));
        let call = server.engine_for(None, true, Path::new("/project/sub"));
        assert!(
            !call.options().allow_command,
            "the server forbids command providers by default"
        );
        assert_eq!(
            call.options().confine_outputs_to.as_deref(),
            Some(Path::new("/project/sub"))
        );
        assert!(!call.options().read_cache);
        assert_eq!(call.env().get("UNRELATED").as_deref(), Some("1"));

        let allowed = Server::new(
            engine(),
            &ServerOptions::with_base_dir("/project").with_allow_command(true),
        );
        assert!(
            allowed
                .engine_for(None, false, Path::new("/p"))
                .options()
                .allow_command
        );
        let by_env = Server::new(
            Engine::new(soniccraft_engine::EngineOptions::default())
                .with_env(EnvSource::fixed([(ALLOW_COMMAND_ENV, "1")])),
            &ServerOptions::with_base_dir("/project"),
        );
        assert!(
            by_env
                .engine_for(None, false, Path::new("/p"))
                .options()
                .allow_command
        );
    }

    #[test]
    fn instructions_fit_client_limits() {
        let text = instructions(Path::new("/some/project"), BaseDirSource::ProjectEnv);
        assert!(text.len() <= 4096, "{} chars", text.len());
        assert!(text.starts_with("SonicCraft compiles"));
        for tool in [
            tools::VALIDATE_TOOL,
            tools::DRY_RUN_TOOL,
            tools::RENDER_TOOL,
        ] {
            assert!(text.contains(tool), "{tool}");
        }
        assert!(text.contains("/some/project") && text.contains("CLAUDE_PROJECT_DIR"));
    }

    #[tokio::test]
    async fn queued_renders_wake_on_shutdown() {
        let registry = Arc::new(RenderRegistry::default());
        let waiter = tokio::spawn({
            let registry = registry.clone();
            async move { registry.shutting_down().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        registry.cancel_all();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("woken")
            .unwrap();
        // Later waiters return at once.
        tokio::time::timeout(Duration::from_millis(100), registry.shutting_down())
            .await
            .expect("already shutting down");
    }

    #[tokio::test]
    async fn registry_cancels_and_waits() {
        let registry = Arc::new(RenderRegistry::default());
        assert!(registry.wait_idle(Duration::from_millis(10)).await);
        let flag: CancelFlag = Arc::new(AtomicBool::new(false));
        let registration = registry.register(flag.clone());
        assert_eq!(registry.cancel_all(), 1);
        assert!(flag.load(Ordering::SeqCst));
        assert!(!registry.wait_idle(Duration::from_millis(20)).await);
        let waiter = tokio::spawn({
            let registry = registry.clone();
            async move { registry.wait_idle(Duration::from_secs(5)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(registration);
        assert!(waiter.await.unwrap());
    }
}
