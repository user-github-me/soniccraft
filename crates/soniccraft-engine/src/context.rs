//! What every operation starts with: absolute source paths, the compiled program, the TTS cache directory
//! and client, and one synthesis request per speech cue.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use soniccraft_core::diagnostics::codes;
use soniccraft_core::ir::{NodeId, OutputFormat, Program};
use soniccraft_core::{AssetRef, Compiled, Diagnostic, LowerOptions, SpanIndex};
use soniccraft_tts::{SynthesisRequest, TtsClient, TtsOptions};

use crate::{Engine, TimelineSource, cache};

/// Output file stem for timelines that do not come from a file (MCP `yaml_content`).
pub(crate) const INLINE_STEM: &str = "soniccraft-output";

/// CLI/MCP overrides that lowering must know about (they change the inferred output format).
#[derive(Debug, Clone, Default)]
pub(crate) struct LowerOverrides {
    /// Output path override, as given.
    pub output_path: Option<String>,
    /// Output format override.
    pub format: Option<OutputFormat>,
}

/// One operation's view of a timeline.
pub(crate) struct Session {
    /// Absolute directory relative paths resolve against.
    pub base_dir: PathBuf,
    /// Absolute path of the timeline file, if any.
    pub source_path: Option<PathBuf>,
    /// Default output stem: the timeline file stem, or [`INLINE_STEM`].
    pub stem: String,
    /// The lowered program, present only when the document has no errors.
    pub program: Option<Arc<Program>>,
    /// Parse and lowering diagnostics, positioned and sorted.
    pub diagnostics: Vec<Diagnostic>,
    /// Source positions for the engine's own diagnostics.
    pub spans: Arc<SpanIndex>,
    /// Every audio file the document names, also when it has errors (core's [`Compiled::assets`]).
    pub assets: Vec<AssetRef>,
    /// The CLI/MCP overrides the document was lowered with.
    pub overrides: LowerOverrides,
    /// Resolved TTS cache directory (absolute).
    pub cache_dir: PathBuf,
    /// TTS client using that cache.
    pub client: TtsClient,
}

impl Session {
    /// Compile `source` (on the blocking pool: documents can be up to 5 MiB) and set up the TTS client.
    pub(crate) async fn open(
        engine: &Engine,
        source: &TimelineSource,
        overrides: LowerOverrides,
    ) -> Session {
        let base_dir = absolute(&source.base_dir);
        let source_path = source.path.as_deref().map(absolute);
        let stem = source_path
            .as_deref()
            .and_then(Path::file_stem)
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| INLINE_STEM.to_string());

        let options = LowerOptions {
            base_dir: base_dir.clone(),
            output_path_override: overrides.output_path.clone(),
            format_override: overrides.format,
        };
        let text = source.text.clone();
        let format = source.format;
        let compiled = match tokio::task::spawn_blocking(move || {
            soniccraft_core::compile(&text, format, &options)
        })
        .await
        {
            Ok(compiled) => compiled,
            Err(join) => Compiled {
                diagnostics: vec![Diagnostic::error(
                    codes::YAML_SYNTAX,
                    format!("internal error: the timeline parser failed ({join})"),
                )],
                ..Compiled::default()
            },
        };

        let cache_dir = cache::resolve(engine, Some(&base_dir));
        // Programs that synthesis runs (piper, `say`, `command`) never see a key the timeline points at.
        let child_env_remove = compiled
            .program
            .as_ref()
            .map(|program| {
                let mut vars: Vec<String> = program
                    .providers
                    .values()
                    .filter_map(|p| p.api_key_env.clone())
                    .filter(|v| !v.trim().is_empty())
                    .collect();
                vars.sort();
                vars.dedup();
                vars
            })
            .unwrap_or_default();
        let client = TtsClient::new(TtsOptions {
            cache_dir: Some(cache_dir.clone()),
            read_cache: engine.options.read_cache,
            write_cache: true,
            jobs: engine.options.jobs,
            env: engine.env.clone(),
            child_env_remove,
            ..TtsOptions::default()
        });
        Session {
            base_dir,
            source_path,
            stem,
            program: compiled.program.map(Arc::new),
            diagnostics: compiled.diagnostics,
            spans: Arc::new(compiled.spans),
            assets: compiled.assets,
            overrides,
            cache_dir,
            client,
        }
    }

    /// Fill in line/column of diagnostics that have a path but no position.
    pub(crate) fn attach(&self, diagnostics: &mut [Diagnostic]) {
        for d in diagnostics {
            self.spans.attach(d);
        }
    }
}

/// The synthesis request of one speech cue.
#[derive(Debug, Clone)]
pub(crate) struct CueRequest {
    /// The cue's node.
    pub node: NodeId,
    /// What to synthesize.
    pub request: SynthesisRequest,
}

/// One request per speech cue, in node (declaration) order. A cue whose provider instance is missing from
/// the program (lowering guarantees it is not) gets an `unknown_provider` error instead.
pub(crate) fn cue_requests(program: &Program) -> (Vec<CueRequest>, Vec<Diagnostic>) {
    let mut requests = Vec::new();
    let mut diagnostics = Vec::new();
    for (node_id, node, cue) in program.cues() {
        match program.providers.get(&cue.voice.provider) {
            Some(provider) => requests.push(CueRequest {
                node: node_id,
                request: SynthesisRequest {
                    cue_id: node.id.clone(),
                    provider: provider.clone(),
                    voice: cue.voice.clone(),
                    text: cue.text.clone(),
                    base_dir: program.base_dir.clone(),
                },
            }),
            None => diagnostics.push(
                Diagnostic::error(
                    codes::UNKNOWN_PROVIDER,
                    format!(
                        "cue `{}` uses provider `{}`, which is not configured",
                        node.id, cue.voice.provider
                    ),
                )
                .with_path(node.path.clone())
                .with_hint("Add the provider under `providers:` or use a built-in provider name")
                .with_related([node.id.clone()]),
            ),
        }
    }
    (requests, diagnostics)
}

/// `path` made absolute against the current directory (lexically: `.` components are dropped, symlinks
/// and `..` are kept). Falls back to `path` itself when the current directory is unknown.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        if component != Component::CurDir {
            out.push(component.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths() {
        assert_eq!(absolute(Path::new("/a/./b/c")), PathBuf::from("/a/b/c"));
        let rel = absolute(Path::new("x/./y"));
        assert!(rel.is_absolute());
        assert!(rel.ends_with("x/y"));
    }
}
