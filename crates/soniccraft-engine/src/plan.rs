//! Dry runs (`plan`, DESIGN.md §5.3, §8): validate, build the duration table, lay the timeline out and
//! compute the cue-mode ducking regions. No provider is ever called.
//!
//! Durations: cached cues are exact (the cached audio decoded, resampled and trimmed exactly like a render
//! does), and so are uncached `mock` cues (the mock voice is rendered in-process, which is what a render
//! does too); other cues use their provider's estimate, clips the length of their source. The layout marks
//! every diagnostic that depends on an estimate; dry runs report such errors as warnings (exact durations
//! may make them go away), and renders check them again before and after synthesis.
//!
//! Nothing here allocates memory proportional to the timeline's length: ducking regions are computed from
//! the item intervals ([`soniccraft_dsp::duck::cue_regions`]), and validation computes none.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use soniccraft_core::diagnostics;
use soniccraft_core::ir::{NodeKind, Program, ProviderType};
use soniccraft_core::{Diagnostic, Severity};
use soniccraft_dsp::decode::ProbeInfo;
use soniccraft_layout::{DiagramOptions, DuckedTrack, Layout, NodeDuration};
use soniccraft_tts::CachedMeta;

use crate::budget::{self, Budget};
use crate::cancel::{blocking, par_map, workers};
use crate::checks::{self, CheckOptions};
use crate::context::{CueRequest, LowerOverrides, Session, cue_requests};
use crate::report::{DuckingReport, PlanReport, ProviderSynthesisPlan, SynthesisPlan};
use crate::{Engine, TimelineSource, reports, speech};

/// Longest plan diagram returned; a longer one (tens of thousands of items) is cut at a line boundary with
/// a note, since `items` has every item anyway.
pub(crate) const MAX_DIAGRAM_BYTES: usize = 256 * 1024;

/// Appended to layout errors that dry runs downgrade to warnings because they depend on estimated
/// durations.
pub(crate) const ESTIMATE_NOTE: &str =
    " (based on estimated speech durations; the render checks again with exact durations)";

/// [`crate::Engine::plan`].
pub(crate) async fn plan(engine: &Engine, source: &TimelineSource) -> PlanReport {
    let session = Session::open(engine, source, LowerOverrides::default()).await;
    let mut diagnostics = session.diagnostics.clone();
    let Some(program) = session.program.clone() else {
        diagnostics.extend(checks::document_assets(&session, true).await);
        return invalid(diagnostics);
    };
    let (requests, mut request_errors) = cue_requests(&program);
    session.attach(&mut request_errors);
    diagnostics.extend(request_errors);
    let env = checks::environment(
        engine,
        &session,
        &program,
        &requests,
        CheckOptions {
            probe_assets: true,
            online: false,
        },
    )
    .await;
    diagnostics.extend(env.diagnostics);

    let input = DryRunInput {
        clip_frames: clip_frames(&program, &env.probes),
        source_samples: source_samples(&program, &env.probes),
        requests,
        cached: env.cached,
        ducking: true,
        diagram: true,
    };
    let geometry = match geometry(&session, &program, input).await {
        Ok(geometry) => geometry,
        Err(stop) => {
            diagnostics.push(Diagnostic::error(
                diagnostics::codes::INVALID_VALUE,
                format!("internal error: the dry run failed ({stop:?})"),
            ));
            return invalid(diagnostics);
        }
    };
    let mut layout_diagnostics = dry_run_layout_diagnostics(&geometry.layout, false);
    layout_diagnostics.extend(budget_diagnostics(engine, &program, &geometry));
    session.attach(&mut layout_diagnostics);
    diagnostics.extend(layout_diagnostics);
    diagnostics::sort(&mut diagnostics);

    PlanReport {
        valid: !diagnostics::has_errors(&diagnostics),
        diagnostics,
        duration_ms: geometry.layout.total_ms(),
        estimated: geometry.layout.estimated(),
        sample_rate: program.output.sample_rate,
        tracks: reports::tracks(&program),
        items: reports::items(&program, &geometry.layout),
        ducking: geometry.ducking,
        synthesis: geometry.synthesis,
        diagram: geometry.diagram,
    }
}

/// The report of a timeline that did not compile.
fn invalid(mut diagnostics: Vec<Diagnostic>) -> PlanReport {
    diagnostics::sort(&mut diagnostics);
    PlanReport {
        valid: !diagnostics::has_errors(&diagnostics),
        diagnostics,
        duration_ms: 0.0,
        estimated: false,
        sample_rate: soniccraft_core::lower::DEFAULT_SAMPLE_RATE,
        tracks: Vec::new(),
        items: Vec::new(),
        ducking: Vec::new(),
        synthesis: SynthesisPlan {
            cues: 0,
            cached: 0,
            characters_to_synthesize: 0,
            by_provider: IndexMap::new(),
        },
        diagram: String::new(),
    }
}

/// The layout diagnostics a dry run reports: errors that depend on estimated durations become warnings
/// with [`ESTIMATE_NOTE`]; with `errors_only` (validation) the other warnings and notes are left out.
pub(crate) fn dry_run_layout_diagnostics(layout: &Layout, errors_only: bool) -> Vec<Diagnostic> {
    layout
        .diagnostics_with_estimates()
        .filter_map(|(d, estimated)| {
            let mut d = d.clone();
            if d.is_error() && estimated {
                d.severity = Severity::Warning;
                d.message.push_str(ESTIMATE_NOTE);
                return Some(d);
            }
            (!errors_only || d.is_error()).then_some(d)
        })
        .collect()
}

/// The memory-budget warning of a dry run (and a warning about a bad `SONICCRAFT_MAX_RENDER_GB`).
pub(crate) fn budget_diagnostics(
    engine: &Engine,
    program: &Program,
    geometry: &Geometry,
) -> Vec<Diagnostic> {
    let (budget, bad_setting) = Budget::from_env(&engine.env);
    let peak = budget::peak_bytes(program, &geometry.layout, geometry.source_samples);
    bad_setting
        .into_iter()
        .chain(budget::check(program, &geometry.layout, peak, budget, true))
        .collect()
}

/// Whole length of every probed clip source at the output rate.
pub(crate) fn clip_frames(
    program: &Program,
    probes: &HashMap<PathBuf, ProbeInfo>,
) -> HashMap<PathBuf, u64> {
    let rate = program.output.sample_rate;
    probes
        .iter()
        .map(|(path, probe)| (path.clone(), output_frames(probe, rate)))
        .collect()
}

/// Σ frames × channels of the distinct probed sources as a render holds them (at the output rate; mono
/// stays mono, wider sources become stereo, everything is mono for mono output).
pub(crate) fn source_samples(program: &Program, probes: &HashMap<PathBuf, ProbeInfo>) -> u64 {
    let rate = program.output.sample_rate;
    let out_channels = u64::from(program.output.channels.max(1));
    probes
        .values()
        .map(|probe| {
            let channels = (probe.channels.max(1) as u64).min(2).min(out_channels);
            output_frames(probe, rate).saturating_mul(channels)
        })
        .fold(0u64, u64::saturating_add)
}

fn output_frames(probe: &ProbeInfo, rate: u32) -> u64 {
    soniccraft_dsp::resample::output_len(
        usize::try_from(probe.frames).unwrap_or(usize::MAX),
        probe.sample_rate,
        rate,
    ) as u64
}

/// What a dry-run layout needs besides the program.
pub(crate) struct DryRunInput {
    /// One request per speech cue.
    pub requests: Vec<CueRequest>,
    /// Cache metadata per request.
    pub cached: Vec<Option<CachedMeta>>,
    /// Whole length of each clip source at the output rate; a source missing here has an unknown length.
    pub clip_frames: HashMap<PathBuf, u64>,
    /// Σ frames × channels of the distinct sources at the output rate (for the memory budget).
    pub source_samples: u64,
    /// Compute the ducking regions (plan).
    pub ducking: bool,
    /// Render the ASCII diagram (plan).
    pub diagram: bool,
}

/// Lay `program` out with dry-run durations on the blocking pool. Shared by `plan`, `validate` (which
/// reports the layout's errors) and renders (which stop on errors that exact durations cannot remove
/// before synthesizing anything).
pub(crate) async fn geometry(
    session: &Session,
    program: &Arc<Program>,
    input: DryRunInput,
) -> Result<Geometry, crate::cancel::Stop> {
    let program = Arc::clone(program);
    let client = session.client.clone();
    let cache_dir = session.cache_dir.clone();
    blocking(move || Ok(DryRun::run(&program, &client, &cache_dir, input))).await
}

/// Outputs of the CPU-bound part of a dry run.
pub(crate) struct Geometry {
    /// The layout (diagnostics not yet positioned).
    pub layout: Layout,
    /// Cue-mode ducking regions of every ducked track (empty unless requested).
    pub ducking: Vec<DuckingReport>,
    /// What a render would synthesize.
    pub synthesis: SynthesisPlan,
    /// The ASCII diagram (empty unless requested).
    pub diagram: String,
    /// Σ frames × channels of the distinct sources at the output rate.
    pub source_samples: u64,
}

/// How a cue's dry-run length is measured exactly.
enum Exact<'a> {
    /// From its cache entry (falls back to the mock voice for mock cues when the entry is unreadable).
    Cached(&'a CachedMeta, PathBuf),
    /// By rendering the mock voice.
    Mock,
}

/// The CPU-bound part of a dry run.
struct DryRun;

impl DryRun {
    fn run(
        program: &Program,
        client: &soniccraft_tts::TtsClient,
        cache_dir: &Path,
        input: DryRunInput,
    ) -> Geometry {
        let sample_rate = program.output.sample_rate;
        let requests = &input.requests;
        let keys: Vec<Option<String>> = requests
            .iter()
            .map(|r| client.cache_key(&r.request).ok())
            .collect();

        // Exact lengths: cached cues (decoded, resampled and trimmed exactly like a render does) and mock
        // cues (rendered like a render does).
        let jobs: Vec<(usize, Exact<'_>)> = requests
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let cached = input.cached[i]
                    .as_ref()
                    .zip(keys[i].as_ref())
                    .map(|(meta, key)| Exact::Cached(meta, cache_dir.join(format!("{key}.wav"))));
                let mock =
                    (r.request.provider.provider_type == ProviderType::Mock).then_some(Exact::Mock);
                Some((i, cached.or(mock)?))
            })
            .collect();
        let measured = par_map(jobs, workers(), |(i, job)| {
            let r = &requests[i];
            let trim = program
                .tracks
                .get(program.nodes[r.node].track)
                .and_then(|t| t.trim_silence.as_ref());
            let mock = || {
                let audio = soniccraft_tts::render_mock(
                    &r.request.provider,
                    &r.request.voice,
                    &r.request.text,
                )?;
                let (prepared, _) = speech::prepare(&audio.audio, sample_rate, trim).ok()?;
                Some(NodeDuration::exact(prepared.frames() as u64))
            };
            let duration = match job {
                Exact::Cached(meta, wav) => cached_frames(meta, &wav, trim, sample_rate)
                    .map(NodeDuration::cache)
                    .or_else(|| {
                        (r.request.provider.provider_type == ProviderType::Mock)
                            .then(mock)
                            .flatten()
                    }),
                Exact::Mock => mock(),
            };
            (i, duration)
        });
        let mut cue_durations: Vec<Option<NodeDuration>> = vec![None; requests.len()];
        for (i, duration) in measured.into_iter().flatten() {
            cue_durations[i] = duration;
        }
        let from_cache: Vec<bool> = cue_durations
            .iter()
            .map(|d| d.is_some_and(|d| d.source == soniccraft_layout::DurationSource::Cache))
            .collect();

        let request_of: HashMap<usize, usize> = requests
            .iter()
            .enumerate()
            .map(|(i, r)| (r.node, i))
            .collect();
        let durations: Vec<NodeDuration> = program
            .nodes
            .iter()
            .enumerate()
            .map(|(node_id, node)| match &node.kind {
                NodeKind::Cue(cue) => {
                    let Some(&i) = request_of.get(&node_id) else {
                        return NodeDuration::estimate(0);
                    };
                    cue_durations[i].unwrap_or_else(|| {
                        let r = &requests[i].request;
                        let secs = soniccraft_tts::estimate_duration_secs(
                            &r.provider,
                            &cue.voice,
                            &r.text,
                        );
                        NodeDuration::estimate(seconds_to_frames(secs, sample_rate))
                    })
                }
                NodeKind::Pause { .. } => NodeDuration::fixed(0),
                NodeKind::Clip(clip) => input
                    .clip_frames
                    .get(&clip.source_path)
                    .map_or(NodeDuration::estimate(0), |&frames| {
                        NodeDuration::source(frames)
                    }),
            })
            .collect();

        let layout = soniccraft_layout::layout(program, &durations);

        // Cue-mode ducking regions from the planned intervals (DESIGN.md §4.5), without per-millisecond
        // arrays.
        let total = usize::try_from(layout.total_frames).unwrap_or(usize::MAX);
        let mut ducked = Vec::new();
        let mut ducking = Vec::new();
        if input.ducking {
            for (index, track) in program.tracks.iter().enumerate() {
                let Some(config) = &track.ducking else {
                    continue;
                };
                let triggers = reports::active_triggers(program, config);
                let regions = if triggers.is_empty() {
                    Vec::new()
                } else {
                    soniccraft_dsp::duck::cue_regions(
                        &reports::trigger_intervals(&layout, &triggers),
                        sample_rate,
                        total,
                        &reports::duck_params(config),
                    )
                };
                ducked.push(DuckedTrack {
                    track: index,
                    regions: reports::to_intervals(&regions),
                });
                ducking.extend(reports::ducking_report(program, &layout, index, &regions));
            }
        }
        let diagram = if input.diagram {
            capped_diagram(soniccraft_layout::render_diagram(
                program,
                &layout,
                &ducked,
                &DiagramOptions::default(),
            ))
        } else {
            String::new()
        };
        let synthesis = synthesis_plan(requests, &from_cache, &keys);
        Geometry {
            layout,
            ducking,
            synthesis,
            diagram,
            source_samples: input.source_samples,
        }
    }
}

/// `diagram`, cut after [`MAX_DIAGRAM_BYTES`] at a line boundary with a note saying how much is left out.
fn capped_diagram(diagram: String) -> String {
    if diagram.len() <= MAX_DIAGRAM_BYTES {
        return diagram;
    }
    let mut end = MAX_DIAGRAM_BYTES;
    while !diagram.is_char_boundary(end) {
        end -= 1;
    }
    let cut = diagram[..end].rfind('\n').map_or(0, |i| i + 1);
    let left_out = diagram[cut..].lines().count();
    format!(
        "{}… {left_out} more lines: the diagram is cut at {} KB; `items` lists every item\n",
        &diagram[..cut],
        MAX_DIAGRAM_BYTES / 1024
    )
}

/// Length in output frames of a cached cue after resampling and trimming; `None` when the cached audio
/// cannot be read (a render would synthesize it again).
fn cached_frames(
    meta: &CachedMeta,
    wav: &Path,
    trim: Option<&soniccraft_core::ir::TrimConfig>,
    sample_rate: u32,
) -> Option<u64> {
    if trim.is_none() {
        let frames = usize::try_from(meta.frames).ok()?;
        return Some(
            soniccraft_dsp::resample::output_len(frames, meta.sample_rate, sample_rate) as u64,
        );
    }
    let audio = soniccraft_dsp::wav::read(wav).ok()?;
    let (prepared, _) = speech::prepare(&audio, sample_rate, trim).ok()?;
    Some(prepared.frames() as u64)
}

/// `round(secs × sample_rate)`, 0 for negative or non-finite estimates.
fn seconds_to_frames(secs: f64, sample_rate: u32) -> u64 {
    let frames = (secs * f64::from(sample_rate)).round();
    if frames.is_finite() && frames > 0.0 {
        frames as u64
    } else {
        0
    }
}

/// What a render would synthesize: cues not served from the cache, with identical requests counted once.
fn synthesis_plan(
    requests: &[CueRequest],
    from_cache: &[bool],
    keys: &[Option<String>],
) -> SynthesisPlan {
    let mut plan = SynthesisPlan {
        cues: requests.len(),
        cached: 0,
        characters_to_synthesize: 0,
        by_provider: IndexMap::new(),
    };
    let mut planned: HashSet<String> = HashSet::new();
    for (i, r) in requests.iter().enumerate() {
        let provider = &r.request.provider;
        let entry = plan
            .by_provider
            .entry(provider.name.clone())
            .or_insert_with(|| ProviderSynthesisPlan {
                provider_type: provider.provider_type.as_str().to_string(),
                cues: 0,
                cached: 0,
                characters: 0,
            });
        entry.cues += 1;
        if from_cache[i] {
            entry.cached += 1;
            plan.cached += 1;
            continue;
        }
        let identity = keys[i]
            .clone()
            .unwrap_or_else(|| format!("{}\u{0}{}", provider.name, r.request.text));
        if planned.insert(identity) {
            let characters = r.request.text.chars().count();
            entry.characters += characters;
            plan.characters_to_synthesize += characters;
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_diagrams_are_cut_at_a_line() {
        let small = "a\nb\n".to_string();
        assert_eq!(capped_diagram(small.clone()), small);
        let line = format!("{}\n", "é".repeat(39)); // 79 bytes, multi-byte characters
        let big = line.repeat(MAX_DIAGRAM_BYTES / line.len() + 100);
        let capped = capped_diagram(big.clone());
        assert!(capped.len() < MAX_DIAGRAM_BYTES + 200, "{}", capped.len());
        assert!(
            capped.ends_with("lines: the diagram is cut at 256 KB; `items` lists every item\n")
        );
        let kept = capped.lines().count() - 1;
        let note = capped.lines().last().unwrap();
        assert!(
            note.starts_with(&format!("… {} more lines", big.lines().count() - kept)),
            "{note}"
        );
    }

    #[test]
    fn estimates_to_frames() {
        assert_eq!(seconds_to_frames(1.5, 48_000), 72_000);
        assert_eq!(seconds_to_frames(-1.0, 48_000), 0);
        assert_eq!(seconds_to_frames(f64::NAN, 48_000), 0);
    }
}
