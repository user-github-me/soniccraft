//! The per-cue chain (DESIGN.md §4.1): mono → output rate → silence trim → source normalization → cue gain,
//! plus word timing for subtitles (§5.4): provider words mapped onto the display text, or estimated from
//! the speech regions of the trimmed audio.

use soniccraft_captions::{Alignment, Interval, Word, align};
use soniccraft_core::ir::{NodeId, TrimConfig};
use soniccraft_dsp::{AudioBuffer, analysis, loudness};
use soniccraft_tts::WordTiming;

use crate::cancel::{Cancel, Stop, par_map, workers};
use crate::speech;

/// One cue to process.
#[derive(Debug)]
pub(crate) struct CueJob {
    /// The cue's node.
    pub node: NodeId,
    /// Provider audio at its native rate, untrimmed.
    pub audio: AudioBuffer,
    /// Provider word timings (seconds from the start of the provider audio).
    pub words: Option<Vec<WordTiming>>,
    /// The track's silence trimming.
    pub trim: Option<TrimConfig>,
    /// The track's source-normalization target.
    pub normalize_lufs: Option<f64>,
    /// Linear cue gain (volume, gain_db, speaker gain).
    pub gain: f64,
    /// Subtitle text, when the cue's words are needed (subtitles or preview).
    pub display: Option<String>,
}

/// A processed cue, ready to place.
#[derive(Debug)]
pub(crate) struct CueAudio {
    /// The cue's node.
    pub node: NodeId,
    /// Mono at the output rate, trimmed, normalized, cue gain applied.
    pub buffer: AudioBuffer,
    /// Source normalization was requested but the loudness could not be measured.
    pub normalization_skipped: bool,
    /// Word timing relative to the start of the trimmed audio (when requested).
    pub alignment: Option<Alignment>,
}

/// The outcome of one cue: its audio, or its node and why it failed.
pub(crate) type CueOutcome = Result<CueAudio, (NodeId, String)>;

/// Process every cue (in parallel; results keep the input order).
pub(crate) fn process_all(
    jobs: Vec<CueJob>,
    sample_rate: u32,
    cancel: &Cancel,
) -> Result<Vec<CueOutcome>, Stop> {
    let results = par_map(jobs, workers(), |job| {
        if cancel.is_cancelled() {
            return Err(None);
        }
        let node = job.node;
        process(job, sample_rate).map_err(|message| Some((node, message)))
    });
    cancel.check()?;
    let mut out = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Some(Ok(cue)) => out.push(Ok(cue)),
            Some(Err(Some(failure))) => out.push(Err(failure)),
            Some(Err(None)) => return Err(Stop::Cancelled),
            None => {
                return Err(Stop::failed(
                    "internal error: a cue processing thread stopped",
                ));
            }
        }
    }
    Ok(out)
}

fn process(job: CueJob, sample_rate: u32) -> Result<CueAudio, String> {
    let (mut buffer, trim_start) = speech::prepare(&job.audio, sample_rate, job.trim.as_ref())
        .map_err(|e| format!("cannot process the synthesized audio: {e}"))?;
    drop(job.audio);

    let mut normalization_skipped = false;
    if let Some(target) = job.normalize_lufs {
        let measured = loudness::integrated_lufs(&buffer)
            .map_err(|e| format!("cannot measure the cue's loudness: {e}"))?;
        match loudness::normalization_gain_db(measured, target) {
            Some(gain_db) => buffer.apply_gain(analysis::db_to_linear(gain_db) as f32),
            None => normalization_skipped = true,
        }
    }

    // Speech regions are measured at the normalized level (a stable reference for the -45 dBFS gate),
    // before the cue's own gain.
    let alignment = job.display.as_deref().map(|display| {
        let regions: Vec<Interval> = analysis::speech_regions(
            &buffer,
            speech::REGION_THRESHOLD_DB,
            speech::frames(speech::REGION_MIN_GAP_MS, sample_rate),
        )
        .into_iter()
        .map(|r| {
            Interval::new(
                speech::ms(r.start, sample_rate),
                speech::ms(r.end, sample_rate),
            )
        })
        .collect();
        let trim_ms = speech::ms(trim_start, sample_rate);
        let words: Vec<Word> = job
            .words
            .iter()
            .flatten()
            .map(|w| {
                Word::new(
                    w.text.clone(),
                    w.start_secs * 1000.0 - trim_ms,
                    w.end_secs * 1000.0 - trim_ms,
                )
            })
            .collect();
        let span = Interval::new(0.0, speech::ms(buffer.frames(), sample_rate));
        align::map_provider_words(display, &words, span, &regions)
    });

    if job.gain != 1.0 {
        buffer.apply_gain(job.gain as f32);
    }
    Ok(CueAudio {
        node: job.node,
        buffer,
        normalization_skipped,
        alignment,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f64, amp: f32) -> Vec<f32> {
        let n = (secs * f64::from(rate)) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn normalizes_trims_and_aligns() {
        let rate = 24_000;
        let mut samples = vec![0.0f32; rate as usize / 4];
        samples.extend(tone(rate, 1.0, 0.05));
        samples.extend(vec![0.0f32; rate as usize / 4]);
        let job = CueJob {
            node: 3,
            audio: AudioBuffer::mono(rate, samples),
            words: Some(vec![WordTiming {
                text: "Hello.".into(),
                start_secs: 0.25,
                end_secs: 1.25,
            }]),
            trim: Some(TrimConfig {
                threshold_db: -50.0,
                padding_ms: 30.0,
            }),
            normalize_lufs: Some(-20.0),
            gain: 0.5,
            display: Some("Hello.".into()),
        };
        let cue = process(job, 48_000).unwrap();
        assert_eq!(cue.node, 3);
        assert!(!cue.normalization_skipped);
        let frames = cue.buffer.frames();
        assert!(
            (frames as f64 - 1.06 * 48_000.0).abs() < 0.02 * 48_000.0,
            "{frames}"
        );
        let lufs = loudness::integrated_lufs(&cue.buffer).unwrap().unwrap();
        assert!((lufs - (-20.0 - 6.02)).abs() < 0.1, "{lufs}");
        let alignment = cue.alignment.unwrap();
        assert!(!alignment.is_estimated());
        assert!(
            (alignment.words[0].start_ms - 30.0).abs() < 1.0,
            "{:?}",
            alignment.words
        );
    }

    #[test]
    fn short_cues_skip_normalization() {
        let job = CueJob {
            node: 0,
            audio: AudioBuffer::mono(48_000, tone(48_000, 0.2, 0.1)),
            words: None,
            trim: None,
            normalize_lufs: Some(-20.0),
            gain: 1.0,
            display: None,
        };
        let cue = process(job, 48_000).unwrap();
        assert!(cue.normalization_skipped);
        assert!(cue.alignment.is_none());
        assert_eq!(cue.buffer.frames(), 9_600);
    }
}
