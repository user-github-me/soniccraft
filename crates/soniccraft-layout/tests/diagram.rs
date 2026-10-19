//! The ASCII plan diagram: snapshots plus structural properties.
//!
//! Snapshots live in `tests/snapshots/*.txt`. To accept intentional changes, run
//! `SONICCRAFT_UPDATE_SNAPSHOTS=1 cargo test -p soniccraft-layout --test diagram` and review the diff.

mod common;

use std::path::PathBuf;

use soniccraft_core::ir::OverlapPolicy;
use soniccraft_layout::test_support::{ProgramBuilder, TestTimeline};
use soniccraft_layout::{
    DiagramOptions, DuckedTrack, DurationSource, Interval, Layout, render_diagram,
};

fn assert_snapshot(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("SONICCRAFT_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("snapshot dir")).expect("create snapshot dir");
        std::fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {}: {e}\nrun with SONICCRAFT_UPDATE_SNAPSHOTS=1 to create it",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "snapshot `{name}` differs ({})\n--- expected\n{expected}--- actual\n{actual}",
        path.display()
    );
}

/// The README timeline plus a ducked music bed and a sound effect, as a dry run would see it.
fn readme_timeline() -> TestTimeline {
    ProgramBuilder::new()
        .title("Tech briefing")
        .speech("host", |t| {
            t.cue(
                "cue_01",
                "Autonomous software development is changing how teams ship code.",
                4200.0,
            )
            .speaker("host")
            .duration_source(DurationSource::Estimate);
            t.pause(350.0);
            t.cue("cue_03", "So what does that mean for code review?", 2600.0)
                .speaker("host")
                .duration_source(DurationSource::Estimate);
        })
        .speech("guest", |t| {
            t.cue(
                "cue_02",
                "It means the bottleneck moves from writing code to verifying it.",
                3100.0,
            )
            .after("cue_01", 150.0)
            .speaker("guest")
            .duration_source(DurationSource::Cache);
        })
        .music("ambient_bed", |t| {
            t.shorthand("music/ambient.wav", 30_000.0)
                .looped()
                .fade_in(1000.0)
                .fade_out(2000.0);
            t.ducked_by(&["host", "guest"], 15.0);
        })
        .sfx("sfx", |t| {
            t.clip("whoosh", "sfx/whoosh.wav", 800.0)
                .with("cue_03", -400.0);
        })
        .build()
}

/// Cue-mode style duck regions for the README bed: speech intervals widened by 80 ms lookahead and 250 ms
/// hold, merged.
fn readme_ducking(tl: &TestTimeline, layout: &Layout) -> Vec<DuckedTrack> {
    let sr = i64::from(layout.sample_rate);
    let (lookahead, hold) = (80 * sr / 1000, 250 * sr / 1000);
    let mut speech = layout.track_intervals(tl.track("host"));
    speech.extend(layout.track_intervals(tl.track("guest")));
    let widened = speech
        .into_iter()
        .map(|iv| Interval::new((iv.start - lookahead).max(0), iv.end + hold));
    vec![DuckedTrack {
        track: tl.track("ambient_bed"),
        regions: soniccraft_layout::merge_intervals(widened),
    }]
}

#[test]
fn readme_snapshot() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let diagram = render_diagram(
        &tl.program,
        &layout,
        &readme_ducking(&tl, &layout),
        &DiagramOptions::default(),
    );
    assert_snapshot("readme", &diagram);
}

#[test]
fn narrow_snapshot_truncates_ids_and_text() {
    let tl = ProgramBuilder::new()
        .sample_rate(24_000)
        .title("  Grüße\naus Köln  ")
        .overlap_policy(OverlapPolicy::Push)
        .speech("narrator_with_a_very_long_track_id", |t| {
            t.cue(
                "intro_cue_with_an_exceptionally_long_identifier",
                "Grüße aus Köln — ça va? Ünïcödé text stays.",
                3000.0,
            )
            .speaker("Zoë the narrator, also very long");
            t.cue("second", "Line\nbreaks\tand   spaces collapse.", 1200.0);
        })
        .speech("muted", |t| {
            t.mute();
            t.cue("m1", "Muted but laid out.", 1000.0);
        })
        .build();
    let layout = tl.layout();
    let options = DiagramOptions {
        width: 50,
        label_width: 6,
        text_width: 20,
    };
    let diagram = render_diagram(&tl.program, &layout, &[], &options);
    assert_snapshot("narrow", &diagram);
}

#[test]
fn long_snapshot_uses_minute_labels() {
    let tl = ProgramBuilder::new()
        .sample_rate(24_000)
        .title("Two-person podcast")
        .speech("alice", |t| {
            for i in 0..8 {
                t.cue(
                    &format!("a{i}"),
                    "Alice speaks for a while about the topic at hand.",
                    7000.0 + 500.0 * i as f64,
                );
                t.pause(9000.0);
            }
        })
        .speech("bob", |t| {
            t.start_at(7600.0);
            for i in 0..8 {
                t.cue(&format!("b{i}"), "Bob answers.", 5000.0)
                    .duration_source(DurationSource::Estimate);
                t.pause(11_000.0);
            }
        })
        .music("bed", |t| {
            t.shorthand("bed.flac", 60_000.0)
                .looped()
                .fade_in(3000.0)
                .fade_out(5000.0);
        })
        .build();
    let layout = tl.layout();
    let diagram = render_diagram(&tl.program, &layout, &[], &DiagramOptions::default());
    assert!(diagram.contains("1:00"), "{diagram}");
    assert_snapshot("long", &diagram);
}

#[test]
fn empty_snapshot() {
    let tl = ProgramBuilder::new()
        .speech("v", |t| {
            t.pause(1500.0);
        })
        .build();
    let layout = tl.layout();
    let diagram = render_diagram(&tl.program, &layout, &[], &DiagramOptions::default());
    assert_snapshot("empty", &diagram);

    // A timeline of length zero renders too.
    let tl = ProgramBuilder::new().speech("v", |_| {}).build();
    let diagram = render_diagram(&tl.program, &tl.layout(), &[], &DiagramOptions::default());
    assert!(diagram.contains("duration 0:00.0"), "{diagram}");
}

#[test]
fn wide_snapshot() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let options = DiagramOptions {
        width: 120,
        ..DiagramOptions::default()
    };
    let diagram = render_diagram(
        &tl.program,
        &layout,
        &readme_ducking(&tl, &layout),
        &options,
    );
    assert_snapshot("wide", &diagram);
}

#[test]
fn lanes_map_time_to_columns_exactly() {
    // 10 columns of one second each.
    let tl = ProgramBuilder::new()
        .sample_rate(1000)
        .speech("v", |t| {
            t.cue("c1", "one", 3000.0);
            t.pause(2000.0);
            t.cue("c2", "two", 5000.0);
        })
        .music("m", |t| {
            t.clip("bed", "bed.wav", 10_000.0);
        })
        .build();
    let layout = tl.layout();
    let ducking = [DuckedTrack {
        track: tl.track("m"),
        regions: vec![Interval::new(0, 3000), Interval::new(5000, 10_000)],
    }];
    let options = DiagramOptions {
        width: 15,
        label_width: 4,
        text_width: 20,
    };
    let diagram = render_diagram(&tl.program, &layout, &ducking, &options);
    let lane = |id: &str| {
        diagram
            .lines()
            .find(|l| l.starts_with(&format!("{id:<4} ")))
            .map(|l| l[5..].to_string())
            .unwrap_or_default()
    };
    assert_eq!(lane("v"), "###  #####");
    assert_eq!(lane("m"), "---==-----");
    assert!(diagram.contains("(1 col = 1.00 s)"), "{diagram}");
}

#[test]
fn half_ducked_columns_count_as_ducked() {
    let tl = ProgramBuilder::new()
        .sample_rate(1000)
        .music("m", |t| {
            t.clip("bed", "bed.wav", 10_000.0);
        })
        .build();
    let layout = tl.layout();
    // Column 0 is half ducked, column 1 a quarter, column 2 fully.
    let ducking = [DuckedTrack {
        track: 0,
        regions: vec![Interval::new(500, 1000), Interval::new(1750, 3000)],
    }];
    let options = DiagramOptions {
        width: 15,
        label_width: 4,
        text_width: 20,
    };
    let diagram = render_diagram(&tl.program, &layout, &ducking, &options);
    let lane = diagram
        .lines()
        .find(|l| l.starts_with("m    "))
        .unwrap_or_default();
    assert_eq!(&lane[5..], "-=-=======");
}

#[test]
fn truncated_track_ids_stay_distinct() {
    // review-timing d1: three track ids that share a 14-character prefix.
    let tl = ProgramBuilder::new()
        .sample_rate(1000)
        .overlap_policy(OverlapPolicy::Allow)
        .speech("voice_primary_left", |t| {
            t.cue("c1", "one", 2000.0);
        })
        .speech("voice_primary_right", |t| {
            t.cue("c2", "two", 2000.0);
        })
        .speech("voice_primary_center", |t| {
            t.cue(
                "very_long_cue_identifier_that_goes_on_and_on",
                "three",
                2000.0,
            );
        })
        .build();
    let diagram = render_diagram(&tl.program, &tl.layout(), &[], &DiagramOptions::default());
    let lanes: Vec<&str> = diagram
        .lines()
        .skip_while(|l| !l.starts_with("time"))
        .skip(2)
        .take(3)
        .map(|l| &l[..12])
        .collect();
    assert_eq!(
        lanes,
        ["voic..._left", "voic...right", "voic...enter"],
        "{diagram}"
    );
    // The cue table's track column is wide enough for the whole ids.
    for id in [
        "voice_primary_left",
        "voice_primary_right",
        "voice_primary_center",
    ] {
        assert!(diagram.contains(&format!("  {id}  ")), "{id}\n{diagram}");
    }
    // Long cue ids are shortened, still at most 24 columns.
    assert!(diagram.contains("very_long_cue_identif..."), "{diagram}");
}

#[test]
fn wide_characters_keep_the_tables_aligned() {
    // review-timing#2 uni: a CJK speaker and text, an emoji speaker, combining marks.
    let tl = ProgramBuilder::new()
        .sample_rate(1000)
        .speech("v", |t| {
            t.cue("c1", "こんにちは、皆さん。今日は良い天気ですね。", 2000.0)
                .speaker("司会者");
            t.cue("c2", "Hello there, nice weather.", 1500.0)
                .speaker("guest");
            t.cue("c3", "Cafe\u{301} talk 🎙\u{fe0f}", 1000.0)
                .speaker("🎙\u{fe0f} host");
        })
        .build();
    let diagram = render_diagram(&tl.program, &tl.layout(), &[], &DiagramOptions::default());
    let rows: Vec<&str> = diagram
        .lines()
        .skip_while(|l| !l.ends_with("cues:"))
        .skip(1)
        .take(4)
        .collect();
    // Every row's `source` column (and so its `text` column) starts at the same terminal column.
    let column_of = |row: &str, needle: &str| {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {row:?}"));
        soniccraft_layout_width(&row[..byte])
    };
    let source = column_of(rows[0], "source");
    assert_eq!(column_of(rows[1], "exact"), source, "{diagram}");
    assert_eq!(column_of(rows[2], "exact"), source, "{diagram}");
    assert_eq!(column_of(rows[3], "exact"), source, "{diagram}");
}

/// Terminal columns of `text`, counted like the diagram does for the characters used in these tests.
fn soniccraft_layout_width(text: &str) -> usize {
    text.chars()
        .map(|c| match u32::from(c) {
            0x0300..=0x036F | 0xFE00..=0xFE0F => 0,
            0x3000..=0x303E | 0x3041..=0x33FF | 0x4E00..=0x9FFF | 0x1F300..=0x1F64F => 2,
            _ => 1,
        })
        .sum()
}

fn all_timelines() -> Vec<TestTimeline> {
    vec![
        readme_timeline(),
        ProgramBuilder::new()
            .title("x".repeat(30).as_str())
            .speech("a_track_id_longer_than_the_label", |t| {
                t.cue("c", "text", 61_000.0);
            })
            .build(),
        ProgramBuilder::new()
            .speech("v", |t| {
                t.pause(10.0);
            })
            .build(),
    ]
}

#[test]
fn ruler_and_lanes_respect_the_width() {
    for tl in all_timelines() {
        let layout = tl.layout();
        for width in [30, 50, 80, 100, 160] {
            let options = DiagramOptions {
                width,
                ..DiagramOptions::default()
            };
            let diagram = render_diagram(&tl.program, &layout, &[], &options);
            let effective = width.max(options.label_width + 1 + 10);
            let lane_lines = diagram
                .lines()
                .skip_while(|l| !l.starts_with("time"))
                .take_while(|l| !l.starts_with("legend"));
            for line in lane_lines {
                assert!(
                    line.chars().count() <= effective,
                    "width {width}: {line:?} is too long\n{diagram}"
                );
            }
            assert!(diagram.ends_with('\n'));
            assert!(
                diagram.lines().all(|l| l == l.trim_end()),
                "trailing whitespace\n{diagram}"
            );
        }
    }
}

#[test]
fn diagram_is_ascii_apart_from_document_text() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let diagram = render_diagram(
        &tl.program,
        &layout,
        &readme_ducking(&tl, &layout),
        &DiagramOptions::default(),
    );
    assert!(diagram.is_ascii(), "{diagram}");
}

#[test]
fn diagram_is_deterministic() {
    let tl = readme_timeline();
    let layout = tl.layout();
    let ducking = readme_ducking(&tl, &layout);
    let first = render_diagram(&tl.program, &layout, &ducking, &DiagramOptions::default());
    for _ in 0..3 {
        let again = tl.layout();
        assert_eq!(
            render_diagram(&tl.program, &again, &ducking, &DiagramOptions::default()),
            first
        );
    }
}
