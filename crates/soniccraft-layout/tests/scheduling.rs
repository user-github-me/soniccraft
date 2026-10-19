//! Scheduling, anchors and the speech-overlap policy (DESIGN.md §3.2, §3.4, §2.13).
//!
//! Most timelines use a 1000 Hz sample rate so that one frame is one millisecond.

mod common;

use common::{assert_clean, assert_contains, at, codes, single, span, with_code};
use soniccraft_core::Severity;
use soniccraft_core::ir::{AnchorMode, AnchorRule, AnchorTarget, OverlapPolicy, StartRule};
use soniccraft_layout::test_support::{ProgramBuilder, TestTimeline};
use soniccraft_layout::{
    DurationSource, Interval, MAX_SCHEDULING_PASSES, NodeDuration, PlacementKind, layout,
};

fn ms_builder() -> ProgramBuilder {
    ProgramBuilder::new().sample_rate(1000)
}

/// The README timeline: `cue_03` follows a 350 ms pause but would collide with the explicitly anchored
/// `cue_02`, so it is pushed to `end(cue_02) + 350 ms`.
fn readme_timeline() -> TestTimeline {
    ProgramBuilder::new()
        .speech("host", |t| {
            t.cue("cue_01", "Welcome to the briefing.", 2000.0);
            t.pause(350.0);
            t.cue("cue_03", "So, what changed?", 2600.0);
        })
        .speech("guest", |t| {
            t.cue("cue_02", "Thanks. A lot changed.", 3100.0)
                .after("cue_01", 150.0);
        })
        .build()
}

#[test]
fn readme_worked_example() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let f = |ms: i64| ms * 48;
    assert_eq!(layout.sample_rate, 48_000);
    assert_eq!(span(&tl, &layout, "cue_01"), (0, f(2000)));
    assert_eq!(span(&tl, &layout, "cue_02"), (f(2150), f(5250)));
    let cue_03 = at(&tl, &layout, "cue_03");
    assert_eq!(
        cue_03.pushed_from,
        Some(f(2350)),
        "tentative start = end(cue_01) + pause"
    );
    assert_eq!(
        (cue_03.start, cue_03.end()),
        (f(5600), f(8200)),
        "end(cue_02) + the 350 ms pause"
    );
    let pause = &layout.placements[1];
    assert_eq!(
        (pause.kind, pause.start, pause.length),
        (PlacementKind::Pause, f(2000), 16_800)
    );
    assert_eq!(pause.duration_source, DurationSource::Fixed);

    let pushed = single(&layout, "cue_pushed");
    assert_eq!(pushed.severity, Severity::Info);
    assert_contains(
        &pushed.message,
        &[
            "`cue_03` (track `host`)",
            "from 2.35 s to 5.60 s",
            "`cue_02` (track `guest`, 2.15-5.25 s)",
        ],
    );
    assert_eq!(pushed.path.as_deref(), Some("tracks[0].sequence[2]"));
    assert_eq!(pushed.related, ["cue_03", "cue_02"]);
    assert!(
        pushed
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("allow_overlap: true"))
    );
    assert_eq!(codes(&layout), ["cue_pushed"]);
    assert_eq!(layout.passes, 1);

    assert_eq!(layout.content_end_frames, f(8200).unsigned_abs());
    assert_eq!(layout.total_frames, f(8200).unsigned_abs());
    assert_eq!(layout.start_ms(tl.node("cue_03")), Some(5600.0));
    assert_eq!(layout.end_ms(tl.node("cue_03")), Some(8200.0));
    assert_eq!(layout.duration_ms(tl.node("cue_03")), Some(2600.0));
    assert_eq!(layout.pushed_from_ms(tl.node("cue_03")), Some(2350.0));
    assert_eq!(layout.total_ms(), 8200.0);
}

#[test]
fn explicit_cues_are_scheduled_first_and_never_move() {
    // The explicit cue is declared in a later track; the implicit one still has to make way.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "implicit", 2000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "explicit", 1000.0).at(1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (1000, 2000));
    assert_eq!(at(&tl, &layout, "b1").pushed_from, None);
    assert_eq!(
        span(&tl, &layout, "a1"),
        (2250, 4250),
        "end(b1) + min_gap_ms"
    );
    assert_eq!(at(&tl, &layout, "a1").pushed_from, Some(0));
    assert_contains(&single(&layout, "cue_pushed").message, &["`a1`", "by `b1`"]);
    assert_clean(&layout);

    // Same within one track: the explicit second item is placed before the sequential first item.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "implicit", 2000.0);
            t.cue("a2", "explicit", 1000.0).at(500.0);
            t.cue("a3", "after the explicit one", 500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "a2"), (500, 1500));
    assert_eq!(span(&tl, &layout, "a1"), (1750, 3750));
    // a3 follows a2 (cursor = end of the explicit cue), then makes way for the pushed a1.
    assert_eq!(at(&tl, &layout, "a3").pushed_from, Some(1500));
    assert_eq!(span(&tl, &layout, "a3"), (4000, 4500));
    assert_clean(&layout);
}

#[test]
fn push_uses_min_gap_without_preceding_pauses() {
    let tl = ms_builder()
        .min_gap_ms(400.0)
        .speech("a", |t| {
            t.cue("a1", "long", 3000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "short", 1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (3400, 4400));
    assert_eq!(at(&tl, &layout, "b1").pushed_from, Some(0));
}

#[test]
fn push_gap_is_the_sum_of_the_directly_preceding_pauses() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "long", 3000.0);
        })
        .speech("b", |t| {
            t.pause(60.0);
            t.pause(40.0);
            t.cue("b1", "pushed by a1", 100.0);
            t.pause(400.0);
            t.cue("b2", "pushed by c1", 100.0);
        })
        .speech("c", |t| {
            t.cue("c1", "obstacle", 1000.0).at(3550.0);
        })
        .build();
    let layout = tl.layout();
    // gap(b1) = 60 + 40: both pauses directly precede it.
    assert_eq!(at(&tl, &layout, "b1").pushed_from, Some(100));
    assert_eq!(span(&tl, &layout, "b1"), (3100, 3200));
    // gap(b2) = 400 only: the earlier pauses are separated from it by b1.
    assert_eq!(at(&tl, &layout, "b2").pushed_from, Some(3600));
    assert_eq!(span(&tl, &layout, "b2"), (4950, 5050));
    assert_clean(&layout);
}

#[test]
fn push_chains_across_three_tracks() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0);
        })
        .speech("c", |t| {
            t.cue("c1", "three", 1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "a1"), (0, 1000));
    assert_eq!(span(&tl, &layout, "b1"), (1250, 2250));
    // c1 is pushed past a1, lands on b1, and is pushed again: repeat until it overlaps nothing. One
    // `cue_pushed` per moved cue, naming the cue it finally waits for.
    assert_eq!(span(&tl, &layout, "c1"), (2500, 3500));
    assert_eq!(at(&tl, &layout, "c1").pushed_from, Some(0));
    let pushes = with_code(&layout, "cue_pushed");
    assert_eq!(pushes.len(), 2);
    assert_contains(
        &pushes[0].message,
        &["`b1`", "from 0.00 s to 1.25 s", "by `a1`"],
    );
    assert_contains(
        &pushes[1].message,
        &[
            "`c1`",
            "from 0.00 s to 2.50 s",
            "by `b1` (track `b`, 1.25-2.25 s)",
        ],
    );
    assert_clean(&layout);
}

#[test]
fn speakers_relying_on_push_take_turns() {
    // Neither track anchors the other; alice's lines grow longer, so their slots start to collide with
    // bob's. A pushed cue keeps the priority of its original tentative start, so the speakers alternate
    // instead of one finishing all of their lines first.
    let tl = ms_builder()
        .speech("alice", |t| {
            for i in 0..4 {
                t.cue(&format!("a{i}"), "alice", 7000.0 + 500.0 * f64::from(i));
                t.pause(9000.0);
            }
        })
        .speech("bob", |t| {
            t.start_at(7600.0);
            for i in 0..4 {
                t.cue(&format!("b{i}"), "bob", 5000.0);
                t.pause(11_000.0);
            }
        })
        .build();
    let layout = tl.layout();
    let order: Vec<&str> = layout
        .speech_cue_intervals()
        .iter()
        .map(|(v, _)| tl.program.nodes[*v].id.as_str())
        .collect();
    assert_eq!(order, ["a0", "b0", "a1", "b1", "a2", "b2", "a3", "b3"]);
    // Each pushed cue keeps its own gap (its preceding pause) after the cue that blocked it.
    assert_eq!(span(&tl, &layout, "b2"), (51_500, 56_500), "end(a2) + 11 s");
    assert_eq!(span(&tl, &layout, "a3"), (65_500, 74_000), "end(b2) + 9 s");
    assert_eq!(span(&tl, &layout, "b3"), (85_000, 90_000), "end(a3) + 11 s");
    assert_clean(&layout);
}

#[test]
fn push_waits_for_the_latest_ending_overlapping_cue() {
    // Both a1 and b1 overlap c1's tentative slot; c1 waits for the later end.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0).at(0.0);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0).at(500.0);
        })
        .speech("c", |t| {
            t.cue("c1", "three", 2000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "c1"), (1750, 3750));
    assert_contains(&single(&layout, "cue_pushed").message, &["by `b1`"]);
}

/// Track `a` speaks three implicit cues; `m` holds three sequential music clips; `e` is an explicit cue that
/// starts 5 s before `m3`. `e` only becomes ready after the whole music chain is placed, i.e. after `a2` has
/// already been scheduled into the slot `e` lands in.
fn late_explicit_timeline() -> TestTimeline {
    ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 2000.0);
            t.cue("a2", "two", 2000.0);
            t.cue("a3", "three", 2000.0);
        })
        .music("m", |t| {
            t.clip("m1", "m1.wav", 4000.0);
            t.clip("m2", "m2.wav", 4000.0);
            t.clip("m3", "m3.wav", 4000.0);
        })
        .speech("b", |t| {
            t.cue("e", "explicit", 1000.0).with("m3", -5000.0);
        })
        .build()
}

#[test]
fn multi_pass_seeds_explicit_cues_scheduled_late() {
    let tl = late_explicit_timeline();
    let layout = tl.layout();
    assert_eq!(layout.passes, 2);
    assert_eq!(span(&tl, &layout, "e"), (3000, 4000));
    assert_eq!(span(&tl, &layout, "a1"), (0, 2000));
    // In pass 2, `e`'s pass-1 interval is an obstacle before `e` itself is scheduled.
    assert_eq!(at(&tl, &layout, "a2").pushed_from, Some(2000));
    assert_eq!(span(&tl, &layout, "a2"), (4250, 6250));
    assert_eq!(span(&tl, &layout, "a3"), (6250, 8250));
    let pushed = single(&layout, "cue_pushed");
    assert_contains(
        &pushed.message,
        &["`a2`", "by `e`", "previous scheduling pass"],
    );
    assert!(with_code(&layout, "speech_overlap").is_empty());
    assert_clean(&layout);
}

#[test]
fn an_explicit_cue_never_pushes_an_implicit_cue_it_depends_on() {
    // `b1` starts 3 s before `a3`, which follows `a1` and `a2`: pushing them past `b1` would only move `b1`
    // with them (the old behaviour chased it for four passes and inserted dead air). The layout keeps the first
    // pass and reports the overlaps.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 2000.0);
            t.cue("a2", "two", 2000.0);
            t.cue("a3", "three", 1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "chases a3", 1500.0).with("a3", -3000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, 1);
    assert_eq!(span(&tl, &layout, "a1"), (0, 2000));
    assert_eq!(span(&tl, &layout, "a3"), (4000, 5000));
    assert_eq!(span(&tl, &layout, "b1"), (1000, 2500));
    assert_eq!(layout.total_frames, 5000, "no dead air");
    assert!(with_code(&layout, "cue_pushed").is_empty());
    let overlaps = with_code(&layout, "speech_overlap");
    assert_eq!(overlaps.len(), 2, "{:#?}", layout.diagnostics);
    assert!(overlaps.iter().all(|d| d.severity == Severity::Warning));
    assert_contains(
        &overlaps[0].message,
        &["`b1`", "overlaps `a1`", "by 1.00 s"],
    );
    assert_contains(
        overlaps[0].hint.as_deref().unwrap_or_default(),
        &[
            "set `start_relative: {after_cue: a1, offset_ms: 250}` on `b1`",
            "`push` leaves `a1` in place because `b1` is anchored to it",
        ],
    );
    // b1 starts before a2 and depends on it: the hint ends b1 before a2 instead of anchoring a2 to b1.
    assert_contains(
        &overlaps[1].message,
        &["`a2`", "overlaps `b1`", "by 0.50 s"],
    );
    assert_contains(
        overlaps[1].hint.as_deref().unwrap_or_default(),
        &["set `start_relative: {before_cue: a2, offset_ms: -250}` on `b1`"],
    );
    assert_eq!(tl.layout(), layout);
}

#[test]
fn with_cue_interjections_keep_their_place() {
    // review-timing p1: a guest interjects 1 s into the host's line. The host's line is not pushed (that would
    // move the interjection with it): the overlap stays where the script put it and is reported.
    let tl = ms_builder()
        .speech("host", |t| {
            t.cue(
                "q1",
                "So tell me, what made you start this project?",
                4430.0,
            );
        })
        .speech("guest", |t| {
            t.cue("interrupt", "Honestly? Boredom.", 2190.0)
                .with("q1", 1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, 1);
    assert_eq!(span(&tl, &layout, "q1"), (0, 4430));
    assert_eq!(span(&tl, &layout, "interrupt"), (1000, 3190));
    assert_eq!(layout.total_frames, 4430);
    assert!(with_code(&layout, "cue_pushed").is_empty());
    let d = single(&layout, "speech_overlap");
    assert_eq!(d.severity, Severity::Warning);
    assert_contains(&d.message, &["`interrupt`", "overlaps `q1`", "by 2.19 s"]);
    let hint = d.hint.as_deref().unwrap_or_default();
    assert!(
        hint.starts_with("set `start_relative: {after_cue: q1, offset_ms: 250}` on `interrupt`"),
        "{hint}"
    );
    // Applying the hint removes the overlap.
    let fixed = ms_builder()
        .speech("host", |t| {
            t.cue(
                "q1",
                "So tell me, what made you start this project?",
                4430.0,
            );
        })
        .speech("guest", |t| {
            t.cue("interrupt", "Honestly? Boredom.", 2190.0)
                .after("q1", 250.0);
        })
        .build();
    let layout = fixed.layout();
    assert_clean(&layout);
    assert_eq!(span(&fixed, &layout, "interrupt"), (4680, 6870));
}

#[test]
fn eager_replies_keep_the_question_in_place() {
    // review-timing p2: the reply starts 300 ms before the question ends. The question stays at 0; the host's
    // next line, which follows a pause, makes way for the reply.
    let tl = ms_builder()
        .speech("host", |t| {
            t.cue("q1", "So what is the one thing you would change?", 2820.0);
            t.pause(400.0);
            t.cue("q2", "Interesting. Why that?", 2230.0);
        })
        .speech("guest", |t| {
            t.cue("a1", "The onboarding, without a doubt.", 2610.0)
                .after("q1", -300.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, 1);
    assert_eq!(span(&tl, &layout, "q1"), (0, 2820));
    assert_eq!(span(&tl, &layout, "a1"), (2520, 5130));
    assert_eq!(at(&tl, &layout, "q2").pushed_from, Some(3220));
    assert_eq!(
        span(&tl, &layout, "q2"),
        (5530, 7760),
        "end(a1) + the 400 ms pause"
    );
    assert_eq!(layout.total_frames, 7760);
    assert_contains(&single(&layout, "cue_pushed").message, &["`q2`", "by `a1`"]);
    assert_contains(
        &single(&layout, "speech_overlap").message,
        &["`a1`", "overlaps `q1`", "by 0.30 s"],
    );
}

#[test]
fn before_cue_interjections_keep_their_place() {
    // A back-channel ("Right.") that ends exactly when the host's next line starts overlaps the end of the
    // host's previous line; it depends on that line (through the next one), so pushing cannot help.
    let tl = ms_builder()
        .speech("host", |t| {
            t.cue("q1", "first line", 3000.0);
            t.cue("q2", "second line", 2000.0);
        })
        .speech("guest", |t| {
            t.cue("x", "Right.", 800.0).before("q2", 0.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, 1);
    assert_eq!(span(&tl, &layout, "q1"), (0, 3000));
    assert_eq!(span(&tl, &layout, "q2"), (3000, 5000));
    assert_eq!(span(&tl, &layout, "x"), (2200, 3000));
    assert!(with_code(&layout, "cue_pushed").is_empty());
    let d = single(&layout, "speech_overlap");
    assert_contains(&d.message, &["`x`", "overlaps `q1`", "by 0.80 s"]);
    assert_contains(
        d.hint.as_deref().unwrap_or_default(),
        &["`push` leaves `q1` in place because `x` is anchored to it"],
    );
}

#[test]
fn many_interjections_add_no_dead_air() {
    // review-timing bx200: 200 lines, each with a short interjection 300 ms in. The old multi-pass chase pushed
    // the first line to 580 s and doubled the length; now nothing moves.
    let tl = ms_builder()
        .speech("a", |t| {
            for i in 0..200 {
                t.cue(&format!("a{i}"), "alpha bravo charlie delta echo.", 2000.0);
            }
        })
        .speech("b", |t| {
            for i in 0..200 {
                t.cue(&format!("b{i}"), "yes.", 500.0)
                    .with(&format!("a{i}"), 300.0);
            }
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, 1);
    assert_eq!(layout.total_frames, 400_000);
    for i in [0_i64, 1, 99, 199] {
        assert_eq!(
            span(&tl, &layout, &format!("a{i}")),
            (2000 * i, 2000 * i + 2000)
        );
        assert_eq!(
            span(&tl, &layout, &format!("b{i}")),
            (2000 * i + 300, 2000 * i + 800)
        );
    }
    assert!(with_code(&layout, "cue_pushed").is_empty());
    let overlaps = with_code(&layout, "speech_overlap");
    assert_eq!(overlaps.len(), 51);
    assert_eq!(
        overlaps[50].message,
        "150 more speech overlaps are not listed individually"
    );
}

#[test]
fn the_best_pass_is_kept_when_passes_alternate() {
    // `b1` ends 1.6 s before `c1`, which `a1` pushes: `b1` does not depend on `a1`, so a re-run pushes `a1`
    // past `b1`, which moves `c1` and with it `b1`; the passes alternate between two layouts. The first one
    // is better (the same overlap, less displacement), so it is kept instead of the last.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "a", 3400.0);
        })
        .speech("c", |t| {
            t.cue("c1", "c", 3300.0);
        })
        .speech("b", |t| {
            t.cue("b1", "b", 600.0).before("c1", -1600.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.passes, MAX_SCHEDULING_PASSES);
    assert_eq!(span(&tl, &layout, "a1"), (0, 3400));
    assert_eq!(span(&tl, &layout, "c1"), (3650, 6950));
    assert_eq!(span(&tl, &layout, "b1"), (1450, 2050));
    assert_contains(&single(&layout, "cue_pushed").message, &["`c1`", "by `a1`"]);
    assert_contains(
        &single(&layout, "speech_overlap").message,
        &["`b1`", "overlaps `a1`", "by 0.60 s"],
    );
    assert_eq!(tl.layout(), layout);
}

#[test]
fn warn_policy_reports_without_moving() {
    let tl = ms_builder()
        .overlap_policy(OverlapPolicy::Warn)
        .speech("a", |t| {
            t.cue("a1", "first", 2000.0);
        })
        .speech("b", |t| {
            t.pause(500.0);
            t.cue("b1", "second", 2000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (500, 2500));
    assert!(with_code(&layout, "cue_pushed").is_empty());
    let d = single(&layout, "speech_overlap");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "speech cue `b1` (track `b`, 0.50-2.50 s) overlaps `a1` (track `a`, 0.00-2.00 s) by 1.50 s"
    );
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "set `start_relative: {after_cue: a1, offset_ms: 500}` on `b1`, or `allow_overlap: true` on either cue \
             for intentional crosstalk; `timing.speech_overlap: push` moves implicitly placed cues automatically"
        )
    );
    assert_eq!(d.path.as_deref(), Some("tracks[1].sequence[1]"));
    assert_eq!(d.related, ["a1", "b1"]);
    assert!(!layout.has_errors());
}

#[test]
fn error_policy_reports_errors() {
    let tl = ms_builder()
        .overlap_policy(OverlapPolicy::Error)
        .speech("a", |t| {
            t.cue("a1", "first", 2000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "second", 2000.0).at(1000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "speech_overlap");
    assert_eq!(d.severity, Severity::Error);
    assert_contains(
        &d.message,
        &["`b1`", "1.00-3.00 s", "`a1`", "0.00-2.00 s", "by 1.00 s"],
    );
    assert!(layout.has_errors());
    assert_eq!(span(&tl, &layout, "b1"), (1000, 3000));
}

#[test]
fn allow_policy_reports_nothing() {
    let tl = ms_builder()
        .overlap_policy(OverlapPolicy::Allow)
        .speech("a", |t| {
            t.cue("a1", "first", 2000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "second", 2000.0);
        })
        .build();
    let layout = tl.layout();
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
    assert_eq!(span(&tl, &layout, "b1"), (0, 2000));
}

#[test]
fn overlaps_within_the_tolerance_are_ignored() {
    let overlap_by = |pause_ms: f64, policy: OverlapPolicy, tolerance_ms: f64| {
        let tl = ms_builder()
            .overlap_policy(policy)
            .overlap_tolerance_ms(tolerance_ms)
            .speech("a", |t| {
                t.cue("a1", "first", 2000.0);
            })
            .speech("b", |t| {
                t.pause(pause_ms);
                t.cue("b1", "second", 1000.0);
            })
            .build();
        let layout = tl.layout();
        (
            span(&tl, &layout, "b1"),
            with_code(&layout, "speech_overlap").len(),
        )
    };
    // 15 ms overlap, 20 ms tolerance: fine.
    assert_eq!(
        overlap_by(1985.0, OverlapPolicy::Warn, 20.0),
        ((1985, 2985), 0)
    );
    assert_eq!(
        overlap_by(1985.0, OverlapPolicy::Push, 20.0),
        ((1985, 2985), 0)
    );
    // Exactly the tolerance is still fine; one frame more is an overlap.
    assert_eq!(
        overlap_by(1980.0, OverlapPolicy::Warn, 20.0),
        ((1980, 2980), 0)
    );
    assert_eq!(
        overlap_by(1979.0, OverlapPolicy::Warn, 20.0),
        ((1979, 2979), 1)
    );
    // Pushed past a1 with the preceding pause as the gap.
    assert_eq!(
        overlap_by(1975.0, OverlapPolicy::Push, 20.0),
        ((3975, 4975), 0)
    );
    // Zero tolerance: a single shared frame counts.
    assert_eq!(
        overlap_by(1999.0, OverlapPolicy::Warn, 0.0),
        ((1999, 2999), 1)
    );
    assert_eq!(
        overlap_by(2000.0, OverlapPolicy::Warn, 0.0),
        ((2000, 3000), 0)
    );
}

#[test]
fn allow_overlap_exempts_a_cue_from_pushing_and_reporting() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "short", 500.0);
        })
        .speech("b", |t| {
            t.cue("b1", "crosstalk", 3000.0).allow_overlap();
        })
        .speech("c", |t| {
            t.pause(1000.0);
            t.cue("c1", "overlaps only the crosstalk", 500.0);
        })
        .build();
    let layout = tl.layout();
    // b1 overlaps a1 but is exempt; c1 overlaps only b1, which is not an obstacle.
    assert_eq!(span(&tl, &layout, "b1"), (0, 3000));
    assert_eq!(span(&tl, &layout, "c1"), (1000, 1500));
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);

    // Under `warn`, nothing is reported either.
    let mut warn = tl.clone();
    warn.program.timing.speech_overlap = OverlapPolicy::Warn;
    assert!(warn.layout().diagnostics.is_empty());
}

#[test]
fn muted_speech_tracks_take_part_in_overlap_handling() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.mute();
            t.cue("a1", "muted", 1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "audible", 500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (1250, 1750));
}

#[test]
fn before_cue_ends_the_item_at_the_anchor() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "anchor", 1000.0).at(5000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "lead-in", 1500.0).before("a1", -200.0);
        })
        .sfx("fx", |t| {
            t.clip("riser", "riser.wav", 2000.0).before("a1", 0.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (3300, 4800));
    assert_eq!(span(&tl, &layout, "riser"), (3000, 5000));
    assert_clean(&layout);
}

#[test]
fn with_cue_negative_offsets_and_negative_start_clamp() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "anchor", 2000.0).at(1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "early", 500.0)
                .with("a1", -300.0)
                .allow_overlap();
        })
        .speech("c", |t| {
            t.cue("c1", "too early", 200.0).with("a1", -1500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (700, 1200));
    assert_eq!(span(&tl, &layout, "c1"), (0, 200));
    let d = single(&layout, "negative_start");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "`c1` would start at -0.50 s (`with_cue: a1`, `offset_ms: -1500`); it was clamped to 0.00 s"
    );
    assert_eq!(
        d.hint.as_deref(),
        Some("use `offset_ms: -1000` or greater on `with_cue: a1`")
    );
    assert_eq!(
        d.path.as_deref(),
        Some("tracks[2].sequence[0].start_relative")
    );
}

#[test]
fn anchors_on_tracks_use_their_first_and_last_items() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.start_at(1000.0);
            t.cue("a1", "one", 1000.0);
            t.pause(500.0);
            t.cue("a2", "two", 1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "trailing pause", 400.0).at(6000.0);
            t.pause(700.0);
        })
        .sfx("fx", |t| {
            t.clip("hit", "hit.wav", 300.0).after("a", 100.0);
            t.clip("pre", "pre.wav", 400.0).before("a", 0.0);
            t.clip("mid", "mid.wav", 200.0).with("a", 250.0);
            t.clip("tail", "tail.wav", 100.0).after("b", 0.0);
        })
        .build();
    let layout = tl.layout();
    let a = tl.track("a");
    let span_a = layout.track_span(a).copied();
    assert_eq!(
        span_a.map(|s| (s.start, s.end, s.open_ended)),
        Some((1000, 3500, false))
    );
    assert_eq!(span(&tl, &layout, "hit"), (3600, 3900));
    assert_eq!(span(&tl, &layout, "pre"), (600, 1000));
    assert_eq!(span(&tl, &layout, "mid"), (1250, 1450));
    // A track's span ends with its last item, here a pause.
    assert_eq!(span(&tl, &layout, "tail"), (7100, 7200));
    assert_eq!(layout.track_span(tl.track("b")).map(|s| s.end), Some(7100));
    assert_clean(&layout);
}

#[test]
fn track_start_rules_set_the_initial_cursor() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
        })
        .speech("b", |t| {
            t.start_after("a1", 500.0);
            t.cue("b1", "two", 1000.0);
            t.cue("b2", "three", 1000.0);
        })
        .speech("c", |t| {
            t.start_at(4000.0);
            t.pause(100.0);
            t.cue("c1", "four", 500.0);
        })
        .speech("d", |t| {
            t.start_before("c1", -100.0);
            t.cue("d1", "five", 500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (1500, 2500));
    assert_eq!(span(&tl, &layout, "b2"), (2500, 3500));
    assert_eq!(span(&tl, &layout, "c1"), (4100, 4600));
    assert_eq!(
        span(&tl, &layout, "d1"),
        (3500, 4000),
        "ends 100 ms before c1 starts"
    );
    assert_clean(&layout);
}

#[test]
fn negative_track_start_is_clamped() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
        })
        .speech("b", |t| {
            t.start_with("a1", -500.0);
            t.cue("b1", "two", 300.0).allow_overlap();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "b1"), (0, 300));
    let d = single(&layout, "negative_start");
    assert_contains(
        &d.message,
        &["track `b` would start at -0.50 s", "first item `b1`"],
    );
    assert_eq!(d.path.as_deref(), Some("tracks[1].start_relative"));
    assert_eq!(
        d.hint.as_deref(),
        Some("use `offset_ms: 0` or greater on `with_cue: a1`")
    );
}

#[test]
fn the_cursor_follows_explicit_items() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
            t.cue("a2", "jump", 500.0).at(5000.0);
            t.pause(200.0);
            t.cue("a3", "after the jump", 500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "a3"), (5700, 6200));
    assert_clean(&layout);
}

#[test]
fn overlap_hints_never_suggest_a_cycle() {
    // a1 is anchored to b1; anchoring b1 after a1 would be a cycle, so the hint ends a1 before b1 instead.
    let tl = ms_builder()
        .overlap_policy(OverlapPolicy::Warn)
        .speech("a", |t| {
            t.cue("a1", "depends on b1", 2000.0).with("b1", -1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "anchor", 1000.0).at(2000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "speech_overlap");
    assert_contains(
        &d.message,
        &["speech cue `b1`", "overlaps `a1`", "by 1.00 s"],
    );
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h
                .starts_with("set `start_relative: {before_cue: b1, offset_ms: -250}` on `a1`")),
        "{d:#?}"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].sequence[0]"));
    // Both cues are explicit: `push` cannot help, so the hint does not mention it.
    assert!(
        !d.hint
            .as_deref()
            .unwrap_or_default()
            .contains("speech_overlap: push")
    );
}

#[test]
fn equal_starts_suggest_moving_the_later_declared_cue() {
    let tl = ms_builder()
        .overlap_policy(OverlapPolicy::Warn)
        .speech("a", |t| {
            t.cue("a1", "long", 2000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "short", 1000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "speech_overlap");
    assert_contains(&d.message, &["speech cue `b1`", "overlaps `a1`"]);
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h.contains("{after_cue: a1, offset_ms: 250}` on `b1`"))
    );
}

#[test]
fn dependency_cycles_are_reported_and_broken() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0).after("b1", 0.0);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0).after("a1", 0.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "dependency_cycle");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(
        d.message,
        "dependency cycle `a1` → `b1` → `a1`; the layout ignored `tracks[0].sequence[0].start_relative` to break \
         it (`a1` is placed sequentially)"
    );
    assert_eq!(
        d.path.as_deref(),
        Some("tracks[0].sequence[0].start_relative")
    );
    assert_eq!(d.related, ["a1", "b1"]);
    // Only a1 lost its anchor: b1 still follows it.
    assert_eq!(span(&tl, &layout, "a1"), (0, 1000));
    assert_eq!(span(&tl, &layout, "b1"), (1000, 2000));
    assert!(layout.has_errors());
}

#[test]
fn longer_cycles_and_their_dependents_are_still_placed() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0).after("c1", 100.0);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0).after("a1", 100.0);
        })
        .speech("c", |t| {
            t.cue("c1", "three", 1000.0).after("b1", 100.0);
            t.cue("c2", "depends on the cycle", 1000.0);
        })
        .speech("d", |t| {
            t.cue("d1", "outside", 500.0).after("c2", 0.0);
        })
        .build();
    let layout = tl.layout();
    assert_contains(
        &single(&layout, "dependency_cycle").message,
        &["`a1` → `c1` → `b1` → `a1`"],
    );
    assert_eq!(span(&tl, &layout, "a1"), (0, 1000));
    assert_eq!(span(&tl, &layout, "b1"), (1100, 2100));
    assert_eq!(span(&tl, &layout, "c1"), (2200, 3200));
    assert_eq!(span(&tl, &layout, "c2"), (3200, 4200));
    assert_eq!(span(&tl, &layout, "d1"), (4200, 4700));
}

#[test]
fn self_references_are_reported() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a0", "first", 500.0);
            t.cue("a1", "itself", 1000.0).with("a1", 0.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "self_reference");
    assert_eq!(d.severity, Severity::Error);
    assert_contains(
        &d.message,
        &[
            "`a1` is anchored to itself",
            "`tracks[0].sequence[1].start_relative`",
        ],
    );
    assert_eq!(span(&tl, &layout, "a1"), (500, 1500));
}

#[test]
fn cycles_through_track_starts_and_clip_ends_are_broken() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.start_after("b1", 0.0);
            t.cue("a1", "one", 1000.0);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0).after("a1", 0.0);
        })
        .music("m", |t| {
            t.clip("c1", "long.wav", 10_000.0).end_after("c2", 0.0);
            t.clip("c2", "short.wav", 2000.0).after("c1", 0.0);
        })
        .build();
    let layout = tl.layout();
    let cycles = with_code(&layout, "dependency_cycle");
    assert_eq!(cycles.len(), 2, "{:#?}", layout.diagnostics);
    assert_contains(
        &cycles[0].message,
        &["`tracks[0].start_relative`", "track `a` starts at 0 s"],
    );
    assert_contains(
        &cycles[1].message,
        &[
            "`tracks[2].clips[0].end_relative`",
            "`c1` plays its natural length",
        ],
    );
    assert_eq!(span(&tl, &layout, "a1"), (0, 1000));
    assert_eq!(span(&tl, &layout, "b1"), (1000, 2000));
    assert_eq!(span(&tl, &layout, "c1"), (0, 10_000));
    assert_eq!(span(&tl, &layout, "c2"), (10_000, 12_000));
}

#[test]
fn out_of_range_references_resolve_to_zero() {
    let mut tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0).at(3000.0);
            t.cue("a2", "ghost anchor", 1000.0);
        })
        .build();
    let ghost = AnchorRule {
        target: AnchorTarget::Node(999),
        target_id: "ghost".to_string(),
        mode: AnchorMode::After,
        offset_ms: 100.0,
    };
    tl.program.nodes[1].start = StartRule::Relative(ghost);
    let layout = tl.layout();
    let d = single(&layout, "unknown_reference");
    assert_eq!(d.severity, Severity::Error);
    assert_contains(&d.message, &["`a2`", "`ghost`"]);
    assert_eq!(span(&tl, &layout, "a2"), (100, 1100));
}

#[test]
fn pause_lengths_come_from_the_ir_not_the_table() {
    let mut tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
            t.pause(350.0);
            t.cue("a2", "two", 1000.0);
        })
        .build();
    tl.set_duration("a.pause1", NodeDuration::estimate(99_999));
    let layout = tl.layout();
    let pause = at(&tl, &layout, "a.pause1");
    assert_eq!(
        (pause.start, pause.length, pause.duration_source),
        (1000, 350, DurationSource::Fixed)
    );
    assert_eq!(span(&tl, &layout, "a2"), (1350, 2350));
    assert!(!layout.estimated());
}

#[test]
fn missing_duration_entries_are_zero_length_estimates() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0);
            t.cue("a2", "two", 1000.0);
        })
        .music("m", |t| {
            t.clip("bed", "bed.wav", 5000.0).length(3000.0);
        })
        .build();
    let layout = layout(&tl.program, &tl.durations[..1]);
    assert_eq!(layout.placements.len(), 3);
    assert_eq!(span(&tl, &layout, "a1"), (0, 1000));
    assert_eq!(span(&tl, &layout, "a2"), (1000, 1000));
    assert_eq!(
        at(&tl, &layout, "a2").duration_source,
        DurationSource::Estimate
    );
    // An unknown source length honours the requested clip length.
    assert_eq!(span(&tl, &layout, "bed"), (0, 3000));
    assert!(layout.estimated());
    assert_eq!(layout.estimated_count(), 1);
    assert_clean(&layout);
}

#[test]
fn durations_report_their_sources() {
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("exact", "one", 1000.0)
                .duration_source(DurationSource::Exact);
            t.cue("cached", "two", 1000.0)
                .duration_source(DurationSource::Cache);
            t.cue("guess", "three", 1000.0)
                .duration_source(DurationSource::Estimate);
        })
        .build();
    let layout = tl.layout();
    let sources: Vec<DurationSource> = layout
        .placements
        .iter()
        .map(|p| p.duration_source)
        .collect();
    assert_eq!(
        sources,
        [
            DurationSource::Exact,
            DurationSource::Cache,
            DurationSource::Estimate
        ]
    );
    assert!(layout.estimated());
}

#[test]
fn frames_are_rounded_at_the_output_rate() {
    let tl = ProgramBuilder::new()
        .sample_rate(44_100)
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0).at(0.5);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0).after("a1", 150.0);
        })
        .build();
    let layout = tl.layout();
    // round(0.5 × 44.1) = 22; 1000 ms = 44100 frames; round(150 × 44.1) = 6615.
    assert_eq!(span(&tl, &layout, "a1"), (22, 44_122));
    assert_eq!(span(&tl, &layout, "b1"), (50_737, 94_837));
}

#[test]
fn interval_helpers() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let cues = layout.speech_cue_intervals();
    let ids: Vec<&str> = cues
        .iter()
        .map(|(v, _)| tl.program.nodes[*v].id.as_str())
        .collect();
    assert_eq!(ids, ["cue_01", "cue_02", "cue_03"]);
    assert_eq!(cues[1].1, Interval::new(103_200, 252_000));
    // Pauses are not part of a track's audible intervals.
    assert_eq!(
        layout.track_intervals(tl.track("host")),
        [Interval::new(0, 96_000), Interval::new(268_800, 393_600)]
    );
    assert_eq!(
        soniccraft_layout::merge_intervals([
            Interval::new(5, 9),
            Interval::new(0, 3),
            Interval::new(3, 4),
            Interval::new(8, 12),
            Interval::new(20, 20),
        ]),
        [Interval::new(0, 4), Interval::new(5, 12)]
    );
    let iv = Interval::new(10, 5);
    assert!(iv.is_empty());
    assert_eq!(
        (iv.start, iv.end, iv.len()),
        (10, 10, 0),
        "an end before the start is clamped"
    );
    assert_eq!(Interval::new(0, 10).overlap(&Interval::new(5, 20)), 5);
    assert!(!Interval::new(0, 10).intersects(&Interval::new(10, 20)));
    assert!(Interval::new(0, 10).contains(9) && !Interval::new(0, 10).contains(10));
}

#[test]
fn track_anchors_use_declaration_order_and_warn_when_items_are_out_of_order() {
    // review-timing a2: the host's sequence declares the outro (at 6 s) before the intro (at 0 s).
    let timeline = |outro_first: bool| {
        ms_builder()
            .speech("host", |t| {
                if outro_first {
                    t.cue("outro", "And that is all for today.", 2200.0)
                        .at(6000.0);
                    t.cue("intro", "Welcome to the show.", 1620.0).at(0.0);
                } else {
                    t.cue("intro", "Welcome to the show.", 1620.0).at(0.0);
                    t.cue("outro", "And that is all for today.", 2200.0)
                        .at(6000.0);
                }
            })
            .sfx("closing_sting", |t| {
                t.start_after("host", 0.0);
                t.shorthand("chime.wav", 1500.0);
            })
            .sfx("opening_sting", |t| {
                t.start_with("host", 0.0);
                t.shorthand("impact.wav", 2000.0);
            })
            .build()
    };
    let tl = timeline(true);
    let layout = tl.layout();
    // The anchors still resolve to the first and last declared items...
    assert_eq!(span(&tl, &layout, "closing_sting"), (1620, 3120));
    assert_eq!(span(&tl, &layout, "opening_sting"), (6000, 8000));
    // ...and say so.
    let warnings = with_code(&layout, "track_anchor_out_of_order");
    assert_eq!(warnings.len(), 2, "{:#?}", layout.diagnostics);
    assert!(warnings.iter().all(|d| d.severity == Severity::Warning));
    assert_eq!(
        warnings[0].message,
        "track `closing_sting` is anchored with `after_cue: host`, which resolves to the end of `intro` (1.62 s), \
         the last item declared on track `host`; but `outro` on that track ends later, at 8.20 s"
    );
    assert_eq!(
        warnings[0].path.as_deref(),
        Some("tracks[1].start_relative.after_cue")
    );
    assert_eq!(
        warnings[0].hint.as_deref(),
        Some(
            "to start after `outro`, anchor to it instead (`after_cue: outro`), or reorder the items of track \
             `host` so that the one that ends last is declared last"
        )
    );
    assert_eq!(
        warnings[0].related,
        ["closing_sting", "host", "intro", "outro"]
    );
    assert_eq!(
        warnings[1].message,
        "track `opening_sting` is anchored with `with_cue: host`, which resolves to the start of `outro` (6.00 s), \
         the first item declared on track `host`; but `intro` on that track starts earlier, at 0.00 s"
    );
    assert_eq!(
        warnings[1].path.as_deref(),
        Some("tracks[2].start_relative.with_cue")
    );
    assert_eq!(
        warnings[1].hint.as_deref(),
        Some(
            "to anchor to `intro`, use `with_cue: intro` instead, or reorder the items of track `host` so that the \
             one that starts first is declared first"
        )
    );

    // Declared in time order: the same anchors mean the track's span, and nothing is reported.
    let tl = timeline(false);
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "closing_sting"), (8200, 9700));
    assert_eq!(span(&tl, &layout, "opening_sting"), (0, 2000));
    assert_clean(&layout);
}

#[test]
fn track_anchors_on_clip_ends_and_overlapping_items_warn() {
    // review-timing ts1: a bed meant to span the stings, whose clips are declared out of order.
    let tl = ms_builder()
        .sfx("stings", |t| {
            t.clip("outro_sting", "chime.wav", 1500.0).at(9000.0);
            t.clip("intro_sting", "impact.wav", 2000.0).at(0.0);
        })
        .music("bed", |t| {
            t.start_with("stings", 0.0);
            t.shorthand("loop.wav", 8000.0)
                .looped()
                .end_after("stings", 500.0)
                .fade_out(400.0);
        })
        .build();
    let layout = tl.layout();
    let warnings = with_code(&layout, "track_anchor_out_of_order");
    assert_eq!(warnings.len(), 2, "{:#?}", layout.diagnostics);
    assert_eq!(
        warnings[0].path.as_deref(),
        Some("tracks[1].end_relative.after_cue")
    );
    assert_contains(
        &warnings[0].message,
        &[
            "the end of `bed` is anchored with `after_cue: stings`",
            "the end of `intro_sting` (2.00 s)",
            "`outro_sting` on that track ends later, at 10.50 s",
        ],
    );
    assert!(
        warnings[0]
            .hint
            .as_deref()
            .is_some_and(|h| h.starts_with("to end after `outro_sting`, anchor to it instead"))
    );
    assert_eq!(
        warnings[1].path.as_deref(),
        Some("tracks[1].start_relative.with_cue")
    );
    // The inverted span is still an error of its own.
    assert_contains(
        &single(&layout, "invalid_value").message,
        &["clip `bed` would end at 2.50 s, before it starts at 9.00 s"],
    );

    // review-timing track_span: an explicit early item declared last.
    let tl = ms_builder()
        .speech("voice", |t| {
            t.cue("long_one", "This is a fairly long sentence.", 5020.0);
            t.cue("early", "Short.", 500.0).at(0.0).allow_overlap();
        })
        .sfx("sting", |t| {
            t.start_after("voice", 0.0);
            t.shorthand("chime.wav", 1500.0);
        })
        .music("bed", |t| {
            t.start_with("voice", 0.0);
            t.shorthand("impact.wav", 2000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "sting"), (500, 2000));
    let d = single(&layout, "track_anchor_out_of_order");
    assert_contains(
        &d.message,
        &[
            "`after_cue: voice`",
            "end of `early` (0.50 s)",
            "`long_one` on that track ends later, at 5.02 s",
        ],
    );
    // `with_cue: voice` is fine: no item starts before `long_one`.
    assert_eq!(
        d.path.as_deref(),
        Some("tracks[1].start_relative.after_cue")
    );
}

#[test]
fn overlap_hints_converge_when_a_before_cue_item_does_not_fit() {
    // review-timing hintfix: `a1` (3.28 s) should end 0.5 s after `b1` starts at 1 s, so it is clamped to 0
    // and still overlaps. A larger or smaller `offset_ms` alone cannot help: `b1` has to move.
    let tl = ms_builder()
        .speech("A", |t| {
            t.cue("a1", "This is a rather long sentence spoken first.", 3280.0)
                .before("b1", 500.0);
        })
        .speech("B", |t| {
            t.cue("b1", "Second speaker.", 1290.0).at(1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "a1"), (0, 3280));
    let overlap = single(&layout, "speech_overlap");
    assert_eq!(
        overlap.hint.as_deref(),
        Some(
            "set `start_relative: {before_cue: b1, offset_ms: -250}` on `a1` and move `b1` to start at 3530 ms or \
             later (`start_at_ms: 3530`), so that `a1` (3.28 s) fits before it, or `allow_overlap: true` on either \
             cue for intentional crosstalk"
        )
    );
    let negative = single(&layout, "negative_start");
    assert_eq!(
        negative.message,
        "`a1` would start at -1.78 s (`before_cue: b1`, `offset_ms: 500`: it lasts 3.28 s and has to end at 1.50 s); \
         it was clamped to 0.00 s"
    );
    assert_eq!(
        negative.hint.as_deref(),
        Some(
            "make `b1` start at 2780 ms or later (for example `start_at_ms: 2780`; it starts at 1.00 s) so that `a1` \
             (3.28 s) fits before it, or shorten `a1` to at most 1500 ms"
        )
    );
    // Applying the overlap hint literally removes the overlap and the clamp.
    let fixed = ms_builder()
        .speech("A", |t| {
            t.cue("a1", "This is a rather long sentence spoken first.", 3280.0)
                .before("b1", -250.0);
        })
        .speech("B", |t| {
            t.cue("b1", "Second speaker.", 1290.0).at(3530.0);
        })
        .build();
    let layout = fixed.layout();
    assert_clean(&layout);
    assert_eq!(span(&fixed, &layout, "a1"), (0, 3280));

    // A clip placed with `before_cue` is told about `duration_ms`.
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("hit", "x", 1000.0).at(500.0);
        })
        .sfx("fx", |t| {
            t.clip("riser", "riser.wav", 2000.0).before("hit", 0.0);
        })
        .build();
    let hint = single(&tl.layout(), "negative_start").hint.clone();
    assert_eq!(
        hint.as_deref(),
        Some(
            "make `hit` start at 2000 ms or later (for example `start_at_ms: 2000`; it starts at 0.50 s) so that \
             `riser` (2.00 s) fits before it, or give `riser` a `duration_ms` of at most 500"
        )
    );
}

#[test]
fn layout_diagnostics_say_whether_they_depend_on_estimates() {
    // review-timing#2 estflip2: a sting anchored after an estimated cue, with an absolute end.
    let timeline = |source: DurationSource| {
        ms_builder()
            .speech("v", |t| {
                t.cue("c1", "Call 1234567890 now.", 3000.0)
                    .duration_source(source);
            })
            .sfx("sting", |t| {
                t.clip("s1", "chime.wav", 1500.0)
                    .after("c1", 0.0)
                    .end_at(2500.0);
            })
            .build()
    };
    let flags = |layout: &soniccraft_layout::Layout, code: &str| -> Vec<bool> {
        layout
            .diagnostics_with_estimates()
            .filter(|(d, _)| d.code == code)
            .map(|(_, estimated)| estimated)
            .collect()
    };
    let estimated = timeline(DurationSource::Estimate).layout();
    assert_eq!(
        estimated.estimate_dependent.len(),
        estimated.diagnostics.len()
    );
    assert_eq!(flags(&estimated, "invalid_value"), [true]);
    assert!(estimated.has_errors() && !estimated.has_certain_errors());
    let cached = timeline(DurationSource::Cache).layout();
    assert_eq!(flags(&cached, "invalid_value"), [false]);
    assert!(cached.has_certain_errors());

    // Static problems never depend on estimates.
    let tl = ms_builder()
        .speech("a", |t| {
            t.cue("a1", "one", 1000.0)
                .after("b1", 0.0)
                .duration_source(DurationSource::Estimate);
        })
        .speech("b", |t| {
            t.cue("b1", "two", 1000.0)
                .after("a1", 0.0)
                .duration_source(DurationSource::Estimate);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(flags(&layout, "dependency_cycle"), [false]);
    assert!(layout.has_certain_errors());

    // Overlaps under the `error` policy: certain between exact cues at fixed times, estimate-dependent as soon
    // as one length is estimated.
    let overlap = |source: DurationSource| {
        ms_builder()
            .overlap_policy(OverlapPolicy::Error)
            .speech("a", |t| {
                t.cue("a1", "first", 2000.0).at(0.0).duration_source(source);
            })
            .speech("b", |t| {
                t.cue("b1", "second", 2000.0).at(1000.0);
            })
            .build()
            .layout()
    };
    assert_eq!(
        flags(&overlap(DurationSource::Exact), "speech_overlap"),
        [false]
    );
    assert_eq!(
        flags(&overlap(DurationSource::Estimate), "speech_overlap"),
        [true]
    );

    // A push among estimated speech depends on estimates; among exact speech it does not.
    let push = |source: DurationSource| {
        ms_builder()
            .speech("a", |t| {
                t.cue("a1", "first", 2000.0).duration_source(source);
            })
            .speech("b", |t| {
                t.cue("b1", "second", 1000.0);
            })
            .build()
            .layout()
    };
    assert_eq!(flags(&push(DurationSource::Cache), "cue_pushed"), [false]);
    assert_eq!(flags(&push(DurationSource::Estimate), "cue_pushed"), [true]);
}

/// A timeline exercising most rules at once.
fn busy_timeline() -> TestTimeline {
    ProgramBuilder::new()
        .sample_rate(44_100)
        .title("Busy")
        .tail_ms(750.0)
        .speech("host", |t| {
            t.cue("h1", "Hello", 1234.5);
            t.pause(333.0);
            t.cue("h2", "And now", 2100.0);
            t.cue("h3", "Back", 900.0).after("g1", 120.0);
            t.auto_cue("Closing words", 1800.0).speaker("host");
        })
        .speech("guest", |t| {
            t.cue("g1", "Hi", 1500.0).with("h2", 300.0).allow_overlap();
            t.pause(100.0);
            t.auto_cue("Thanks", 700.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 9000.0)
                .looped()
                .fade_in(500.0)
                .fade_out(1500.0);
            t.ducked_by(&["host", "guest"], 12.0);
        })
        .sfx("fx", |t| {
            t.clip("sting", "sting.wav", 1200.0)
                .before("h3", 50.0)
                .fade_out(4000.0);
            t.clip("swoosh", "swoosh.wav", 800.0).crossfade(300.0);
        })
        .build()
}

#[test]
fn layout_is_deterministic() {
    let tl = busy_timeline();
    let first = tl.layout();
    for _ in 0..5 {
        assert_eq!(tl.layout(), first);
    }
    let json = serde_json::to_string(&first).expect("layout serializes");
    assert_eq!(
        serde_json::to_string(&tl.layout()).expect("layout serializes"),
        json
    );
    let back: soniccraft_layout::Layout = serde_json::from_str(&json).expect("layout deserializes");
    assert_eq!(back, first);
    // Every node is placed exactly once, in node order.
    assert!(
        first
            .placements
            .iter()
            .enumerate()
            .all(|(i, p)| p.node == i)
    );
    assert_eq!(first.placements.len(), tl.program.nodes.len());
    assert_eq!(first.track_spans.len(), tl.program.tracks.len());
}
