//! Randomized (fixed-seed, so deterministic) property tests: arbitrary timelines, including cycles,
//! self-references, open-ended anchors and conflicting rules, must never panic or loop, and the layout must be
//! internally consistent; the `push` policy's ordered structures must give exactly the results of the
//! straightforward implementation. Plus scale tests for the diagnostic caps and the push cost.

mod common;

use common::with_code;
use soniccraft_core::ir::{OverlapPolicy, ms_to_frames};
use soniccraft_layout::test_support::{ItemHandle, ProgramBuilder, TestTimeline, TrackBuilder};
use soniccraft_layout::{DurationSource, Layout, PlacementKind};

/// xorshift64*: tiny, fixed-seed, good enough for test-case generation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn ms(&mut self, max: u64) -> f64 {
        self.below(max + 1) as f64
    }

    fn pick<'a>(&mut self, items: &'a [String]) -> &'a str {
        &items[self.below(items.len() as u64) as usize]
    }
}

fn random_anchor<'a>(
    rng: &mut Rng,
    item: ItemHandle<'a>,
    ids: &[String],
    allow_before: bool,
) -> ItemHandle<'a> {
    let target = rng.pick(ids).to_string();
    let offset = rng.ms(4000) - 2000.0;
    match rng.below(if allow_before { 4 } else { 3 }) {
        0 => item.at(rng.ms(12_000)),
        1 => item.after(&target, offset),
        2 => item.with(&target, offset),
        _ => item.before(&target, offset),
    }
}

fn random_track_start(rng: &mut Rng, t: &mut TrackBuilder, ids: &[String]) {
    if !rng.chance(25) {
        return;
    }
    let target = rng.pick(ids).to_string();
    let offset = rng.ms(3000) - 1000.0;
    match rng.below(4) {
        0 => t.start_at(rng.ms(5000)),
        1 => t.start_after(&target, offset),
        2 => t.start_with(&target, offset),
        _ => t.start_before(&target, offset),
    };
}

fn random_timeline(seed: u64) -> TestTimeline {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let sample_rate = [1000, 44_100, 48_000][rng.below(3) as usize];
    let policy = [
        OverlapPolicy::Push,
        OverlapPolicy::Warn,
        OverlapPolicy::Error,
        OverlapPolicy::Allow,
    ][rng.below(4) as usize];
    let speech_tracks = 1 + rng.below(4) as usize;
    let music_tracks = rng.below(3) as usize;

    // Ids first, so anchors can point anywhere (forward, backward, at themselves, at tracks).
    let mut ids: Vec<String> = Vec::new();
    let mut plan: Vec<(String, bool, Vec<Option<String>>)> = Vec::new();
    for t in 0..speech_tracks + music_tracks {
        let speech = t < speech_tracks;
        let track_id = format!("{}{t}", if speech { "s" } else { "m" });
        ids.push(track_id.clone());
        let items = 1 + rng.below(if speech { 8 } else { 3 }) as usize;
        let mut item_ids = Vec::new();
        for i in 0..items {
            if speech && rng.chance(30) {
                item_ids.push(None); // a pause
            } else {
                let id = format!("{track_id}_{i}");
                ids.push(id.clone());
                item_ids.push(Some(id));
            }
        }
        plan.push((track_id, speech, item_ids));
    }

    let mut builder = ProgramBuilder::new()
        .sample_rate(sample_rate)
        .overlap_policy(policy)
        .min_gap_ms(rng.ms(500))
        .overlap_tolerance_ms(rng.ms(60));
    if rng.chance(30) {
        builder = builder.tail_ms(rng.ms(3000));
    }
    if rng.chance(25) {
        builder = builder.duration_ms(rng.ms(20_000));
    }
    for (track_id, speech, item_ids) in plan {
        let build = |t: &mut TrackBuilder| {
            random_track_start(&mut rng, t, &ids);
            if rng.chance(15) {
                t.mute();
            }
            for id in &item_ids {
                let Some(id) = id else {
                    t.pause(rng.ms(1200));
                    continue;
                };
                if speech {
                    let mut item = t.cue(id, "words", rng.ms(4000));
                    if rng.chance(35) {
                        item = random_anchor(&mut rng, item, &ids, true);
                    }
                    if rng.chance(10) {
                        item = item.allow_overlap();
                    }
                    if rng.chance(30) {
                        item.duration_source(DurationSource::Estimate);
                    }
                } else {
                    let mut item = t.clip(id, "a.wav", rng.ms(8000));
                    if rng.chance(40) {
                        item = random_anchor(&mut rng, item, &ids, true);
                    }
                    item = match rng.below(5) {
                        0 => item.length(rng.ms(10_000)),
                        1 => item.end_at(rng.ms(15_000)),
                        2 => item.end_after(rng.pick(&ids), rng.ms(2000) - 1000.0),
                        3 => item.end_with(rng.pick(&ids), rng.ms(2000)),
                        _ => item,
                    };
                    if rng.chance(40) {
                        item = item.looped().loop_crossfade(rng.ms(3000));
                    }
                    if rng.chance(30) {
                        item = item.trim_start(rng.ms(9000));
                    }
                    if rng.chance(15) {
                        item = item.duration_source(DurationSource::Estimate);
                    }
                    if rng.chance(50) {
                        item = item.fade_in(rng.ms(3000)).fade_out(rng.ms(3000));
                    }
                    if rng.chance(40) {
                        item.crossfade(rng.ms(2000));
                    }
                }
            }
        };
        builder = if speech {
            builder.speech(&track_id, build)
        } else {
            builder.music(&track_id, build)
        };
    }
    builder.build()
}

fn assert_contains_code(layout: &Layout, code: &str) {
    assert!(
        !with_code(layout, code).is_empty(),
        "expected `{code}` in {:#?}",
        layout.diagnostics
    );
}

/// Overlapping speech pairs computed independently of the layout's own bookkeeping.
fn overlapping_pairs(tl: &TestTimeline, layout: &Layout) -> usize {
    let tolerance = ms_to_frames(
        tl.program.timing.overlap_tolerance_ms,
        tl.program.output.sample_rate,
    )
    .max(0);
    let cues: Vec<(i64, i64)> = tl
        .program
        .cues()
        .filter(|(_, _, cue)| !cue.allow_overlap)
        .map(|(v, _, _)| (layout.placements[v].start, layout.placements[v].end()))
        .collect();
    let mut pairs = 0;
    for (i, a) in cues.iter().enumerate() {
        for b in &cues[i + 1..] {
            if a.1.min(b.1) - a.0.max(b.0) > tolerance {
                pairs += 1;
            }
        }
    }
    pairs
}

fn check_invariants(seed: u64, tl: &TestTimeline, layout: &Layout) {
    let ctx = || format!("seed {seed}: {:#?}", layout.diagnostics);
    assert_eq!(layout.placements.len(), tl.program.nodes.len(), "{}", ctx());
    assert_eq!(
        layout.track_spans.len(),
        tl.program.tracks.len(),
        "{}",
        ctx()
    );
    for (v, p) in layout.placements.iter().enumerate() {
        assert_eq!(p.node, v, "{}", ctx());
        assert!(p.start >= 0, "negative start for node {v}; {}", ctx());
        assert!(
            p.fade_in.saturating_add(p.fade_out) <= p.length || p.length == 0,
            "fades exceed length; {}",
            ctx()
        );
        if let Some(from) = p.pushed_from {
            assert!(from < p.start, "pushed earlier; {}", ctx());
            assert_eq!(
                tl.program.timing.speech_overlap,
                OverlapPolicy::Push,
                "{}",
                ctx()
            );
            assert_eq!(p.kind, PlacementKind::Cue, "{}", ctx());
        }
        if p.kind == PlacementKind::Pause {
            assert_eq!(p.duration_source, DurationSource::Fixed, "{}", ctx());
        }
    }

    assert_eq!(
        layout.estimate_dependent.len(),
        layout.diagnostics.len(),
        "{}",
        ctx()
    );
    let total = i64::try_from(layout.total_frames).expect("total fits i64");
    let content_end = layout
        .placements
        .iter()
        .filter(|p| !p.open_ended)
        .map(|p| p.end())
        .max()
        .unwrap_or(0)
        .max(0);
    assert_eq!(
        layout.content_end_frames,
        content_end.unsigned_abs(),
        "{}",
        ctx()
    );
    if tl.program.output.duration_ms.is_none() {
        let limit = ms_to_frames(soniccraft_core::MAX_TIME_MS, tl.program.output.sample_rate);
        assert_eq!(
            layout.total_frames,
            (layout.content_end_frames + layout.tail_frames).min(limit.unsigned_abs()),
            "{}",
            ctx()
        );
    }
    assert_eq!(
        layout.total_frames == 0,
        with_code(layout, "invalid_value")
            .iter()
            .any(|d| d.message.starts_with("the timeline is empty")),
        "{}",
        ctx()
    );
    for p in layout.placements.iter().filter(|p| p.open_ended) {
        if p.start < total {
            assert_eq!(
                p.end(),
                total,
                "open-ended clip must end at the total; {}",
                ctx()
            );
        } else {
            assert_eq!(p.length, 0, "{}", ctx());
        }
    }

    let reported = with_code(layout, "speech_overlap").len();
    let actual = overlapping_pairs(tl, layout);
    match tl.program.timing.speech_overlap {
        OverlapPolicy::Allow => assert_eq!(reported, 0, "{}", ctx()),
        _ if actual <= 50 => assert_eq!(reported, actual, "every overlap is reported; {}", ctx()),
        _ => assert_eq!(reported, 51, "capped overlaps; {}", ctx()),
    }
    if tl.program.timing.speech_overlap != OverlapPolicy::Push {
        assert!(with_code(layout, "cue_pushed").is_empty(), "{}", ctx());
    }
    assert!(
        (1..=soniccraft_layout::MAX_SCHEDULING_PASSES).contains(&layout.passes),
        "{}",
        ctx()
    );
    for d in &layout.diagnostics {
        assert!(!d.message.is_empty() && !d.code.is_empty(), "{}", ctx());
    }
}

#[test]
fn random_timelines_never_panic_and_stay_consistent() {
    for seed in 0..400 {
        let tl = random_timeline(seed);
        let layout = tl.layout();
        check_invariants(seed, &tl, &layout);
        assert_eq!(
            tl.layout(),
            layout,
            "seed {seed}: layout is not deterministic"
        );
        // The diagram renders for every layout.
        let diagram =
            soniccraft_layout::render_diagram(&tl.program, &layout, &[], &Default::default());
        assert!(diagram.ends_with('\n'));
    }
}

#[test]
fn random_timelines_reach_every_defensive_path() {
    // Guard against the generator silently losing coverage.
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..400 {
        for d in random_timeline(seed).layout().diagnostics {
            seen.insert(d.code);
        }
    }
    for code in [
        "speech_overlap",
        "cue_pushed",
        "negative_start",
        "content_truncated",
        "clip_shorter_than_requested",
        "fades_scaled",
        "no_fade_out_on_bed",
        "dependency_cycle",
        "self_reference",
        "anchor_open_ended",
        "conflicting_fields",
        "invalid_value",
        "field_not_applicable",
        "track_anchor_out_of_order",
    ] {
        assert!(
            seen.contains(code),
            "no random timeline produced `{code}`; seen: {seen:?}"
        );
    }
}

#[test]
fn overlap_floods_are_capped() {
    // 120 cues stacked at 0 under `warn`: 7140 overlapping pairs.
    let tl = ProgramBuilder::new()
        .sample_rate(1000)
        .overlap_policy(OverlapPolicy::Warn)
        .speech("v", |t| {
            for i in 0..120 {
                t.cue(&format!("c{i}"), "x", 1000.0).at(0.0);
            }
        })
        .build();
    let layout = tl.layout();
    let overlaps = with_code(&layout, "speech_overlap");
    assert_eq!(overlaps.len(), 51);
    assert_eq!(
        overlaps[50].message,
        "7090 more speech overlaps are not listed individually"
    );
    assert!(overlaps[50].path.is_none());
}

#[test]
fn push_floods_are_capped() {
    // 60 tracks with one implicit cue each: cue k is pushed past the k cues before it; 59 cues move.
    let mut builder = ProgramBuilder::new().sample_rate(1000);
    for k in 0..60 {
        builder = builder.speech(&format!("t{k}"), |t| {
            t.cue(&format!("c{k}"), "x", 1000.0);
        });
    }
    let tl = builder.build();
    let layout = tl.layout();
    let pushes = with_code(&layout, "cue_pushed");
    assert_eq!(pushes.len(), 51);
    assert_eq!(
        pushes[50].message,
        "9 more cues were moved by the speech-overlap policy; they are not listed individually"
    );
    // The geometry is complete regardless of the cap: every cue lands after the previous one.
    for k in 1..60 {
        let (prev, cur) = (tl.node(&format!("c{}", k - 1)), tl.node(&format!("c{k}")));
        assert_eq!(
            layout.placements[cur].start,
            layout.placements[prev].end() + 250
        );
    }
}

#[test]
fn large_timelines_lay_out() {
    // ~3000 nodes: a host asking, a guest answering explicitly, a third voice chiming in after every tenth
    // answer, a looping bed and stingers. The host's implicit questions make way for all explicit replies.
    let turns = 600;
    let tl = ProgramBuilder::new()
        .speech("a", |t| {
            for i in 0..turns {
                t.cue(
                    &format!("a{i}"),
                    "question",
                    1800.0 + f64::from(i % 7) * 100.0,
                );
                t.pause(400.0);
            }
        })
        .speech("b", |t| {
            for i in 0..turns {
                t.cue(&format!("b{i}"), "answer", 1500.0)
                    .after(&format!("a{i}"), 200.0);
            }
        })
        .speech("c", |t| {
            for i in 0..turns / 10 {
                t.cue(&format!("c{i}"), "aside", 900.0)
                    .after(&format!("b{}", i * 10), 300.0);
            }
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 120_000.0).looped().fade_out(3000.0);
        })
        .sfx("fx", |t| {
            for i in 0..turns / 20 {
                t.clip(&format!("x{i}"), "x.wav", 700.0)
                    .before(&format!("b{}", i * 20), -100.0);
            }
        })
        .build();
    let layout = tl.layout();
    check_invariants(u64::MAX, &tl, &layout);
    assert_eq!(layout.placements.len(), tl.program.nodes.len());
    // Implicit cues of track `a` got out of the way of the explicit replies: no overlap remains.
    assert_eq!(
        overlapping_pairs(&tl, &layout),
        0,
        "{:#?}",
        with_code(&layout, "speech_overlap")
    );
}

/// A timeline built for the `push` policy: many speech tracks of implicit cues (with pauses, so gaps differ),
/// some explicit cues anchored to them or placed absolutely, `allow_overlap` crosstalk, estimated lengths, and
/// tolerances from 0 to 60 ms.
fn push_heavy_timeline(seed: u64) -> TestTimeline {
    let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let tracks = 2 + rng.below(12) as usize;
    let mut ids: Vec<String> = Vec::new();
    let mut plan: Vec<Vec<Option<String>>> = Vec::new();
    for t in 0..tracks {
        let mut items = Vec::new();
        for i in 0..(1 + rng.below(9) as usize) {
            if rng.chance(20) {
                items.push(None);
            } else {
                let id = format!("t{t}_{i}");
                ids.push(id.clone());
                items.push(Some(id));
            }
        }
        plan.push(items);
    }
    let mut builder = ProgramBuilder::new()
        .sample_rate([1000, 48_000][rng.below(2) as usize])
        .min_gap_ms(rng.ms(400))
        .overlap_tolerance_ms(rng.ms(60));
    for (t, items) in plan.iter().enumerate() {
        builder = builder.speech(&format!("t{t}"), |tb| {
            if rng.chance(20) {
                tb.start_at(rng.ms(8000));
            }
            for id in items {
                let Some(id) = id else {
                    tb.pause(rng.ms(1500));
                    continue;
                };
                let len = if rng.chance(10) {
                    rng.ms(30)
                } else {
                    200.0 + rng.ms(3000)
                };
                let mut item = tb.cue(id, "words", len);
                if rng.chance(25) {
                    let target = rng.pick(&ids).to_string();
                    let offset = rng.ms(3000) - 1500.0;
                    item = match rng.below(4) {
                        0 => item.at(rng.ms(20_000)),
                        1 => item.after(&target, offset),
                        2 => item.with(&target, offset),
                        _ => item.before(&target, offset),
                    };
                }
                if rng.chance(5) {
                    item = item.allow_overlap();
                }
                if rng.chance(30) {
                    item.duration_source(DurationSource::Estimate);
                }
            }
        });
    }
    builder.build()
}

#[test]
fn fast_push_matches_the_reference_implementation() {
    let mut pushed = 0;
    let mut multi_pass = 0;
    for seed in 0..600 {
        let tl = if seed % 3 == 0 {
            random_timeline(seed)
        } else {
            push_heavy_timeline(seed)
        };
        let fast = tl.layout();
        let reference = tl.reference_layout();
        assert_eq!(fast, reference, "seed {seed}");
        pushed += fast
            .placements
            .iter()
            .filter(|p| p.pushed_from.is_some())
            .count();
        multi_pass += usize::from(fast.passes > 1);
    }
    // Guard against the generators losing coverage.
    assert!(pushed > 2000, "{pushed} pushed cues");
    assert!(multi_pass > 20, "{multi_pass} multi-pass layouts");
}

#[test]
fn pushes_through_thousands_of_colliding_cues_stay_cheap() {
    // review-timing#2 push-cubic: 3000 tracks with one implicit cue each (6.7 s to validate before); every cue
    // lands after all the cues before it. The ordered structures make this a few milliseconds.
    let mut builder = ProgramBuilder::new().sample_rate(1000);
    for k in 0..3000 {
        builder = builder.speech(&format!("t{k}"), |t| {
            t.cue(&format!("c{k}"), "x", 600.0 + f64::from(k % 7) * 50.0);
        });
    }
    let tl = builder.build();
    let layout = tl.layout();
    for k in 1..3000 {
        let (prev, cur) = (tl.node(&format!("c{}", k - 1)), tl.node(&format!("c{k}")));
        assert_eq!(
            layout.placements[cur].start,
            layout.placements[prev].end() + 250
        );
    }
    assert!(with_code(&layout, "speech_overlap").is_empty());

    // review-code push-policy-cubic: 300 tracks x 10 cues collide at every step (800 x 10 took 19.7 s).
    let mut builder = ProgramBuilder::new().sample_rate(1000);
    for k in 0..300 {
        builder = builder.speech(&format!("v{k}"), |t| {
            for i in 0..10 {
                t.cue(
                    &format!("v{k}_{i}"),
                    "Hello.",
                    700.0 + f64::from((k + i) % 5) * 100.0,
                );
            }
        });
    }
    let tl = builder.build();
    let layout = tl.layout();
    check_invariants(u64::MAX - 1, &tl, &layout);
    assert_eq!(overlapping_pairs(&tl, &layout), 0);
}

#[test]
fn malformed_ir_does_not_panic() {
    let mut tl = ProgramBuilder::new()
        .sample_rate(1000)
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
            t.cue("a2", "two", 1000.0);
            t.cue("a3", "three", 1000.0);
            t.pause(-500.0);
        })
        .music("m", |t| {
            t.clip("c1", "c.wav", 5000.0)
                .length(f64::NAN)
                .fade_in(f64::INFINITY)
                .fade_out(-50.0)
                .trim_start(-100.0);
        })
        .build();
    // a2 is listed by no track (an orphan); a3 is listed twice and by two tracks; 999 is out of range.
    tl.program.tracks[0].nodes = vec![0, 2, 2, 3, 999];
    tl.program.tracks[1].nodes.push(2);
    tl.program.timing.min_gap_ms = f64::NAN;
    tl.program.output.tail_ms = Some(1e300);
    let layout = tl.layout();
    assert_eq!(layout.placements.len(), 5);
    let (a1, a2, a3) = (tl.node("a1"), tl.node("a2"), tl.node("a3"));
    assert_eq!(layout.placements[a1].start, 0);
    // The orphan starts at 0 and is pushed past a1 (a NaN gap counts as 0); a3 then follows a1 and a2.
    assert_eq!(layout.placements[a2].start, 1000);
    assert_eq!(layout.placements[a3].start, 2000);
    let pause = &layout.placements[tl.node("a.pause1")];
    assert_eq!(
        (pause.start, pause.length),
        (3000, 0),
        "negative pauses are empty"
    );
    let clip = &layout.placements[tl.node("c1")];
    assert_eq!(
        (clip.length, clip.fade_in, clip.fade_out, clip.trim_start),
        (0, 0, 0, 0)
    );
    // An absurd tail is too long, not an overflow; the total is capped at 24 h.
    assert_eq!(layout.total_frames, 86_400_000);
    assert_contains_code(&layout, "timeline_too_long");
    let diagram = soniccraft_layout::render_diagram(&tl.program, &layout, &[], &Default::default());
    assert!(diagram.contains("SonicCraft plan"));

    // A zero sample rate turns every millisecond value into 0 frames (the table is already in frames) and
    // never divides by zero.
    tl.program.output.sample_rate = 0;
    let layout = tl.layout();
    assert_eq!(
        layout.total_frames, 3000,
        "the three cues from the table; the tail is 0 frames"
    );
    assert_eq!(layout.ms(12_345), 0.0);
    let diagram = soniccraft_layout::render_diagram(&tl.program, &layout, &[], &Default::default());
    assert!(diagram.ends_with('\n'));
}
