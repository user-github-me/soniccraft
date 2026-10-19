//! The speech that the `push` policy moves implicit cues past (DESIGN.md §3.4), in ordered structures.
//!
//! A cue at `s` with length `len` is blocked by an obstacle `o` when they overlap by more than the tolerance
//! `tol`. For `len > tol` and `o.end − o.start > tol` that is exactly `o.start < s + len − tol` and
//! `o.end > s + tol`; a cue or an obstacle that is not longer than the tolerance never blocks anything. The
//! cue moves past the latest-ending blocking obstacle (ties: the smallest node id), and among the obstacles
//! that start before `s + len − tol` that is the one with the greatest `(end, smaller node)`: a prefix maximum
//! over the obstacles ordered by start. If it ends after `s + tol` it blocks, otherwise nothing does.
//!
//! - [`Staircase`] holds the cues scheduled in the current pass, reduced to their prefix maxima ("records":
//!   the obstacles that end later than every obstacle starting before them). Records have increasing starts
//!   and increasing ends, a prefix-maximum query is a binary search, and an insertion is amortized
//!   `O(log n + CHUNK)`. Records are stored in chunks with aggregates over consecutive pairs, so that a push
//!   can skip a whole run of chained obstacles at once ([`Staircase::jump`]).
//! - [`SeedSet`] holds the intervals pre-seeded from the previous pass (explicit cues): a static segment tree
//!   whose seeds can be withdrawn (when their cue is scheduled, or while a cue they depend on is pushed).

use soniccraft_core::ir::NodeId;

use crate::types::Interval;

/// An obstacle: a speech interval that can block a pushed cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rec {
    pub(super) start: i64,
    pub(super) end: i64,
    pub(super) node: NodeId,
    /// Pre-seeded from the previous scheduling pass.
    pub(super) seed: bool,
}

impl Rec {
    fn key(&self) -> (i64, NodeId) {
        (self.start, self.node)
    }

    /// Which of two blocking obstacles a cue waits for: the later end, then the smaller node id.
    pub(super) fn beats(&self, other: &Rec) -> bool {
        self.end > other.end || (self.end == other.end && self.node < other.node)
    }
}

/// A seed of the next pass: an explicit cue's interval in the previous pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Seed {
    pub(super) node: NodeId,
    pub(super) interval: Interval,
    /// The interval depends on estimated durations.
    pub(super) est: bool,
}

/// Records per chunk before a chunk is split in two.
const CHUNK_MAX: usize = 128;

/// How many records before a slow pair [`Staircase::jump`] inspects for a barrier.
const BARRIER_SCAN: usize = 8;

/// The position of a record in a [`Staircase`]; invalidated by the next insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pos {
    chunk: usize,
    index: usize,
}

#[derive(Debug)]
struct Chunk {
    recs: Vec<Rec>,
    /// Over the pairs `(record, next record)` whose first record is in this chunk (the last pair reaches into
    /// the next chunk; the very last record has no pair): the largest `next.start − record.end`...
    max_hole: i64,
    /// ... and the smallest `next.end − record.end`.
    min_rise: i64,
}

/// The prefix maxima of the cues scheduled in a pass (see the module docs).
#[derive(Debug, Default)]
pub(super) struct Staircase {
    chunks: Vec<Chunk>,
}

impl Staircase {
    pub(super) fn get(&self, p: Pos) -> &Rec {
        &self.chunks[p.chunk].recs[p.index]
    }

    fn next(&self, p: Pos) -> Option<Pos> {
        if p.index + 1 < self.chunks[p.chunk].recs.len() {
            Some(Pos {
                chunk: p.chunk,
                index: p.index + 1,
            })
        } else {
            (p.chunk + 1 < self.chunks.len()).then_some(Pos {
                chunk: p.chunk + 1,
                index: 0,
            })
        }
    }

    fn prev(&self, p: Pos) -> Option<Pos> {
        if p.index > 0 {
            Some(Pos {
                chunk: p.chunk,
                index: p.index - 1,
            })
        } else if p.chunk > 0 {
            Some(Pos {
                chunk: p.chunk - 1,
                index: self.chunks[p.chunk - 1].recs.len() - 1,
            })
        } else {
            None
        }
    }

    /// The last record that starts before `x`: the latest-ending obstacle among those starting before `x`.
    pub(super) fn last_before(&self, x: i64) -> Option<Pos> {
        let c = self.chunks.partition_point(|ch| ch.recs[0].start < x);
        let chunk = c.checked_sub(1)?;
        let index = self.chunks[chunk].recs.partition_point(|r| r.start < x) - 1;
        Some(Pos { chunk, index })
    }

    /// [`Staircase::last_before`] for an `x` after the start of record `from`: an exponential search forward
    /// from `from`, first over chunks, then within the chunk. A push's next obstacle is usually only a few
    /// records ahead of the last one, so this avoids a search from the top (and its cache misses).
    pub(super) fn last_before_after(&self, from: Pos, x: i64) -> Pos {
        debug_assert!(self.get(from).start < x);
        let n = self.chunks.len();
        let (mut c, mut step) = (from.chunk, 1);
        while c + step < n && self.chunks[c + step].recs[0].start < x {
            c += step;
            step *= 2;
        }
        // The answer lies in chunks `c..c + step` (`c` starts before `x`).
        let end = (c + step).min(n);
        let chunk = c + self.chunks[c..end].partition_point(|ch| ch.recs[0].start < x) - 1;
        let recs = &self.chunks[chunk].recs;
        let (mut lo, mut step) = (if chunk == from.chunk { from.index } else { 0 }, 1);
        while lo + step < recs.len() && recs[lo + step].start < x {
            lo += step;
            step *= 2;
        }
        let end = (lo + step).min(recs.len());
        let index = lo + recs[lo..end].partition_point(|r| r.start < x) - 1;
        Pos { chunk, index }
    }

    fn last_key_below(&self, key: (i64, NodeId)) -> Option<Pos> {
        let c = self.chunks.partition_point(|ch| ch.recs[0].key() < key);
        let chunk = c.checked_sub(1)?;
        let index = self.chunks[chunk].recs.partition_point(|r| r.key() < key) - 1;
        Some(Pos { chunk, index })
    }

    /// Add a scheduled cue's interval. An obstacle that starts no earlier and ends no later than an existing
    /// record can never be the one a cue waits for, so it is dropped; the records the new one supersedes are
    /// removed.
    pub(super) fn insert(&mut self, rec: Rec) {
        let pred = self.last_key_below(rec.key());
        if let Some(p) = pred
            && !rec.beats(self.get(p))
        {
            return;
        }
        let (c, i) = match pred {
            Some(p) => (p.chunk, p.index + 1),
            None => (0, 0),
        };
        if self.chunks.is_empty() {
            self.chunks.push(Chunk {
                recs: vec![rec],
                max_hole: i64::MIN,
                min_rise: i64::MAX,
            });
            return;
        }
        self.chunks[c].recs.insert(i, rec);
        // The superseded records follow the new one directly (records' scores increase).
        let (mut cc, mut ii) = (c, i + 1);
        loop {
            let recs = &mut self.chunks[cc].recs;
            let dominated = recs[ii..].iter().take_while(|r| !r.beats(&rec)).count();
            recs.drain(ii..ii + dominated);
            if ii < recs.len() || cc + 1 == self.chunks.len() {
                break;
            }
            cc += 1;
            ii = 0;
        }
        // Chunks after `c` that lost all their records.
        let first_empty = c + 1;
        let mut end_empty = first_empty;
        while end_empty < self.chunks.len() && self.chunks[end_empty].recs.is_empty() {
            end_empty += 1;
        }
        self.chunks.drain(first_empty..end_empty);
        if self.chunks[c].recs.len() > CHUNK_MAX {
            let tail = self.chunks[c].recs.split_off(CHUNK_MAX / 2);
            self.chunks.insert(
                c + 1,
                Chunk {
                    recs: tail,
                    max_hole: i64::MIN,
                    min_rise: i64::MAX,
                },
            );
        }
        for k in c.saturating_sub(1)..(c + 3).min(self.chunks.len()) {
            self.refresh(k);
        }
    }

    /// Recompute the pair aggregates of chunk `c`.
    fn refresh(&mut self, c: usize) {
        let following = self.chunks.get(c + 1).map(|ch| ch.recs[0]);
        let chunk = &mut self.chunks[c];
        let (mut max_hole, mut min_rise) = (i64::MIN, i64::MAX);
        let pairs = chunk
            .recs
            .windows(2)
            .map(|w| (w[0], w[1]))
            .chain(chunk.recs.last().copied().zip(following));
        for (r, s) in pairs {
            max_hole = max_hole.max(s.start.saturating_sub(r.end));
            min_rise = min_rise.min(s.end.saturating_sub(r.end));
        }
        chunk.max_hole = max_hole;
        chunk.min_rise = min_rise;
    }

    /// Skip ahead along the push trajectory. A cue that has just been moved past record `from` (it now starts
    /// `gap` after `from.end`) next waits for the latest-ending record starting before `from.end + g`, where
    /// `g = gap + len − tol`, as long as that record ends more than `d = gap + tol` after `from.end`; then for
    /// the latest-ending one starting before that record's end plus `g`, and so on.
    ///
    /// Call a pair of consecutive records `(r, s)` *good* when `s.start − r.end < g` (the cue moved past `r` is
    /// blocked by `s`) and `s.end − r.end > d` (and moves past it). Across a run of good pairs the trajectory
    /// never stops and never jumps over the first record `z` whose following hole is at least `g`, where it
    /// stops. A *slow* pair (`s.end − r.end ≤ d`) can make it stop or skip, so before one the trajectory is only
    /// known to visit a *barrier*: a record `y` whose successor starts at least `g` after the end of `y`'s
    /// predecessor, which no earlier record's window reaches past.
    ///
    /// Returns a later record that the trajectory surely visits (where it stops, or a barrier), or None when no
    /// such shortcut is known. The caller continues stepping from there, so the result is exact either way.
    /// Obstacles outside the staircase (seeds) are the caller's concern.
    pub(super) fn jump(&self, from: Pos, g: i64, d: i64) -> Option<Pos> {
        let mut p = from;
        loop {
            let Some(q) = self.next(p) else {
                // The last record: nothing after it can block.
                return (p != from).then_some(p);
            };
            let (r, s) = (self.get(p), self.get(q));
            if s.start.saturating_sub(r.end) >= g {
                return (p != from).then_some(p);
            }
            if s.end.saturating_sub(r.end) <= d {
                return self.barrier(from, p, g);
            }
            p = q;
            if p.index == 0 {
                // Whole chunks of good pairs.
                while self.chunks[p.chunk].max_hole < g && self.chunks[p.chunk].min_rise > d {
                    if p.chunk + 1 == self.chunks.len() {
                        let index = self.chunks[p.chunk].recs.len() - 1;
                        return Some(Pos {
                            chunk: p.chunk,
                            index,
                        });
                    }
                    p = Pos {
                        chunk: p.chunk + 1,
                        index: 0,
                    };
                }
            }
        }
    }

    /// The last barrier in `(from, slow]` among the few records before the slow pair at `slow` (see
    /// [`Staircase::jump`]); every pair from `from` to `slow` is good.
    fn barrier(&self, from: Pos, slow: Pos, g: i64) -> Option<Pos> {
        let mut y = slow;
        for _ in 0..BARRIER_SCAN {
            if y == from {
                return None;
            }
            let prev = self.prev(y)?;
            let next = self.next(y)?;
            if self.get(next).start.saturating_sub(self.get(prev).end) >= g {
                return Some(y);
            }
            y = prev;
        }
        None
    }
}

/// No seed.
const NONE: u32 = u32::MAX;

/// The seeds of a pass: a segment tree over the seeds ordered by start that finds the latest-ending active
/// seed starting before a time.
#[derive(Debug)]
pub(super) struct SeedSet {
    recs: Vec<Rec>,
    /// Per tree node, the best active seed of its range (`NONE` for none).
    tree: Vec<u32>,
    size: usize,
}

impl SeedSet {
    fn new(mut recs: Vec<Rec>) -> Self {
        recs.sort_unstable_by_key(Rec::key);
        let size = recs.len().next_power_of_two();
        let mut set = SeedSet {
            tree: vec![NONE; 2 * size],
            recs,
            size,
        };
        for i in 0..set.recs.len() {
            set.tree[size + i] = i as u32;
        }
        for k in (1..size).rev() {
            set.tree[k] = set.better(set.tree[2 * k], set.tree[2 * k + 1]);
        }
        set
    }

    fn better(&self, a: u32, b: u32) -> u32 {
        match (a, b) {
            (NONE, x) | (x, NONE) => x,
            _ if self.recs[b as usize].beats(&self.recs[a as usize]) => b,
            _ => a,
        }
    }

    fn set_active(&mut self, i: usize, active: bool) {
        let mut k = self.size + i;
        self.tree[k] = if active { i as u32 } else { NONE };
        while k > 1 {
            k /= 2;
            self.tree[k] = self.better(self.tree[2 * k], self.tree[2 * k + 1]);
        }
    }

    /// The latest-ending active seed that starts before `x`.
    fn best_before(&self, x: i64) -> Option<usize> {
        let k = self.recs.partition_point(|r| r.start < x);
        let (mut l, mut r) = (self.size, self.size + k);
        let mut best = NONE;
        while l < r {
            if l & 1 == 1 {
                best = self.better(best, self.tree[l]);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                best = self.better(best, self.tree[r]);
            }
            l /= 2;
            r /= 2;
        }
        (best != NONE).then_some(best as usize)
    }
}

/// The obstacles of one scheduling pass.
#[derive(Debug)]
pub(super) struct Obstacles {
    tolerance: i64,
    stair: Staircase,
    seeds: SeedSet,
    /// Per node: the index of its seed in `seeds` (`NONE` when it has none or it can never block).
    seed_index: Vec<u32>,
    /// Per node: its seed is still in effect (the node is not scheduled yet); with the seed's estimate flag.
    seed_active: Vec<Option<bool>>,
    /// Seeds withdrawn while the current cue is pushed (they depend on it).
    withdrawn: Vec<usize>,
    /// Reference mode: every obstacle in one list, scanned on every move (seeds stay in it; `seed_active`
    /// says whether they still count).
    reference: Option<Vec<Rec>>,
    /// Speech intervals in effect (scheduled cues and active seeds), and how many of them depend on estimates.
    seen: usize,
    seen_est: usize,
}

impl Obstacles {
    pub(super) fn new(nodes: usize, seeds: &[Seed], tolerance: i64, reference: bool) -> Self {
        let mut seed_active = vec![None; nodes];
        let mut blocking = Vec::new();
        let (mut seen, mut seen_est) = (0, 0);
        for seed in seeds.iter().filter(|s| s.node < nodes) {
            if seed_active[seed.node].is_some() {
                continue;
            }
            seed_active[seed.node] = Some(seed.est);
            seen += 1;
            seen_est += usize::from(seed.est);
            blocking.push(Rec {
                start: seed.interval.start,
                end: seed.interval.end,
                node: seed.node,
                seed: true,
            });
        }
        let reference = reference.then(|| blocking.clone());
        // Seeds that can never block stay out of the tree.
        blocking.retain(|r| r.end.saturating_sub(r.start) > tolerance);
        let seeds = SeedSet::new(blocking);
        let mut seed_index = vec![NONE; nodes];
        for (i, r) in seeds.recs.iter().enumerate() {
            seed_index[r.node] = i as u32;
        }
        Obstacles {
            tolerance,
            stair: Staircase::default(),
            seeds,
            seed_index,
            seed_active,
            withdrawn: Vec::new(),
            reference,
            seen,
            seen_est,
        }
    }

    /// Speech intervals in effect, and how many of them depend on estimated durations.
    pub(super) fn counts(&self) -> (usize, usize) {
        (self.seen, self.seen_est)
    }

    pub(super) fn is_reference(&self) -> bool {
        self.reference.is_some()
    }

    pub(super) fn staircase(&self) -> &Staircase {
        &self.stair
    }

    /// Record a scheduled speech cue; it supersedes its own seed.
    pub(super) fn schedule(&mut self, node: NodeId, interval: Interval, est: bool) {
        if let Some(seed_est) = self.seed_active.get_mut(node).and_then(Option::take) {
            self.seen -= 1;
            self.seen_est -= usize::from(seed_est);
            let i = self.seed_index[node];
            if i != NONE {
                self.seeds.set_active(i as usize, false);
            }
        }
        self.seen += 1;
        self.seen_est += usize::from(est);
        let rec = Rec {
            start: interval.start,
            end: interval.end,
            node,
            seed: false,
        };
        match &mut self.reference {
            Some(all) => all.push(rec),
            None if interval.end.saturating_sub(interval.start) > self.tolerance => {
                self.stair.insert(rec);
            }
            None => {}
        }
    }

    /// The latest-ending seed in effect that starts before `x`, skipping (and withdrawing until
    /// [`Obstacles::restore_seeds`]) the seeds for which `depends(node)` is true.
    pub(super) fn seed_before(
        &mut self,
        x: i64,
        depends: &mut impl FnMut(NodeId) -> bool,
    ) -> Option<Rec> {
        loop {
            let i = self.seeds.best_before(x)?;
            let rec = self.seeds.recs[i];
            if depends(rec.node) {
                self.seeds.set_active(i, false);
                self.withdrawn.push(i);
                continue;
            }
            return Some(rec);
        }
    }

    /// Put back the seeds withdrawn by [`Obstacles::seed_before`].
    pub(super) fn restore_seeds(&mut self) {
        while let Some(i) = self.withdrawn.pop() {
            self.seeds.set_active(i, true);
        }
    }

    /// Reference mode: the obstacle a cue occupying `current` waits for (the latest-ending one overlapping it
    /// by more than the tolerance; ties: the smallest node id), scanning every obstacle in effect. Seeds for
    /// which `depends(node)` is true are ignored.
    pub(super) fn reference_hit(
        &self,
        v: NodeId,
        current: Interval,
        depends: &mut impl FnMut(NodeId) -> bool,
    ) -> Option<Rec> {
        let all = self.reference.as_deref()?;
        let tolerance = self.tolerance.unsigned_abs();
        let mut best: Option<Rec> = None;
        for o in all {
            let in_effect = !o.seed || self.seed_active[o.node].is_some();
            let interval = Interval::new(o.start, o.end);
            if o.node == v || !in_effect || current.overlap(&interval) <= tolerance {
                continue;
            }
            if best.is_some_and(|b| !o.beats(&b)) {
                continue;
            }
            if o.seed && depends(o.node) {
                continue;
            }
            best = Some(*o);
        }
        best
    }

    /// Reference mode: an upper bound on the moves of one cue.
    pub(super) fn reference_len(&self) -> usize {
        self.reference.as_ref().map_or(0, Vec::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64*, as in the integration tests.
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

        fn below(&mut self, n: i64) -> i64 {
            (self.next() % n.max(1).unsigned_abs()) as i64
        }
    }

    fn rec(start: i64, end: i64, node: NodeId) -> Rec {
        Rec {
            start,
            end,
            node,
            seed: false,
        }
    }

    /// The latest-ending (then smallest-node) obstacle starting before `x`, by brute force.
    fn naive_before(all: &[Rec], x: i64) -> Option<Rec> {
        all.iter()
            .filter(|r| r.start < x)
            .copied()
            .reduce(|a, b| if b.beats(&a) { b } else { a })
    }

    /// The push trajectory from a cue moved past `from` (see [`Staircase::jump`]), step by step: the record
    /// where it stops.
    fn naive_stop(all: &[Rec], from: Rec, g: i64, d: i64) -> Rec {
        let mut m = from;
        loop {
            let next = naive_before(all, m.end + g).expect("`m` itself starts before");
            if next.end <= m.end + d {
                return m;
            }
            m = next;
        }
    }

    #[test]
    fn the_staircase_answers_prefix_maxima() {
        let mut rng = Rng(0x5eed);
        for round in 0..200 {
            let mut stair = Staircase::default();
            let mut all = Vec::new();
            let count = 1 + rng.below(400) as usize;
            let spread = [50, 500, 5000][round % 3];
            for node in 0..count {
                let start = rng.below(spread);
                let r = rec(start, start + 1 + rng.below(120), node);
                all.push(r);
                stair.insert(r);
                if node % 7 == 0 {
                    for _ in 0..5 {
                        let x = rng.below(spread + 200) - 50;
                        let got = stair.last_before(x);
                        assert_eq!(
                            got.map(|p| *stair.get(p)),
                            naive_before(&all, x),
                            "round {round}, x {x}"
                        );
                        // The forward search from any earlier record finds the same one.
                        if let Some(p) = got {
                            let mut from = stair
                                .last_before(stair.get(p).start - rng.below(spread))
                                .unwrap_or(p);
                            if stair.get(from).start >= x {
                                from = p;
                            }
                            assert_eq!(stair.last_before_after(from, x), p, "round {round}, x {x}");
                        }
                    }
                }
            }
            // Records are strictly increasing in start order and in score.
            let recs: Vec<Rec> = stair.chunks.iter().flat_map(|c| c.recs.clone()).collect();
            for w in recs.windows(2) {
                assert!(w[0].key() < w[1].key() && w[1].beats(&w[0]), "{w:?}");
            }
            assert!(
                stair
                    .chunks
                    .iter()
                    .all(|c| !c.recs.is_empty() && c.recs.len() <= CHUNK_MAX)
            );
        }
    }

    #[test]
    fn jumps_land_on_the_trajectory() {
        let mut rng = Rng(0xfeed);
        for round in 0..300 {
            let mut stair = Staircase::default();
            let mut all = Vec::new();
            // Chains of cues (as the push policy builds them) mixed with stray overlapping ones.
            let mut t = 0;
            for node in 0..(1 + rng.below(600) as usize) {
                let len = 1 + rng.below(if round % 2 == 0 { 40 } else { 400 });
                let start = if rng.below(10) < 8 {
                    t + rng.below(30)
                } else {
                    (t - rng.below(200)).max(0)
                };
                t = t.max(start + len);
                let r = rec(start, start + len, node);
                all.push(r);
                stair.insert(r);
            }
            for _ in 0..40 {
                let g = 1 + rng.below(300);
                let d = rng.below(g.max(2));
                let x = rng.below(t + 10);
                let Some(from) = stair.last_before(x) else {
                    continue;
                };
                let expected = naive_stop(&all, *stair.get(from), g, d);
                // Follow the shortcuts and single steps the way a push does.
                let mut p = from;
                loop {
                    if let Some(q) = stair.jump(p, g, d) {
                        assert!(stair.get(q).end > stair.get(p).end);
                        p = q;
                    }
                    let m = *stair.get(p);
                    let next = stair.last_before(m.end + g).expect("non-empty");
                    if stair.get(next).end <= m.end + d {
                        break;
                    }
                    p = next;
                }
                assert_eq!(*stair.get(p), expected, "round {round}, g {g}, d {d}");
            }
        }
    }

    #[test]
    fn seeds_are_withdrawn_and_restored() {
        let seeds: Vec<Seed> = [(0, 0, 1000), (1, 500, 3000), (2, 2500, 2600), (3, 100, 110)]
            .iter()
            .map(|&(node, start, end)| Seed {
                node,
                interval: Interval::new(start, end),
                est: node == 1,
            })
            .collect();
        let mut obstacles = Obstacles::new(5, &seeds, 20, false);
        assert_eq!(obstacles.counts(), (4, 1));
        let mut never = |_: NodeId| false;
        assert_eq!(
            obstacles.seed_before(400, &mut never).map(|r| r.node),
            Some(0)
        );
        assert_eq!(
            obstacles.seed_before(4000, &mut never).map(|r| r.node),
            Some(1)
        );
        // A cue that seed 1 depends on ignores it until the seeds are restored.
        let mut on_one = |node: NodeId| node == 1;
        assert_eq!(
            obstacles.seed_before(4000, &mut on_one).map(|r| r.node),
            Some(2)
        );
        assert_eq!(
            obstacles.seed_before(4000, &mut never).map(|r| r.node),
            Some(2)
        );
        obstacles.restore_seeds();
        assert_eq!(
            obstacles.seed_before(4000, &mut never).map(|r| r.node),
            Some(1)
        );
        // Scheduling node 1 retires its seed; the scheduled interval goes to the staircase.
        obstacles.schedule(1, Interval::new(5000, 6000), false);
        assert_eq!(obstacles.counts(), (4, 0));
        assert_eq!(
            obstacles.seed_before(4000, &mut never).map(|r| r.node),
            Some(2)
        );
        let stair = obstacles.staircase();
        assert_eq!(stair.last_before(5001).map(|p| stair.get(p).node), Some(1));
        // Seed 3 (10 frames) can never block with a 20-frame tolerance.
        assert_eq!(
            obstacles.seed_before(120, &mut never).map(|r| r.node),
            Some(0)
        );
    }
}
