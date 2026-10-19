//! Alignment with timing payloads shaped like the real providers' (see the TTS API cookbook):
//! ElevenLabs character alignment, Cartesia batched word timestamps, Kokoro captioned speech.

use soniccraft_captions::align::{
    AlignmentSource, CharAlignment, fold_punctuation, map_provider_words,
};
use soniccraft_captions::text::display_text;
use soniccraft_captions::{CaptionConfig, CueInput, Interval, Word, build_captions, to_webvtt};

fn texts(words: &[Word]) -> Vec<&str> {
    words.iter().map(|w| w.text.as_str()).collect()
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Character entries and times (seconds) for `text`, 50 ms per character.
fn alignment_json(text: &str) -> String {
    let chars: Vec<String> = text
        .chars()
        .map(|c| serde_json::to_string(&c.to_string()).unwrap_or_default())
        .collect();
    let starts: Vec<String> = (0..chars.len())
        .map(|i| format!("{:.2}", i as f64 * 0.05))
        .collect();
    let ends: Vec<String> = (0..chars.len())
        .map(|i| format!("{:.2}", (i + 1) as f64 * 0.05))
        .collect();
    format!(
        r#"{{"characters":[{}],"character_start_times_seconds":[{}],"character_end_times_seconds":[{}]}}"#,
        chars.join(","),
        starts.join(","),
        ends.join(",")
    )
}

#[test]
fn elevenlabs_alignment_with_audio_tags() {
    let spoken = "[whispering] Don't tell anyone, okay?";
    let alignment: CharAlignment =
        serde_json::from_str(&alignment_json(spoken)).expect("alignment JSON");
    let provider = alignment.to_words();
    assert_eq!(texts(&provider), ["Don't", "tell", "anyone,", "okay?"]);

    let display = display_text(spoken, None);
    assert_eq!(display, "Don't tell anyone, okay?");
    let result = map_provider_words(&display, &provider, Interval::new(0.0, 2000.0), &[]);
    assert_eq!(result.source, AlignmentSource::Provider);
    // "Don't" starts after "[whispering] " (13 characters × 50 ms).
    assert!(
        approx(result.words[0].start_ms, 650.0),
        "{:?}",
        result.words[0]
    );
    assert!(
        approx(result.words[3].end_ms, 1850.0),
        "{:?}",
        result.words[3]
    );
}

#[test]
fn elevenlabs_normalized_alignment_spells_numbers() {
    // `normalized_alignment` covers the normalized text: "$5" became "five dollars".
    let normalized = "It costs five dollars today.";
    let alignment: CharAlignment =
        serde_json::from_str(&alignment_json(normalized)).expect("alignment JSON");
    let provider = alignment.to_words();
    let result = map_provider_words(
        "It costs $5 today.",
        &provider,
        Interval::new(0.0, 1500.0),
        &[],
    );
    assert_eq!(result.source, AlignmentSource::Mixed);
    assert_eq!(texts(&result.words), ["It", "costs", "$5", "today."]);
    // "$5" takes the span of "five dollars".
    assert!(
        approx(result.words[2].start_ms, 450.0) && approx(result.words[2].end_ms, 1050.0),
        "{:?}",
        result.words[2]
    );
}

#[test]
fn cartesia_batched_word_timestamps() {
    // Two `timestamps` events, concatenated in arrival order (seconds).
    let batches = [
        (vec!["Hello", "world"], vec![0.0, 0.5], vec![0.4, 0.9]),
        (
            vec!["this", "is", "SonicCraft"],
            vec![1.1, 1.3, 1.45],
            vec![1.3, 1.45, 2.1],
        ),
    ];
    let provider: Vec<Word> = batches
        .iter()
        .flat_map(|(words, starts, ends)| {
            words
                .iter()
                .zip(starts)
                .zip(ends)
                .map(|((w, s), e)| Word::new(*w, s * 1000.0, e * 1000.0))
        })
        .collect();
    let result = map_provider_words(
        "Hello, world! This is SonicCraft.",
        &provider,
        Interval::new(0.0, 2200.0),
        &[],
    );
    assert_eq!(result.source, AlignmentSource::Provider);
    assert_eq!(
        result.words,
        [
            Word::new("Hello,", 0.0, 400.0),
            Word::new("world!", 500.0, 900.0),
            Word::new("This", 1100.0, 1300.0),
            Word::new("is", 1300.0, 1450.0),
            Word::new("SonicCraft.", 1450.0, 2100.0),
        ]
    );
}

#[test]
fn kokoro_punctuation_tokens() {
    let json = r#"{"timestamps":[{"word":"Hello","start_time":0.275,"end_time":0.5},{"word":",","start_time":0.5,"end_time":0.6},{"word":"world","start_time":0.6,"end_time":0.9},{"word":"!","start_time":0.9,"end_time":1.2}]}"#;
    let value: serde_json::Value = serde_json::from_str(json).expect("kokoro JSON");
    let raw: Vec<Word> = value["timestamps"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    Word::new(
                        e["word"].as_str().unwrap_or_default(),
                        e["start_time"].as_f64().unwrap_or(f64::NAN) * 1000.0,
                        e["end_time"].as_f64().unwrap_or(f64::NAN) * 1000.0,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // Folding is optional for the mapping (punctuation-only words are ignored) but gives the provider
    // words their display form.
    let folded = fold_punctuation(&raw);
    assert_eq!(texts(&folded), ["Hello,", "world!"]);
    for provider in [&raw, &folded] {
        let result = map_provider_words("Hello, world!", provider, Interval::new(0.0, 1300.0), &[]);
        assert_eq!(result.source, AlignmentSource::Provider);
        assert_eq!(
            result.words,
            [
                Word::new("Hello,", 275.0, 500.0),
                Word::new("world!", 600.0, 900.0)
            ]
        );
    }
}

#[test]
fn trim_offset_shift_and_absolute_captions() {
    // Provider times are relative to the untrimmed audio; 120 ms of leading silence was trimmed, so the
    // engine shifts by -120 ms (a word starting inside the trimmed padding is clamped to 0).
    let provider = [
        Word::new("Good", 100.0, 400.0),
        Word::new("morning", 420.0, 900.0),
    ];
    let trimmed: Vec<Word> = provider.iter().map(|w| w.shifted(-120.0)).collect();
    let cue_audio = Interval::new(0.0, 820.0);
    let result = map_provider_words(
        "Good morning.",
        &trimmed,
        cue_audio,
        &[Interval::new(0.0, 800.0)],
    );
    assert_eq!(result.words[0], Word::new("Good", 0.0, 280.0));
    let cue_start = 61_000.0;
    let cue = CueInput {
        cue_id: "gm".into(),
        track_id: "voice".into(),
        speaker: None,
        text: "Good morning.".into(),
        start_ms: cue_start,
        end_ms: cue_start + 820.0,
        words: result.words.iter().map(|w| w.shifted(cue_start)).collect(),
    };
    let captions = build_captions(&[cue], &CaptionConfig::default());
    assert_eq!(
        to_webvtt(&captions),
        "WEBVTT\n\n00:01:01.000 --> 00:01:01.800\nGood morning.\n"
    );
}

#[test]
fn translated_caption_override_is_estimated() {
    let provider = vec![
        Word::new("Good", 0.0, 300.0),
        Word::new("morning", 350.0, 800.0),
        Word::new("everyone", 850.0, 1400.0),
    ];
    let display = display_text("Good morning everyone", Some("Bonjour à tous"));
    let span = Interval::new(0.0, 1500.0);
    let regions = [Interval::new(0.0, 1420.0)];
    let result = map_provider_words(&display, &provider, span, &regions);
    assert!(result.is_estimated());
    assert_eq!(result.source, AlignmentSource::Estimated);
    assert_eq!(texts(&result.words), ["Bonjour", "à", "tous"]);
    assert_eq!(result.words[0].start_ms, 0.0);
    assert_eq!(result.words[2].end_ms, 1420.0);
}

#[test]
fn cognates_keep_provider_timing() {
    // "Morgen" shares "mor" with "morning" (half of its key) and takes that word's timing; "Guten" is
    // estimated over the provider word nothing matched ("Good").
    let provider = vec![
        Word::new("Good", 0.0, 300.0),
        Word::new("morning", 350.0, 800.0),
    ];
    let result = map_provider_words("Guten Morgen", &provider, Interval::new(0.0, 900.0), &[]);
    assert_eq!(result.source, AlignmentSource::Mixed);
    assert_eq!(result.words[1], Word::new("Morgen", 350.0, 800.0));
    assert_eq!(
        (result.words[0].start_ms, result.words[0].end_ms),
        (0.0, 300.0)
    );
    // A short shared suffix inside unrelated long words is not a match.
    let provider = vec![Word::new("singing", 0.0, 500.0)];
    let result = map_provider_words("running", &provider, Interval::new(0.0, 600.0), &[]);
    assert_eq!(result.source, AlignmentSource::Estimated);
}
