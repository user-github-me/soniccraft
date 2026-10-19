//! The ASCII plan diagram (`PlanReport.diagram`, DESIGN.md §8).
//!
//! Layout of the text, top to bottom:
//! 1. a header: title, total duration (`m:ss.s`), sample rate, counts, and how many durations are estimates;
//! 2. a time ruler with tick labels;
//! 3. one lane per track: the track id padded or shortened to a fixed width, then one character per time
//!    column: `#` speech, `=` music/sfx at full level, `-` music/sfx while ducked, blank for pauses and silence;
//! 4. a legend with the time scale;
//! 5. a cue table (start, end, dur, track, id, speaker, duration source, text) and a clip table.
//!
//! Shortened ids stay distinct: an id that does not fit is cut at the end (`narrator_wi...`), or, when that
//! would make two labels equal, in the middle (`voic..._left`), or else numbered (`aaaaaaa~1`). Widths are
//! terminal columns (wide CJK characters and emoji count two, combining marks zero), so the tables stay aligned
//! whatever the script of ids, speakers and text.
//!
//! The output is pure ASCII apart from document text (titles, ids, speakers, cue text, file names), every
//! line is right-trimmed, and the text ends with a newline. Rendering is deterministic: all positions come
//! from integer arithmetic on frames.

use soniccraft_core::Program;
use soniccraft_core::ir::{NodeKind, TrackIdx};

use crate::text::{clock, count, display_width, one_line, pad, secs, truncate, unique_labels};
use crate::types::{Interval, Layout, Placement, PlacementKind, merge_intervals};

/// Options for [`render_diagram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramOptions {
    /// Width in columns of the time ruler and the track lanes, label column included. Default 80. Widths too
    /// small for a lane of 10 columns are raised.
    pub width: usize,
    /// Width of the track-label column in terminal columns; longer ids are shortened with `...` (keeping
    /// labels distinct). Default 12, clamped to 4..=40.
    pub label_width: usize,
    /// Maximum characters of cue text and clip file names in the tables, `...` included. Default 48, min 8.
    pub text_width: usize,
}

impl Default for DiagramOptions {
    fn default() -> Self {
        DiagramOptions {
            width: 80,
            label_width: 12,
            text_width: 48,
        }
    }
}

/// Ducking activity of one track (an input: the engine computes it), drawn as `-` on that track's lane.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DuckedTrack {
    /// The ducked track.
    pub track: TrackIdx,
    /// Active regions in frames at the layout's sample rate (after hold and lookahead).
    pub regions: Vec<Interval>,
}

/// Smallest number of time columns in a lane.
const MIN_LANE_COLUMNS: usize = 10;
/// Width caps of the table's id-like columns in terminal columns (longer values are shortened with `...`).
const TRACK_COLUMN_MAX: usize = 24;
const ID_COLUMN_MAX: usize = 24;
const SPEAKER_COLUMN_MAX: usize = 16;
/// Ruler tick spacings, in tenths of a second.
const TICK_STEPS_TENTHS: [u64; 23] = [
    1, 2, 5, 10, 20, 50, 100, 150, 200, 300, 600, 1200, 1800, 3000, 6000, 9000, 12000, 18000,
    36000, 72000, 144_000, 288_000, 576_000,
];

/// Render the plan diagram of `layout` (which must come from [`crate::layout`] on `program`).
///
/// `ducking` lists the ducked tracks with their active regions in frames; tracks not listed are drawn
/// without ducking.
#[must_use]
pub fn render_diagram(
    program: &Program,
    layout: &Layout,
    ducking: &[DuckedTrack],
    options: &DiagramOptions,
) -> String {
    let label_width = options.label_width.clamp(4, 40);
    let columns = options
        .width
        .saturating_sub(label_width + 1)
        .max(MIN_LANE_COLUMNS);
    let text_width = options.text_width.max(8);
    let grid = Grid {
        total: layout.total_frames,
        columns,
        sample_rate: layout.sample_rate,
    };

    let track_ids: Vec<String> = program.tracks.iter().map(|t| one_line(&t.id)).collect();
    let node_ids: Vec<String> = program.nodes.iter().map(|n| one_line(&n.id)).collect();
    let labels = Labels {
        lanes: unique_labels(&track_ids, label_width),
        tracks: unique_labels(&track_ids, TRACK_COLUMN_MAX),
        nodes: unique_labels(&node_ids, ID_COLUMN_MAX),
    };

    let mut lines: Vec<String> = Vec::new();
    header(program, layout, &mut lines);
    lines.push(String::new());
    ruler(&grid, label_width, &mut lines);
    lanes(layout, ducking, &grid, label_width, &labels, &mut lines);
    lines.push(legend(&grid));
    cue_table(program, layout, text_width, &labels, &mut lines);
    clip_table(program, layout, text_width, &labels, &mut lines);

    let mut out = String::new();
    for line in lines {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Distinct display labels of tracks (lane width and table width) and nodes.
struct Labels {
    lanes: Vec<String>,
    tracks: Vec<String>,
    nodes: Vec<String>,
}

impl Labels {
    fn track(&self, p: &Placement) -> &str {
        self.tracks.get(p.track).map_or("?", String::as_str)
    }

    fn node(&self, p: &Placement) -> &str {
        self.nodes.get(p.node).map_or("?", String::as_str)
    }
}

/// Maps lane columns to frame ranges.
struct Grid {
    total: u64,
    columns: usize,
    sample_rate: u32,
}

impl Grid {
    /// Frames shown by column `c`: `[floor(c·T/W), ceil((c+1)·T/W))`, so each column covers at least one
    /// frame of a non-empty timeline.
    fn bucket(&self, c: usize) -> Interval {
        let (t, w, c) = (u128::from(self.total), self.columns as u128, c as u128);
        let lo = c * t / w;
        let hi = ((c + 1) * t).div_ceil(w);
        Interval::new(clamp_i64(lo), clamp_i64(hi))
    }

    /// Column of the time `num / den` frames, if it lies on the timeline.
    fn column_at(&self, num: u128, den: u128) -> Option<usize> {
        if self.total == 0 || den == 0 {
            return (num == 0).then_some(0);
        }
        let col = num * self.columns as u128 / (den * u128::from(self.total));
        usize::try_from(col).ok().filter(|&c| c < self.columns)
    }

    /// Column of `tenths` tenths of a second.
    fn column_of_tenths(&self, tenths: u64) -> Option<usize> {
        self.column_at(u128::from(tenths) * u128::from(self.sample_rate), 10)
    }

    /// True when the timeline is at least a minute long (ruler labels use `m:ss`).
    fn is_long(&self) -> bool {
        u128::from(self.total) >= 60 * u128::from(self.sample_rate)
    }
}

fn clamp_i64(v: u128) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn header(program: &Program, layout: &Layout, lines: &mut Vec<String>) {
    let title = program
        .title
        .as_deref()
        .map(one_line)
        .filter(|t| !t.is_empty());
    lines.push(match title {
        Some(title) => format!("SonicCraft plan: {title}"),
        None => "SonicCraft plan".to_string(),
    });
    let of_kind = |kind: PlacementKind| layout.placements.iter().filter(|p| p.kind == kind).count();
    let (cues, pauses, clips) = (
        of_kind(PlacementKind::Cue),
        of_kind(PlacementKind::Pause),
        of_kind(PlacementKind::Clip),
    );
    let total = i64::try_from(layout.total_frames).unwrap_or(i64::MAX);
    lines.push(format!(
        "duration {} | {} Hz | {} | {}, {}, {}",
        clock(total, layout.sample_rate, 1),
        layout.sample_rate,
        count(program.tracks.len(), "track", "tracks"),
        count(cues, "cue", "cues"),
        count(pauses, "pause", "pauses"),
        count(clips, "clip", "clips"),
    ));
    let mut status = format!(
        "durations: {} of {} estimated",
        layout.estimated_count(),
        cues + clips
    );
    let muted: Vec<&str> = program
        .tracks
        .iter()
        .filter(|t| t.mute)
        .map(|t| t.id.as_str())
        .collect();
    if !muted.is_empty() {
        status.push_str(&format!(" | muted: {}", muted.join(", ")));
    }
    lines.push(status);
}

fn tick_label(tenths: u64, long: bool) -> String {
    let (seconds, tenth) = (tenths / 10, tenths % 10);
    match (long, tenth) {
        (true, 0) => format!("{}:{:02}", seconds / 60, seconds % 60),
        (true, d) => format!("{}:{:02}.{d}", seconds / 60, seconds % 60),
        (false, 0) => format!("{seconds}s"),
        (false, d) => format!("{seconds}.{d}s"),
    }
}

fn ruler(grid: &Grid, label_width: usize, lines: &mut Vec<String>) {
    let long = grid.is_long();
    let total_tenths =
        u64::try_from((u128::from(grid.total) * 10).div_ceil(u128::from(grid.sample_rate.max(1))))
            .unwrap_or(u64::MAX);
    // Leave room for the longest label plus a space before the next one.
    let widest = tick_label(total_tenths, long).len().max(4);
    let spacing = (widest + 2) as u128;
    let w = grid.columns as u128;
    let sr = u128::from(grid.sample_rate);
    let t = u128::from(grid.total);
    let step = TICK_STEPS_TENTHS
        .iter()
        .copied()
        .find(|&s| u128::from(s) * sr * w >= spacing * 10 * t)
        .unwrap_or(TICK_STEPS_TENTHS[TICK_STEPS_TENTHS.len() - 1]);
    // Columns between two major ticks (rounded down), to decide whether half ticks fit.
    let step_columns = if t == 0 {
        w
    } else {
        u128::from(step) * sr * w / (10 * t)
    };

    let mut labels = vec![' '; grid.columns];
    let mut ticks = vec!['-'; grid.columns];
    let mut next_free = 0;
    for k in 0..=grid.columns as u64 {
        let tenths = k * step;
        let Some(col) = grid.column_of_tenths(tenths) else {
            break;
        };
        ticks[col] = '|';
        let text = tick_label(tenths, long);
        if col >= next_free && col + text.len() <= grid.columns {
            for (i, ch) in text.chars().enumerate() {
                labels[col + i] = ch;
            }
            next_free = col + text.len() + 1;
        }
        if step_columns >= 6 {
            let half = grid.column_at(u128::from(2 * k + 1) * u128::from(step) * sr, 20);
            if let Some(hc) = half.filter(|&hc| ticks[hc] == '-') {
                ticks[hc] = '+';
            }
        }
        if grid.total == 0 {
            break;
        }
    }
    lines.push(format!(
        "{} {}",
        pad("time", label_width),
        labels.into_iter().collect::<String>()
    ));
    lines.push(format!(
        "{} {}",
        pad("", label_width),
        ticks.into_iter().collect::<String>()
    ));
}

/// Total frames of `list` (sorted, disjoint) that fall inside `bucket`.
fn covered(list: &[Interval], bucket: Interval) -> u64 {
    list.iter().map(|iv| iv.overlap(&bucket)).sum()
}

/// Intersection of two sorted, disjoint interval lists.
fn intersect(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let start = a[i].start.max(b[j].start);
        let end = a[i].end.min(b[j].end);
        if end > start {
            out.push(Interval::new(start, end));
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn lanes(
    layout: &Layout,
    ducking: &[DuckedTrack],
    grid: &Grid,
    label_width: usize,
    labels: &Labels,
    lines: &mut Vec<String>,
) {
    // Intervals by track in one sweep (a lane per track must not rescan every placement).
    let tracks = labels.lanes.len();
    let mut cues: Vec<Vec<Interval>> = vec![Vec::new(); tracks];
    let mut clips: Vec<Vec<Interval>> = vec![Vec::new(); tracks];
    let mut ducks: Vec<Vec<Interval>> = vec![Vec::new(); tracks];
    for p in &layout.placements {
        let list = match p.kind {
            PlacementKind::Cue => &mut cues,
            PlacementKind::Clip => &mut clips,
            PlacementKind::Pause => continue,
        };
        if let Some(list) = list.get_mut(p.track) {
            list.push(p.interval());
        }
    }
    for d in ducking {
        if let Some(list) = ducks.get_mut(d.track) {
            list.extend(d.regions.iter().copied());
        }
    }
    for (t, label) in labels.lanes.iter().enumerate() {
        let cues = merge_intervals(std::mem::take(&mut cues[t]));
        let clips = merge_intervals(std::mem::take(&mut clips[t]));
        let ducked = intersect(&clips, &merge_intervals(std::mem::take(&mut ducks[t])));
        let cells: String = (0..grid.columns)
            .map(|c| {
                let bucket = grid.bucket(c);
                if covered(&cues, bucket) > 0 {
                    return '#';
                }
                let audible = covered(&clips, bucket);
                if audible == 0 {
                    ' '
                } else if 2 * covered(&ducked, bucket) >= audible {
                    '-'
                } else {
                    '='
                }
            })
            .collect();
        lines.push(format!("{} {cells}", pad(label, label_width)));
    }
}

fn legend(grid: &Grid) -> String {
    let per_column = if grid.columns == 0 {
        0
    } else {
        grid.total.div_ceil(grid.columns as u64)
    };
    format!(
        "legend: # speech  = music/sfx  - ducked  blank = silence  (1 col = {} s)",
        secs(
            i64::try_from(per_column).unwrap_or(i64::MAX),
            grid.sample_rate
        )
    )
}

/// Format a table: indented by two spaces, every column but the last padded to its widest cell (in terminal
/// columns), two-space gutters.
fn table(header: &[&str], rows: &[Vec<String>], lines: &mut Vec<String>) {
    let mut widths: Vec<usize> = header.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(display_width(cell));
        }
    }
    lines.push(table_row(header, &widths));
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        lines.push(table_row(&cells, &widths));
    }
}

fn table_row(cells: &[&str], widths: &[usize]) -> String {
    let mut line = String::from(" ");
    for (i, cell) in cells.iter().enumerate() {
        line.push(' ');
        if i + 1 == cells.len() {
            line.push_str(cell);
        } else {
            line.push_str(&pad(cell, widths.get(i).copied().unwrap_or(0)));
            line.push(' ');
        }
    }
    line
}

fn sorted_of_kind(layout: &Layout, kind: PlacementKind) -> Vec<&Placement> {
    let mut rows: Vec<&Placement> = layout
        .placements
        .iter()
        .filter(|p| p.kind == kind)
        .collect();
    rows.sort_by_key(|p| (p.start, p.node));
    rows
}

fn cue_table(
    program: &Program,
    layout: &Layout,
    text_width: usize,
    labels: &Labels,
    lines: &mut Vec<String>,
) {
    let sr = layout.sample_rate;
    let rows = sorted_of_kind(layout, PlacementKind::Cue);
    if rows.is_empty() {
        return;
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|p| {
            let cue = program.nodes.get(p.node).and_then(|n| n.as_cue());
            let speaker = cue
                .and_then(|c| c.speaker.as_deref())
                .map(one_line)
                .filter(|s| !s.is_empty())
                .map_or_else(|| "-".to_string(), |s| truncate(&s, SPEAKER_COLUMN_MAX));
            let text = cue.map_or_else(String::new, |c| truncate(&one_line(&c.text), text_width));
            let marker = if p.pushed_from.is_some() { "*" } else { "" };
            vec![
                format!("{}{marker}", clock(p.start, sr, 2)),
                clock(p.end(), sr, 2),
                format!("{}s", secs(p.end() - p.start, sr)),
                labels.track(p).to_string(),
                labels.node(p).to_string(),
                speaker,
                p.duration_source.as_str().to_string(),
                text,
            ]
        })
        .collect();
    lines.push(String::new());
    lines.push(format!("{}:", count(rows.len(), "cue", "cues")));
    table(
        &[
            "start", "end", "dur", "track", "id", "speaker", "source", "text",
        ],
        &cells,
        lines,
    );
    for p in rows.iter().filter(|p| p.pushed_from.is_some()) {
        if let Some(from) = p.pushed_from {
            lines.push(format!(
                "  * {} was pushed from {} by the speech-overlap policy",
                labels.node(p),
                clock(from, sr, 2)
            ));
        }
    }
}

fn clip_table(
    program: &Program,
    layout: &Layout,
    text_width: usize,
    labels: &Labels,
    lines: &mut Vec<String>,
) {
    let sr = layout.sample_rate;
    let rows = sorted_of_kind(layout, PlacementKind::Clip);
    if rows.is_empty() {
        return;
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|p| {
            let source = match program.nodes.get(p.node).map(|n| &n.kind) {
                Some(NodeKind::Clip(clip)) => truncate(&one_line(&clip.source), text_width),
                _ => String::new(),
            };
            let frames = |f: u64| secs(i64::try_from(f).unwrap_or(i64::MAX), sr);
            let mut flags: Vec<String> = Vec::new();
            if p.looped {
                flags.push("loop".to_string());
            }
            if p.open_ended {
                flags.push("open-ended".to_string());
            }
            if p.trim_start > 0 {
                flags.push(format!("trim {}s", frames(p.trim_start)));
            }
            if p.fade_in > 0 {
                flags.push(format!("fade in {}s", frames(p.fade_in)));
            }
            if p.fade_out > 0 {
                flags.push(format!("fade out {}s", frames(p.fade_out)));
            }
            if p.loop_crossfade > 0 {
                flags.push(format!("loop xfade {}s", frames(p.loop_crossfade)));
            }
            let file = if flags.is_empty() {
                source
            } else {
                format!("{source} ({})", flags.join(", "))
            };
            vec![
                clock(p.start, sr, 2),
                clock(p.end(), sr, 2),
                format!("{}s", secs(p.end() - p.start, sr)),
                labels.track(p).to_string(),
                labels.node(p).to_string(),
                p.duration_source.as_str().to_string(),
                file,
            ]
        })
        .collect();
    lines.push(String::new());
    lines.push(format!("{}:", count(rows.len(), "clip", "clips")));
    table(
        &["start", "end", "dur", "track", "id", "source", "file"],
        &cells,
        lines,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_the_timeline_without_gaps() {
        let grid = Grid {
            total: 1000,
            columns: 7,
            sample_rate: 1000,
        };
        let mut previous_end = 0;
        for c in 0..7 {
            let b = grid.bucket(c);
            assert!(b.start <= previous_end, "gap before column {c}");
            assert!(!b.is_empty());
            previous_end = b.end;
        }
        assert_eq!(previous_end, 1000);
        // A timeline shorter than the lane still gives every column a frame.
        let tiny = Grid {
            total: 3,
            columns: 10,
            sample_rate: 1000,
        };
        assert!((0..10).all(|c| !tiny.bucket(c).is_empty()));
    }

    #[test]
    fn tick_labels() {
        assert_eq!(tick_label(0, false), "0s");
        assert_eq!(tick_label(25, false), "2.5s");
        assert_eq!(tick_label(100, false), "10s");
        assert_eq!(tick_label(900, true), "1:30");
        assert_eq!(tick_label(905, true), "1:30.5");
    }

    #[test]
    fn interval_intersection() {
        let a = [Interval::new(0, 10), Interval::new(20, 30)];
        let b = [Interval::new(5, 25)];
        assert_eq!(
            intersect(&a, &b),
            vec![Interval::new(5, 10), Interval::new(20, 25)]
        );
        assert!(intersect(&a, &[]).is_empty());
    }

    #[test]
    fn empty_timeline_renders() {
        let grid = Grid {
            total: 0,
            columns: 20,
            sample_rate: 48_000,
        };
        let mut lines = Vec::new();
        ruler(&grid, 6, &mut lines);
        assert_eq!(lines[0].trim_end(), "time   0s");
        assert!(grid.bucket(3).is_empty());
    }
}
