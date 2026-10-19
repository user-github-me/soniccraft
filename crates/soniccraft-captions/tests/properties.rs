//! Invariant tests over many pseudo-random inputs (fixed-seed PRNG, so failures are reproducible).

use soniccraft_captions::align::{AlignmentSource, estimate, map_provider_words};
use soniccraft_captions::text::{display_len, normalize_whitespace, tokenize};
use soniccraft_captions::{
    Caption, CaptionConfig, CueInput, Interval, Word, WordsDocument, build_captions, to_srt,
    to_transcript_json_string, to_webvtt, to_words_json_string, transcript,
};

/// xorshift64* — tiny deterministic PRNG for test data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const VOCABULARY: &[&str] = &[
    "the",
    "a",
    "software",
    "development",
    "teams",
    "build,",
    "test;",
    "ship.",
    "Really?",
    "Wow!",
    "don't",
    "state-of-the-art",
    "“quoted”",
    "(aside)",
    "1999,",
    "12.5%",
    "$3,000",
    "10:30",
    "2nd",
    "R&D",
    "AI",
    "SQL",
    "wait—what",
    "so...then",
    "—",
    "…",
    "«",
    "»",
    "🎉",
    "great🎉",
    "e.g.",
    "Dr.",
    "U.S.",
    "你好，",
    "世界。",
    "きょうは",
    "안녕하세요",
    "привет,",
    "καλημέρα",
    "Pneumonoultramicroscopicsilicovolcanoconiosis",
    "x",
    "I",
    "OK",
    "co-op",
    "rock’n’roll",
    "10\u{a0}km",
    "C++",
    "&",
    "+",
];

const SEPARATORS: &[&str] = &[" ", " ", " ", " ", "  ", "\n", "\t", " \n "];

fn random_text(rng: &mut Rng, max_words: usize) -> String {
    let n = 1 + rng.below(max_words);
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push_str(rng.pick(SEPARATORS));
        }
        out.push_str(rng.pick(VOCABULARY));
    }
    out
}

/// Provider-style words for `text`: the tokens with random perturbations and increasing times.
fn random_provider(rng: &mut Rng, text: &str, span: Interval) -> Vec<Word> {
    let mut spoken: Vec<String> = Vec::new();
    for token in tokenize(text) {
        match rng.below(10) {
            0 => {}                                                   // dropped
            1 => spoken.extend(token.text.chars().map(String::from)), // spelled / split
            2 => spoken.push(token.text.to_uppercase()),
            3 => {
                spoken.push(token.text.clone());
                spoken.push("um".to_string()); // extra word
            }
            4 => match spoken.last_mut() {
                Some(last) => last.push_str(&token.text), // merged with the previous word
                None => spoken.push(token.text.clone()),
            },
            _ => spoken.push(
                token
                    .text
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_string(),
            ),
        }
    }
    let duration = span.duration_ms();
    let mut t = span.start_ms + rng.below(50) as f64;
    let step = if spoken.is_empty() {
        0.0
    } else {
        duration / spoken.len() as f64
    };
    spoken
        .into_iter()
        .map(|text| {
            let start = t;
            let end = (start + step * (0.5 + rng.below(50) as f64 / 100.0)).min(span.end_ms);
            t = (start + step).min(span.end_ms);
            Word::new(text, start, end.max(start))
        })
        .collect()
}

fn random_regions(rng: &mut Rng, span: Interval) -> Vec<Interval> {
    let count = rng.below(4);
    let mut regions = Vec::new();
    let mut t = span.start_ms + rng.below(80) as f64;
    for _ in 0..count {
        let len = 200.0 + rng.below(1500) as f64;
        let end = (t + len).min(span.end_ms);
        if end > t {
            regions.push(Interval::new(t, end));
        }
        t = end + 120.0 + rng.below(500) as f64;
    }
    regions
}

fn assert_words_valid(words: &[Word], text: &str, span: Interval, context: &str) {
    let tokens = tokenize(text);
    assert_eq!(words.len(), tokens.len(), "{context}");
    for (w, t) in words.iter().zip(&tokens) {
        assert_eq!(w.text, t.text, "{context}");
        assert!(
            w.start_ms.is_finite() && w.end_ms.is_finite(),
            "{context}: {w:?}"
        );
        assert!(
            w.start_ms >= span.start_ms - 1e-6 && w.end_ms <= span.end_ms + 1e-6,
            "{context}: {w:?} {span:?}"
        );
        assert!(w.end_ms >= w.start_ms, "{context}: {w:?}");
    }
    for pair in words.windows(2) {
        assert!(
            pair[1].start_ms >= pair[0].end_ms - 1e-6,
            "{context}: {pair:?}"
        );
    }
}

#[test]
fn tokenization_round_trips() {
    let mut rng = Rng(0x5eed_0001);
    for case in 0..500 {
        let text = random_text(&mut rng, 30);
        let mut rebuilt = String::new();
        for t in tokenize(&text) {
            assert!(!t.text.is_empty(), "case {case}: {text:?}");
            assert!(
                !t.text.contains(' ')
                    || !t.key.is_empty()
                    || t.text.chars().all(|c| !c.is_alphanumeric())
            );
            if t.space_before {
                rebuilt.push(' ');
            }
            rebuilt.push_str(&t.text);
        }
        assert_eq!(rebuilt, normalize_whitespace(&text), "case {case}");
    }
}

#[test]
fn alignment_invariants_hold() {
    let mut rng = Rng(0x5eed_0002);
    let mut coverage = [0usize; 3]; // provider, mixed, estimated
    for case in 0..400 {
        let text = random_text(&mut rng, 40);
        let start = rng.below(10_000) as f64;
        let span = Interval::new(start, start + 300.0 + rng.below(20_000) as f64);
        let regions = random_regions(&mut rng, span);
        let context = format!("case {case}: {text:?}");

        let estimated = estimate(&text, span, &regions);
        assert_words_valid(&estimated, &text, span, &context);

        let provider = random_provider(&mut rng, &text, span);
        let alignment = map_provider_words(&text, &provider, span, &regions);
        assert_words_valid(&alignment.words, &text, span, &context);
        assert!(alignment.matched <= alignment.words.len());

        match alignment.source {
            AlignmentSource::Provider => coverage[0] += 1,
            AlignmentSource::Mixed => coverage[1] += 1,
            AlignmentSource::Estimated => coverage[2] += 1,
        }

        // Exact provider words map exactly (unless nothing in the text is speakable).
        if tokenize(&text).iter().all(|t| t.key.is_empty()) {
            continue;
        }
        let mapped = map_provider_words(&text, &estimated, span, &regions);
        assert!(!mapped.is_estimated(), "{context}");
        for (a, b) in mapped.words.iter().zip(&estimated) {
            assert!(
                (a.start_ms - b.start_ms).abs() < 1e-6 && (a.end_ms - b.end_ms).abs() < 1e-6,
                "{context}"
            );
        }
    }
    // Every alignment outcome was exercised.
    assert!(coverage.iter().all(|&n| n > 0), "{coverage:?}");
}

fn random_cues(rng: &mut Rng) -> Vec<CueInput> {
    let tracks = ["voice_a", "voice_b", "voice_c"];
    let mut cues = Vec::new();
    let mut cursor = [0.0f64; 3];
    for i in 0..(1 + rng.below(12)) {
        let track = rng.below(tracks.len());
        let text = random_text(rng, 45);
        let start = cursor[track] + rng.below(1500) as f64;
        let duration = 200.0 + rng.below(15_000) as f64;
        let span = Interval::new(start, start + duration);
        let words = match rng.below(3) {
            0 => Vec::new(),
            1 => estimate(&text, span, &random_regions(rng, span)),
            _ => {
                let rel = Interval::new(0.0, duration);
                let provider = random_provider(rng, &text, rel);
                map_provider_words(&text, &provider, rel, &[])
                    .words
                    .into_iter()
                    .map(|w| w.shifted(start))
                    .collect()
            }
        };
        cursor[track] = start + duration;
        cues.push(CueInput {
            cue_id: format!("cue{i}"),
            track_id: tracks[track].to_string(),
            speaker: rng
                .chance(80)
                .then(|| ["Ann", "Bob <B>", "Chloé & co"][track].to_string()),
            text,
            start_ms: start,
            end_ms: start + duration,
            words,
        });
    }
    cues
}

fn random_config(rng: &mut Rng) -> CaptionConfig {
    CaptionConfig {
        max_chars_per_line: [12, 20, 32, 42, 42, 60][rng.below(6)],
        max_lines: 1 + rng.below(3),
        max_duration_ms: [2500.0, 6000.0, 6000.0, 10_000.0][rng.below(4)],
        min_duration_ms: [0.0, 800.0, 800.0, 1500.0][rng.below(4)],
        speaker_labels: rng.chance(50),
        timeline_end_ms: rng.chance(30).then(|| 50_000.0 + rng.below(100_000) as f64),
    }
}

/// Rebuild a cue's text from its captions: captions join with a space unless the text had none there.
fn joined_without_boundaries(captions: &[&Caption]) -> String {
    captions.iter().map(|c| c.text.replace(' ', "")).collect()
}

fn check_captions(cues: &[CueInput], config: &CaptionConfig, captions: &[Caption], context: &str) {
    // Indices, sorting.
    for (i, c) in captions.iter().enumerate() {
        assert_eq!(c.index, i + 1, "{context}");
        assert!(c.start_ms.is_finite() && c.end_ms.is_finite(), "{context}");
        assert!(c.end_ms >= c.start_ms + 1.0 - 1e-9, "{context}: {c:?}");
        assert!(
            !c.lines.is_empty() && c.lines.len() <= config.max_lines.max(1),
            "{context}: {c:?}"
        );
        assert!(!c.words.is_empty(), "{context}");
        assert_eq!(c.start_ms, c.words[0].start_ms, "{context}");
        for line in &c.lines {
            assert!(
                !line.is_empty() && line.trim() == line,
                "{context}: {line:?}"
            );
            let single_word = c.words.iter().any(|w| w.text == *line);
            assert!(
                display_len(line) <= config.max_chars_per_line.max(1) || single_word,
                "{context}: {line:?}"
            );
        }
        assert_eq!(
            c.lines.join("").replace(' ', ""),
            c.text.replace(' ', ""),
            "{context}"
        );
        let raw = c.words[c.words.len() - 1].end_ms - c.words[0].start_ms;
        assert!(
            raw <= config.max_duration_ms + 1e-6 || c.words.len() == 1,
            "{context}: {raw} {c:?}"
        );
        assert_eq!(
            c.label.is_some(),
            config.speaker_labels && c.speaker.is_some(),
            "{context}"
        );
    }
    for pair in captions.windows(2) {
        assert!(pair[0].start_ms <= pair[1].start_ms, "{context}");
    }
    // Per cue: text preserved, no merging, no same-track overlap introduced.
    for (order, cue) in cues.iter().enumerate() {
        let own: Vec<&Caption> = captions.iter().filter(|c| c.cue_order == order).collect();
        let display = normalize_whitespace(&cue.text);
        if display.is_empty() {
            assert!(own.is_empty(), "{context}");
            continue;
        }
        assert!(
            own.iter()
                .all(|c| c.cue_id == cue.cue_id && c.track_id == cue.track_id),
            "{context}"
        );
        if cue.words.is_empty()
            || cue
                .words
                .iter()
                .map(|w| w.text.as_str())
                .eq(tokenize(&display).iter().map(|t| t.text.as_str()))
        {
            assert_eq!(
                joined_without_boundaries(&own),
                display.replace(' ', ""),
                "{context}"
            );
        }
    }
    for track in ["voice_a", "voice_b", "voice_c"] {
        let on_track: Vec<&Caption> = captions.iter().filter(|c| c.track_id == track).collect();
        for pair in on_track.windows(2) {
            let words_overlap = pair[0].words.last().map_or(0.0, |w| w.end_ms) > pair[1].start_ms;
            assert!(
                pair[0].end_ms <= pair[1].start_ms + 1e-9
                    || words_overlap
                    || pair[0].end_ms - pair[0].start_ms <= 1.0 + 1e-9,
                "{context}: {:?} / {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}

fn check_writers(captions: &[Caption], context: &str) {
    // WebVTT structure.
    let vtt = to_webvtt(captions);
    assert!(vtt.starts_with("WEBVTT\n"), "{context}");
    assert!(vtt.ends_with('\n'), "{context}");
    let blocks: Vec<&str> = vtt.trim_end_matches('\n').split("\n\n").skip(1).collect();
    assert_eq!(blocks.len(), captions.len(), "{context}\n{vtt}");
    for block in &blocks {
        let mut lines = block.lines();
        let timing = lines.next().unwrap_or_default();
        assert_eq!(
            timing.len(),
            "00:00:00.000 --> 00:00:00.000".len(),
            "{context}: {timing}"
        );
        assert!(timing.contains(" --> "), "{context}");
        let payload: Vec<&str> = lines.collect();
        assert!(
            !payload.is_empty() && payload.iter().all(|l| !l.trim().is_empty()),
            "{context}"
        );
        assert!(payload.iter().all(|l| !l.contains("-->")), "{context}");
    }
    // SRT numbering.
    let srt = to_srt(captions);
    let blocks: Vec<&str> = if srt.is_empty() {
        Vec::new()
    } else {
        srt.trim_end_matches('\n').split("\n\n").collect()
    };
    assert_eq!(blocks.len(), captions.len(), "{context}");
    for (i, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.lines().next(),
            Some((i + 1).to_string().as_str()),
            "{context}"
        );
    }
    // JSON round trip and transcript order.
    let doc: WordsDocument =
        serde_json::from_str(&to_words_json_string(captions)).expect("valid words.json");
    assert_eq!(doc.captions.len(), captions.len(), "{context}");
    assert_eq!(
        doc.words.len(),
        captions.iter().map(|c| c.words.len()).sum::<usize>(),
        "{context}"
    );
    let words = transcript(captions);
    assert!(
        words.windows(2).all(|p| p[0].start <= p[1].start),
        "{context}"
    );
    assert!(
        words
            .iter()
            .enumerate()
            .all(|(i, w)| w.id == format!("w{i}") && w.end >= w.start),
        "{context}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&to_transcript_json_string(captions)).expect("valid transcript");
    assert_eq!(
        parsed.as_array().map(Vec::len),
        Some(words.len()),
        "{context}"
    );
}

#[test]
fn caption_invariants_hold() {
    let mut rng = Rng(0x5eed_0003);
    let (mut multi_line, mut overlong, mut multi_caption_cues) = (0usize, 0usize, 0usize);
    for case in 0..300 {
        let cues = random_cues(&mut rng);
        let config = random_config(&mut rng);
        let context = format!("case {case}: {config:?}");
        let captions = build_captions(&cues, &config);
        check_captions(&cues, &config, &captions, &context);
        check_writers(&captions, &context);
        assert_eq!(
            captions,
            build_captions(&cues, &config),
            "{context}: not deterministic"
        );
        for c in &captions {
            multi_line += usize::from(c.lines.len() > 1);
            overlong += c
                .lines
                .iter()
                .filter(|l| display_len(l) > config.max_chars_per_line)
                .count();
            multi_caption_cues +=
                usize::from(c.index > 1 && captions[c.index - 2].cue_order == c.cue_order);
        }
    }
    // The interesting layouts actually occurred.
    assert!(
        multi_line > 0 && overlong > 0 && multi_caption_cues > 0,
        "{multi_line} {overlong} {multi_caption_cues}"
    );
}
