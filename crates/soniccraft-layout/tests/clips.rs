//! Clip lengths, loops, crossfades, fades and the timeline end (DESIGN.md §3.3, §3.5).
//!
//! All timelines use a 1000 Hz sample rate: one frame is one millisecond.

mod common;

use common::{assert_clean, assert_contains, at, codes, single, span, with_code};
use soniccraft_core::Severity;
use soniccraft_core::ir::FadeCurve;
use soniccraft_layout::DurationSource;
use soniccraft_layout::test_support::ProgramBuilder;

fn ms_builder() -> ProgramBuilder {
    ProgramBuilder::new().sample_rate(1000)
}

#[test]
fn natural_length_is_the_source_minus_the_trim() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c", "a.wav", 10_000.0).trim_start(2500.0);
        })
        .build();
    let layout = tl.layout();
    let c = at(&tl, &layout, "c");
    assert_eq!((c.start, c.end(), c.trim_start), (0, 7500, 2500));
    assert_eq!(c.duration_source, DurationSource::Source);
    assert!(!c.open_ended && !c.looped);
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
}

#[test]
fn trim_start_beyond_the_source_is_an_error() {
    // (A tail keeps the timeline from being empty, which would be an error of its own.)
    let tl = ms_builder()
        .tail_ms(500.0)
        .music("m", |t| {
            t.clip("past", "a.wav", 10_000.0).trim_start(12_000.0);
            t.clip("at_end", "b.wav", 10_000.0).trim_start(10_000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "invalid_value");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(
        d.message,
        "`trim_start_ms` of clip `past` (12.00 s) is beyond the end of its source `a.wav` (10.00 s)"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[0].trim_start_ms"));
    assert_eq!(
        d.hint.as_deref(),
        Some("use a `trim_start_ms` below 10000 (the source length in ms)")
    );
    assert_eq!(span(&tl, &layout, "past"), (0, 0));
    // Trimming exactly to the end leaves nothing to play, which is not an error for a non-looping clip.
    assert_eq!(span(&tl, &layout, "at_end"), (0, 0));
}

#[test]
fn unknown_source_lengths_are_not_checked_or_cut() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("trimmed", "missing.wav", 0.0)
                .trim_start(12_000.0)
                .duration_source(DurationSource::Estimate);
        })
        .music("n", |t| {
            t.clip("requested", "missing.wav", 0.0)
                .length(5000.0)
                .duration_source(DurationSource::Estimate);
        })
        .build();
    let layout = tl.layout();
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
    assert_eq!(span(&tl, &layout, "requested"), (0, 5000));
    assert_eq!(
        at(&tl, &layout, "requested").duration_source,
        DurationSource::Fixed
    );
    assert_eq!(
        at(&tl, &layout, "trimmed").duration_source,
        DurationSource::Estimate
    );
}

#[test]
fn duration_ms_rules() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("short", "a.wav", 10_000.0).length(4000.0);
            t.clip("long", "b.wav", 10_000.0).length(15_000.0);
            t.clip("looped", "c.wav", 10_000.0)
                .length(25_000.0)
                .looped();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "short"), (0, 4000));
    assert_eq!(span(&tl, &layout, "long"), (4000, 14_000));
    assert_eq!(span(&tl, &layout, "looped"), (14_000, 39_000));
    assert_eq!(
        at(&tl, &layout, "short").duration_source,
        DurationSource::Fixed
    );
    assert_eq!(
        at(&tl, &layout, "long").duration_source,
        DurationSource::Source
    );
    assert_eq!(
        at(&tl, &layout, "looped").duration_source,
        DurationSource::Fixed
    );
    assert!(!at(&tl, &layout, "looped").open_ended);
    let d = single(&layout, "clip_shorter_than_requested");
    assert_eq!(d.severity, Severity::Info);
    assert_eq!(
        d.message,
        "clip `long` requests 15.00 s (`duration_ms`), but its source `b.wav` provides only 10.00 s; it plays 10.00 s"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[1].duration_ms"));
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "set `loop: true` on `long` to fill the requested length, or request at most 10000 ms"
        )
    );
    assert_eq!(layout.content_end_frames, 39_000);
}

#[test]
fn end_at_ms_rules() {
    let tl = ms_builder()
        .music("a", |t| {
            t.clip("fits", "a.wav", 10_000.0).at(1000.0).end_at(5000.0);
        })
        .music("b", |t| {
            t.clip("cut", "b.wav", 3000.0)
                .trim_start(500.0)
                .at(6000.0)
                .end_at(12_000.0);
        })
        .music("c", |t| {
            t.clip("looped", "c.wav", 3000.0)
                .at(6000.0)
                .end_at(12_000.0)
                .looped();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "fits"), (1000, 5000));
    assert_eq!(span(&tl, &layout, "cut"), (6000, 8500));
    assert_eq!(span(&tl, &layout, "looped"), (6000, 12_000));
    let d = single(&layout, "clip_shorter_than_requested");
    assert_contains(
        &d.message,
        &[
            "`cut` requests 6.00 s (`end_at_ms`)",
            "only 2.50 s after `trim_start_ms` (0.50 s)",
        ],
    );
    assert_eq!(d.path.as_deref(), Some("tracks[1].clips[0].end_at_ms"));
}

#[test]
fn end_relative_rules() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 3000.0).at(500.0);
        })
        .music("m", |t| {
            t.clip("bed", "bed.wav", 60_000.0).end_after("c1", 2000.0);
        })
        .music("n", |t| {
            t.clip("intro", "intro.wav", 60_000.0)
                .end_with("c1", 1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "bed"), (0, 5500));
    assert_eq!(span(&tl, &layout, "intro"), (0, 1500));
    assert_clean(&layout);
}

#[test]
fn an_end_before_the_start_is_an_error() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("x", "x.wav", 10_000.0)
                .at(5000.0)
                .end_at(3000.0)
                .fade_in(500.0);
        })
        .music("n", |t| {
            t.clip("empty", "y.wav", 10_000.0).at(2000.0).end_at(2000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "invalid_value");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(
        d.message,
        "clip `x` would end at 3.00 s, before it starts at 5.00 s (`end_at_ms`); it plays nothing"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[0].end_at_ms"));
    let x = at(&tl, &layout, "x");
    assert_eq!((x.start, x.length, x.fade_in), (5000, 0, 0));
    // An end equal to the start is an empty clip, not an error; no fade diagnostics for empty clips.
    assert_eq!(span(&tl, &layout, "empty"), (2000, 2000));
    assert!(with_code(&layout, "fades_scaled").is_empty());
}

#[test]
fn open_ended_beds_run_to_the_end_and_set_the_tail() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 3000.0).looped().fade_out(2000.0);
        })
        .build();
    let layout = tl.layout();
    // The bed does not contribute to the content end; its fade-out becomes the tail.
    assert_eq!(
        (
            layout.content_end_frames,
            layout.tail_frames,
            layout.total_frames
        ),
        (5000, 2000, 7000)
    );
    let bed = at(&tl, &layout, "bed");
    assert!(bed.open_ended && bed.looped);
    assert_eq!(
        (bed.start, bed.end(), bed.fade_out),
        (0, 7000, 2000),
        "the fade-out ends at the total"
    );
    assert_eq!(bed.duration_source, DurationSource::Fixed);
    let bed_span = layout.track_span(tl.track("bed")).copied();
    assert_eq!(
        bed_span.map(|s| (s.start, s.end, s.open_ended)),
        Some((0, 7000, true))
    );
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
}

#[test]
fn open_ended_beds_without_a_fade_out_get_an_info() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 3000.0).looped();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.total_frames, 5000);
    let d = single(&layout, "no_fade_out_on_bed");
    assert_eq!(d.severity, Severity::Info);
    assert_contains(
        &d.message,
        &["open-ended clip `bed`", "stops abruptly at 5.00 s"],
    );
    assert_eq!(d.path.as_deref(), Some("tracks[1]"));
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h.contains("fade_out_ms: 2000"))
    );
}

#[test]
fn the_automatic_tail_is_the_longest_open_ended_fade_out() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed1", |t| {
            t.shorthand("bed1.wav", 3000.0).looped().fade_out(1000.0);
        })
        .music("bed2", |t| {
            t.shorthand("bed2.wav", 3000.0).looped().fade_out(3000.0);
        })
        .sfx("fx", |t| {
            // A finite clip's fade-out does not extend the tail.
            t.clip("hit", "hit.wav", 1000.0).at(1000.0).fade_out(800.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!((layout.tail_frames, layout.total_frames), (3000, 8000));
    assert_eq!(at(&tl, &layout, "bed1").end(), 8000);
    assert_eq!(at(&tl, &layout, "bed2").fade_out, 3000);
}

#[test]
fn an_explicit_tail_overrides_the_automatic_one() {
    let tl = ms_builder()
        .tail_ms(1500.0)
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 3000.0).looped().fade_out(3000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!((layout.tail_frames, layout.total_frames), (1500, 6500));
    let bed = at(&tl, &layout, "bed");
    assert_eq!((bed.end(), bed.fade_out), (6500, 3000));

    // Without open-ended clips, an explicit tail still extends the timeline.
    let tl = ms_builder()
        .tail_ms(800.0)
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .build();
    assert_eq!(tl.layout().total_frames, 5800);
}

#[test]
fn output_duration_truncates_content() {
    let tl = ms_builder()
        .duration_ms(6000.0)
        .speech("v", |t| {
            t.cue("c1", "x", 4000.0);
            t.cue("c2", "y", 4000.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 1000.0).looped().fade_out(1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(
        (layout.content_end_frames, layout.total_frames),
        (8000, 6000)
    );
    let d = single(&layout, "content_truncated");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "`output.duration_ms` (6.00 s) cuts content that ends at 8.00 s; 1 item is cut: `c2` (4.00-8.00 s)"
    );
    assert_eq!(d.path.as_deref(), Some("output.duration_ms"));
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "set `output.duration_ms` to at least 8000 to keep all content (or remove it to end 1.00 s after the content)"
        )
    );
    assert_eq!(d.related, ["c2"]);
    // Placements keep their geometry (the renderer discards frames after the total); beds end at the total.
    assert_eq!(span(&tl, &layout, "c2"), (4000, 8000));
    assert_eq!(at(&tl, &layout, "bed").end(), 6000);
}

#[test]
fn output_duration_lists_at_most_five_cut_items() {
    let tl = ms_builder()
        .duration_ms(100.0)
        .speech("v", |t| {
            for i in 0..7 {
                t.cue(&format!("c{i}"), "x", 1000.0);
            }
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "content_truncated");
    assert_contains(
        &d.message,
        &[
            "7 items are cut",
            "`c0` (0.00-1.00 s)",
            "`c4`",
            "and 2 more",
        ],
    );
    assert!(!d.message.contains("`c5`"));
    assert_eq!(d.related.len(), 7);
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h.contains("(or remove it to end with the content)"))
    );
}

#[test]
fn output_duration_can_extend_the_timeline() {
    let tl = ms_builder()
        .duration_ms(20_000.0)
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 3000.0).looped().fade_out(1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.total_frames, 20_000);
    assert_eq!(at(&tl, &layout, "bed").end(), 20_000);
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
}

#[test]
fn an_open_ended_clip_starting_after_the_end_is_silent() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.start_at(30_000.0);
            t.shorthand("bed.wav", 3000.0).looped().fade_out(1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.total_frames, 6000);
    let bed = at(&tl, &layout, "bed");
    assert_eq!((bed.start, bed.length), (30_000, 0));
    let d = single(&layout, "content_truncated");
    assert_eq!(
        d.message,
        "open-ended clip `bed` starts at 30.00 s, at or after the timeline end (6.00 s), so it is silent"
    );
}

#[test]
fn a_silent_open_ended_clip_without_a_fade_out_gets_no_abrupt_stop_info() {
    // review-timing#2/cascading-diagnostics (beds.yaml): a bed that never plays cannot "stop abruptly";
    // `content_truncated` alone describes it.
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 5000.0);
        })
        .music("bed", |t| {
            t.start_at(30_000.0);
            t.shorthand("bed.wav", 3000.0).looped();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.total_frames, 5000);
    assert_eq!(
        codes(&layout),
        ["content_truncated"],
        "{:#?}",
        layout.diagnostics
    );
}

#[test]
fn clips_follow_each_other_sequentially() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "a.wav", 4000.0);
            t.clip("c2", "b.wav", 3000.0);
            t.clip("c3", "c.wav", 1000.0).at(500.0);
            t.clip("c4", "d.wav", 1000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "c1"), (0, 4000));
    assert_eq!(span(&tl, &layout, "c2"), (4000, 7000));
    assert_eq!(span(&tl, &layout, "c3"), (500, 1500));
    assert_eq!(span(&tl, &layout, "c4"), (1500, 2500));
    assert_clean(&layout);
}

#[test]
fn crossfades_overlap_clips_with_equal_power_fades_on_both_sides() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "a.wav", 4000.0);
            t.clip("c2", "b.wav", 3000.0).crossfade(1000.0);
        })
        .build();
    let layout = tl.layout();
    let (c1, c2) = (at(&tl, &layout, "c1"), at(&tl, &layout, "c2"));
    assert_eq!(
        (c2.start, c2.end()),
        (3000, 6000),
        "next clip starts at prev end − crossfade"
    );
    assert_eq!(
        (c1.fade_in, c1.fade_out, c1.fade_out_curve),
        (0, 1000, FadeCurve::EqualPower)
    );
    assert_eq!(
        (c2.fade_in, c2.fade_out, c2.fade_in_curve),
        (1000, 0, FadeCurve::EqualPower)
    );
    assert_eq!(layout.content_end_frames, 6000);
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
}

#[test]
fn crossfade_fades_are_the_max_of_own_and_crossfade() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "a.wav", 4000.0)
                .fade_out(1500.0)
                .fade_curve(FadeCurve::Linear);
            t.clip("c2", "b.wav", 3000.0)
                .crossfade(1000.0)
                .fade_in(500.0)
                .fade_curve(FadeCurve::Linear);
        })
        .build();
    let layout = tl.layout();
    let (c1, c2) = (at(&tl, &layout, "c1"), at(&tl, &layout, "c2"));
    assert_eq!(
        (c1.fade_out, c1.fade_out_curve),
        (1500, FadeCurve::Linear),
        "own fade is longer"
    );
    assert_eq!(
        (c2.fade_in, c2.fade_in_curve),
        (1000, FadeCurve::EqualPower),
        "crossfade is longer"
    );
    assert_eq!(c2.start, 3000);
}

#[test]
fn crossfades_on_explicit_or_first_clips() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "a.wav", 4000.0).crossfade(700.0);
            t.clip("c2", "b.wav", 3000.0).at(10_000.0).crossfade(1000.0);
        })
        .build();
    let layout = tl.layout();
    let (c1, c2) = (at(&tl, &layout, "c1"), at(&tl, &layout, "c2"));
    // The first clip has nothing to crossfade with (lowering already warns about it).
    assert_eq!((c1.start, c1.fade_in), (0, 0));
    // An explicit start is kept, and a clip that does not follow its predecessor has nothing to crossfade
    // with either: the crossfade is ignored with a warning.
    assert_eq!((c2.start, c2.fade_in), (10_000, 0));
    assert_eq!(c1.fade_out, 0);
    let d = single(&layout, "field_not_applicable");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[1].crossfade_ms"));
}

#[test]
fn crossfades_of_explicitly_placed_clips_are_ignored_with_a_warning() {
    // review-timing x1: c2 at 5 s and c3 at 0 s declare crossfades with the clips listed before them, which do
    // not touch them in time.
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "impact.wav", 2000.0);
            t.clip("c2", "impact.wav", 2000.0)
                .at(5000.0)
                .crossfade(1000.0);
            t.clip("c3", "impact.wav", 2000.0).at(0.0).crossfade(1500.0);
            t.clip("c4", "impact.wav", 2000.0).crossfade(500.0);
        })
        .build();
    let layout = tl.layout();
    let fades = |id: &str| {
        let p = at(&tl, &layout, id);
        (p.fade_in, p.fade_out)
    };
    assert_eq!(fades("c1"), (0, 0));
    assert_eq!(fades("c2"), (0, 0));
    // c4 follows c3 sequentially: that crossfade applies.
    assert_eq!(fades("c3"), (0, 500));
    assert_eq!(fades("c4"), (500, 0));
    assert_eq!(span(&tl, &layout, "c4"), (1500, 3500));
    let warnings = with_code(&layout, "field_not_applicable");
    assert_eq!(warnings.len(), 2, "{:#?}", layout.diagnostics);
    assert_eq!(
        warnings[0].message,
        "`crossfade_ms` has no effect on clip `c2`: it has its own start (`start_at_ms`), and only a clip that \
         follows the previous clip of its track sequentially crossfades with it; it was ignored"
    );
    assert_eq!(
        warnings[0].hint.as_deref(),
        Some(
            "remove `crossfade_ms` from `c2`, or remove its `start_at_ms` so that it starts `crossfade_ms` before \
             the end of `c1`; use `fade_in_ms` to fade it in where it is"
        )
    );
    assert_eq!(
        warnings[1].path.as_deref(),
        Some("tracks[0].clips[2].crossfade_ms")
    );
    assert!(with_code(&layout, "fades_scaled").is_empty());
}

#[test]
fn fades_longer_than_the_clip_are_scaled_proportionally() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c", "a.wav", 4000.0)
                .fade_in(2000.0)
                .fade_out(3000.0);
        })
        .build();
    let layout = tl.layout();
    let c = at(&tl, &layout, "c");
    assert_eq!((c.fade_in, c.fade_out), (1600, 2400));
    let d = single(&layout, "fades_scaled");
    assert_eq!(d.severity, Severity::Info);
    assert_contains(
        &d.message,
        &[
            "`c`",
            "in 2.00 s + out 3.00 s",
            "(4.00 s)",
            "1.60 s + 2.40 s",
        ],
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[0]"));
}

#[test]
fn a_crossfade_longer_than_the_previous_clip_clamps_the_start() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "a.wav", 1000.0);
            t.clip("c2", "b.wav", 2000.0).crossfade(3000.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "c2"), (0, 2000));
    let d = single(&layout, "negative_start");
    assert_contains(
        &d.message,
        &[
            "`c2` would start at -2.00 s",
            "`crossfade_ms` (3.00 s)",
            "`c1`, which ends at 1.00 s",
        ],
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[1].crossfade_ms"));
    assert_eq!(
        d.hint.as_deref(),
        Some("reduce `crossfade_ms` on `c2` to at most 1000")
    );
    // Both crossfade fades are longer than their clips and get scaled.
    assert_eq!(at(&tl, &layout, "c1").fade_out, 1000);
    assert_eq!(at(&tl, &layout, "c2").fade_in, 2000);
    assert_eq!(with_code(&layout, "fades_scaled").len(), 2);
}

#[test]
fn loop_crossfades_are_limited_to_half_the_looped_segment() {
    let tl = ms_builder()
        .music("a", |t| {
            t.clip("wide", "a.wav", 2000.0)
                .looped()
                .length(10_000.0)
                .loop_crossfade(1500.0);
        })
        .music("b", |t| {
            t.clip("ok", "b.wav", 2000.0)
                .looped()
                .length(10_000.0)
                .loop_crossfade(500.0);
        })
        .music("c", |t| {
            t.clip("plain", "c.wav", 2000.0).loop_crossfade(500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(at(&tl, &layout, "wide").loop_crossfade, 1000);
    assert_eq!(at(&tl, &layout, "ok").loop_crossfade, 500);
    assert_eq!(
        at(&tl, &layout, "plain").loop_crossfade,
        0,
        "only looping clips crossfade at seams"
    );
    let d = single(&layout, "fades_scaled");
    assert_contains(
        &d.message,
        &[
            "`loop_crossfade_ms` of clip `wide` (1.50 s)",
            "reduced to 1.00 s",
        ],
    );
    assert_eq!(
        d.path.as_deref(),
        Some("tracks[0].clips[0].loop_crossfade_ms")
    );
}

#[test]
fn looping_nothing_is_an_error() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("bed", "bed.wav", 2000.0)
                .looped()
                .trim_start(2000.0)
                .length(5000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "invalid_value");
    assert_contains(&d.message, &["clip `bed` loops", "nothing to loop"]);
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[0].loop"));
}

#[test]
fn items_after_an_open_ended_clip_have_no_start() {
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("bed", "bed.wav", 2000.0).looped().at(1000.0);
            t.clip("after", "x.wav", 1000.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "anchor_open_ended");
    assert_eq!(d.severity, Severity::Error);
    assert_contains(&d.message, &["`after` follows `bed`", "open-ended"]);
    assert_eq!(d.path.as_deref(), Some("tracks[0].clips[1]"));
    // Placed at the start of the open-ended clip.
    assert_eq!(span(&tl, &layout, "after"), (1000, 2000));
}

#[test]
fn anchoring_to_the_end_of_an_open_ended_clip_is_an_error() {
    let tl = ms_builder()
        .music("bed", |t| {
            t.clip("loop", "bed.wav", 2000.0).looped().at(500.0);
        })
        .sfx("fx", |t| {
            t.clip("hit", "hit.wav", 500.0).after("loop", 0.0);
            t.clip("ok", "ok.wav", 500.0).with("loop", 100.0);
            t.clip("track_end", "end.wav", 500.0).after("bed", 0.0);
        })
        .build();
    let layout = tl.layout();
    let errors = with_code(&layout, "anchor_open_ended");
    assert_eq!(errors.len(), 2, "{:#?}", layout.diagnostics);
    assert_contains(
        &errors[0].message,
        &["`hit` is anchored to the end of `loop`"],
    );
    assert_eq!(
        errors[0].path.as_deref(),
        Some("tracks[1].clips[0].start_relative")
    );
    assert_contains(
        &errors[1].message,
        &["`track_end` is anchored to the end of `bed`"],
    );
    assert!(
        errors[0]
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("with_cue: loop"))
    );
    // The unknown end reads as the start of the open-ended clip.
    assert_eq!(span(&tl, &layout, "hit"), (500, 1000));
    assert_eq!(span(&tl, &layout, "ok"), (600, 1100));
    assert_eq!(span(&tl, &layout, "track_end"), (500, 1000));
}

#[test]
fn before_cue_conflicts_with_an_end_rule() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 1000.0).at(5000.0);
        })
        .sfx("fx", |t| {
            t.clip("riser", "r.wav", 3000.0)
                .before("c1", 0.0)
                .end_at(6000.0);
        })
        .music("bed", |t| {
            t.clip("loop", "bed.wav", 3000.0).before("c1", 0.0).looped();
        })
        .build();
    let layout = tl.layout();
    let conflicts = with_code(&layout, "conflicting_fields");
    assert_eq!(conflicts.len(), 2, "{:#?}", layout.diagnostics);
    assert_contains(
        &conflicts[0].message,
        &[
            "`riser` is placed with `before_cue`",
            "`end_at_ms`/`end_relative`",
        ],
    );
    assert_eq!(
        conflicts[0].path.as_deref(),
        Some("tracks[1].clips[0].start_relative")
    );
    assert_contains(&conflicts[1].message, &["`loop`", "open-ended"]);
    // Fallback: the start is the referenced start.
    assert_eq!(span(&tl, &layout, "riser"), (5000, 6000));
    assert_eq!(at(&tl, &layout, "loop").start, 5000);
}

#[test]
fn before_cue_is_invalid_in_end_relative() {
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 1000.0).at(5000.0);
        })
        .music("m", |t| {
            t.clip("bed", "b.wav", 60_000.0).end_before("c1", 0.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "invalid_value");
    assert_eq!(
        d.path.as_deref(),
        Some("tracks[1].clips[0].end_relative.before_cue")
    );
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h.contains("end_relative: {with_cue: c1}"))
    );
    assert_eq!(span(&tl, &layout, "bed"), (0, 5000), "treated as with_cue");
}

/// The documented "bounded bed" and "intro before the voice" patterns together (mixing-guide.md): the bed's
/// end waits for the last line, whose track waits for the bed's start. Start and end are separate events, so
/// this is no cycle.
#[test]
fn a_bounded_bed_can_lead_into_the_voice() {
    // review-timing cy2: the track id as the anchor.
    let tl = ms_builder()
        .music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0)
                .looped()
                .fade_in(1200.0)
                .fade_out(3000.0)
                .end_after("last_line", 3000.0);
        })
        .speech("narration", |t| {
            t.cue("first_line", "Welcome to the show.", 1620.0)
                .with("bed", 2000.0);
            t.cue("last_line", "Thanks for listening.", 1580.0);
        })
        .build();
    let layout = tl.layout();
    assert!(layout.diagnostics.is_empty(), "{:#?}", layout.diagnostics);
    assert_eq!(span(&tl, &layout, "first_line"), (2000, 3620));
    assert_eq!(span(&tl, &layout, "last_line"), (3620, 5200));
    assert_eq!(
        span(&tl, &layout, "bed"),
        (0, 8200),
        "3 s after the last line"
    );
    let bed = at(&tl, &layout, "bed");
    assert!(!bed.open_ended && bed.looped);
    assert_eq!((bed.fade_in, bed.fade_out), (1200, 3000));
    assert_eq!(layout.total_frames, 8200);

    // review-timing cy3: a clip of a `clips` list as the anchor.
    let tl = ms_builder()
        .music("music", |t| {
            t.clip("bed_clip", "synthwave_loop.wav", 8000.0)
                .looped()
                .end_after("outro", 3000.0);
        })
        .speech("voice", |t| {
            t.cue("intro", "Welcome.", 750.0).with("bed_clip", 2000.0);
            t.cue("outro", "Goodbye.", 750.0);
        })
        .build();
    let layout = tl.layout();
    assert_clean(&layout);
    assert_eq!(span(&tl, &layout, "bed_clip"), (0, 6500));

    // review-timing cy1: the voice track starts with the bed, the bed ends after the voice track.
    let tl = ms_builder()
        .music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0)
                .looped()
                .end_after("narration", 2000.0)
                .fade_out(1500.0);
        })
        .speech("narration", |t| {
            t.start_with("bed", 500.0);
            t.cue("hello", "Welcome to the show.", 1620.0);
        })
        .build();
    let layout = tl.layout();
    assert_clean(&layout);
    assert_eq!(span(&tl, &layout, "hello"), (500, 2120));
    assert_eq!(span(&tl, &layout, "bed"), (0, 4120));
}

#[test]
fn a_clip_end_waiting_for_its_own_dependents_is_still_a_cycle() {
    // The end of `c1` waits for `x`, which waits for the end of `c1`: a real cycle.
    let tl = ms_builder()
        .music("m", |t| {
            t.clip("c1", "long.wav", 10_000.0).end_after("x", 0.0);
        })
        .sfx("fx", |t| {
            t.clip("x", "x.wav", 1000.0).after("c1", 0.0);
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "dependency_cycle");
    assert_eq!(
        d.message,
        "dependency cycle `c1` → `x` → `c1`; the layout ignored `tracks[0].clips[0].end_relative` to break it \
         (`c1` plays its natural length)"
    );
    assert_eq!(span(&tl, &layout, "c1"), (0, 10_000));
    assert_eq!(span(&tl, &layout, "x"), (10_000, 11_000));
}

/// The core fixture `offline_demo.yaml` (valid for lowering) lays out without the false cycle
/// `loop_bed → outro → answer → question → host.pause1 → welcome → loop_bed` (review-timing).
#[test]
fn the_offline_demo_fixture_lays_out() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../soniccraft-core/tests/fixtures/valid/offline_demo.yaml");
    let text = std::fs::read_to_string(&path).expect("read the core fixture");
    let compiled = soniccraft_core::compile(
        &text,
        soniccraft_core::SourceFormat::Yaml,
        &soniccraft_core::LowerOptions::default(),
    );
    let program = compiled.program.expect("the fixture lowers");
    let sr = i64::from(program.output.sample_rate);
    // Plausible exact lengths: cues by text length, sources by name.
    let durations: Vec<soniccraft_layout::NodeDuration> = program
        .nodes
        .iter()
        .map(|node| match &node.kind {
            soniccraft_core::ir::NodeKind::Cue(cue) => {
                soniccraft_layout::NodeDuration::exact((cue.text.len() as i64 * sr / 15) as u64)
            }
            soniccraft_core::ir::NodeKind::Pause { .. } => {
                soniccraft_layout::NodeDuration::fixed(0)
            }
            soniccraft_core::ir::NodeKind::Clip(clip) => {
                let seconds = match clip.source.as_str() {
                    "audio/intro.wav" => 6,
                    "audio/loop.wav" => 8,
                    _ => 1,
                };
                soniccraft_layout::NodeDuration::source((seconds * sr) as u64)
            }
        })
        .collect();
    let layout = soniccraft_layout::layout(&program, &durations);
    assert!(!layout.has_errors(), "{:#?}", layout.diagnostics);
    let node = |id: &str| program.nodes.iter().position(|n| n.id == id).expect(id);
    let p = |id: &str| &layout.placements[node(id)];
    // The host track starts 500 ms after the bed loop starts; the loop ends 2 s after the outro.
    assert_eq!(p("welcome").start, p("loop_bed").start + sr / 2);
    assert_eq!(p("loop_bed").end(), p("outro").end() + 2 * sr);
    // The chime ends 100 ms before the welcome line starts.
    assert_eq!(p("chime").end(), p("welcome").start - sr / 10);
}

#[test]
fn an_empty_timeline_is_an_error() {
    // review-timing bo1: a single looping bed without a fade-out has length 0, which the render rejects;
    // validate and plan must say so too.
    let tl = ms_builder()
        .music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0).looped();
        })
        .build();
    let layout = tl.layout();
    assert_eq!(layout.total_frames, 0);
    let d = single(&layout, "invalid_value");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(
        d.message,
        "the timeline is empty: its total length is 0 ms. `bed` loops until the timeline end, which is the end of \
         the finite content plus the tail, and both are 0 here"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0]"));
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "give the looping bed a `fade_out_ms`, or set `output.tail_ms` or `output.duration_ms`, or add finite \
             content"
        )
    );
    // The error says it all: no "starts at or after the timeline end" warning on top.
    assert!(with_code(&layout, "content_truncated").is_empty());
    assert!(layout.has_certain_errors());

    // Each fix from the hint gives the timeline a length.
    let fixed = [
        ms_builder().music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0)
                .looped()
                .fade_out(2000.0);
        }),
        ms_builder().tail_ms(3000.0).music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0).looped();
        }),
        ms_builder().duration_ms(3000.0).music("bed", |t| {
            t.shorthand("synthwave_loop.wav", 8000.0).looped();
        }),
    ];
    for builder in fixed {
        let layout = builder.build().layout();
        assert!(layout.total_frames > 0);
        assert!(!layout.has_errors(), "{:#?}", layout.diagnostics);
    }

    // Without any open-ended clip the error points at the output settings.
    let tl = ms_builder().speech("v", |_| {}).build();
    let d = single(&tl.layout(), "invalid_value").clone();
    assert_eq!(d.path.as_deref(), Some("output"));
    // Estimated zero-length cues may get a length once synthesized.
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c1", "x", 0.0)
                .duration_source(DurationSource::Estimate);
        })
        .build();
    let layout = tl.layout();
    assert!(layout.has_errors() && !layout.has_certain_errors());
}

#[test]
fn timelines_longer_than_24_hours_are_errors() {
    const HOUR: f64 = 3_600_000.0;
    // 25 one-hour cues (1000 Hz: one frame per millisecond).
    let tl = ms_builder()
        .speech("v", |t| {
            for i in 0..25 {
                t.cue(&format!("c{i}"), "x", HOUR);
            }
        })
        .build();
    let layout = tl.layout();
    let d = single(&layout, "timeline_too_long");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(
        d.message,
        "the timeline would be 25 h 0 min (90000000 ms) long: its content ends at 25 h 0 min with `c24` (track \
         `v`); SonicCraft renders at most 24 h"
    );
    assert_eq!(d.path.as_deref(), Some("tracks[0].sequence[24]"));
    assert!(d.hint.as_deref().is_some_and(|h| {
        h.starts_with("make everything end within 24 h (86400000 ms): move `c24` earlier")
    }));
    // The total is capped (the placements keep their times), so nothing downstream sizes buffers for it.
    assert_eq!(layout.total_frames, 86_400_000);
    assert_eq!(layout.content_end_frames, 90_000_000);
    assert!(layout.has_certain_errors());

    // The tail pushes a 23.5 h timeline over the limit.
    let tl = ms_builder()
        .tail_ms(HOUR)
        .speech("v", |t| {
            t.cue("c", "x", 23.5 * HOUR);
        })
        .build();
    let d = single(&tl.layout(), "timeline_too_long").clone();
    assert_eq!(d.path.as_deref(), Some("output.tail_ms"));
    assert_eq!(
        d.hint.as_deref(),
        Some("set `output.tail_ms` to at most 1800000")
    );

    // ...or the fade-out of an open-ended bed (the automatic tail).
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c", "x", 23.5 * HOUR);
        })
        .music("bed", |t| {
            t.shorthand("bed.wav", 60_000.0).looped().fade_out(HOUR);
        })
        .build();
    let d = single(&tl.layout(), "timeline_too_long").clone();
    assert_eq!(d.path.as_deref(), Some("tracks[1].fade_out_ms"));

    // Exactly 24 h is fine.
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c", "x", 24.0 * HOUR);
        })
        .build();
    assert!(!tl.layout().has_errors());

    // With estimated lengths the error may go away once the exact durations are known.
    let tl = ms_builder()
        .speech("v", |t| {
            t.cue("c", "x", 25.0 * HOUR)
                .duration_source(DurationSource::Estimate);
        })
        .build();
    let layout = tl.layout();
    assert!(layout.has_errors() && !layout.has_certain_errors());
}

#[test]
fn the_shorthand_clip_is_referenced_by_its_track_id() {
    let tl = ms_builder()
        .music("bed", |t| {
            t.shorthand("bed.wav", 4000.0);
        })
        .speech("v", |t| {
            t.cue("c1", "x", 1000.0).after("bed", 500.0);
        })
        .build();
    let layout = tl.layout();
    assert_eq!(span(&tl, &layout, "c1"), (4500, 5500));
    assert_eq!(
        at(&tl, &layout, "bed").kind,
        soniccraft_layout::PlacementKind::Clip
    );
}
