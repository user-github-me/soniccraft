//! Render outputs besides the audio: captions and subtitle files (DESIGN.md §6, §10.1, §10.3), the render
//! report and its manifest (§8), and the web preview payload (§10.2).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use soniccraft_captions::{
    Caption, CaptionConfig, CueInput, build_captions, to_srt, to_transcript_json_string, to_webvtt,
    to_words_json_string, words_document,
};
use soniccraft_core::Diagnostic;
use soniccraft_core::ir::{Program, SubtitleFormat};
use soniccraft_dsp::AudioBuffer;
use soniccraft_dsp::encode::EncodeSummary;
use soniccraft_layout::Layout;

use super::cues::CueAudio;
use super::mix::Ducked;
use crate::files::OutputPlan;
use crate::preview::{self, Envelope, Payload, Waveform};
use crate::report::{
    DurationSource, FileRef, FilesReport, LoudnessReport, OutputReport, RenderReport, StemRef,
    SynthesisReport,
};
use crate::reports;

/// Captions of the cues whose word timings were computed (subtitled cues of audible tracks), in
/// declaration order. Takes the alignments out of `processed`.
pub(crate) fn captions(
    program: &Program,
    layout: &Layout,
    processed: &mut [CueAudio],
) -> Vec<Caption> {
    let mut inputs = Vec::new();
    for cue in processed.iter_mut() {
        let Some(alignment) = cue.alignment.take() else {
            continue;
        };
        let node = &program.nodes[cue.node];
        let (Some(ir), Some(placement), Some(track)) = (
            node.as_cue(),
            layout.placement(cue.node),
            program.tracks.get(node.track),
        ) else {
            continue;
        };
        let start_ms = layout.ms(placement.start);
        inputs.push(CueInput {
            cue_id: node.id.clone(),
            track_id: track.id.clone(),
            speaker: Some(ir.speaker_label.clone()),
            text: soniccraft_captions::text::display_text(&ir.text, ir.caption.as_deref()),
            start_ms,
            end_ms: layout.ms(placement.end()),
            words: alignment
                .words
                .iter()
                .map(|w| w.shifted(start_ms))
                .collect(),
        });
    }
    let s = &program.subtitles;
    let config = CaptionConfig {
        max_chars_per_line: s.max_chars_per_line,
        max_lines: s.max_lines,
        max_duration_ms: s.max_duration_ms,
        min_duration_ms: s.min_duration_ms,
        speaker_labels: s.speaker_labels,
        timeline_end_ms: Some(layout.total_ms()),
    };
    build_captions(&inputs, &config)
}

/// The contents of one subtitle file.
pub(crate) fn subtitles(format: SubtitleFormat, captions: &[Caption]) -> String {
    match format {
        SubtitleFormat::Vtt => to_webvtt(captions),
        SubtitleFormat::Srt => to_srt(captions),
        SubtitleFormat::Json => to_words_json_string(captions),
        SubtitleFormat::Transcript => to_transcript_json_string(captions),
    }
}

/// The preview waveform of the mastered output.
pub(crate) fn waveform(master: &AudioBuffer) -> Waveform {
    let w = soniccraft_dsp::waveform::waveform(master, preview::POINTS_PER_SECOND);
    Waveform {
        points_per_second: w.points_per_second,
        min: w.min,
        max: w.max,
    }
}

/// Everything the render report is made of.
pub(crate) struct ReportParts<'a> {
    pub program: &'a Program,
    pub layout: &'a Layout,
    pub plan: &'a OutputPlan,
    pub encoded: &'a EncodeSummary,
    pub loudness: LoudnessReport,
    pub stems: &'a [(String, PathBuf)],
    pub ducking: &'a [Ducked],
    pub synthesis: SynthesisReport,
    pub diagnostics: Vec<Diagnostic>,
}

/// The render report with absolute paths (timings are added by the caller).
pub(crate) fn report(parts: ReportParts<'_>) -> RenderReport {
    let ReportParts {
        program,
        layout,
        plan,
        encoded,
        loudness,
        stems,
        ducking,
        synthesis,
        diagnostics,
    } = parts;
    RenderReport {
        output: OutputReport {
            path: plan.audio.display().to_string(),
            format: program.output.format.as_str().to_string(),
            sample_rate: encoded.sample_rate,
            channels: u16::try_from(encoded.channels).unwrap_or(u16::MAX),
            bit_depth: encoded.bit_depth,
            bitrate_bps: encoded.bitrate_bps,
            duration_ms: layout.total_ms(),
            bytes: encoded.bytes,
        },
        loudness,
        files: files_report(plan, stems, |p| p.display().to_string()),
        tracks: reports::tracks(program),
        items: reports::items(program, layout),
        ducking: ducking
            .iter()
            .filter_map(|d| reports::ducking_report(program, layout, d.track, &d.regions))
            .collect(),
        synthesis,
        diagnostics,
        timings_ms: IndexMap::new(),
    }
}

fn files_report(
    plan: &OutputPlan,
    stems: &[(String, PathBuf)],
    show: impl Fn(&Path) -> String,
) -> FilesReport {
    FilesReport {
        subtitles: plan
            .subtitles
            .iter()
            .map(|(format, path)| FileRef {
                format: format.as_str().to_string(),
                path: show(path),
            })
            .collect(),
        stems: stems
            .iter()
            .map(|(track_id, path)| StemRef {
                track_id: track_id.clone(),
                path: show(path),
            })
            .collect(),
        manifest: plan.manifest.as_deref().map(&show),
        preview: plan.preview.as_deref().map(&show),
    }
}

/// The report with paths relative to the output directory and no timings (the base of [`normalized`]).
pub(crate) fn relative(
    report: &RenderReport,
    plan: &OutputPlan,
    stems: &[(String, PathBuf)],
) -> RenderReport {
    let mut out = report.clone();
    out.timings_ms.clear();
    out.output.path = plan.relative(&plan.audio);
    out.files = files_report(plan, stems, |p| plan.relative(p));
    out
}

/// The report as the manifest and the preview carry it (see [`relative`]), as JSON, without run provenance:
/// no `synthesis` counters, cue durations from the cache read `exact` (cached audio is measured exactly like
/// fresh audio), and no `cache_write_failed` warning (it is about the TTS cache, not the output). Identical
/// audio therefore always gives identical files, whether or not the cache was warm.
pub(crate) fn normalized(report: &RenderReport) -> Result<serde_json::Value, String> {
    let mut report = report.clone();
    for item in &mut report.items {
        if item.duration_source == DurationSource::Cache {
            item.duration_source = DurationSource::Exact;
        }
    }
    report
        .diagnostics
        .retain(|d| d.code != soniccraft_core::diagnostics::codes::CACHE_WRITE_FAILED);
    let mut value =
        serde_json::to_value(&report).map_err(|e| format!("cannot serialize the manifest: {e}"))?;
    if let Some(fields) = value.as_object_mut() {
        fields.shift_remove("synthesis");
        fields.shift_remove("timings_ms");
    }
    Ok(value)
}

/// `<stem>.manifest.json`: the [`normalized`] report, pretty-printed, with a trailing newline.
pub(crate) fn manifest_json(normalized: &serde_json::Value) -> Vec<u8> {
    // Serializing a `Value` cannot fail.
    let mut bytes = serde_json::to_vec_pretty(normalized).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// `<stem>.preview.html`, embedding the [`normalized`] report (the manifest's), so fresh and warm renders
/// write identical pages.
pub(crate) fn preview_html(
    program: &Program,
    plan: &OutputPlan,
    report: &serde_json::Value,
    waveform: Waveform,
    ducking: &[Ducked],
    captions: &[Caption],
) -> Result<String, String> {
    let title = program
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            plan.audio
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let audio = plan.relative(&plan.audio);
    let document = words_document(captions);
    let envelopes = ducking
        .iter()
        .filter_map(|d| {
            Some(Envelope {
                track_id: program.tracks.get(d.track)?.id.clone(),
                points_per_second: preview::POINTS_PER_SECOND,
                gain_db: d.envelope.clone()?,
            })
        })
        .collect();
    preview::render(&Payload {
        soniccraft_preview: preview::PAYLOAD_VERSION,
        title: &title,
        audio: &audio,
        report,
        waveform,
        envelopes,
        captions: document.captions,
        words: document.words,
    })
}
