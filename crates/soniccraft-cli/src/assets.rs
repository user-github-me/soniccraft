//! Procedural starter assets for `soniccraft init`.
//!
//! The example timelines reference four royalty-free sounds (`examples/offline-demo/assets/`). Embedding
//! those WAVs would add ~2 MB to the binary, so `init` synthesizes equivalents instead: same file names,
//! channel layouts and lengths, 48 kHz 16-bit PCM written through `soniccraft_dsp::wav`.
//!
//! Every sound is a pure function of this file: fixed-seed noise ([`SplitMix64`]) and deterministic math
//! ([`crate::dmath`]), so the files are byte-identical across runs and platforms.
//!
//! | file | content |
//! |---|---|
//! | `assets/audio/synthwave_loop.wav` | 8 s stereo (exactly 384 000 frames) Am–F–C–G pad with a pumping pulse, eighth-note bass and a ping-pong echo; seamless (the tail rendered past the loop end is folded back onto the start); peak −6 dBFS |
//! | `assets/sfx/whoosh.wav` | 1.2 s stereo noise through a rising resonant band-pass, panned left to right |
//! | `assets/sfx/impact.wav` | 2 s mono low thump with a pitch drop, a noise burst and a rumble, exponential decays |
//! | `assets/sfx/chime.wav` | 1.5 s mono two-note bell (E5, A5) with inharmonic partials |
//!
//! The sound effects start and end on exact digital silence; nothing clips.

use std::path::Path;

use soniccraft_dsp::AudioBuffer;
use soniccraft_dsp::wav::{self, WavEncoding};

use crate::dmath::{SplitMix64, cos_turns, exp, sin_turns, smoothstep, tanh};

/// Output sample rate of every asset.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
const SR: f64 = SAMPLE_RATE as f64;

/// `10^(dB/20)` for the peak targets (literals keep the values platform-independent).
const MINUS_6_DB: f64 = 0.501_187_233_627_272_3;
const MINUS_7_DB: f64 = 0.446_683_592_150_963_2;
const MINUS_9_DB: f64 = 0.354_813_389_233_575_5;

/// One generated starter asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Asset {
    SynthwaveLoop,
    Whoosh,
    Impact,
    Chime,
}

impl Asset {
    /// Every asset `init` can write.
    pub(crate) const ALL: [Asset; 4] = [
        Asset::SynthwaveLoop,
        Asset::Whoosh,
        Asset::Impact,
        Asset::Chime,
    ];

    /// Path relative to the timeline directory (what the templates' `source:` fields say).
    pub(crate) fn relative_path(self) -> &'static str {
        match self {
            Asset::SynthwaveLoop => "assets/audio/synthwave_loop.wav",
            Asset::Whoosh => "assets/sfx/whoosh.wav",
            Asset::Impact => "assets/sfx/impact.wav",
            Asset::Chime => "assets/sfx/chime.wav",
        }
    }

    /// The asset written at `relative_path`, if any.
    pub(crate) fn from_relative_path(path: &str) -> Option<Asset> {
        Asset::ALL.into_iter().find(|a| a.relative_path() == path)
    }

    /// Length in frames.
    pub(crate) fn frames(self) -> usize {
        match self {
            Asset::SynthwaveLoop => 384_000,
            Asset::Whoosh => 57_600,
            Asset::Impact => 96_000,
            Asset::Chime => 72_000,
        }
    }

    /// Channel count.
    #[cfg(test)]
    pub(crate) fn channels(self) -> usize {
        match self {
            Asset::SynthwaveLoop | Asset::Whoosh => 2,
            Asset::Impact | Asset::Chime => 1,
        }
    }

    /// One-line description for `init`'s summary.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Asset::SynthwaveLoop => "8.0 s seamless stereo synth loop (Am-F-C-G)",
            Asset::Whoosh => "1.2 s stereo whoosh",
            Asset::Impact => "2.0 s mono impact",
            Asset::Chime => "1.5 s mono chime",
        }
    }

    /// Synthesize the sound.
    pub(crate) fn render(self) -> AudioBuffer {
        let channels = match self {
            Asset::SynthwaveLoop => synthwave_loop(),
            Asset::Whoosh => whoosh(),
            Asset::Impact => impact(),
            Asset::Chime => chime(),
        };
        AudioBuffer::new(
            SAMPLE_RATE,
            channels
                .into_iter()
                .map(|ch| ch.into_iter().map(|x| x as f32).collect())
                .collect(),
        )
    }

    /// The WAV file bytes (16-bit PCM, no dither: deterministic, and exact silence stays exact).
    #[cfg(test)]
    pub(crate) fn wav_bytes(self) -> Result<Vec<u8>, soniccraft_dsp::DspError> {
        wav::encode_to_vec(&self.render(), WavEncoding::Int16 { dither: false })
    }

    /// Write the asset to `path` (atomically, creating parent directories); returns the file size.
    pub(crate) fn write(self, path: &Path) -> Result<u64, soniccraft_dsp::DspError> {
        wav::write(path, &self.render(), WavEncoding::Int16 { dither: false })
    }
}

/// Scale every channel so the largest magnitude equals `target`.
fn normalize_peak(channels: &mut [Vec<f64>], target: f64) {
    let peak = channels
        .iter()
        .flatten()
        .fold(0.0f64, |m, &x| m.max(x.abs()));
    if peak > 0.0 {
        let gain = target / peak;
        for x in channels.iter_mut().flatten() {
            *x *= gain;
        }
    }
}

/// Smoothstep fades over the first `fade_in` and last `fade_out` frames: the first and the last sample
/// become exactly 0.
fn edge_fades(channel: &mut [f64], fade_in: usize, fade_out: usize) {
    for (i, sample) in channel.iter_mut().take(fade_in).enumerate() {
        *sample *= smoothstep(i as f64 / fade_in as f64);
    }
    for (i, sample) in channel.iter_mut().rev().take(fade_out).enumerate() {
        *sample *= smoothstep(i as f64 / fade_out as f64);
    }
}

fn frames(seconds: f64) -> usize {
    (seconds * SR).round() as usize
}

// ---------------------------------------------------------------------------------------------------
// synthwave_loop.wav
// ---------------------------------------------------------------------------------------------------

/// Am (A3 C4 E4), F (A3 C4 F4), C (G3 C4 E4), G (G3 B3 D4) in Hz; two seconds each.
const CHORDS: [[f64; 3]; 4] = [
    [220.000, 261.626, 329.628],
    [220.000, 261.626, 349.228],
    [195.998, 261.626, 329.628],
    [195.998, 246.942, 293.665],
];
/// Bass roots per chord (A2, F2, C3, G2).
const BASS_ROOTS: [f64; 4] = [110.000, 87.307, 130.813, 97.999];
/// Two detuned voices per channel: left −8/+6 cents, right −6/+8 cents.
const DETUNE: [[f64; 2]; 2] = [[0.995_390, 1.003_472], [0.996_540, 1.004_632]];

const LOOP_FRAMES: usize = 384_000; // 8 s
const CHORD_FRAMES: usize = 96_000; // 2 s (one bar of 4/4 at 120 BPM)
const BEAT_FRAMES: usize = 24_000; // a quarter note
const EIGHTH_FRAMES: usize = 12_000;
const ECHO_FRAMES: usize = 18_000; // a dotted eighth (375 ms)

const PAD_ATTACK: f64 = 0.6;
const PAD_SUSTAIN_END: f64 = 2.0;
const PAD_RELEASE: f64 = 0.6;
const PAD_GAIN: f64 = 0.045;
const BASS_GAIN: f64 = 0.30;

/// Raised-cosine pad envelope: attack 0.6 s, sustain to 2 s, release 0.6 s (overlapping the next chord's
/// attack, so every chord change, including the one at the seam, is the same crossfade).
fn pad_envelope(t: f64) -> f64 {
    if t < PAD_ATTACK {
        0.5 - 0.5 * cos_turns(0.5 * t / PAD_ATTACK)
    } else if t < PAD_SUSTAIN_END {
        1.0
    } else if t < PAD_SUSTAIN_END + PAD_RELEASE {
        0.5 + 0.5 * cos_turns(0.5 * (t - PAD_SUSTAIN_END) / PAD_RELEASE)
    } else {
        0.0
    }
}

/// The pumping pulse: a 30 % dip 40 ms after every beat (an alpha function), period 0.5 s.
fn pump(frame: usize) -> f64 {
    let x = (frame % BEAT_FRAMES) as f64 / SR / 0.04;
    1.0 - 0.3 * x * exp(1.0 - x)
}

fn synthwave_loop() -> Vec<Vec<f64>> {
    let note_frames = frames(PAD_SUSTAIN_END + PAD_RELEASE);
    let total = 3 * CHORD_FRAMES + note_frames; // the last chord's release runs past the loop end
    let mut pad = vec![vec![0.0f64; total]; 2];
    for (k, chord) in CHORDS.iter().enumerate() {
        let onset = k * CHORD_FRAMES;
        for &freq in chord {
            for (channel, detunes) in DETUNE.iter().enumerate() {
                for &detune in detunes {
                    let f = freq * detune;
                    let out = &mut pad[channel][onset..onset + note_frames];
                    for (n, sample) in out.iter_mut().enumerate() {
                        let t = n as f64 / SR;
                        let turns = f * t;
                        let s = sin_turns(turns);
                        let c = cos_turns(turns);
                        // harmonics 1, 2, 3: sin 2θ = 2 sin θ cos θ, sin 3θ = sin θ (3 − 4 sin² θ)
                        let voice = s + 0.3 * (2.0 * s * c) + 0.12 * s * (3.0 - 4.0 * s * s);
                        *sample += pad_envelope(t) * voice;
                    }
                }
            }
        }
    }
    // Fold the tail rendered past the loop end back onto the start: playing the loop back to back is
    // then exactly the continuous performance, so the seam is inaudible.
    for channel in &mut pad {
        let (head, tail) = channel.split_at_mut(LOOP_FRAMES);
        for (h, t) in head.iter_mut().zip(tail.iter()) {
            *h += *t;
        }
        channel.truncate(LOOP_FRAMES);
    }

    // Eighth-note bass, each note confined to its slot (zero at both ends of the slot).
    let mut bass = vec![0.0f64; LOOP_FRAMES];
    for slot in 0..LOOP_FRAMES / EIGHTH_FRAMES {
        let root = BASS_ROOTS[slot * EIGHTH_FRAMES / CHORD_FRAMES];
        let f = if slot % 4 == 2 { 2.0 * root } else { root };
        let out = &mut bass[slot * EIGHTH_FRAMES..(slot + 1) * EIGHTH_FRAMES];
        for (n, sample) in out.iter_mut().enumerate() {
            let t = n as f64 / SR;
            let turns = f * t;
            let tone = sin_turns(turns)
                + 0.5 * exp(-t / 0.06) * sin_turns(2.0 * turns)
                + 0.25 * exp(-t / 0.04) * sin_turns(3.0 * turns);
            let remaining = (EIGHTH_FRAMES - n) as f64 / SR;
            let envelope = (t / 0.004).min(1.0)
                * (0.55 + 0.45 * exp(-t / 0.09))
                * (remaining / 0.025).min(1.0);
            *sample = tanh(1.6 * tone) * envelope;
        }
    }

    let mut dry = vec![vec![0.0f64; LOOP_FRAMES]; 2];
    for (n, (l, r)) in pad[0].iter().zip(&pad[1]).enumerate() {
        let p = pump(n);
        dry[0][n] = PAD_GAIN * l * p + BASS_GAIN * bass[n];
        dry[1][n] = PAD_GAIN * r * p + BASS_GAIN * bass[n];
    }

    // Ping-pong echo of the mono sum, computed circularly (the echo of the loop's end lands on its
    // start), so it is seamless too: taps alternate left/right, each 0.45 × the previous one.
    let mono: Vec<f64> = dry[0]
        .iter()
        .zip(&dry[1])
        .map(|(l, r)| 0.5 * (l + r))
        .collect();
    let mut out = dry;
    let mut gain = 0.22;
    for tap in 1..=10usize {
        let channel = (tap + 1) % 2; // 1st tap left, 2nd right, …
        let shift = (tap * ECHO_FRAMES) % LOOP_FRAMES;
        for (n, sample) in out[channel].iter_mut().enumerate() {
            *sample += gain * mono[(n + LOOP_FRAMES - shift) % LOOP_FRAMES];
        }
        gain *= 0.45;
    }
    normalize_peak(&mut out, MINUS_6_DB);
    out
}

// ---------------------------------------------------------------------------------------------------
// whoosh.wav
// ---------------------------------------------------------------------------------------------------

fn whoosh() -> Vec<Vec<f64>> {
    const LENGTH: f64 = 1.2;
    const PEAK_AT: f64 = 0.85;
    let n_frames = Asset::Whoosh.frames();
    let mut rng = SplitMix64::new(0x5EED_2026_1019_0001);
    let (mut b0, mut b1, mut b2) = (0.0f64, 0.0f64, 0.0f64);
    let (mut low, mut band) = (0.0f64, 0.0f64);
    let mut left = vec![0.0f64; n_frames];
    let mut right = vec![0.0f64; n_frames];
    for n in 0..n_frames {
        let t = n as f64 / SR;
        // Swell: sin² up to the peak at 0.85 s, then cos² down to silence at 1.2 s.
        let envelope = if t < PEAK_AT {
            let s = sin_turns(0.25 * t / PEAK_AT);
            s * s
        } else {
            let c = cos_turns(0.25 * ((t - PEAK_AT) / (LENGTH - PEAK_AT)).min(1.0));
            c * c
        };
        // Pink-ish noise (Paul Kellet's economy filter).
        let white = rng.bipolar();
        b0 = 0.997_65 * b0 + white * 0.099_046_0;
        b1 = 0.963_00 * b1 + white * 0.296_516_4;
        b2 = 0.570_00 * b2 + white * 1.052_691_3;
        let pink = 0.25 * (b0 + b1 + b2 + white * 0.184_8);
        // Chamberlin state-variable band-pass; the centre rises with the swell (280 Hz → 5.5 kHz).
        let centre = 280.0 + 5200.0 * envelope * envelope.sqrt();
        let coefficient = 2.0 * sin_turns(0.5 * centre / SR);
        low += coefficient * band;
        let high = pink - low - 0.45 * band;
        band += coefficient * high;
        let y = band * envelope;
        // Constant-power pan from left to right.
        let theta = 0.25 * (0.18 + 0.64 * t / LENGTH);
        left[n] = y * cos_turns(theta);
        right[n] = y * sin_turns(theta);
    }
    let mut out = vec![left, right];
    normalize_peak(&mut out, MINUS_9_DB);
    for channel in &mut out {
        edge_fades(channel, frames(0.005), frames(0.005));
    }
    out
}

// ---------------------------------------------------------------------------------------------------
// impact.wav
// ---------------------------------------------------------------------------------------------------

fn impact() -> Vec<Vec<f64>> {
    let n_frames = Asset::Impact.frames();
    let mut rng = SplitMix64::new(0x5EED_2026_1019_0002);
    let (mut lp1, mut lp2, mut lp3) = (0.0f64, 0.0f64, 0.0f64);
    let norm = tanh(1.4);
    let mut out = vec![0.0f64; n_frames];
    for (n, sample) in out.iter_mut().enumerate() {
        let t = n as f64 / SR;
        let noise = rng.bipolar();
        // Sub boom: 158 Hz dropping to 38 Hz (closed-form phase), 2 ms attack, 0.45 s decay.
        let phase = 38.0 * t + 120.0 * 0.07 * (1.0 - exp(-t / 0.07));
        let boom = (1.0 - exp(-t / 0.002)) * exp(-t / 0.45) * sin_turns(phase);
        // Noise burst: one-pole low-passed noise, 25 ms decay.
        lp1 += 0.2791 * (noise - lp1);
        let burst = 0.6 * exp(-t / 0.025) * lp1;
        // Rumble: darker noise, 0.35 s decay.
        lp2 += 0.051_01 * (noise - lp2);
        lp3 += 0.051_01 * (lp2 - lp3);
        let rumble = 0.9 * exp(-t / 0.35) * lp3;
        let x = (t / 0.0008).min(1.0) * (boom + burst + rumble);
        *sample = tanh(1.4 * x) / norm;
    }
    let mut out = vec![out];
    normalize_peak(&mut out, MINUS_6_DB);
    for channel in &mut out {
        edge_fades(channel, 1, frames(0.08));
    }
    out
}

// ---------------------------------------------------------------------------------------------------
// chime.wav
// ---------------------------------------------------------------------------------------------------

fn chime() -> Vec<Vec<f64>> {
    /// (frequency Hz, onset s, gain): E5, then A5.
    const NOTES: [(f64, f64, f64); 2] = [(659.255, 0.0, 0.55), (880.000, 0.14, 0.45)];
    /// (frequency ratio, gain, decay s): bell-like inharmonic partials.
    const PARTIALS: [(f64, f64, f64); 5] = [
        (1.0, 1.0, 0.45),
        (2.0, 0.35, 0.30),
        (2.76, 0.18, 0.22),
        (5.40, 0.08, 0.12),
        (8.93, 0.03, 0.06),
    ];
    let n_frames = Asset::Chime.frames();
    let mut out = vec![0.0f64; n_frames];
    for (n, sample) in out.iter_mut().enumerate() {
        let t = n as f64 / SR;
        let mut y = 0.0;
        for (f0, onset, gain) in NOTES {
            if t < onset {
                continue;
            }
            let x = t - onset;
            let attack = 1.0 - exp(-x / 0.0015);
            let partials: f64 = PARTIALS
                .iter()
                .map(|&(ratio, g, decay)| g * exp(-x / decay) * sin_turns(f0 * ratio * x))
                .sum();
            y += gain * attack * partials;
        }
        *sample = y;
    }
    let mut out = vec![out];
    normalize_peak(&mut out, MINUS_7_DB);
    for channel in &mut out {
        edge_fades(channel, 1, frames(0.25));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a 64: a dependency-free fingerprint of the generated bytes.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    /// The 16-bit samples of a WAV written by `wav_bytes` (channels interleaved).
    fn samples(bytes: &[u8]) -> Vec<i16> {
        let data = bytes
            .windows(4)
            .position(|w| w == b"data")
            .map(|p| p + 8)
            .unwrap_or(bytes.len());
        let (pairs, _) = bytes[data..].as_chunks::<2>();
        pairs.iter().map(|&b| i16::from_le_bytes(b)).collect()
    }

    #[test]
    fn layouts_match_the_example_assets() {
        for asset in Asset::ALL {
            let buffer = asset.render();
            assert_eq!(buffer.sample_rate, 48_000, "{asset:?}");
            assert_eq!(buffer.num_channels(), asset.channels(), "{asset:?}");
            assert_eq!(buffer.frames(), asset.frames(), "{asset:?}");
            assert_eq!(
                Asset::from_relative_path(asset.relative_path()),
                Some(asset)
            );
        }
        assert_eq!(Asset::SynthwaveLoop.frames(), 8 * 48_000);
    }

    #[test]
    fn deterministic_bytes() {
        for asset in Asset::ALL {
            let a = asset.wav_bytes().expect("encode");
            let b = asset.wav_bytes().expect("encode");
            assert_eq!(a, b, "{asset:?} differs between two runs");
            assert_eq!(
                a.len(),
                44 + asset.frames() * asset.channels() * 2,
                "{asset:?}"
            );
        }
    }

    #[test]
    fn levels_and_silent_edges() {
        for asset in Asset::ALL {
            let bytes = asset.wav_bytes().expect("encode");
            let pcm = samples(&bytes);
            let peak = pcm.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
            assert!(
                peak < 32_000,
                "{asset:?} peak {peak} is too close to clipping"
            );
            let peak_db = 20.0 * (f64::from(peak) / 32_768.0).log10();
            let target = match asset {
                Asset::SynthwaveLoop | Asset::Impact => -6.0,
                Asset::Chime => -7.0,
                Asset::Whoosh => -9.0,
            };
            assert!(
                (peak_db - target).abs() < 0.1,
                "{asset:?} peak {peak_db:.2} dBFS"
            );
            if asset != Asset::SynthwaveLoop {
                let ch = asset.channels();
                assert!(
                    pcm[..ch].iter().all(|&s| s == 0),
                    "{asset:?} must start at 0"
                );
                assert!(
                    pcm[pcm.len() - ch..].iter().all(|&s| s == 0),
                    "{asset:?} must end at 0"
                );
            }
        }
    }

    #[test]
    fn loop_seam_is_smooth() {
        let buffer = Asset::SynthwaveLoop.render();
        for channel in &buffer.channels {
            let n = channel.len();
            let interior = channel
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0f32, f32::max);
            let seam = (channel[0] - channel[n - 1]).abs();
            assert!(
                seam <= interior,
                "seam step {seam} exceeds the largest interior step {interior}"
            );
            // The loop is not silent at the seam: the folded release of the G chord is audible there.
            assert!(channel[..480].iter().any(|x| x.abs() > 1e-3));
        }
    }

    #[test]
    fn fingerprints_are_stable() {
        // Pinned so that accidental changes to the synthesis (or platform-dependent math) are caught.
        // Regenerate with `SONICCRAFT_PRINT_ASSET_HASHES=1 cargo test -p soniccraft-cli fingerprints -- --nocapture`.
        let expected: [(Asset, u64); 4] = [
            (Asset::SynthwaveLoop, 0xDA32EAE9A31FF35A),
            (Asset::Whoosh, 0xA5A512496E2FD981),
            (Asset::Impact, 0x212B6B84132DF90D),
            (Asset::Chime, 0x1BA54722DF54D2E9),
        ];
        let print = std::env::var_os("SONICCRAFT_PRINT_ASSET_HASHES").is_some();
        for (asset, hash) in expected {
            let got = fnv1a(&asset.wav_bytes().expect("encode"));
            if print {
                println!("(Asset::{asset:?}, 0x{got:016X}),");
            } else {
                assert_eq!(got, hash, "{asset:?} fingerprint changed: 0x{got:016X}");
            }
        }
    }
}
