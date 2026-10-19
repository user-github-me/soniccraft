//! Golden tests: a multi-track episode run through the whole pipeline (display text → alignment →
//! segmentation → writers) must reproduce the checked-in files byte for byte.
//!
//! Regenerate after an intentional change with
//! `UPDATE_GOLDEN=1 cargo test -p soniccraft-captions --test golden` and review the diff.

use std::path::PathBuf;

use soniccraft_captions::align::{
    char_alignment_to_words, estimate, fold_punctuation, map_provider_words,
};
use soniccraft_captions::text::display_text;
use soniccraft_captions::{
    CaptionConfig, CueInput, Interval, Word, auto_speaker_labels, build_captions, to_srt,
    to_transcript_json_string, to_webvtt, to_words_json_string,
};

/// Deterministic stand-in for ElevenLabs character timings: letters 55 ms, spaces 25 ms, punctuation
/// 140 ms, starting `lead_s` seconds into the audio. Returns (characters, starts, ends, duration_ms).
fn fake_char_alignment(text: &str, lead_s: f64) -> (Vec<String>, Vec<f64>, Vec<f64>, f64) {
    let mut t = lead_s;
    let (mut chars, mut starts, mut ends) = (Vec::new(), Vec::new(), Vec::new());
    for c in text.chars() {
        let d = if c.is_whitespace() {
            0.025
        } else if c.is_alphanumeric() {
            0.055
        } else {
            0.140
        };
        chars.push(c.to_string());
        starts.push(t);
        t += d;
        ends.push(t);
    }
    (chars, starts, ends, (t + lead_s) * 1000.0)
}

fn shifted(words: &[Word], offset_ms: f64) -> Vec<Word> {
    words.iter().map(|w| w.shifted(offset_ms)).collect()
}

fn cue(
    id: &str,
    track: &str,
    speaker: Option<&str>,
    text: &str,
    start: f64,
    end: f64,
    words: Vec<Word>,
) -> CueInput {
    CueInput {
        cue_id: id.into(),
        track_id: track.into(),
        speaker: speaker.map(String::from),
        text: text.into(),
        start_ms: start,
        end_ms: end,
        words,
    }
}

/// The episode: two speech tracks, provider timings, estimates with and without speech regions, a
/// caption override, audio tags, numbers, CJK, emoji, an overlong word and an empty cue.
fn episode() -> Vec<CueInput> {
    let mut cues = Vec::new();

    // 1. Narrator, ElevenLabs-style character timings for the spoken text (with an audio tag).
    let spoken = "[warmly] Autonomous software development is changing how teams build, test, and ship \
                  products every single day. Let's see what that means.";
    let (chars, starts, ends, duration) = fake_char_alignment(spoken, 0.03);
    let provider = char_alignment_to_words(&chars, &starts, &ends);
    let display = display_text(spoken, None);
    let alignment = map_provider_words(&display, &provider, Interval::new(0.0, duration), &[]);
    assert!(!alignment.is_estimated());
    cues.push(cue(
        "intro",
        "narrator",
        Some("Narrator"),
        &display,
        1000.0,
        1000.0 + duration,
        shifted(&alignment.words, 1000.0),
    ));
    let narrator_end = 1000.0 + duration;

    // 2. Guest on a second track, overlapping the narrator's tail; label needs WebVTT escaping.
    let spoken = "Wait — what? R&D <beta> teams ship daily?";
    let (chars, starts, ends, duration) = fake_char_alignment(spoken, 0.02);
    // Kokoro-style: punctuation arrives as separate tokens and is folded.
    let provider: Vec<Word> = fold_punctuation(&char_alignment_to_words(&chars, &starts, &ends));
    let display = display_text(spoken, None);
    let alignment = map_provider_words(&display, &provider, Interval::new(0.0, duration), &[]);
    let guest_start = narrator_end - 400.0;
    cues.push(cue(
        "guest_q",
        "guest",
        Some("Dr. Ada <Lovelace> & Co"),
        &display,
        guest_start,
        guest_start + duration,
        shifted(&alignment.words, guest_start),
    ));

    // 3. Narrator, no provider timings: estimate from detected speech regions (numbers, symbols, dash,
    //    ellipsis).
    let text = "In 1999, prices rose 12.5% — that's $3,000 per year… Wow! Nobody expected it.";
    let start = narrator_end + 1200.0;
    let span = Interval::new(0.0, 5200.0);
    let regions = [
        Interval::new(40.0, 1350.0),
        Interval::new(1700.0, 3900.0),
        Interval::new(4350.0, 5160.0),
    ];
    let words = estimate(&display_text(text, None), span, &regions);
    cues.push(cue(
        "numbers",
        "narrator",
        Some("Narrator"),
        text,
        start,
        start + 5200.0,
        shifted(&words, start),
    ));
    let numbers_end = start + 5200.0;

    // 4. Caption override: spoken "S Q L", shown "SQL"; provider timings for the spoken text.
    let spoken = "S Q L databases are fast 🎉";
    let caption = "SQL databases are fast 🎉";
    let (chars, starts, ends, duration) = fake_char_alignment(spoken, 0.0);
    let provider = char_alignment_to_words(&chars, &starts, &ends);
    let display = display_text(spoken, Some(caption));
    let alignment = map_provider_words(&display, &provider, Interval::new(0.0, duration), &[]);
    let start = numbers_end + 300.0;
    cues.push(cue(
        "sql",
        "guest",
        Some("Dr. Ada <Lovelace> & Co"),
        &display,
        start,
        start + duration,
        shifted(&alignment.words, start),
    ));
    let sql_end = start + duration;

    // 5. CJK, words estimated by build_captions itself (no words given).
    let start = sql_end + 500.0;
    cues.push(cue(
        "zh",
        "narrator",
        Some("Narrator"),
        "你好，世界。今天我们讨论人工智能的未来。",
        start,
        start + 4200.0,
        Vec::new(),
    ));

    // 6. An overlong word and a very short tail (minimum-duration extension).
    let start = start + 4600.0;
    cues.push(cue(
        "long",
        "narrator",
        Some("Narrator"),
        "Pneumonoultramicroscopicsilicovolcanoconiosis-adjacent jargon. Done.",
        start,
        start + 3000.0,
        Vec::new(),
    ));

    // 7. Empty after removing the audio tag: skipped.
    cues.push(cue(
        "tag_only",
        "guest",
        None,
        &display_text("[laughs]", None),
        start,
        start + 500.0,
        Vec::new(),
    ));

    cues
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} (run with UPDATE_GOLDEN=1)",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "{name} differs from the golden file {}.\n--- expected ---\n{expected}\n--- actual ---\n{actual}",
        path.display()
    );
}

fn config(cues: &[CueInput]) -> CaptionConfig {
    CaptionConfig {
        speaker_labels: auto_speaker_labels(cues),
        ..CaptionConfig::default()
    }
}

#[test]
fn episode_matches_golden_files() {
    let cues = episode();
    let config = config(&cues);
    assert!(config.speaker_labels, "two distinct speakers → labels on");
    let captions = build_captions(&cues, &config);
    check_golden("episode.vtt", &to_webvtt(&captions));
    check_golden("episode.srt", &to_srt(&captions));
    check_golden("episode.words.json", &to_words_json_string(&captions));
    check_golden(
        "episode.transcript.json",
        &to_transcript_json_string(&captions),
    );
}

#[test]
fn episode_is_deterministic() {
    let render = || {
        let cues = episode();
        let captions = build_captions(&cues, &config(&cues));
        (
            to_webvtt(&captions),
            to_srt(&captions),
            to_words_json_string(&captions),
            to_transcript_json_string(&captions),
        )
    };
    assert_eq!(render(), render());
}

#[test]
fn single_speaker_episode_has_no_labels() {
    let cues: Vec<CueInput> = episode()
        .into_iter()
        .filter(|c| c.track_id == "narrator")
        .collect();
    let config = config(&cues);
    assert!(!config.speaker_labels);
    let captions = build_captions(&cues, &config);
    let vtt = to_webvtt(&captions);
    assert!(!vtt.contains("<v "), "{vtt}");
    // Data still carries the speaker.
    assert!(to_words_json_string(&captions).contains("\"speaker\": \"Narrator\""));
}
