//! The `mock` provider: a deterministic, in-process synthetic voice for tests, demos and offline iteration.
//!
//! Every word becomes a vowel-like harmonic tone burst: one burst per (heuristic) syllable, each with a
//! raised-cosine envelope and a formant-shaped harmonic spectrum. The fundamental comes from a stable hash of
//! `voice_id` (90–220 Hz), with a gentle declination over the cue, a ±3 % per-word variation and a rise on
//! questions. Timing is exact and integer-frame based:
//!
//! - 24 kHz mono; 100 ms of leading and 150 ms of trailing silence;
//! - each syllable lasts `60 / wpm / 1.5 / speed` seconds (`settings.wpm`, default 160; `settings.speed`,
//!   default 1), so an average 1.5-syllable word takes `60 / wpm` seconds;
//! - 40 ms between words, plus 200 ms after `, ; : – —` and 400 ms after `. ! ? …` (divided by `speed`);
//! - `[audio tags]` are silent and punctuation-only tokens only add their pause.
//!
//! [`duration_secs`] computes the same plan without synthesizing, so the dry-run estimate equals the
//! rendered length exactly.

use std::f64::consts::{PI, TAU};

use serde_json::{Map, Value};
use soniccraft_dsp::AudioBuffer;

use crate::settings::get_positive;
use crate::text;
use crate::types::WordTiming;

/// Output sample rate of the mock voice.
pub(crate) const SAMPLE_RATE: u32 = 24_000;
const DEFAULT_WPM: f64 = 160.0;
const AVG_SYLLABLES_PER_WORD: f64 = 1.5;
const LEAD_SECS: f64 = 0.10;
const TAIL_SECS: f64 = 0.15;
const WORD_GAP_SECS: f64 = 0.04;
const MINOR_PAUSE_SECS: f64 = 0.20;
const MAJOR_PAUSE_SECS: f64 = 0.40;
/// Peak level of the normalized output (≈ -6 dBFS).
const PEAK: f64 = 0.5;
const MAX_HARMONIC_HZ: f64 = 4_000.0;
const MIN_F0_HZ: f64 = 90.0;
const F0_STEPS: u64 = 1_301; // 90.0 ..= 220.0 Hz in 0.1 Hz steps

/// Vowel formants (F1, F2, F3) in Hz.
const VOWELS: [[f64; 3]; 5] = [
    [730.0, 1090.0, 2440.0], // a
    [530.0, 1840.0, 2480.0], // e
    [270.0, 2290.0, 3010.0], // i
    [570.0, 840.0, 2410.0],  // o
    [300.0, 870.0, 2240.0],  // u
];
const FORMANT_BANDWIDTHS: [f64; 3] = [90.0, 110.0, 170.0];
const FORMANT_GAINS: [f64; 3] = [1.0, 0.6, 0.3];

fn frames(secs: f64) -> usize {
    (secs * SAMPLE_RATE as f64).round().max(0.0) as usize
}

struct Timing {
    syllable: usize,
    gap: usize,
    minor_pause: usize,
    major_pause: usize,
    lead: usize,
    tail: usize,
}

impl Timing {
    fn from_settings(settings: &Map<String, Value>) -> Self {
        let wpm = get_positive(settings, "wpm")
            .unwrap_or(DEFAULT_WPM)
            .clamp(40.0, 600.0);
        let speed = get_positive(settings, "speed")
            .unwrap_or(1.0)
            .clamp(0.25, 4.0);
        Timing {
            syllable: frames(60.0 / wpm / AVG_SYLLABLES_PER_WORD / speed).max(1),
            gap: frames(WORD_GAP_SECS / speed),
            minor_pause: frames(MINOR_PAUSE_SECS / speed),
            major_pause: frames(MAJOR_PAUSE_SECS / speed),
            lead: frames(LEAD_SECS),
            tail: frames(TAIL_SECS),
        }
    }

    /// Pause after a token, from its final punctuation (closing quotes and brackets ignored).
    fn pause_after(&self, token: &str) -> usize {
        let trimmed = token.trim_end_matches(['"', '\'', ')', ']', '}', '»', '”', '’', '」', '』']);
        match trimmed.chars().last() {
            Some('.' | '!' | '?' | '…' | '。' | '！' | '？') => self.major_pause,
            Some(',' | ';' | ':' | '–' | '—' | '、' | '，' | '；' | '：') => {
                self.minor_pause
            }
            _ => 0,
        }
    }
}

struct PlannedWord {
    text: String,
    start: usize,
    syllables: usize,
    question: bool,
}

struct Plan {
    words: Vec<PlannedWord>,
    syllable_frames: usize,
    total_frames: usize,
}

fn plan(text: &str, settings: &Map<String, Value>) -> Plan {
    let timing = Timing::from_settings(settings);
    let mut words: Vec<PlannedWord> = Vec::new();
    let mut cursor = timing.lead;
    let mut pending_pause = 0usize;
    for token in text::tokens(text) {
        if !text::is_spoken_token(&token) {
            pending_pause = pending_pause.max(timing.pause_after(&token));
            if token.contains('?')
                && let Some(last) = words.last_mut()
            {
                last.question = true;
            }
            continue;
        }
        if !words.is_empty() {
            cursor += timing.gap + pending_pause;
        }
        let syllables = text::syllables(&token).max(1);
        let question = token
            .trim_end_matches(['"', '\'', ')', '»', '”', '’'])
            .ends_with('?');
        words.push(PlannedWord {
            text: token.clone(),
            start: cursor,
            syllables,
            question,
        });
        cursor += syllables * timing.syllable;
        pending_pause = timing.pause_after(&token);
    }
    Plan {
        words,
        syllable_frames: timing.syllable,
        total_frames: cursor + timing.tail,
    }
}

/// Exact length in seconds of the mock rendering of `text` with `settings`.
pub(crate) fn duration_secs(text: &str, settings: &Map<String, Value>) -> f64 {
    plan(text, settings).total_frames as f64 / SAMPLE_RATE as f64
}

/// 64 bits of BLAKE3 over length-prefixed parts (stable across platforms and releases).
fn hash64(parts: &[&[u8]]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(first)
}

/// SplitMix64 finalizer: decorrelates small integer variations of a seed.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fundamental frequency for a voice: 90–220 Hz, from a stable hash of the voice id.
pub(crate) fn base_f0(voice_id: &str) -> f64 {
    MIN_F0_HZ + (hash64(&[b"soniccraft.mock.f0", voice_id.as_bytes()]) % F0_STEPS) as f64 / 10.0
}

fn harmonic_amplitudes(f0: f64, formants: &[f64; 3]) -> Vec<f64> {
    let count = ((MAX_HARMONIC_HZ / f0).floor() as usize).max(1);
    (1..=count)
        .map(|k| {
            let f = k as f64 * f0;
            let resonance: f64 = (0..3)
                .map(|j| {
                    FORMANT_GAINS[j] / (1.0 + ((f - formants[j]) / FORMANT_BANDWIDTHS[j]).powi(2))
                })
                .sum();
            (0.05 + resonance) / (k as f64).sqrt()
        })
        .collect()
}

/// `Σ_k amps[k-1] · sin(k·phase)`, using the Chebyshev recurrence `s_k = 2cos(φ)·s_{k-1} − s_{k-2}`.
fn harmonic_sum(phase: f64, amps: &[f64]) -> f64 {
    let (sin1, cos1) = phase.sin_cos();
    let two_cos = 2.0 * cos1;
    let (mut s_km2, mut s_km1) = (0.0, sin1);
    let mut acc = amps.first().copied().unwrap_or(0.0) * sin1;
    for a in amps.iter().skip(1) {
        let s_k = two_cos * s_km1 - s_km2;
        acc += a * s_k;
        s_km2 = s_km1;
        s_km1 = s_k;
    }
    acc
}

/// Syllable envelope over `p ∈ [0, 1)`: 20 % raised-cosine attack, sustain, 30 % raised-cosine release.
fn envelope(p: f64) -> f64 {
    if p < 0.2 {
        0.5 - 0.5 * (PI * p / 0.2).cos()
    } else if p > 0.7 {
        0.5 + 0.5 * (PI * (p - 0.7) / 0.3).cos()
    } else {
        1.0
    }
}

/// Synthesize `text` with the mock voice `voice_id`. Returns 24 kHz mono audio and exact word timings.
pub(crate) fn synthesize(
    text: &str,
    voice_id: &str,
    settings: &Map<String, Value>,
) -> (AudioBuffer, Vec<WordTiming>) {
    let plan = plan(text, settings);
    let sr = SAMPLE_RATE as f64;
    let syllable = plan.syllable_frames;
    let total = plan.total_frames.max(1) as f64;
    let f0_voice = base_f0(voice_id);
    let mut samples = vec![0.0f64; plan.total_frames];

    for (index, word) in plan.words.iter().enumerate() {
        let word_hash = hash64(&[
            voice_id.as_bytes(),
            word.text.as_bytes(),
            &(index as u64).to_le_bytes(),
        ]);
        let word_f0 = f0_voice * (1.0 + ((word_hash % 61) as f64 - 30.0) / 1000.0);
        let mut phase = 0.0f64;
        for s in 0..word.syllables {
            let vowel = &VOWELS[(mix(word_hash ^ s as u64) % VOWELS.len() as u64) as usize];
            let amps = harmonic_amplitudes(word_f0, vowel);
            let rising = word.question && s + 1 == word.syllables;
            let seg_start = word.start + s * syllable;
            let seg_end = (seg_start + syllable).min(samples.len());
            for (i, out) in samples[seg_start.min(seg_end)..seg_end]
                .iter_mut()
                .enumerate()
            {
                let progress = i as f64 / syllable as f64;
                let declination = 1.08 - 0.16 * ((seg_start + i) as f64 / total);
                let rise = if rising { 1.0 + 0.15 * progress } else { 1.0 };
                phase = (phase + TAU * word_f0 * declination * rise / sr) % TAU;
                *out += envelope(progress) * harmonic_sum(phase, &amps);
            }
        }
    }

    let peak = samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    let scale = if peak > 0.0 { PEAK / peak } else { 0.0 };
    let audio = AudioBuffer::mono(
        SAMPLE_RATE,
        samples.iter().map(|s| (s * scale) as f32).collect(),
    );
    let words = plan
        .words
        .iter()
        .map(|w| WordTiming {
            text: w.text.clone(),
            start_secs: w.start as f64 / sr,
            end_secs: (w.start + w.syllables * syllable) as f64 / sr,
        })
        .collect();
    (audio, words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn deterministic_and_exact_length() {
        let s = settings(json!({}));
        let text = "Hello, world. Is this a mock voice? [laughs] Yes — it is!";
        let (a1, w1) = synthesize(text, "narrator", &s);
        let (a2, w2) = synthesize(text, "narrator", &s);
        assert_eq!(a1, a2);
        assert_eq!(w1, w2);
        assert_eq!(a1.sample_rate, SAMPLE_RATE);
        assert_eq!(a1.num_channels(), 1);
        assert_eq!(a1.duration_secs(), duration_secs(text, &s));
        let texts: Vec<&str> = w1.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Hello,", "world.", "Is", "this", "a", "mock", "voice?", "Yes", "it", "is!"
            ]
        );
        for pair in w1.windows(2) {
            assert!(pair[0].end_secs <= pair[1].start_secs, "{pair:?}");
        }
        assert!(w1.last().is_some_and(|w| w.end_secs < a1.duration_secs()));
        let peak = a1.peak();
        assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
    }

    #[test]
    fn timing_follows_wpm_speed_and_punctuation() {
        let slow = duration_secs("one two three four", &settings(json!({"wpm": 100})));
        let fast = duration_secs("one two three four", &settings(json!({"wpm": 200})));
        assert!(slow > fast);
        let faster = duration_secs(
            "one two three four",
            &settings(json!({"wpm": 200, "speed": 2.0})),
        );
        assert!(fast > faster);
        let plain = duration_secs("one two", &settings(json!({})));
        let comma = duration_secs("one, two", &settings(json!({})));
        let stop = duration_secs("one. two", &settings(json!({})));
        assert!((comma - plain - MINOR_PAUSE_SECS).abs() < 1e-9);
        assert!((stop - plain - MAJOR_PAUSE_SECS).abs() < 1e-9);
        // Standalone punctuation adds the pause without sound.
        assert_eq!(duration_secs("one — two", &settings(json!({}))), comma);
        // Only silence for texts without words.
        assert!(
            (duration_secs("[sigh]", &settings(json!({}))) - (LEAD_SECS + TAIL_SECS)).abs() < 1e-9
        );
    }

    #[test]
    fn default_rate_is_about_160_wpm() {
        // 1.5 syllables per word on average → 60/160 s per word plus the small inter-word gap.
        let text = "seven seven seven seven"; // 2 syllables each
        let d = duration_secs(text, &settings(json!({}))) - LEAD_SECS - TAIL_SECS;
        let expected = 4.0 * 2.0 * (60.0 / 160.0 / 1.5) + 3.0 * WORD_GAP_SECS;
        assert!((d - expected).abs() < 1e-3, "{d} vs {expected}");
    }

    #[test]
    fn voices_differ_by_pitch_in_range() {
        let mut seen = std::collections::BTreeSet::new();
        for v in ["mock", "alice", "bob", "carol", "dave", "narrator"] {
            let f0 = base_f0(v);
            assert!((90.0..=220.0).contains(&f0), "{v}: {f0}");
            seen.insert((f0 * 10.0) as i64);
        }
        assert!(seen.len() >= 5, "voices should mostly differ: {seen:?}");
        let s = settings(json!({}));
        let (a, _) = synthesize("same text", "alice", &s);
        let (b, _) = synthesize("same text", "bob", &s);
        assert_eq!(a.frames(), b.frames());
        assert_ne!(a, b);
    }

    #[test]
    fn audible_where_words_are_and_silent_elsewhere() {
        let s = settings(json!({}));
        let (audio, words) = synthesize("Testing one two", "mock", &s);
        let samples = &audio.channels[0];
        let rms = |a: f64, b: f64| {
            let (i, j) = (
                (a * 24_000.0).round() as usize,
                (b * 24_000.0).round() as usize,
            );
            let seg = &samples[i..j];
            (seg.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / seg.len().max(1) as f64).sqrt()
        };
        assert_eq!(rms(0.0, 0.1), 0.0);
        for w in &words {
            assert!(rms(w.start_secs, w.end_secs) > 0.05, "{w:?}");
        }
        let end = words.last().map(|w| w.end_secs).unwrap_or_default();
        assert_eq!(rms(end, audio.duration_secs()), 0.0);
    }
}
