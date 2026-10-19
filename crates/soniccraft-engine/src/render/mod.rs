//! Renders (DESIGN.md §4): compile and check → plan the output files → decode clip sources → lay the
//! timeline out with dry-run lengths (stop before any TTS request on layout errors that exact speech
//! lengths cannot remove, and on the memory budget) → synthesize every cue → per-cue processing → exact
//! layout → stems, ducking and the master sum → de-click and mastering → encoding (lossy formats: measure
//! the delivered file and compensate codec overshoot) → subtitles, manifest and preview → commit.
//!
//! Clip sources are decoded *before* synthesis, and the dry-run layout runs on their exact lengths, so a
//! broken asset or a certain layout error fails the render before any paid API call. CPU-heavy stages run
//! on the blocking pool; the cancellation flag is checked between stages, while synthesizing, per cue, per
//! clip source, per mixing window, inside mastering and limiting, and while measuring the encoded file.
//!
//! Outputs are published with a two-phase commit ([`crate::files`]): every file is written to a hidden
//! temporary name next to its destination, and all of them are renamed into place only after the last stage
//! and the last cancellation check. A failed or cancelled render deletes its temporary files and nothing
//! else, so the outputs of an earlier render survive untouched.

mod cues;
mod export;
mod mix;
mod sources;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use indexmap::IndexMap;
use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::{self, codes, display_path};
use soniccraft_core::ir::{NodeId, NodeKind, OutputFormat, Program, TrackIdx};
use soniccraft_dsp::encode::{EncodeOptions, EncodeSummary};
use soniccraft_dsp::ffmpeg::Ffmpeg;
use soniccraft_dsp::loudness::{FileMeasurement, LoudnessStats};
use soniccraft_dsp::master::{MasterParams, MasterResult};
use soniccraft_dsp::{AudioBuffer, DspError};
use soniccraft_layout::{Layout, NodeDuration};
use soniccraft_tts::{
    CachedMeta, SynthesisProgress, SynthesisRequest, SynthesisResult, TtsError, TtsErrorKind,
};

use crate::budget::{self, Budget};
use crate::cancel::{Cancel, Stop, blocking};
use crate::checks::{self, CheckOptions, FormatSource, cue_ref};
use crate::context::{CueRequest, LowerOverrides, Session, cue_requests};
use crate::diag::{self, CueRef, Notes};
use crate::files::{self, Exports, OutputPlan, Staging};
use crate::plan::{self, DryRunInput};
use crate::report::{
    LoudnessReport, PreEncodeLoudness, RenderFailure, RenderReport, SynthesisReport,
};
use crate::{Engine, ProgressCallback, ProgressEvent, RenderOverrides, TimelineSource};

use cues::{CueAudio, CueJob};
use mix::{MixError, MixJob};
use sources::SourceBank;

/// Extra limit-and-encode passes when a lossy file peaks above the true-peak ceiling or its loudness drifted
/// from the target.
const MAX_COMPENSATION_PASSES: u32 = 3;

/// Each compensation pass lowers the ceiling by the overshoot plus this much.
const COMPENSATION_MARGIN_DB: f64 = 0.1;

/// [`crate::Engine::render`].
pub(crate) async fn render(
    engine: &Engine,
    source: &TimelineSource,
    overrides: &RenderOverrides,
) -> Result<RenderReport, RenderFailure> {
    let mut job = Job {
        engine,
        cancel: Cancel::new(overrides.cancel.clone()),
        progress: engine.options.progress.clone(),
        diagnostics: Vec::new(),
        timings: IndexMap::new(),
        staging: Arc::new(Mutex::new(Staging::default())),
    };
    let started = Instant::now();
    match job.run(source, overrides).await {
        Ok(mut report) => {
            job.time("total", started);
            report.timings_ms = job.timings;
            Ok(report)
        }
        Err(stop) => {
            lock(&job.staging).abort();
            Err(job.failure(stop))
        }
    }
}

/// State of one render.
struct Job<'a> {
    engine: &'a Engine,
    cancel: Cancel,
    progress: Option<ProgressCallback>,
    /// Every diagnostic so far (positioned).
    diagnostics: Vec<Diagnostic>,
    timings: IndexMap<String, f64>,
    /// The render's output files, under temporary names until the commit.
    staging: Arc<Mutex<Staging>>,
}

fn lock(staging: &Mutex<Staging>) -> std::sync::MutexGuard<'_, Staging> {
    staging.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The compiled, checked timeline.
struct Compiled {
    session: Session,
    program: Arc<Program>,
    requests: Vec<CueRequest>,
    /// Cache metadata per request.
    cached: Vec<Option<CachedMeta>>,
    notes: Notes,
}

/// The encoded output and what the delivered file measures.
struct Encoded {
    /// The master that was encoded (limited again when a lossy file peaked above the ceiling).
    master: AudioBuffer,
    summary: EncodeSummary,
    /// Lossy formats only.
    delivered: Option<Delivered>,
}

/// The delivered (lossy) file.
struct Delivered {
    /// Its measurement, or why it could not be decoded.
    measured: Result<FileMeasurement, String>,
    /// The master that was encoded.
    master: LoudnessStats,
    /// The limiter ceiling of that master.
    ceiling_dbtp: f64,
    /// Limit-and-encode passes after the first encode.
    passes: u32,
}

/// Why encoding stopped.
enum EncodeFailure {
    Stop(Stop),
    Dsp(DspError),
}

impl From<DspError> for EncodeFailure {
    fn from(error: DspError) -> Self {
        match error {
            DspError::Cancelled => EncodeFailure::Stop(Stop::Cancelled),
            other => EncodeFailure::Dsp(other),
        }
    }
}

impl Job<'_> {
    fn stage(&self, stage: &str, detail: impl Into<String>) {
        if let Some(progress) = &self.progress {
            progress(&ProgressEvent::Stage {
                stage: stage.to_string(),
                detail: detail.into(),
            });
        }
    }

    fn time(&mut self, stage: &str, since: Instant) {
        let ms = since.elapsed().as_secs_f64() * 1000.0;
        *self.timings.entry(stage.to_string()).or_insert(0.0) += (ms * 10.0).round() / 10.0;
    }

    fn failure(&self, stop: Stop) -> RenderFailure {
        match stop {
            Stop::Cancelled => RenderFailure {
                message: "render cancelled".to_string(),
                diagnostics: Vec::new(),
                cancelled: true,
            },
            Stop::Failed(message) => {
                let mut diagnostics = self.diagnostics.clone();
                diagnostics::sort(&mut diagnostics);
                RenderFailure {
                    message,
                    diagnostics,
                    cancelled: false,
                }
            }
        }
    }

    /// Record `diagnostic` (positioned) and fail with `message`.
    fn fail(
        &mut self,
        session: Option<&Session>,
        message: impl Into<String>,
        mut diagnostic: Diagnostic,
    ) -> Stop {
        if let Some(session) = session {
            session.attach(std::slice::from_mut(&mut diagnostic));
        }
        self.diagnostics.push(diagnostic);
        Stop::failed(message)
    }

    fn extend(&mut self, session: &Session, mut diagnostics: Vec<Diagnostic>) {
        session.attach(&mut diagnostics);
        self.diagnostics.extend(diagnostics);
    }

    async fn run(
        &mut self,
        source: &TimelineSource,
        overrides: &RenderOverrides,
    ) -> Result<RenderReport, Stop> {
        // ---- compile and check -------------------------------------------------------------------------
        self.cancel.check()?;
        self.stage("parse", source.display_name());
        let Compiled {
            session,
            program,
            requests,
            cached,
            mut notes,
        } = self.compile(source, overrides).await?;
        self.cancel.check()?;

        // ---- output files ------------------------------------------------------------------------------
        let exports = Exports::resolve(&program, overrides);
        let plan = self.plan_outputs(&session, &program, overrides, &exports)?;
        for dir in plan.directories() {
            let created = lock(&self.staging).create_dir_all(dir);
            if let Err(e) = created {
                let d = Diagnostic::error(
                    codes::ENCODE_FAILED,
                    format!(
                        "cannot create the output directory {}: {e}",
                        display_path(&dir.display().to_string())
                    ),
                )
                .with_hint("Check the output path and the directory permissions");
                return Err(self.fail(None, "cannot create the output directory", d));
            }
        }

        // ---- clip sources (before synthesis: a broken asset must not cost API calls) ------------------
        let since = Instant::now();
        let clips = program.clips().count();
        self.stage("decode", format!("{clips} clips"));
        let sources = self.decode_sources(&session, &program).await?;
        self.time("decode", since);
        self.cancel.check()?;

        // ---- dry-run layout: what exact speech lengths cannot fix stops the render here ---------------
        let since = Instant::now();
        self.preflight(&session, &program, &requests, &cached, &sources)
            .await?;
        self.time("preflight", since);
        self.cancel.check()?;

        // ---- synthesis ---------------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("synthesize", format!("{} cues", requests.len()));
        let results = self
            .synthesize(&session, &program, &requests, &mut notes)
            .await?;
        self.time("synthesize", since);
        self.cancel.check()?;
        let synthesis = synthesis_report(&requests, &results);
        let cache_hits: Vec<bool> = results.iter().map(|r| r.cache_hit).collect();

        // ---- per-cue processing ------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("process", format!("{} cues", requests.len()));
        let captions_wanted = !exports.subtitles.is_empty() || exports.preview;
        let mut processed = self
            .process_cues(&session, &program, &requests, results, captions_wanted)
            .await?;
        self.time("process", since);
        self.cancel.check()?;

        // ---- layout ------------------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("layout", format!("{} items", program.nodes.len()));
        let layout = self
            .layout(
                &session,
                &program,
                &requests,
                &processed,
                &cache_hits,
                &sources,
            )
            .await?;
        self.time("layout", since);
        self.cancel.check()?;

        // ---- captions (cue-relative word timings → absolute) -------------------------------------------
        let captions = if captions_wanted {
            export::captions(&program, &layout, &mut processed)
        } else {
            Vec::new()
        };

        // ---- mix ---------------------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("mix", format!("{} tracks", program.tracks.len()));
        let clip_gains = self.clip_gains(&session, &program, &sources);
        let mut cue_buffers: Vec<Option<AudioBuffer>> = vec![None; program.nodes.len()];
        for cue in processed {
            if let Some(slot) = cue_buffers.get_mut(cue.node) {
                *slot = Some(cue.buffer);
            }
        }
        let mix_job = MixJob {
            program: Arc::clone(&program),
            layout: Arc::clone(&layout),
            cues: cue_buffers,
            sources: Arc::clone(&sources),
            clip_gains,
            stems: plan.stems.as_ref().map(|(_, files)| files.clone()),
            staging: Arc::clone(&self.staging),
            envelopes: exports.preview,
            cancel: self.cancel.clone(),
        };
        let mixed = match blocking(move || Ok(mix_job.run())).await? {
            Ok(mixed) => mixed,
            Err(MixError::Stop(stop)) => return Err(stop),
            Err(MixError::Diagnostic(d)) => {
                return Err(self.fail(None, "mixing failed", *d));
            }
        };
        drop(sources);
        self.time("mix", since);
        self.cancel.check()?;

        // ---- master ------------------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("master", format!("{:.1} s", layout.total_ms() / 1000.0));
        let (master, mastered) = self.master(&session, &program, mixed.master).await?;
        self.time("master", since);
        self.cancel.check()?;

        // ---- encode ------------------------------------------------------------------------------------
        let since = Instant::now();
        self.stage("encode", program.output.format.as_str());
        let encoded = self
            .encode(&session, &program, &plan, master, &mastered)
            .await?;
        self.time("encode", since);
        self.cancel.check()?;

        // ---- side files and the report -----------------------------------------------------------------
        let since = Instant::now();
        self.stage("export", plan.dir.display().to_string());
        let Encoded {
            master,
            summary,
            delivered,
        } = encoded;
        let waveform = if exports.preview {
            Some(blocking(move || Ok(export::waveform(&master))).await?)
        } else {
            drop(master);
            None
        };
        let mut side: Vec<(PathBuf, Vec<u8>)> = plan
            .subtitles
            .iter()
            .map(|(format, path)| {
                (
                    path.clone(),
                    export::subtitles(*format, &captions).into_bytes(),
                )
            })
            .collect();
        let mut all = self.diagnostics.clone();
        diagnostics::sort(&mut all);
        let report = export::report(export::ReportParts {
            program: &program,
            layout: &layout,
            plan: &plan,
            encoded: &summary,
            loudness: loudness_report(&program, &mastered, delivered.as_ref()),
            stems: &mixed.stems,
            ducking: &mixed.ducking,
            synthesis,
            diagnostics: all,
        });
        let shared = export::relative(&report, &plan, &mixed.stems);
        let normalized = export::normalized(&shared).map_err(|message| {
            self.fail(
                None,
                "the manifest could not be built",
                Diagnostic::error(codes::ENCODE_FAILED, message),
            )
        })?;
        if let Some(path) = &plan.manifest {
            side.push((path.clone(), export::manifest_json(&normalized)));
        }
        if let (Some(path), Some(waveform)) = (&plan.preview, waveform) {
            let html = export::preview_html(
                &program,
                &plan,
                &normalized,
                waveform,
                &mixed.ducking,
                &captions,
            )
            .map_err(|message| {
                self.fail(
                    None,
                    "the web preview could not be built",
                    Diagnostic::error(codes::ENCODE_FAILED, message),
                )
            })?;
            side.push((path.clone(), html.into_bytes()));
        }
        self.write_files(side).await?;
        self.time("export", since);

        // ---- commit: the last cancellation check, then every file goes into place ---------------------
        self.cancel.check()?;
        let committed = {
            let staging = Arc::clone(&self.staging);
            blocking(move || Ok(lock(&staging).commit())).await?
        };
        if let Err(error) = committed {
            let shown = display_path(&error.target.display().to_string());
            let d = Diagnostic::error(
                codes::ENCODE_FAILED,
                format!(
                    "cannot put `{shown}` in place: {}; every output was left as it was before this render",
                    error.error
                ),
            )
            .with_hint(
                "Check that the output directory is writable and that nothing else writes these files at the \
                 same time, then render again",
            );
            return Err(self.fail(None, "the outputs could not be put in place", d));
        }
        Ok(report)
    }

    /// Compile with the overrides and run the environment checks; errors stop the render here.
    async fn compile(
        &mut self,
        source: &TimelineSource,
        overrides: &RenderOverrides,
    ) -> Result<Compiled, Stop> {
        let format = match overrides.format.as_deref() {
            None => None,
            // A render option, not the DSL: `MP3` or ` wav ` mean what they say.
            Some(name) => match OutputFormat::parse(name)
                .or_else(|| OutputFormat::spelling_of(name).and_then(OutputFormat::parse))
            {
                Some(format) => Some(format),
                None => {
                    let suggestion =
                        diagnostics::did_you_mean(name, OutputFormat::NAMES.iter().copied())
                            .map(|s| format!("Did you mean `{s}`? "))
                            .unwrap_or_default();
                    let d = Diagnostic::error(
                        codes::INVALID_VALUE,
                        format!("unknown output format `{name}` (a render option)"),
                    )
                    .with_hint(format!(
                        "{suggestion}Use one of {}",
                        diagnostics::list_names(OutputFormat::NAMES.iter().copied(), 8)
                    ));
                    return Err(self.fail(None, "unknown output format", d));
                }
            },
        };
        let lower = LowerOverrides {
            output_path: overrides
                .output_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            format,
        };
        let since = Instant::now();
        let session = Session::open(self.engine, source, lower).await;
        self.time("parse", since);
        self.diagnostics.extend(session.diagnostics.iter().cloned());
        let Some(program) = session.program.clone() else {
            // The files the document names can be checked all the same.
            let assets = checks::document_assets(&session, false).await;
            self.diagnostics.extend(assets);
            return Err(Stop::failed(invalid_message(&self.diagnostics)));
        };
        let (requests, errors) = cue_requests(&program);
        self.extend(&session, errors);
        let since = Instant::now();
        let env = checks::environment(
            self.engine,
            &session,
            &program,
            &requests,
            CheckOptions {
                probe_assets: false,
                online: false,
            },
        )
        .await;
        self.time("check", since);
        self.diagnostics.extend(env.diagnostics);
        if diagnostics::has_errors(&self.diagnostics) {
            return Err(Stop::failed(invalid_message(&self.diagnostics)));
        }
        Ok(Compiled {
            session,
            program,
            requests,
            cached: env.cached,
            notes: env.notes,
        })
    }

    /// The output files, checked: inside the confinement directory (document paths), no collision, and
    /// nothing in the way that SonicCraft did not write.
    fn plan_outputs(
        &mut self,
        session: &Session,
        program: &Program,
        overrides: &RenderOverrides,
        exports: &Exports,
    ) -> Result<OutputPlan, Stop> {
        let audio = files::audio_path(session, program, overrides);
        let from_document = overrides.output_path.is_none() && program.output.path.is_some();
        if let Some(dir) = &self.engine.options.confine_outputs_to
            && from_document
            && let Some(written) = program.output.path.as_deref()
        {
            let stems = exports.stems.then(|| {
                let stem = audio
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                audio.with_file_name(format!("{stem}.stems"))
            });
            if !files::is_inside(&audio, dir) || stems.is_some_and(|s| !files::is_inside(&s, dir)) {
                let d = files::confinement_error(written, &session.base_dir, dir);
                return Err(self.fail(
                    Some(session),
                    "the output path leaves the allowed directory",
                    d,
                ));
            }
        }
        match OutputPlan::new(audio, exports, program, session.source_path.as_deref()) {
            Ok(plan) => Ok(plan),
            Err(problem) => {
                let mut d = Diagnostic::error(codes::INVALID_VALUE, problem.message)
                    .with_hint(problem.hint);
                if from_document {
                    d = d.with_path("output.path");
                }
                Err(self.fail(Some(session), "invalid output path", d))
            }
        }
    }

    async fn decode_sources(
        &mut self,
        session: &Session,
        program: &Arc<Program>,
    ) -> Result<Arc<SourceBank>, Stop> {
        let program_for_task = Arc::clone(program);
        let ffmpeg = Ffmpeg::from_option(self.engine.options.ffmpeg_path.clone());
        let cancel = self.cancel.clone();
        let (bank, diagnostics) =
            blocking(move || Ok(sources::decode_all(&program_for_task, ffmpeg, &cancel))).await?;
        self.extend(session, diagnostics);
        Ok(Arc::new(bank?))
    }

    /// Lay the timeline out as a dry run does (cached and mock cues exact, other cues estimated, clips at
    /// their decoded lengths) and stop on what synthesis cannot change: layout errors that do not depend on
    /// estimates, and a render that would not fit in the memory budget.
    async fn preflight(
        &mut self,
        session: &Session,
        program: &Arc<Program>,
        requests: &[CueRequest],
        cached: &[Option<CachedMeta>],
        sources: &SourceBank,
    ) -> Result<(), Stop> {
        let input = DryRunInput {
            requests: requests.to_vec(),
            cached: cached.to_vec(),
            clip_frames: sources.frames(),
            source_samples: sources.samples(),
            ducking: false,
            diagram: false,
        };
        let geometry = plan::geometry(session, program, input).await?;
        let layout = &geometry.layout;
        if layout.has_certain_errors() {
            let diagnostics = plan::dry_run_layout_diagnostics(layout, true);
            let n = diagnostics.iter().filter(|d| d.is_error()).count();
            self.extend(session, diagnostics);
            return Err(Stop::failed(format!(
                "the timeline cannot be laid out ({n} layout error{}); nothing was synthesized",
                if n == 1 { "" } else { "s" }
            )));
        }
        let (budget, bad_setting) = Budget::from_env(&self.engine.env);
        self.extend(session, bad_setting.into_iter().collect());
        let peak = budget::peak_bytes(program, layout, geometry.source_samples);
        if let Some(d) = budget::check(program, layout, peak, budget, false) {
            return Err(self.fail(
                Some(session),
                "the render needs more memory than its budget; nothing was synthesized",
                d,
            ));
        }
        Ok(())
    }

    async fn synthesize(
        &mut self,
        session: &Session,
        program: &Program,
        requests: &[CueRequest],
        notes: &mut Notes,
    ) -> Result<Vec<SynthesisResult>, Stop> {
        let progress = self.progress.clone();
        let callback = move |p: &SynthesisProgress| {
            if let Some(progress) = &progress {
                progress(&ProgressEvent::Synthesized {
                    cue_id: p.cue_id.clone(),
                    cached: p.cache_hit,
                    done: p.done,
                    total: p.total,
                });
            }
        };
        let batch: Vec<SynthesisRequest> = requests.iter().map(|r| r.request.clone()).collect();
        let results = self
            .cancel
            .race(session.client.synthesize_all(batch, callback))
            .await?;

        let total = requests.len();
        let mut ok = Vec::with_capacity(total);
        let mut failures = Vec::new();
        let mut cache_errors: Vec<(String, String)> = Vec::new();
        for (r, result) in requests.iter().zip(results) {
            match result {
                Ok(result) => {
                    for note in &result.notes {
                        notes.add(&r.request.provider.name, note, cue_ref(program, r.node));
                    }
                    if let Some(error) = &result.cache_write_error {
                        cache_errors.push((program.nodes[r.node].id.clone(), error.clone()));
                    }
                    ok.push(result);
                }
                Err(error) => failures.push(synthesis_failed(program, r, &error)),
            }
        }
        let new_notes = notes.drain_new(&session.spans);
        self.extend(session, new_notes);
        if let Some(d) = cache_write_failed(&session.cache_dir, &cache_errors) {
            self.extend(session, vec![d]);
        }
        if !failures.is_empty() {
            let n = failures.len();
            self.extend(session, failures);
            return Err(Stop::failed(format!(
                "synthesis failed for {n} of {total} cues"
            )));
        }
        Ok(ok)
    }

    async fn process_cues(
        &mut self,
        session: &Session,
        program: &Arc<Program>,
        requests: &[CueRequest],
        results: Vec<SynthesisResult>,
        captions_wanted: bool,
    ) -> Result<Vec<CueAudio>, Stop> {
        let mut jobs = Vec::with_capacity(requests.len());
        for (r, result) in requests.iter().zip(results) {
            let node = &program.nodes[r.node];
            let (Some(cue), Some(track)) = (node.as_cue(), program.tracks.get(node.track)) else {
                continue;
            };
            let display = (captions_wanted && cue.subtitles && !track.mute).then(|| {
                soniccraft_captions::text::display_text(&cue.text, cue.caption.as_deref())
            });
            jobs.push(CueJob {
                node: r.node,
                audio: result.audio,
                words: result.words,
                trim: track.trim_silence.clone(),
                normalize_lufs: track.normalize_lufs,
                gain: cue.gain,
                display,
            });
        }
        let cancel = self.cancel.clone();
        let sample_rate = program.output.sample_rate;
        let outcomes = blocking(move || cues::process_all(jobs, sample_rate, &cancel)).await?;

        let mut processed = Vec::with_capacity(outcomes.len());
        let mut failures = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(cue) => processed.push(cue),
                Err((node, message)) => {
                    let n = &program.nodes[node];
                    failures.push(
                        Diagnostic::error(
                            codes::DECODE_FAILED,
                            format!("cue `{}`: {message}", n.id),
                        )
                        .with_path(n.path.clone())
                        .with_hint("Re-render with `--no-cache` to synthesize the cue again")
                        .with_related([n.id.clone()]),
                    );
                }
            }
        }
        if !failures.is_empty() {
            let n = failures.len();
            self.extend(session, failures);
            return Err(Stop::failed(format!(
                "{n} synthesized cues could not be processed"
            )));
        }
        let notes = cue_notes(program, requests, &processed, &session.spans);
        self.extend(session, notes);
        Ok(processed)
    }

    async fn layout(
        &mut self,
        session: &Session,
        program: &Arc<Program>,
        requests: &[CueRequest],
        processed: &[CueAudio],
        cache_hits: &[bool],
        sources: &SourceBank,
    ) -> Result<Arc<Layout>, Stop> {
        let cache_hit: HashMap<NodeId, bool> = requests
            .iter()
            .zip(cache_hits)
            .map(|(r, &c)| (r.node, c))
            .collect();
        let frames: HashMap<NodeId, u64> = processed
            .iter()
            .map(|c| (c.node, c.buffer.frames() as u64))
            .collect();
        let durations: Vec<NodeDuration> = program
            .nodes
            .iter()
            .enumerate()
            .map(|(id, node)| match &node.kind {
                NodeKind::Cue(_) => match (frames.get(&id), cache_hit.get(&id)) {
                    (Some(&f), Some(true)) => NodeDuration::cache(f),
                    (Some(&f), _) => NodeDuration::exact(f),
                    (None, _) => NodeDuration::estimate(0),
                },
                NodeKind::Pause { .. } => NodeDuration::fixed(0),
                NodeKind::Clip(clip) => sources
                    .get(&clip.source_path)
                    .map_or(NodeDuration::estimate(0), |s| {
                        NodeDuration::source(s.buffer.frames() as u64)
                    }),
            })
            .collect();
        let program_for_task = Arc::clone(program);
        let layout =
            blocking(move || Ok(soniccraft_layout::layout(&program_for_task, &durations))).await?;
        self.extend(session, layout.diagnostics.clone());
        if layout.has_errors() {
            let n = layout.diagnostics.iter().filter(|d| d.is_error()).count();
            return Err(Stop::failed(format!(
                "the timeline cannot be laid out ({n} layout error{})",
                if n == 1 { "" } else { "s" }
            )));
        }
        if layout.total_frames == 0 {
            // The layout reports an empty timeline itself (`invalid_value`); this is only a fallback.
            let d = Diagnostic::error(
                codes::INVALID_VALUE,
                "the timeline is empty: its total length is 0 ms",
            )
            .with_hint("Add speech cues or clips, or set `output.duration_ms`");
            return Err(self.fail(None, "the timeline is empty", d));
        }
        // The memory budget again, with the exact lengths.
        let (budget, _) = Budget::from_env(&self.engine.env);
        let peak = budget::peak_bytes(program, &layout, sources.samples());
        if let Some(d) = budget::check(program, &layout, peak, budget, false) {
            return Err(self.fail(
                Some(session),
                "the render needs more memory than its budget",
                d,
            ));
        }
        Ok(Arc::new(layout))
    }

    /// Clip gains (clip gain × source normalization), with `normalization_skipped` for unmeasurable
    /// sources.
    fn clip_gains(
        &mut self,
        session: &Session,
        program: &Program,
        sources: &SourceBank,
    ) -> Vec<f32> {
        let mut gains = vec![1.0f32; program.nodes.len()];
        let mut skipped: IndexMap<TrackIdx, Vec<CueRef>> = IndexMap::new();
        for (id, node, clip) in program.clips() {
            let mut gain = clip.gain;
            if let Some(target) = program
                .tracks
                .get(node.track)
                .and_then(|t| t.normalize_lufs)
            {
                let measured = sources.get(&clip.source_path).and_then(|s| s.lufs);
                match soniccraft_dsp::loudness::normalization_gain_db(measured, target) {
                    Some(db) => gain *= soniccraft_dsp::analysis::db_to_linear(db),
                    None => skipped.entry(node.track).or_default().push(CueRef {
                        id: node.id.clone(),
                        path: node.path.clone(),
                        speaker: None,
                    }),
                }
            }
            gains[id] = gain as f32;
        }
        let mut notes = Vec::new();
        for (track, clips) in skipped {
            let t = &program.tracks[track];
            let scope = if clips.len() == 1 {
                format!("clip `{}`", clips[0].id)
            } else {
                format!("{} clips ({})", clips.len(), diag::list_ids(&clips))
            };
            notes.push(
                Diagnostic::info(
                    codes::NORMALIZATION_SKIPPED,
                    format!(
                        "the source of {scope} on track `{}` is too short (< 400 ms) or too quiet to measure its \
                         loudness; it plays at its original level",
                        t.id
                    ),
                )
                .with_path(diag::group_path(&clips, &session.spans, Some(&t.path)))
                .with_hint(format!(
                    "Nothing needs to change; set `normalize: false` on track `{}` to silence this note",
                    t.id
                ))
                .with_related(clips.iter().map(|c| c.id.clone())),
            );
        }
        self.extend(session, notes);
        gains
    }

    async fn master(
        &mut self,
        session: &Session,
        program: &Program,
        mix: AudioBuffer,
    ) -> Result<(AudioBuffer, MasterResult), Stop> {
        let output = &program.output;
        let params = MasterParams {
            target_lufs: output.target_lufs,
            true_peak_db: output.true_peak_db,
            normalize: output.normalize,
        };
        let cancel = self.cancel.clone();
        let (master, result) = blocking(move || {
            let mut mix = mix;
            soniccraft_dsp::render::declick_tail(&mut mix);
            let result = soniccraft_dsp::master::master_with_cancel(&mut mix, &params, &|| {
                cancel.is_cancelled()
            })
            .map_err(|e| match e {
                DspError::Cancelled => Stop::Cancelled,
                other => Stop::failed(format!("internal error while mastering: {other}")),
            })?;
            Ok((mix, result))
        })
        .await?;
        let notes = master_notes(program, &result);
        self.extend(session, notes);
        Ok((master, result))
    }

    /// Encode the master to the audio file's temporary name. Lossy files are decoded and measured; one
    /// that peaks above the ceiling gets the master limited again (the ceiling lowered by the overshoot plus
    /// 0.1 dB), and one whose loudness the codec moved more than 0.2 LU off a target the master had reached
    /// gets the difference as gain (limited to the ceiling); each is encoded again, at most three times.
    async fn encode(
        &mut self,
        session: &Session,
        program: &Program,
        plan: &OutputPlan,
        master: AudioBuffer,
        mastered: &MasterResult,
    ) -> Result<Encoded, Stop> {
        let output = &program.output;
        let ffmpeg = Ffmpeg::from_option(self.engine.options.ffmpeg_path.clone());
        let options = EncodeOptions {
            format: checks::encode_format(output),
            dither: output.dither,
            ffmpeg: ffmpeg.clone(),
        };
        let reserved = lock(&self.staging).reserve(&plan.audio);
        let temp = match reserved {
            Ok(temp) => temp,
            Err(e) => {
                let d = write_error(&plan.audio, &e.to_string());
                return Err(self.fail(None, "encoding the output failed", d));
            }
        };
        let lossy = matches!(
            output.format,
            OutputFormat::Mp3 | OutputFormat::Ogg | OutputFormat::Opus | OutputFormat::M4a
        );
        let ceiling = output.true_peak_db;
        let first_master = mastered.after;
        // Codec loudness drift is corrected only towards a target the master itself reached.
        let target = mastered.target_reached.then_some(output.target_lufs);
        let cancel = self.cancel.clone();
        let task_temp = temp.clone();
        let outcome = blocking(move || {
            Ok(encode_and_verify(
                master,
                &task_temp,
                &options,
                &ffmpeg,
                lossy.then_some(Verify {
                    ceiling,
                    master: first_master,
                    target,
                }),
                &cancel,
            ))
        })
        .await?;
        let encoded = match outcome {
            Ok(encoded) => encoded,
            Err(EncodeFailure::Stop(stop)) => return Err(stop),
            Err(EncodeFailure::Dsp(error)) => {
                let shown = display_path(&plan.audio.display().to_string());
                let message = error
                    .to_string()
                    .replace(&temp.display().to_string(), &shown);
                let d = match &error {
                    DspError::FfmpegNotFound(_) => {
                        let source = FormatSource::of(program, &session.overrides);
                        let name = plan
                            .audio
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned());
                        let (message, hint) = checks::ffmpeg_requirement(
                            output.format,
                            source,
                            "ffmpeg was not found on PATH",
                            name.as_deref(),
                        );
                        let mut d =
                            Diagnostic::error(codes::FORMAT_REQUIRES_FFMPEG, message).with_hint(hint);
                        if let Some(path) = source.path() {
                            d = d.with_path(path);
                        }
                        d
                    }
                    _ => Diagnostic::error(
                        codes::ENCODE_FAILED,
                        format!("cannot write {shown}: {message}"),
                    )
                    .with_path(if output.format_explicit {
                        "output.format"
                    } else {
                        "output"
                    })
                    .with_hint(
                        "Check that the output directory is writable and the disk is not full; for ogg/opus/m4a \
                         check the ffmpeg installation",
                    ),
                };
                return Err(self.fail(Some(session), "encoding the output failed", d));
            }
        };
        let mut notes = Vec::new();
        let summary = &encoded.summary;
        if let (Some(requested), Some(used)) = (output.bitrate_bps, summary.bitrate_bps)
            && requested != used
        {
            notes.push(
                Diagnostic::info(
                    codes::BITRATE_ADJUSTED,
                    format!(
                        "the {} encoder used {} kbps instead of {} kbps: at {} Hz with {} channel{} smaller frames \
                         cannot hold the header that makes playback gapless",
                        output.format,
                        used / 1000,
                        requested / 1000,
                        summary.sample_rate,
                        summary.channels,
                        if summary.channels == 1 { "" } else { "s" }
                    ),
                )
                .with_path("output.bitrate")
                .with_hint(format!("Set `bitrate: \"{}k\"` to make this explicit", used / 1000)),
            );
        }
        if let Some(delivered) = &encoded.delivered {
            notes.extend(delivered_notes(program, mastered, delivered));
        }
        self.extend(session, notes);
        Ok(encoded)
    }

    /// Write side files (blocking pool) under their temporary names.
    async fn write_files(&mut self, files: Vec<(PathBuf, Vec<u8>)>) -> Result<(), Stop> {
        if files.is_empty() {
            return Ok(());
        }
        let staging = Arc::clone(&self.staging);
        let cancel = self.cancel.clone();
        let outcome = blocking(move || {
            for (path, bytes) in files {
                cancel.check()?;
                if let Err(e) = lock(&staging).write(&path, &bytes) {
                    return Ok(Err((path, e)));
                }
            }
            Ok(Ok(()))
        })
        .await?;
        match outcome {
            Ok(()) => Ok(()),
            Err((path, e)) => {
                let d = write_error(&path, &e.to_string());
                Err(self.fail(None, "writing a side file failed", d))
            }
        }
    }
}

/// What [`encode_and_verify`] checks the delivered (lossy) file against.
struct Verify {
    /// The true-peak ceiling in dBTP.
    ceiling: f64,
    /// The measurement of the master that is encoded first.
    master: LoudnessStats,
    /// The loudness target, when the master reached it (codec drift is corrected towards it).
    target: Option<f64>,
}

/// Encode `master` to `temp`; with `verify` (lossy formats), measure the delivered file and compensate codec
/// overshoot and loudness drift (see [`Job::encode`]).
fn encode_and_verify(
    mut master: AudioBuffer,
    temp: &Path,
    options: &EncodeOptions,
    ffmpeg: &Ffmpeg,
    verify: Option<Verify>,
    cancel: &Cancel,
) -> Result<Encoded, EncodeFailure> {
    let cancelled = || cancel.is_cancelled();
    let mut summary = soniccraft_dsp::encode::encode_file(&master, temp, options)?;
    let Some(Verify {
        ceiling,
        master: first,
        target,
    }) = verify
    else {
        return Ok(Encoded {
            master,
            summary,
            delivered: None,
        });
    };
    let measure =
        |cancelled: &dyn Fn() -> bool| match soniccraft_dsp::loudness::measure_file_with_cancel(
            temp, ffmpeg, cancelled,
        ) {
            Ok(m) => Ok(Ok(m)),
            Err(DspError::Cancelled) => Err(EncodeFailure::Stop(Stop::Cancelled)),
            Err(other) => Ok(Err(other.to_string())),
        };
    let mut delivered = Delivered {
        measured: measure(&cancelled)?,
        master: first,
        ceiling_dbtp: ceiling,
        passes: 0,
    };
    while delivered.passes < MAX_COMPENSATION_PASSES {
        let Ok(measured) = &delivered.measured else {
            break;
        };
        // The peaks come first: a codec overshoot lowers the ceiling by the overshoot plus a margin. Then
        // loudness: the codec's drift (typically 0.1-0.4 LU) goes back into the master as gain.
        let overshoot = measured.true_peak_dbtp.filter(|&tp| tp > ceiling);
        let drift = target
            .zip(measured.integrated_lufs)
            .map(|(target, lufs)| target - lufs)
            .filter(|drift| drift.abs() > soniccraft_dsp::master::TARGET_TOLERANCE_LU);
        let (gain_db, lowered) = match (overshoot, drift) {
            (Some(peak), _) => (
                0.0,
                delivered.ceiling_dbtp - (peak - ceiling) - COMPENSATION_MARGIN_DB,
            ),
            (None, Some(drift)) => (drift, delivered.ceiling_dbtp),
            (None, None) => break,
        };
        let gain = soniccraft_dsp::analysis::db_to_linear(gain_db) as f32;
        let limited =
            soniccraft_dsp::limiter::limit_in_place(&mut master, gain, lowered, &cancelled)?;
        summary = soniccraft_dsp::encode::encode_file(&master, temp, options)?;
        delivered.master = limited.stats;
        delivered.ceiling_dbtp = lowered;
        delivered.passes += 1;
        delivered.measured = measure(&cancelled)?;
    }
    Ok(Encoded {
        master,
        summary,
        delivered: Some(delivered),
    })
}

/// `true_peak_exceeded` when the delivered file still peaks above the ceiling, `loudness_target_missed` when
/// it misses the loudness target the master had reached (after the compensation passes), and
/// `decode_failed` (warning) when it could not be measured.
fn delivered_notes(
    program: &Program,
    mastered: &MasterResult,
    delivered: &Delivered,
) -> Vec<Diagnostic> {
    let output = &program.output;
    let mut notes = Vec::new();
    if let Ok(measured) = &delivered.measured
        && mastered.target_reached
        && let Some(lufs) = measured.integrated_lufs
        && (output.target_lufs - lufs).abs() > soniccraft_dsp::master::TARGET_TOLERANCE_LU
    {
        let off = output.target_lufs - lufs;
        notes.push(
            Diagnostic::warning(
                codes::LOUDNESS_TARGET_MISSED,
                format!(
                    "the encoded {} file measures {lufs:.1} LUFS, {:.1} LU {} the {:.1} LUFS target after {} \
                     compensation pass{} (lossy codecs shift loudness and raise the peaks of loud masters; \
                     the true peak stays under the {:.1} dBTP ceiling first)",
                    output.format,
                    off.abs(),
                    if off > 0.0 { "below" } else { "above" },
                    output.target_lufs,
                    delivered.passes,
                    if delivered.passes == 1 { "" } else { "es" },
                    output.true_peak_db
                ),
            )
            .with_path("output.target_lufs")
            .with_hint(
                "Lower `output.target_lufs` (loud masters leave a lossy codec no headroom), raise \
                 `output.bitrate`, or deliver `format: wav` or `format: flac`",
            ),
        );
    }
    match &delivered.measured {
        Ok(measured) => match measured.true_peak_dbtp {
            Some(peak) if peak > output.true_peak_db => notes.push(
                Diagnostic::warning(
                    codes::TRUE_PEAK_EXCEEDED,
                    format!(
                        "the encoded {} file peaks at {peak:.2} dBTP, above the {:.1} dBTP ceiling, after {} \
                         compensation pass{} (the codec reshapes peaks)",
                        output.format,
                        output.true_peak_db,
                        delivered.passes,
                        if delivered.passes == 1 { "" } else { "es" }
                    ),
                )
                .with_path("output.true_peak_db")
                .with_hint(format!(
                    "Lower `output.true_peak_db` (e.g. `true_peak_db: {:.1}` leaves headroom for lossy codecs), \
                     raise `output.bitrate`, or deliver `format: wav` or `format: flac`",
                    (output.true_peak_db - 1.0).max(-9.0)
                )),
            ),
            _ => {}
        },
        Err(reason) => notes.push(
            Diagnostic::warning(
                codes::DECODE_FAILED,
                format!(
                    "the encoded {} file could not be decoded to check its loudness and true peak ({reason}); \
                     the report shows the master that was encoded",
                    output.format
                ),
            )
            .with_path("output.format")
            .with_hint("Install or update ffmpeg to measure opus files, or deliver `format: wav` or `format: flac`"),
        ),
    }
    notes
}

/// Mastering notes: unreachable and missed targets, silence, too short to measure.
fn master_notes(program: &Program, result: &MasterResult) -> Vec<Diagnostic> {
    let output = &program.output;
    let mut notes = Vec::new();
    if result.unreachable {
        let achieved = result.after.integrated_lufs;
        let achieved_text = achieved.map_or_else(
            || "an undefined loudness".to_string(),
            |i| format!("{i:.1} LUFS"),
        );
        let hint = match achieved {
            Some(i) => format!(
                "Lower `output.target_lufs` to {i:.1} or below (the loudest this mix allows with at most 6 dB of \
                 limiting), raise `output.true_peak_db`, or turn down loud transients (e.g. sfx `volume`)"
            ),
            None => "Lower `output.target_lufs` (or pick a quieter `loudness_preset`), raise `output.true_peak_db`, \
                     or turn down loud transients (e.g. sfx `volume`)"
                .to_string(),
        };
        notes.push(
            Diagnostic::warning(
                codes::LOUDNESS_TARGET_UNREACHABLE,
                format!(
                    "the loudness target {:.1} LUFS needs more than 6 dB of limiting under the {:.1} dBTP \
                     ceiling; the master reaches {achieved_text}",
                    output.target_lufs, output.true_peak_db
                ),
            )
            .with_path("output.target_lufs")
            .with_hint(hint),
        );
    }
    if result.normalized
        && !result.target_reached
        && !result.unreachable
        && !result.silent
        && !result.too_short
    {
        let passes = format!(
            "after {} limiter pass{}",
            result.iterations,
            if result.iterations == 1 { "" } else { "es" }
        );
        let (message, hint) = match result.after.integrated_lufs {
            Some(i) => (
                format!(
                    "mastering stopped at {i:.1} LUFS, {:.1} LU from the {:.1} LUFS target, {passes} (the \
                     loudness jumps where quiet passages cross the BS.1770 gate)",
                    (i - output.target_lufs).abs(),
                    output.target_lufs
                ),
                format!(
                    "Set `output.target_lufs` to {i:.1} to accept this level, or even out the mix (raise quiet \
                     passages, lower loud transients)"
                ),
            ),
            None => (
                format!(
                    "mastering stopped {passes} with the master below the -70 LUFS measurement gate, short of the \
                     {:.1} LUFS target",
                    output.target_lufs
                ),
                "Raise the level of the mix (track `volume` or `gain_db`), or lower `output.target_lufs`".to_string(),
            ),
        };
        notes.push(
            Diagnostic::warning(codes::LOUDNESS_TARGET_MISSED, message)
                .with_path("output.target_lufs")
                .with_hint(hint),
        );
    }
    if result.silent {
        notes.push(
            Diagnostic::warning(
                codes::SILENT_OUTPUT,
                "the mix is silent (nothing above the -70 LUFS gate), so no loudness gain was applied",
            )
            .with_hint(
                "Check that the tracks are not muted or at `volume: 0`, and that the cues and clips produce \
                 audio",
            ),
        );
    }
    if result.too_short {
        notes.push(
            Diagnostic::info(
                codes::NORMALIZATION_SKIPPED,
                "the output is shorter than 400 ms, too short to measure its loudness; loudness \
                 normalization was skipped (the true-peak limiter still ran)",
            )
            .with_path("output")
            .with_hint("Nothing needs to change for very short renders"),
        );
    }
    notes
}

/// `encode_failed` for a file that could not be written.
fn write_error(path: &Path, error: &str) -> Diagnostic {
    Diagnostic::error(
        codes::ENCODE_FAILED,
        format!(
            "cannot write {}: {error}",
            display_path(&path.display().to_string())
        ),
    )
    .with_hint("Check that the output directory is writable and the disk is not full")
}

/// One `cache_write_failed` warning for the cues whose synthesized audio could not be stored.
fn cache_write_failed(cache_dir: &Path, errors: &[(String, String)]) -> Option<Diagnostic> {
    let (first_cue, first_error) = errors.first()?;
    let n = errors.len();
    let cues = if n == 1 {
        format!("cue `{first_cue}`")
    } else {
        format!(
            "{n} cues ({})",
            diagnostics::list_names(errors.iter().map(|(c, _)| c.as_str()), 6)
        )
    };
    Some(
        Diagnostic::warning(
            codes::CACHE_WRITE_FAILED,
            format!(
                "could not write {cues} to the TTS cache in {}: {first_error}; later renders will synthesize {} \
                 again",
                cache_dir.display(),
                if n == 1 { "it" } else { "them" }
            ),
        )
        .with_hint(
            "Make the cache directory writable, or pass --cache-dir (MCP: start the server with it) or set \
             SONICCRAFT_CACHE_DIR to a writable directory",
        )
        .with_related(errors.iter().map(|(c, _)| c.clone())),
    )
}

/// Error codes that are about the environment or the render options rather than the timeline document.
const ENVIRONMENT_CODES: &[&str] = &[
    codes::ASSET_NOT_FOUND,
    codes::ASSET_UNREADABLE,
    codes::MISSING_CREDENTIALS,
    codes::PROVIDER_UNAVAILABLE,
    codes::UNKNOWN_VOICE,
    codes::FORMAT_REQUIRES_FFMPEG,
    codes::TIMELINE_TOO_LONG,
];

/// `the timeline has 3 errors` (document problems) or `render stopped: 2 errors` (environment or render
/// options); the diagnostics explain them.
fn invalid_message(diagnostics: &[Diagnostic]) -> String {
    let errors: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.is_error()).collect();
    let n = errors.len();
    let (s, them) = if n == 1 { ("", "it") } else { ("s", "them") };
    if errors
        .iter()
        .any(|d| ENVIRONMENT_CODES.contains(&d.code.as_str()))
    {
        format!("render stopped: {n} error{s}; fix {them} and render again")
    } else {
        format!("the timeline has {n} error{s}; fix {them} and render again")
    }
}

/// The `synthesis_failed` diagnostic of one cue.
fn synthesis_failed(program: &Program, request: &CueRequest, error: &TtsError) -> Diagnostic {
    let node = &program.nodes[request.node];
    let hint = error.hint.clone().unwrap_or_else(|| {
        let provider = &request.request.provider;
        match error.kind {
            TtsErrorKind::Auth => format!(
                "Check the API key{} of provider `{}`",
                provider
                    .api_key_env
                    .as_deref()
                    .map(|v| format!(" in `{v}`"))
                    .unwrap_or_default(),
                provider.name
            ),
            TtsErrorKind::RateLimited => format!(
                "Lower `max_concurrency` of provider `{}` (or `--jobs`) and render again; finished cues are cached",
                provider.name
            ),
            TtsErrorKind::QuotaExceeded => {
                "The provider account is out of credits or quota; top it up or use another provider".to_string()
            }
            TtsErrorKind::InvalidRequest => {
                "Check the cue's text, `voice_id`, `model` and `settings` against the provider's documentation"
                    .to_string()
            }
            TtsErrorKind::NotFound => format!(
                "Check `voice_id` and `model`; `soniccraft voices {}` lists the available voices",
                provider.provider_type
            ),
            TtsErrorKind::Network | TtsErrorKind::Timeout => format!(
                "Check the network and the `base_url` of provider `{}`, or raise its `timeout_ms`; then render again",
                provider.name
            ),
            TtsErrorKind::Unavailable => {
                "This provider cannot run here; run `soniccraft providers` to see what it needs".to_string()
            }
            TtsErrorKind::Provider | TtsErrorKind::Decode | TtsErrorKind::Io => {
                "Render again; if it keeps failing, try another voice or provider".to_string()
            }
        }
    });
    Diagnostic::error(
        codes::SYNTHESIS_FAILED,
        format!("cue `{}`: {error}", node.id),
    )
    .with_path(node.path.clone())
    .with_hint(hint)
    .with_related([node.id.clone()])
}

/// `normalization_skipped` (per track) and `alignment_estimated` (per provider) notes of processed cues.
fn cue_notes(
    program: &Program,
    requests: &[CueRequest],
    processed: &[CueAudio],
    spans: &soniccraft_core::SpanIndex,
) -> Vec<Diagnostic> {
    let provider_of: HashMap<NodeId, (&str, soniccraft_core::ir::ProviderType)> = requests
        .iter()
        .map(|r| {
            (
                r.node,
                (
                    r.request.provider.name.as_str(),
                    r.request.provider.provider_type,
                ),
            )
        })
        .collect();
    let mut skipped: IndexMap<TrackIdx, Vec<CueRef>> = IndexMap::new();
    let mut estimated: IndexMap<&str, (soniccraft_core::ir::ProviderType, Vec<CueRef>)> =
        IndexMap::new();
    for cue in processed {
        let node = &program.nodes[cue.node];
        if cue.normalization_skipped {
            skipped
                .entry(node.track)
                .or_default()
                .push(cue_ref(program, cue.node));
        }
        if cue.alignment.as_ref().is_some_and(|a| a.is_estimated())
            && let Some(&(provider, provider_type)) = provider_of.get(&cue.node)
        {
            estimated
                .entry(provider)
                .or_insert_with(|| (provider_type, Vec::new()))
                .1
                .push(cue_ref(program, cue.node));
        }
    }
    let mut out = Vec::new();
    for (track, cues) in skipped {
        let t = &program.tracks[track];
        let scope = if cues.len() == 1 {
            format!("cue `{}`", cues[0].id)
        } else {
            format!("{} cues ({})", cues.len(), diag::list_ids(&cues))
        };
        out.push(
            Diagnostic::info(
                codes::NORMALIZATION_SKIPPED,
                format!(
                    "{scope} of track `{}` {} too short (< 400 ms) or too quiet to measure loudness; left at the \
                     provider's level",
                    t.id,
                    if cues.len() == 1 { "is" } else { "are" }
                ),
            )
            .with_path(diag::group_path(&cues, spans, Some(&t.path)))
            .with_hint(format!(
                "Nothing needs to change for short interjections; set `normalize: false` on track `{}` to silence \
                 this note",
                t.id
            ))
            .with_related(cues.iter().map(|c| c.id.clone())),
        );
    }
    for (provider, (provider_type, cues)) in estimated {
        let has_timestamps = soniccraft_core::providers::meta(provider_type).word_timestamps;
        let reason = if has_timestamps {
            "the provider's word timings did not match all subtitle words (e.g. a `caption` override or \
             normalized numbers), so the rest were estimated"
        } else {
            "the provider returns no word timestamps, so they were estimated from the audio"
        };
        let scope = if cues.len() == 1 {
            format!("cue `{}`", cues[0].id)
        } else {
            format!("{} cues ({})", cues.len(), diag::list_ids(&cues))
        };
        out.push(
            Diagnostic::info(
                codes::ALIGNMENT_ESTIMATED,
                format!("word timings of {scope} (provider `{provider}`): {reason}"),
            )
            .with_path(diag::group_path(&cues, spans, None))
            .with_hint(
                "For exact subtitle timing use a provider with word timestamps (elevenlabs, cartesia, kokoro or \
                 mock)",
            )
            .with_related(cues.iter().map(|c| c.id.clone())),
        );
    }
    out
}

/// What synthesis did: cache hits, provider requests and the providers called.
fn synthesis_report(requests: &[CueRequest], results: &[SynthesisResult]) -> SynthesisReport {
    let mut report = SynthesisReport {
        cues: requests.len(),
        ..SynthesisReport::default()
    };
    for (r, result) in requests.iter().zip(results) {
        if result.cache_hit {
            report.cache_hits += 1;
        } else if result.duplicate_of.is_none() {
            report.synthesized += 1;
            let name = &r.request.provider.name;
            if !report.providers_called.contains(name) {
                report.providers_called.push(name.clone());
            }
        }
    }
    report
}

/// The loudness section of the report: the delivered file's measurement (lossy formats: the decoded
/// file, with the encoded master in `pre_encode`), the mastering loop's values otherwise.
fn loudness_report(
    program: &Program,
    result: &MasterResult,
    delivered: Option<&Delivered>,
) -> LoudnessReport {
    let base = LoudnessReport {
        integrated_lufs: result.after.integrated_lufs,
        true_peak_dbtp: result.after.true_peak_dbtp,
        sample_peak_dbfs: result.after.sample_peak_dbfs,
        loudness_range_lu: result.after.loudness_range_lu,
        target_lufs: program.output.target_lufs,
        true_peak_target_dbtp: program.output.true_peak_db,
        normalized: result.normalized,
        gain_db: result.gain_db,
        limiter_max_reduction_db: result.limiter_max_reduction_db,
        iterations: result.iterations,
        pre_encode: None,
    };
    let Some(delivered) = delivered else {
        return base;
    };
    let master = &delivered.master;
    let pre_encode = Some(PreEncodeLoudness {
        integrated_lufs: master.integrated_lufs,
        true_peak_dbtp: master.true_peak_dbtp,
        sample_peak_dbfs: master.sample_peak_dbfs,
        loudness_range_lu: master.loudness_range_lu,
        ceiling_dbtp: delivered.ceiling_dbtp,
        compensation_passes: delivered.passes,
    });
    match &delivered.measured {
        Ok(file) => LoudnessReport {
            integrated_lufs: file.integrated_lufs,
            true_peak_dbtp: file.true_peak_dbtp,
            sample_peak_dbfs: file.sample_peak_dbfs,
            loudness_range_lu: file.loudness_range_lu,
            pre_encode,
            ..base
        },
        Err(_) => LoudnessReport {
            integrated_lufs: master.integrated_lufs,
            true_peak_dbtp: master.true_peak_dbtp,
            sample_peak_dbfs: master.sample_peak_dbfs,
            loudness_range_lu: master.loudness_range_lu,
            pre_encode,
            ..base
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_messages_tell_documents_from_environments() {
        let doc = [Diagnostic::error(codes::UNKNOWN_FIELD, "x")];
        assert_eq!(
            invalid_message(&doc),
            "the timeline has 1 error; fix it and render again"
        );
        let env = [
            Diagnostic::error(codes::UNKNOWN_FIELD, "x"),
            Diagnostic::error(codes::FORMAT_REQUIRES_FFMPEG, "y"),
            Diagnostic::warning(codes::CUE_PUSHED, "z"),
        ];
        assert_eq!(
            invalid_message(&env),
            "render stopped: 2 errors; fix them and render again"
        );
    }

    #[test]
    fn one_cache_warning_per_render() {
        assert!(cache_write_failed(Path::new("/c"), &[]).is_none());
        let errors = vec![
            (
                "a".to_string(),
                "Permission denied (os error 13)".to_string(),
            ),
            (
                "b".to_string(),
                "Permission denied (os error 13)".to_string(),
            ),
        ];
        let d = cache_write_failed(Path::new("/c"), &errors).unwrap();
        assert_eq!(d.code, codes::CACHE_WRITE_FAILED);
        assert!(
            d.message
                .starts_with("could not write 2 cues (`a`, `b`) to the TTS cache in /c"),
            "{}",
            d.message
        );
        assert!(d.hint.as_deref().unwrap().contains("SONICCRAFT_CACHE_DIR"));
        assert_eq!(d.related, ["a", "b"]);
    }
}
