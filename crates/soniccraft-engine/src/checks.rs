//! `validate()` and the environment checks shared by validation, dry runs and renders (DESIGN.md §7,
//! "environment (engine)"): asset files, provider availability (credentials, the key-sending host policy,
//! local programs, local servers), voices that exist here (macOS voices, piper models), command templates,
//! ffmpeg for the output format, provider text limits, muted ducking triggers and provider setting notes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use soniccraft_core::diagnostics::{self, codes};
use soniccraft_core::ir::{NodeId, OutputFormat, Program, ProviderInstance, ProviderType};
use soniccraft_core::{Diagnostic, SpanIndex, providers};
use soniccraft_dsp::decode::{DecodeOptions, ProbeInfo};
use soniccraft_dsp::encode::EncodeFormat;
use soniccraft_dsp::ffmpeg::Ffmpeg;
use soniccraft_tts::{CachedMeta, CredentialStatus, TtsClient, TtsError, TtsErrorKind};

use crate::cancel::blocking;
use crate::context::{CueRequest, LowerOverrides, Session, cue_requests};
use crate::diag::{self, CueRef, Notes};
use crate::plan::{self, DryRunInput};
use crate::{Engine, TimelineSource, TimelineSummary, ValidateOptions, ValidationReport};

/// How long the reachability probe of a local TTS server may take.
pub(crate) const SERVER_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// The hint of a provider that runs a local program the engine does not allow.
pub(crate) const ALLOW_COMMAND_HINT: &str = "start the MCP server with --allow-command (or SONICCRAFT_ALLOW_COMMAND=1) to let timelines run \
     local programs";

/// What the environment checks do.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CheckOptions {
    /// Probe every asset (length, decodability). Renders decode them fully instead.
    pub probe_assets: bool,
    /// Verify credentials with the providers (network).
    pub online: bool,
}

/// Results of [`environment`].
#[derive(Debug, Default)]
pub(crate) struct Environment {
    /// Positioned diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Cache metadata per request (same order as the requests).
    pub cached: Vec<Option<CachedMeta>>,
    /// Probe results of readable assets (only with `probe_assets`).
    pub probes: HashMap<PathBuf, ProbeInfo>,
    /// Provider notes already reported (renders add the ones synthesis produces).
    pub notes: Notes,
    /// Provider instances already reported as unusable here (later checks skip them).
    pub unusable: HashSet<String>,
    /// Provider instances the `allow_command` policy blocks (nothing about them is checked further).
    pub blocked: HashSet<String>,
}

/// [`crate::Engine::validate`].
pub(crate) async fn validate(
    engine: &Engine,
    source: &TimelineSource,
    options: &ValidateOptions,
) -> ValidationReport {
    let session = Session::open(engine, source, LowerOverrides::default()).await;
    let mut diagnostics = session.diagnostics.clone();
    let Some(program) = session.program.clone() else {
        // The document has errors, but the files it names can be checked all the same.
        diagnostics.extend(document_assets(&session, true).await);
        diagnostics::sort(&mut diagnostics);
        return ValidationReport {
            valid: !diagnostics::has_errors(&diagnostics),
            diagnostics,
            summary: None,
        };
    };
    let (requests, mut request_errors) = cue_requests(&program);
    session.attach(&mut request_errors);
    diagnostics.extend(request_errors);
    let env = environment(
        engine,
        &session,
        &program,
        &requests,
        CheckOptions {
            probe_assets: true,
            online: options.check_credentials_online,
        },
    )
    .await;
    diagnostics.extend(env.diagnostics);
    let summary = summary(&program, &requests, &env.cached);
    // Checks that need lengths (a `trim_start_ms` past the end of its source, overlaps under the `error`
    // policy, an empty timeline, ...) are made by the layout; validation reports its errors (those that
    // depend on estimated speech lengths as warnings) and the memory budget, `plan` everything.
    let input = DryRunInput {
        clip_frames: plan::clip_frames(&program, &env.probes),
        source_samples: plan::source_samples(&program, &env.probes),
        requests,
        cached: env.cached,
        ducking: false,
        diagram: false,
    };
    if let Ok(geometry) = plan::geometry(&session, &program, input).await {
        let mut layout = plan::dry_run_layout_diagnostics(&geometry.layout, true);
        layout.extend(plan::budget_diagnostics(engine, &program, &geometry));
        session.attach(&mut layout);
        diagnostics.extend(layout);
    }
    diagnostics::sort(&mut diagnostics);
    ValidationReport {
        valid: !diagnostics::has_errors(&diagnostics),
        diagnostics,
        summary: Some(summary),
    }
}

/// Run the environment checks for `program`.
pub(crate) async fn environment(
    engine: &Engine,
    session: &Session,
    program: &Arc<Program>,
    requests: &[CueRequest],
    options: CheckOptions,
) -> Environment {
    let ffmpeg = Ffmpeg::from_option(engine.options.ffmpeg_path.clone());
    let offline = {
        let program = Arc::clone(program);
        let requests: Vec<CueRequest> = requests.to_vec();
        let client = session.client.clone();
        let base_dir = session.base_dir.clone();
        let spans = Arc::clone(&session.spans);
        let ffmpeg = ffmpeg.clone();
        let context = OfflineContext {
            allow_command: engine.options.allow_command,
            format_source: FormatSource::of(&program, &session.overrides),
            // `output.path` holds the render's output path option when one is given: the caller chose it.
            confine: engine
                .options
                .confine_outputs_to
                .clone()
                .filter(|_| session.overrides.output_path.is_none()),
        };
        blocking(move || {
            Ok(offline_checks(
                &program, &requests, &client, &base_dir, &spans, &ffmpeg, options, &context,
            ))
        })
        .await
    };
    let mut env = match offline {
        Ok(env) => env,
        Err(stop) => Environment {
            diagnostics: vec![Diagnostic::error(
                codes::ASSET_UNREADABLE,
                format!("internal error: the environment checks failed ({stop:?})"),
            )],
            cached: vec![None; requests.len()],
            ..Environment::default()
        },
    };
    let unreachable = server_checks(&session.client, program, requests, &mut env).await;
    env.diagnostics.extend(unreachable);
    if options.online {
        let online = online_checks(&session.client, program, requests, &env).await;
        env.diagnostics.extend(online);
    }
    env.diagnostics.extend(env.notes.drain_new(&session.spans));
    session.attach(&mut env.diagnostics);
    env
}

/// What the offline checks need beyond the program.
struct OfflineContext {
    allow_command: bool,
    format_source: FormatSource,
    confine: Option<PathBuf>,
}

/// Everything that needs only the filesystem and the environment variables.
#[allow(clippy::too_many_arguments)]
fn offline_checks(
    program: &Program,
    requests: &[CueRequest],
    client: &TtsClient,
    base_dir: &Path,
    spans: &SpanIndex,
    ffmpeg: &Ffmpeg,
    options: CheckOptions,
    context: &OfflineContext,
) -> Environment {
    let mut env = Environment {
        cached: requests.iter().map(|r| client.cached(&r.request)).collect(),
        ..Environment::default()
    };
    let decode = DecodeOptions {
        ffmpeg: ffmpeg.clone(),
    };
    let uses = program_assets(program);
    check_assets(&uses, base_dir, options.probe_assets, &decode, &mut env);
    check_providers(
        program,
        requests,
        client,
        base_dir,
        context.allow_command,
        &mut env,
    );
    check_command_templates(requests, &env.unusable, &mut env.diagnostics);
    check_voices(program, requests, client, base_dir, spans, &mut env);
    check_ffmpeg(program, ffmpeg, context.format_source, &mut env.diagnostics);
    check_text_lengths(program, requests, &mut env.diagnostics);
    check_muted_triggers(program, &mut env.diagnostics);
    if let Some(dir) = &context.confine
        && let Some(d) = crate::files::document_output_escape(program, base_dir, dir)
    {
        env.diagnostics.push(d);
    }
    collect_voice_notes(program, requests, &mut env.notes);
    env
}

/// `asset_not_found` / `asset_unreadable` for the files a document with errors names (no program was
/// built): the checks of [`environment`] that need nothing but the document.
pub(crate) async fn document_assets(session: &Session, probe: bool) -> Vec<Diagnostic> {
    if session.assets.is_empty() {
        return Vec::new();
    }
    let uses: Vec<AssetUse> = session
        .assets
        .iter()
        .map(|asset| AssetUse {
            source: asset.source.clone(),
            path: session.base_dir.join(&asset.source),
            field: asset.path.clone(),
            id: asset.id.clone(),
        })
        .collect();
    let positions: HashMap<String, (Option<u32>, Option<u32>)> = session
        .assets
        .iter()
        .map(|a| (a.path.clone(), (a.line, a.column)))
        .collect();
    let base_dir = session.base_dir.clone();
    let checked = blocking(move || {
        let mut env = Environment::default();
        let decode = DecodeOptions::default();
        check_assets(&uses, &base_dir, probe, &decode, &mut env);
        Ok(env.diagnostics)
    })
    .await;
    let mut out = checked.unwrap_or_default();
    for d in &mut out {
        if let Some(&(line, column)) = d.path.as_ref().and_then(|p| positions.get(p)) {
            d.line = line;
            d.column = column;
        }
        session.spans.attach(d);
    }
    out
}

/// One `source` field that names an audio file.
#[derive(Debug, Clone)]
struct AssetUse {
    /// The value as written.
    source: String,
    /// Where it resolves to.
    path: PathBuf,
    /// Diagnostic path of the field (`tracks[1].clips[0].source`).
    field: String,
    /// The clip's id.
    id: String,
}

/// The `source` fields of the program's clips.
fn program_assets(program: &Program) -> Vec<AssetUse> {
    program
        .clips()
        .map(|(_, node, clip)| AssetUse {
            source: clip.source.clone(),
            path: clip.source_path.clone(),
            field: format!("{}.source", node.path),
            id: node.id.clone(),
        })
        .collect()
}

/// `asset_not_found` / `asset_unreadable` for every clip source.
fn check_assets(
    uses: &[AssetUse],
    base_dir: &Path,
    probe: bool,
    decode: &DecodeOptions,
    env: &mut Environment,
) {
    let mut status: HashMap<PathBuf, Result<(), AssetProblem>> = HashMap::new();
    for asset in uses {
        let path = &asset.path;
        let result = status
            .entry(path.clone())
            .or_insert_with(|| asset_status(path, &asset.source, probe, decode, &mut env.probes))
            .clone();
        match result {
            Ok(()) => {}
            Err(AssetProblem::NotFound) => env.diagnostics.push(
                Diagnostic::error(
                    codes::ASSET_NOT_FOUND,
                    format!(
                        "audio file `{}` of clip `{}` does not exist (looked for {})",
                        asset.source,
                        asset.id,
                        path.display()
                    ),
                )
                .with_path(asset.field.clone())
                .with_hint(diag::missing_asset_hint(&asset.source, path, base_dir))
                .with_related([asset.id.clone()]),
            ),
            Err(AssetProblem::Unreadable(reason)) => env.diagnostics.push(
                Diagnostic::error(
                    codes::ASSET_UNREADABLE,
                    format!(
                        "audio file `{}` of clip `{}` cannot be read: {reason}",
                        asset.source, asset.id
                    ),
                )
                .with_path(asset.field.clone())
                .with_hint(unreadable_hint(&reason))
                .with_related([asset.id.clone()]),
            ),
        }
    }
}

/// Why an asset cannot be used.
#[derive(Debug, Clone)]
enum AssetProblem {
    /// The file does not exist.
    NotFound,
    /// It exists but cannot be read or decoded (the reason).
    Unreadable(String),
}

/// Whether the asset at `path` exists and (with `probe`) decodes; probe results are kept in `probes`.
/// Reasons show the file as written, never its absolute path.
fn asset_status(
    path: &Path,
    written: &str,
    probe: bool,
    decode: &DecodeOptions,
    probes: &mut HashMap<PathBuf, ProbeInfo>,
) -> Result<(), AssetProblem> {
    let unreadable = |reason: String| {
        Err(AssetProblem::Unreadable(
            reason.replace(&path.display().to_string(), written),
        ))
    };
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AssetProblem::NotFound),
        Err(e) => unreadable(e.to_string()),
        Ok(meta) if meta.is_dir() => unreadable("it is a directory, not an audio file".to_string()),
        Ok(_) if !probe => Ok(()),
        Ok(_) => match soniccraft_dsp::decode::probe_file_with(path, decode) {
            Ok(info) if info.sample_rate == 0 || info.channels == 0 => {
                unreadable("the file contains no audio track".to_string())
            }
            Ok(info) => {
                probes.insert(path.to_path_buf(), info);
                Ok(())
            }
            Err(e) => unreadable(e.to_string()),
        },
    }
}

/// The hint of an `asset_unreadable` (validate) or `decode_failed` (render) diagnostic for `reason`.
pub(crate) fn unreadable_hint(reason: &str) -> String {
    if reason.contains("unsupported sample rate") {
        "Resample the file to a standard rate, e.g. `ffmpeg -i input -ar 48000 output.wav`"
            .to_string()
    } else if reason.contains("ffmpeg") {
        "Install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`) to decode this format, or convert \
         the file to WAV, FLAC or MP3"
            .to_string()
    } else if reason.contains("directory") {
        "Point `source` at an audio file inside the directory".to_string()
    } else {
        "Make sure the file is a complete wav, mp3, flac, ogg, m4a/aac or aiff file; re-export or convert it \
         (e.g. `ffmpeg -i input output.wav`)"
            .to_string()
    }
}

/// Cue refs of the requests that use `provider`.
fn provider_cues<'a>(
    program: &'a Program,
    requests: &'a [CueRequest],
    provider: &'a str,
) -> impl Iterator<Item = (usize, CueRef)> + 'a {
    requests
        .iter()
        .enumerate()
        .filter(move |(_, r)| r.request.provider.name == provider)
        .map(move |(i, r)| (i, cue_ref(program, r.node)))
}

/// The [`CueRef`] of a cue node.
pub(crate) fn cue_ref(program: &Program, node: NodeId) -> CueRef {
    let n = &program.nodes[node];
    CueRef {
        id: n.id.clone(),
        path: n.path.clone(),
        speaker: n.as_cue().and_then(|c| c.speaker.clone()),
    }
}

/// Provider instances used by cues, in order of first use.
fn used_providers(requests: &[CueRequest]) -> IndexMap<&str, &ProviderInstance> {
    let mut used = IndexMap::new();
    for r in requests {
        used.entry(r.request.provider.name.as_str())
            .or_insert(&r.request.provider);
    }
    used
}

/// The cues of one provider instance: refs, and how many are not cached.
struct ProviderUse {
    refs: Vec<CueRef>,
    uncached: usize,
}

impl ProviderUse {
    fn of(
        program: &Program,
        requests: &[CueRequest],
        cached: &[Option<CachedMeta>],
        name: &str,
    ) -> Self {
        let cues: Vec<(usize, CueRef)> = provider_cues(program, requests, name).collect();
        let uncached = cues.iter().filter(|(i, _)| cached[*i].is_none()).count();
        ProviderUse {
            refs: cues.into_iter().map(|(_, c)| c).collect(),
            uncached,
        }
    }

    /// An error when some cues are not cached (a render would call the provider), else a warning; the
    /// message gets the matching cache note.
    fn diagnostic(&self, code: &str, message: &str) -> Diagnostic {
        if self.uncached > 0 {
            Diagnostic::error(
                code,
                format!(
                    "{message}; {} of {} cues are not cached",
                    self.uncached,
                    self.refs.len()
                ),
            )
        } else {
            Diagnostic::warning(
                code,
                format!(
                    "{message}; all {} of its cues are cached, so it is not needed",
                    self.refs.len()
                ),
            )
        }
        .with_related(self.refs.iter().map(|c| c.id.clone()))
    }
}

/// The local program a provider instance runs from its own `command`: every `command` provider, and a
/// piper instance with a custom `command`. None for providers that run only their built-in program.
pub(crate) fn custom_program(instance: &ProviderInstance) -> Option<String> {
    let argv = instance.command.as_deref().filter(|c| !c.is_empty());
    match instance.provider_type {
        ProviderType::Command => Some(
            argv.and_then(|a| a.first().cloned())
                .unwrap_or_else(|| "(none configured)".to_string()),
        ),
        ProviderType::Piper => argv.and_then(|a| a.first().cloned()),
        _ => None,
    }
}

/// `missing_credentials` / `provider_unavailable` for every provider instance used by cues. Errors when
/// the provider has cues that are not cached (a render would call it), warnings otherwise.
fn check_providers(
    program: &Program,
    requests: &[CueRequest],
    client: &TtsClient,
    base_dir: &Path,
    allow_command: bool,
    env: &mut Environment,
) {
    for (name, instance) in used_providers(requests) {
        let usage = ProviderUse::of(program, requests, &env.cached, name);
        let display = providers::meta(instance.provider_type).display_name;
        // Local programs are a policy question first: when they are not allowed, nothing else matters.
        if !allow_command && let Some(program_name) = custom_program(instance) {
            let message = match instance.provider_type {
                ProviderType::Piper => format!(
                    "provider `{name}` (piper) runs the custom program `{program_name}` from its `command`, and \
                     this server does not let timelines run local programs"
                ),
                _ => format!(
                    "provider `{name}` ({display}) runs the local program `{program_name}`, and this server does \
                     not let timelines run local programs"
                ),
            };
            let path = if instance.configured && instance.command.is_some() {
                format!("providers.{name}.command")
            } else {
                provider_path(instance, &usage.refs)
            };
            env.diagnostics.push(
                usage
                    .diagnostic(codes::PROVIDER_UNAVAILABLE, &message)
                    .with_path(path)
                    .with_hint(ALLOW_COMMAND_HINT),
            );
            env.unusable.insert(name.to_string());
            env.blocked.insert(name.to_string());
            continue;
        }
        let status = client.credential_status(instance, base_dir);
        if status.is_usable() {
            continue;
        }
        env.unusable.insert(name.to_string());
        let path = provider_path(instance, &usage.refs);
        let d = match &status {
            CredentialStatus::Missing { env_var } => {
                let mut hint = format!(
                    "Set the API key: `export {env_var}=…` (or point `api_key_env` of provider `{name}` at the \
                     variable that holds it); `soniccraft providers` shows the status of every provider"
                );
                if let Some(extra) = client.credential_hint(instance) {
                    hint = format!("{extra}; {hint}");
                }
                if usage.uncached > 0 {
                    Diagnostic::error(
                        codes::MISSING_CREDENTIALS,
                        format!(
                            "provider `{name}` ({display}) needs an API key in `{env_var}`, which is not set; \
                             {} of {} cues are not cached and would be synthesized",
                            usage.uncached,
                            usage.refs.len()
                        ),
                    )
                    .with_hint(hint)
                } else {
                    Diagnostic::warning(
                        codes::MISSING_CREDENTIALS,
                        format!(
                            "provider `{name}` ({display}) has no API key in `{env_var}`, but all {} of its cues \
                             are cached, so no request is needed",
                            usage.refs.len()
                        ),
                    )
                    .with_hint(format!(
                        "Set `{env_var}` before changing the text or voice of these cues"
                    ))
                }
                .with_related(usage.refs.iter().map(|c| c.id.clone()))
            }
            CredentialStatus::Unavailable { reason } => {
                // The key-sending host policy explains itself: its message and hint, not the generic ones.
                let (message, hint) = match client.key_host_allowed(instance) {
                    Err(refused) => (
                        format!(
                            "provider `{name}` ({display}) cannot run here: {}",
                            refused.message
                        ),
                        refused.hint.unwrap_or_else(|| unavailable_hint(instance)),
                    ),
                    Ok(()) => (
                        format!("provider `{name}` ({display}) cannot run here: {reason}"),
                        unavailable_hint(instance),
                    ),
                };
                usage
                    .diagnostic(codes::PROVIDER_UNAVAILABLE, &message)
                    .with_hint(hint)
            }
            CredentialStatus::Ready | CredentialStatus::NotRequired => continue,
        };
        env.diagnostics.push(d.with_path(path));
    }
}

/// Where provider diagnostics point: the `providers.<name>` entry when the instance is configured, else
/// the cues that use it.
fn provider_path(instance: &ProviderInstance, cues: &[CueRef]) -> String {
    if instance.configured {
        format!("providers.{}", instance.name)
    } else {
        cues.first().map(|c| c.path.clone()).unwrap_or_default()
    }
}

fn unavailable_hint(instance: &ProviderInstance) -> String {
    use soniccraft_core::ir::ProviderType as P;
    match instance.provider_type {
        P::MacosSay => "macos_say only works on macOS; use another provider (e.g. `mock` for offline \
                        drafts, `piper` or `kokoro` for local voices) on this machine"
            .to_string(),
        P::Piper => "Install piper (`pipx install piper-tts`) or set `command` on the provider to the piper \
                     executable"
            .to_string(),
        P::Command => "Fix the provider's `command` template: its first element must be a program on PATH \
                       or a path relative to the timeline directory"
            .to_string(),
        _ => format!(
            "Run `soniccraft providers` to see what provider `{}` needs",
            instance.name
        ),
    }
}

/// `suspicious_value` (warning) for `command` template elements that pass the cue text or the voice id
/// as an argument of their own (text starting with `-` would be read as an option).
fn check_command_templates(
    requests: &[CueRequest],
    unusable: &HashSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, instance) in used_providers(requests) {
        if unusable.contains(name) || !instance.configured {
            continue;
        }
        for warning in soniccraft_tts::command_argument_warnings(instance) {
            diagnostics.push(
                Diagnostic::warning(codes::SUSPICIOUS_VALUE, warning.message)
                    .with_path(format!("providers.{name}.command[{}]", warning.index))
                    .with_hint(capitalize(&warning.hint)),
            );
        }
    }
}

/// `unknown_voice` for voices this machine does not have: macOS voices that are not installed (`say`
/// would silently speak with the default voice) and piper models found nowhere piper looks (also when
/// piper itself is missing: both need installing). An error when a cue with that voice is not cached, else
/// a warning.
fn check_voices(
    program: &Program,
    requests: &[CueRequest],
    client: &TtsClient,
    base_dir: &Path,
    spans: &SpanIndex,
    env: &mut Environment,
) {
    // (provider instance, voice id) → the problem, computed once.
    let mut verdicts: HashMap<(String, String), Option<(String, String)>> = HashMap::new();
    // Diagnostics grouped by the voice, its provider and where the voice is written.
    let mut groups: IndexMap<(String, String, String), MissingVoice> = IndexMap::new();
    for (i, r) in requests.iter().enumerate() {
        let instance = &r.request.provider;
        if env.blocked.contains(&instance.name) {
            continue;
        }
        let Some(voice_id) = r.request.voice.voice_id.clone() else {
            continue;
        };
        let key = (instance.name.clone(), voice_id.clone());
        let verdict = verdicts
            .entry(key)
            .or_insert_with(|| missing_voice(instance, &voice_id, client, base_dir))
            .clone();
        let Some((message, hint)) = verdict else {
            continue;
        };
        let path = voice_field_path(program, spans, r.node, "voice_id");
        let entry = groups
            .entry((instance.name.clone(), voice_id, path))
            .or_insert_with(|| MissingVoice {
                message,
                hint,
                cues: Vec::new(),
                uncached: 0,
            });
        entry.cues.push(cue_ref(program, r.node));
        if env.cached[i].is_none() {
            entry.uncached += 1;
        }
    }
    for (
        (_, _, path),
        MissingVoice {
            message,
            hint,
            cues,
            uncached,
        },
    ) in groups
    {
        let d = if uncached > 0 {
            Diagnostic::error(codes::UNKNOWN_VOICE, message)
        } else {
            Diagnostic::warning(
                codes::UNKNOWN_VOICE,
                format!(
                    "{message}; all {} cues with it are cached, so it is not needed until they change",
                    cues.len()
                ),
            )
        };
        env.diagnostics.push(
            d.with_path(path)
                .with_hint(hint)
                .with_related(cues.iter().map(|c| c.id.clone())),
        );
    }
}

/// One `unknown_voice` diagnostic in the making: what is wrong, the fix, the cues with that voice, and how
/// many of them are not cached.
struct MissingVoice {
    message: String,
    hint: String,
    cues: Vec<CueRef>,
    uncached: usize,
}

/// `(message, hint)` when `voice_id` does not exist for `instance` here; None when it does, or when this
/// machine cannot tell (not macOS, `say` failing).
fn missing_voice(
    instance: &ProviderInstance,
    voice_id: &str,
    client: &TtsClient,
    base_dir: &Path,
) -> Option<(String, String)> {
    match instance.provider_type {
        ProviderType::MacosSay => match soniccraft_tts::resolve_macos_voice(voice_id)? {
            Ok(_) => None,
            Err(suggestions) => {
                let message = format!(
                    "macOS voice `{voice_id}` is not installed (`say` would silently use the default voice)"
                );
                let hint = match suggestions.first() {
                    Some(best) => format!(
                        "Did you mean `{best}`? Use `voice_id: {best}`; `soniccraft voices macos_say` lists the \
                         installed voices{}",
                        if suggestions.len() > 1 {
                            format!(
                                " (other close matches: {})",
                                diagnostics::list_names(suggestions[1..].iter().map(String::as_str), 5)
                            )
                        } else {
                            String::new()
                        }
                    ),
                    None => "Run `soniccraft voices macos_say` to list the installed voices and use one of them as \
                             `voice_id`, or install the voice in System Settings > Accessibility > Spoken Content"
                        .to_string(),
                };
                Some((message, hint))
            }
        },
        ProviderType::Piper => match client.resolve_piper_model(instance, voice_id, base_dir) {
            Ok(_) => None,
            Err(missing) => Some((missing.message, capitalize(&missing.hint))),
        },
        _ => None,
    }
}

/// The document path where a voice field (`voice_id`) of cue `node` is written, following the voice
/// resolution order of DESIGN.md §2.7 (cue, the cue's own speaker, track, the track's speaker,
/// `defaults`); the cue itself when no layer writes it (the provider's default voice).
fn voice_field_path(program: &Program, spans: &SpanIndex, node: NodeId, field: &str) -> String {
    let n = &program.nodes[node];
    let written = |path: String| spans.get(&path).is_some().then_some(path);
    let speaker = n.as_cue().and_then(|c| c.speaker.as_deref());
    let cue_sets_speaker = spans.get(&format!("{}.speaker", n.path)).is_some();
    let speaker_layer = || speaker.and_then(|s| written(format!("speakers.{s}.{field}")));
    let track_layer = || {
        program
            .tracks
            .get(n.track)
            .and_then(|t| written(format!("{}.{field}", t.path)))
    };
    written(format!("{}.{field}", n.path))
        .or_else(|| {
            if cue_sets_speaker {
                speaker_layer()
            } else {
                None
            }
        })
        .or_else(track_layer)
        .or_else(|| {
            if cue_sets_speaker {
                None
            } else {
                speaker_layer()
            }
        })
        .or_else(|| written(format!("defaults.{field}")))
        .unwrap_or_else(|| n.path.clone())
}

/// `provider_unavailable` for local TTS servers (Kokoro-FastAPI, OpenAI-compatible servers on this
/// machine or a private network) that do not answer: one quick TCP probe per distinct URL.
async fn server_checks(
    client: &TtsClient,
    program: &Program,
    requests: &[CueRequest],
    env: &mut Environment,
) -> Vec<Diagnostic> {
    let mut targets: Vec<(&str, &ProviderInstance, String)> = Vec::new();
    for (name, instance) in used_providers(requests) {
        if env.unusable.contains(name) {
            continue;
        }
        if let Some(url) = client.endpoint_url(instance)
            && soniccraft_tts::is_local_url(&url)
        {
            targets.push((name, instance, url));
        }
    }
    if targets.is_empty() {
        return Vec::new();
    }
    let mut urls: Vec<&str> = targets.iter().map(|(_, _, u)| u.as_str()).collect();
    urls.sort_unstable();
    urls.dedup();
    let answers = futures::future::join_all(
        urls.iter()
            .map(|u| soniccraft_tts::probe_reachable(u, SERVER_PROBE_TIMEOUT)),
    )
    .await;
    let reachable: HashMap<&str, bool> = urls.iter().copied().zip(answers).collect();
    let mut out = Vec::new();
    for (name, instance, url) in &targets {
        if reachable.get(url.as_str()).copied().unwrap_or(true) {
            continue;
        }
        env.unusable.insert(name.to_string());
        let usage = ProviderUse::of(program, requests, &env.cached, name);
        let shown = soniccraft_tts::redact_url(url);
        let message = format!(
            "no {} answered at {shown}",
            server_name(instance.provider_type)
        );
        out.push(
            usage
                .diagnostic(codes::PROVIDER_UNAVAILABLE, &message)
                .with_path(provider_path(instance, &usage.refs))
                .with_hint(server_hint(instance, &shown)),
        );
    }
    out
}

/// "Kokoro-FastAPI server", "OpenAI-compatible server", ….
pub(crate) fn server_name(provider_type: ProviderType) -> String {
    match provider_type {
        ProviderType::Kokoro => "Kokoro-FastAPI server".to_string(),
        ProviderType::Openai => "OpenAI-compatible server".to_string(),
        other => format!("{} server", providers::meta(other).display_name),
    }
}

fn server_hint(instance: &ProviderInstance, shown: &str) -> String {
    match instance.provider_type {
        ProviderType::Kokoro => format!(
            "Start the server (e.g. `docker run -p 8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest`), or \
             point `base_url` of provider `{}` (or KOKORO_BASE_URL) at a running one; `soniccraft providers` \
             shows whether it answers",
            instance.name
        ),
        _ => format!(
            "Start the server at {shown}, or fix `base_url` of provider `{}`",
            instance.name
        ),
    }
}

/// Where the output format came from (it decides the wording and the path of `format_requires_ffmpeg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatSource {
    /// `output.format` in the document.
    DocumentFormat,
    /// The extension of `output.path` in the document (or the default `wav`).
    DocumentPath,
    /// The render's format option (CLI `--format`, MCP `format`).
    OverrideFormat,
    /// The extension of the render's output path option (CLI `-o`, MCP `output_path`).
    OverridePath,
}

impl FormatSource {
    /// Where `program`'s format came from, given the overrides it was lowered with.
    pub(crate) fn of(program: &Program, overrides: &LowerOverrides) -> FormatSource {
        if overrides.format.is_some() {
            FormatSource::OverrideFormat
        } else if program.output.format_explicit {
            FormatSource::DocumentFormat
        } else if overrides.output_path.is_some() {
            FormatSource::OverridePath
        } else {
            FormatSource::DocumentPath
        }
    }

    /// The document path of the setting, None for render options.
    pub(crate) fn path(self) -> Option<&'static str> {
        match self {
            FormatSource::DocumentFormat => Some("output.format"),
            FormatSource::DocumentPath => Some("output.path"),
            FormatSource::OverrideFormat | FormatSource::OverridePath => None,
        }
    }
}

/// The formats this build writes without ffmpeg (wav, flac, and mp3 with the native encoder), except
/// `current`, in display order.
pub(crate) fn ffmpeg_free_formats(current: OutputFormat) -> Vec<&'static str> {
    [OutputFormat::Wav, OutputFormat::Flac, OutputFormat::Mp3]
        .into_iter()
        .filter(|f| *f != current)
        .filter(|f| *f != OutputFormat::Mp3 || cfg!(feature = "mp3"))
        .map(OutputFormat::as_str)
        .collect()
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// `format_requires_ffmpeg`: message and hint for `format` coming from `source` (shared by the checks and
/// the encode stage). `output_name` is the output file name (for the override-path wording).
pub(crate) fn ffmpeg_requirement(
    format: OutputFormat,
    source: FormatSource,
    reason: &str,
    output_name: Option<&str>,
) -> (String, String) {
    let alternatives = ffmpeg_free_formats(format);
    let install = "Install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`)";
    let encoded = if format == OutputFormat::Mp3 {
        "mp3 output needs ffmpeg in this build (it was compiled without the native `mp3` encoder)"
            .to_string()
    } else {
        format!("{format} output is encoded with ffmpeg")
    };
    match source {
        FormatSource::DocumentFormat => (
            format!("{encoded}, but {reason}"),
            format!(
                "{install}, or use {}",
                or_list(
                    &alternatives
                        .iter()
                        .map(|f| format!("`format: {f}`"))
                        .collect::<Vec<_>>()
                )
            ),
        ),
        FormatSource::DocumentPath => (
            format!("{encoded} (from the extension of `output.path`), but {reason}"),
            format!(
                "{install}, or give `output.path` the extension {}",
                or_list(
                    &alternatives
                        .iter()
                        .map(|f| format!("`.{f}`"))
                        .collect::<Vec<_>>()
                )
            ),
        ),
        FormatSource::OverrideFormat => (
            format!(
                "the requested output format `{format}` (a render option, not the timeline) is encoded with ffmpeg, but {reason}"
            ),
            format!(
                "{install}, or request {} instead, or leave out the format option to render the timeline's own \
                 format",
                or_list(
                    &alternatives
                        .iter()
                        .map(|f| format!("`{f}`"))
                        .collect::<Vec<_>>()
                )
            ),
        ),
        FormatSource::OverridePath => (
            format!(
                "the output file {} is {format} (from its extension; a render option, not the timeline), which is \
                 encoded with ffmpeg, but {reason}",
                output_name.map_or_else(|| "path".to_string(), |n| format!("`{n}`"))
            ),
            format!(
                "{install}, or give the output path the extension {}",
                or_list(
                    &alternatives
                        .iter()
                        .map(|f| format!("`.{f}`"))
                        .collect::<Vec<_>>()
                )
            ),
        ),
    }
}

/// `format_requires_ffmpeg` when the output format needs ffmpeg (ogg, opus, m4a; mp3 without the `mp3`
/// feature) and none is available.
fn check_ffmpeg(
    program: &Program,
    ffmpeg: &Ffmpeg,
    source: FormatSource,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let output = &program.output;
    let format = encode_format(output);
    if !format.requires_ffmpeg() || ffmpeg_available(ffmpeg) {
        return;
    }
    let reason = match ffmpeg {
        Ffmpeg::Path(p) => format!(
            "the configured ffmpeg `{}` was not found",
            diagnostics::display_path(&p.display().to_string())
        ),
        _ => "ffmpeg was not found on PATH".to_string(),
    };
    let name = output.path.as_deref().map(diagnostics::display_path);
    let (message, hint) = ffmpeg_requirement(output.format, source, &reason, name.as_deref());
    let mut d = Diagnostic::error(codes::FORMAT_REQUIRES_FFMPEG, message).with_hint(hint);
    if let Some(path) = source.path() {
        d = d.with_path(path);
    }
    diagnostics.push(d);
}

/// The encoder settings of `output`.
pub(crate) fn encode_format(output: &soniccraft_core::ir::OutputConfig) -> EncodeFormat {
    let bitrate = output
        .bitrate_bps
        .or_else(|| soniccraft_core::lower::default_bitrate_bps(output.format))
        .unwrap_or(192_000);
    match output.format {
        OutputFormat::Wav => EncodeFormat::Wav {
            bit_depth: output.bit_depth,
        },
        OutputFormat::Flac => EncodeFormat::Flac {
            bit_depth: output.bit_depth,
        },
        OutputFormat::Mp3 => EncodeFormat::Mp3 {
            bitrate_bps: bitrate,
        },
        OutputFormat::Ogg => EncodeFormat::Ogg {
            bitrate_bps: bitrate,
        },
        OutputFormat::Opus => EncodeFormat::Opus {
            bitrate_bps: bitrate,
        },
        OutputFormat::M4a => EncodeFormat::M4a {
            bitrate_bps: bitrate,
        },
    }
}

/// Whether ffmpeg can be run: the explicit binary (a path, or a name looked up on PATH) or `ffmpeg` on
/// PATH.
pub(crate) fn ffmpeg_available(ffmpeg: &Ffmpeg) -> bool {
    match ffmpeg {
        Ffmpeg::Auto => soniccraft_dsp::ffmpeg::find_ffmpeg().is_some(),
        Ffmpeg::Path(p) => p.is_file() || which::which(p).is_ok(),
        Ffmpeg::Disabled => false,
    }
}

/// `text_too_long` when a cue's text exceeds its provider's per-request limit.
fn check_text_lengths(
    program: &Program,
    requests: &[CueRequest],
    diagnostics: &mut Vec<Diagnostic>,
) {
    for r in requests {
        let voice = &r.request.voice;
        let Some(limit) = providers::max_text_chars(voice.provider_type, voice.model.as_deref())
        else {
            continue;
        };
        let length = r.request.text.chars().count();
        if length <= limit {
            continue;
        }
        let node = &program.nodes[r.node];
        let model = voice
            .model
            .as_deref()
            .map(|m| format!(" with model `{m}`"))
            .unwrap_or_default();
        diagnostics.push(
            Diagnostic::error(
                codes::TEXT_TOO_LONG,
                format!(
                    "cue `{}` has {length} characters, but provider `{}` ({}){model} accepts at most {limit} per \
                     request",
                    node.id, voice.provider, voice.provider_type
                ),
            )
            .with_path(format!("{}.text", node.path))
            .with_hint(format!(
                "Split the cue into several cues of at most {limit} characters each (e.g. at sentence \
                 boundaries); consecutive cues in one sequence play back to back"
            ))
            .with_related([node.id.clone()]),
        );
    }
}

/// `muted_trigger` (info) for every muted trigger track of a ducked track.
fn check_muted_triggers(program: &Program, diagnostics: &mut Vec<Diagnostic>) {
    for track in &program.tracks {
        let Some(ducking) = &track.ducking else {
            continue;
        };
        let muted: Vec<&str> = ducking
            .triggers
            .iter()
            .filter_map(|&t| program.tracks.get(t))
            .filter(|t| t.mute)
            .map(|t| t.id.as_str())
            .collect();
        if muted.is_empty() {
            continue;
        }
        let all = muted.len() == ducking.triggers.len();
        let consequence = if all {
            format!(
                "every trigger of `{}` is muted, so it is never ducked",
                track.id
            )
        } else {
            format!("they do not duck `{}`", track.id)
        };
        diagnostics.push(
            Diagnostic::info(
                codes::MUTED_TRIGGER,
                format!(
                    "trigger track{} {} {} muted; {consequence}",
                    if muted.len() == 1 { "" } else { "s" },
                    diagnostics::list_names(muted.iter().copied(), 8),
                    if muted.len() == 1 { "is" } else { "are" },
                ),
            )
            .with_path(format!("{}.ducking.trigger_tracks", track.path))
            .with_hint(format!(
                "Unmute the trigger track{s} or remove {them} from `trigger_tracks`",
                s = if muted.len() == 1 { "" } else { "s" },
                them = if muted.len() == 1 { "it" } else { "them" },
            ))
            .with_related(muted.iter().map(|s| s.to_string())),
        );
    }
}

/// Notes a provider will report for the cues' settings (`setting_ignored`).
fn collect_voice_notes(program: &Program, requests: &[CueRequest], notes: &mut Notes) {
    for r in requests {
        for note in soniccraft_tts::voice_notes(&r.request.provider, &r.request.voice) {
            notes.add(&r.request.provider.name, &note, cue_ref(program, r.node));
        }
    }
}

/// Online credential checks for the providers whose cues are not all cached.
async fn online_checks(
    client: &TtsClient,
    program: &Program,
    requests: &[CueRequest],
    env: &Environment,
) -> Vec<Diagnostic> {
    // Providers already reported as unusable here would only repeat that online.
    let used: Vec<(&str, &ProviderInstance)> = used_providers(requests)
        .into_iter()
        .filter(|(name, _)| !env.unusable.contains(*name))
        .collect();
    let checks = used
        .iter()
        .map(|(_, instance)| client.check_credentials_online(instance));
    let results: Vec<Result<(), TtsError>> = futures::future::join_all(checks).await;
    let mut out = Vec::new();
    for ((name, instance), result) in used.iter().zip(results) {
        let Err(error) = result else { continue };
        let cues: Vec<(usize, CueRef)> = provider_cues(program, requests, name).collect();
        let uncached = cues.iter().any(|(i, _)| env.cached[*i].is_none());
        let refs: Vec<CueRef> = cues.into_iter().map(|(_, c)| c).collect();
        let code = if error.kind == TtsErrorKind::Auth {
            codes::MISSING_CREDENTIALS
        } else {
            codes::PROVIDER_UNAVAILABLE
        };
        let message = format!("the online credential check of provider `{name}` failed: {error}");
        let d = if uncached {
            Diagnostic::error(code, message)
        } else {
            Diagnostic::warning(code, format!("{message} (all its cues are cached)"))
        };
        out.push(
            d.with_path(provider_path(instance, &refs))
                .with_hint(error.hint.clone().unwrap_or_else(|| online_hint(&error)))
                .with_related(refs.iter().map(|c| c.id.clone())),
        );
    }
    out
}

fn online_hint(error: &TtsError) -> String {
    match error.kind {
        TtsErrorKind::Auth => format!(
            "Check the API key of provider `{}` (`soniccraft providers` shows which variable it reads)",
            error.provider
        ),
        TtsErrorKind::Network | TtsErrorKind::Timeout => {
            "Check the network connection and the provider's `base_url`".to_string()
        }
        _ => "Retry later, or check the provider's status page".to_string(),
    }
}

/// `text` with its first letter in upper case (hints are sentences).
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// [`TimelineSummary`] of a lowered program.
pub(crate) fn summary(
    program: &Program,
    requests: &[CueRequest],
    cached: &[Option<CachedMeta>],
) -> TimelineSummary {
    let mut speakers: Vec<String> = Vec::new();
    let mut provider_names: Vec<String> = Vec::new();
    let mut characters = 0usize;
    for (_, _, cue) in program.cues() {
        if let Some(s) = &cue.speaker
            && !speakers.contains(s)
        {
            speakers.push(s.clone());
        }
        if !provider_names.contains(&cue.voice.provider) {
            provider_names.push(cue.voice.provider.clone());
        }
        characters += cue.text.chars().count();
    }
    TimelineSummary {
        tracks: program.tracks.len(),
        speech_cues: program.cues().count(),
        clips: program.clips().count(),
        speakers,
        providers: provider_names,
        characters,
        cached_cues: requests
            .iter()
            .zip(cached)
            .filter(|(_, c)| c.is_some())
            .count(),
        output_format: program.output.format.as_str().to_string(),
        sample_rate: program.output.sample_rate,
        target_lufs: program.output.target_lufs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_free_alternatives_never_name_the_current_format() {
        let wav = ffmpeg_free_formats(OutputFormat::Wav);
        assert!(!wav.contains(&"wav") && wav.contains(&"flac"));
        let mp3 = ffmpeg_free_formats(OutputFormat::Mp3);
        assert_eq!(mp3, ["wav", "flac"]);
        let opus = ffmpeg_free_formats(OutputFormat::Opus);
        assert_eq!(opus.contains(&"mp3"), cfg!(feature = "mp3"));
        let (message, hint) = ffmpeg_requirement(
            OutputFormat::Opus,
            FormatSource::OverrideFormat,
            "ffmpeg was not found on PATH",
            None,
        );
        assert!(
            message.contains("requested output format `opus`"),
            "{message}"
        );
        assert!(!hint.contains("format: "), "{hint}");
        assert!(
            hint.contains("`wav`") && hint.contains("leave out the format option"),
            "{hint}"
        );
        let (_, hint) = ffmpeg_requirement(
            OutputFormat::Mp3,
            FormatSource::DocumentFormat,
            "ffmpeg was not found on PATH",
            None,
        );
        assert_eq!(
            hint,
            "Install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`), or use `format: wav` or \
             `format: flac`"
        );
        assert_eq!(or_list(&["a".into(), "b".into(), "c".into()]), "a, b or c");
    }
}
