//! Marching squares over an elevation grid, with the segments chained into lines.
//!
//! Where `gdal_contour` walks one level at a time over the whole raster, this works a tile's grid
//! once: each cell reports the levels it spans, so the work is proportional to the number of
//! segments and not to grid size times level count. A tile spanning 600 m of relief at a 10 m
//! interval has sixty levels, and a cell crossed by two of them is visited once.
//!
//! # Conventions
//!
//! * A corner is *inside* a level when its value is `>=` the level. That makes a corner sitting
//!   exactly on a level unambiguous, and every edge a crossing for at most one side of it: the two
//!   cells sharing an edge always agree on whether it is crossed, which is what lets segments be
//!   chained by edge and never by comparing coordinates.
//! * A cell with a non-finite corner is skipped. The edge of the data is a ragged boundary, not a
//!   contour; drawing lines around it would outline every missing tile.
//! * Saddles - two opposite corners inside - are resolved by the cell's mean. That is the choice
//!   that keeps a line from crossing itself, and any consistent choice makes neighbouring tiles
//!   agree because the same grid points go through the same rule.

/// A grid of elevations, row 0 first. `f32::NAN` marks no data.
pub struct Grid<'a> {
    pub w: usize,
    pub h: usize,
    pub v: &'a [f32],
}

/// One contour line, in grid units: `x` along a row, `y` down the rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// The level, in metres.
    pub level: i64,
    pub points: Vec<(f32, f32)>,
}

const NONE: u32 = u32::MAX;

/// Scratch space for contouring, kept between tiles so a worker allocates once and not per tile.
#[derive(Default)]
pub struct Marcher {
    /// Segments of each level, as the pair of edges they connect.
    buckets: Vec<Vec<(u32, u32)>>,
    /// For each edge, up to two segments that end on it.
    link: Vec<[u32; 2]>,
    used: Vec<bool>,
}

impl Marcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Contour `grid` at every multiple of `interval` that `wanted` accepts, appending to `out`.
    pub fn contours(
        &mut self,
        grid: &Grid,
        interval: i64,
        wanted: &dyn Fn(i64) -> bool,
        out: &mut Vec<Line>,
    ) {
        let (w, h) = (grid.w, grid.h);
        if w < 2 || h < 2 || interval < 1 {
            return;
        }
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &value in grid.v {
            if value.is_finite() {
                lo = lo.min(value);
                hi = hi.max(value);
            }
        }
        if lo > hi {
            return;
        }
        let step = interval as f64;
        // the smallest multiple above the lowest value, and the largest at or below the highest
        let first = (lo as f64 / step).floor() as i64 + 1;
        let last = (hi as f64 / step).floor() as i64;
        if first > last {
            return;
        }
        let count = (last - first + 1) as usize;
        let accepted: Vec<bool> = (first..=last).map(|i| wanted(i * interval)).collect();
        if !accepted.iter().any(|a| *a) {
            return;
        }

        if self.buckets.len() < count {
            self.buckets.resize_with(count, Vec::new);
        }
        for bucket in &mut self.buckets[..count] {
            bucket.clear();
        }

        // Edge numbering: every horizontal edge first, row by row, then every vertical one.
        let horizontal = h * (w - 1);
        let edges = horizontal + (h - 1) * w;
        let v = grid.v;

        for j in 0..h - 1 {
            for i in 0..w - 1 {
                let a = v[j * w + i]; // top left
                let b = v[j * w + i + 1]; // top right
                let c = v[(j + 1) * w + i + 1]; // bottom right
                let d = v[(j + 1) * w + i]; // bottom left
                if !(a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite()) {
                    continue;
                }
                let low = a.min(b).min(c).min(d);
                let high = a.max(b).max(c).max(d);
                let from = ((low as f64 / step).floor() as i64 + 1).max(first);
                let to = ((high as f64 / step).floor() as i64).min(last);
                if from > to {
                    continue;
                }
                let top = (j * (w - 1) + i) as u32;
                let bottom = ((j + 1) * (w - 1) + i) as u32;
                let left = (horizontal + j * w + i) as u32;
                let right = (horizontal + j * w + i + 1) as u32;

                for index in from..=to {
                    let slot = (index - first) as usize;
                    if !accepted[slot] {
                        continue;
                    }
                    let level = (index * interval) as f32;
                    let mask = (a >= level) as u8
                        | ((b >= level) as u8) << 1
                        | ((c >= level) as u8) << 2
                        | ((d >= level) as u8) << 3;
                    let bucket = &mut self.buckets[slot];
                    match mask {
                        0 | 15 => {}
                        1 | 14 => bucket.push((left, top)),
                        2 | 13 => bucket.push((top, right)),
                        4 | 11 => bucket.push((right, bottom)),
                        8 | 7 => bucket.push((bottom, left)),
                        3 | 12 => bucket.push((left, right)),
                        6 | 9 => bucket.push((top, bottom)),
                        5 | 10 => {
                            let centre_inside = (a + b + c + d) / 4.0 >= level;
                            // 5: top left and bottom right in. 10: the other diagonal.
                            let joined = centre_inside == (mask == 5);
                            if joined {
                                // the outside corners are the ones cut off
                                bucket.push((top, right));
                                bucket.push((bottom, left));
                            } else {
                                bucket.push((left, top));
                                bucket.push((right, bottom));
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        if self.link.len() < edges {
            self.link.resize(edges, [NONE, NONE]);
        }

        for slot in 0..count {
            if self.buckets[slot].is_empty() {
                continue;
            }
            let level = (first + slot as i64) * interval;
            self.chain(slot, level, grid, horizontal, out);
        }
    }

    /// Join one level's segments end to end into lines.
    fn chain(
        &mut self,
        slot: usize,
        level: i64,
        grid: &Grid,
        horizontal: usize,
        out: &mut Vec<Line>,
    ) {
        let segments = std::mem::take(&mut self.buckets[slot]);
        for (index, (a, b)) in segments.iter().enumerate() {
            for edge in [*a, *b] {
                let ends = &mut self.link[edge as usize];
                if ends[0] == NONE {
                    ends[0] = index as u32;
                } else {
                    ends[1] = index as u32;
                }
            }
        }
        self.used.clear();
        self.used.resize(segments.len(), false);

        // the segment on the far side of `edge` from `from`, and the edge it leaves by
        let step = |link: &[[u32; 2]], used: &[bool], edge: u32| -> Option<(u32, u32)> {
            for candidate in link[edge as usize] {
                if candidate == NONE || used[candidate as usize] {
                    continue;
                }
                let (p, q) = segments[candidate as usize];
                return Some((candidate, if p == edge { q } else { p }));
            }
            None
        };

        for (start, &(first, second)) in segments.iter().enumerate() {
            if self.used[start] {
                continue;
            }
            self.used[start] = true;
            let mut tail = vec![first, second];
            // forwards from the second edge, until the line ends or comes back round
            let mut at = second;
            while let Some((next, leaving)) = step(&self.link, &self.used, at) {
                self.used[next as usize] = true;
                tail.push(leaving);
                at = leaving;
            }
            // and backwards from the first, for a line that was started in its middle
            let mut head = Vec::new();
            let mut at = first;
            while let Some((next, leaving)) = step(&self.link, &self.used, at) {
                self.used[next as usize] = true;
                head.push(leaving);
                at = leaving;
            }
            head.reverse();
            head.extend(tail);

            let points = head
                .iter()
                .map(|edge| edge_point(*edge as usize, level as f32, grid, horizontal))
                .collect();
            out.push(Line { level, points });
        }

        for (a, b) in &segments {
            self.link[*a as usize] = [NONE, NONE];
            self.link[*b as usize] = [NONE, NONE];
        }
        // hand the allocation back for the next tile
        self.buckets[slot] = segments;
    }
}

/// Where a level crosses an edge, in grid units.
fn edge_point(edge: usize, level: f32, grid: &Grid, horizontal: usize) -> (f32, f32) {
    let (w, v) = (grid.w, grid.v);
    if edge < horizontal {
        let (j, i) = (edge / (w - 1), edge % (w - 1));
        let (a, b) = (v[j * w + i], v[j * w + i + 1]);
        (i as f32 + (level - a) / (b - a), j as f32)
    } else {
        let k = edge - horizontal;
        let (j, i) = (k / w, k % w);
        let (a, b) = (v[j * w + i], v[(j + 1) * w + i]);
        (i as f32, j as f32 + (level - a) / (b - a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(w: usize, h: usize, v: &[f32], interval: i64) -> Vec<Line> {
        let mut out = Vec::new();
        Marcher::new().contours(&Grid { w, h, v }, interval, &|_| true, &mut out);
        out
    }

    /// A slope rising to the east: every level is one straight vertical line.
    #[test]
    fn a_ramp_gives_straight_lines() {
        // 5 columns, values 0, 10, 20, 30, 40 along each of 4 rows
        let v: Vec<f32> = (0..4).flat_map(|_| [0.0, 10.0, 20.0, 30.0, 40.0]).collect();
        let lines = run(5, 4, &v, 10);
        // 10, 20, 30, 40 - the level equal to the highest value sits on the last column
        let levels: Vec<i64> = lines.iter().map(|l| l.level).collect();
        assert_eq!(levels, vec![10, 20, 30, 40]);
        for line in &lines {
            let column = line.level as f32 / 10.0;
            assert!(line.points.iter().all(|p| (p.0 - column).abs() < 1e-5), "{line:?}");
            assert_eq!(line.points.len(), 4, "one point per row, in one line: {line:?}");
        }
    }

    /// A single hill: the contour is one closed ring.
    #[test]
    fn a_peak_gives_a_closed_ring() {
        let mut v = vec![0.0f32; 7 * 7];
        v[3 * 7 + 3] = 25.0;
        v[3 * 7 + 2] = 12.0;
        v[3 * 7 + 4] = 12.0;
        v[2 * 7 + 3] = 12.0;
        v[4 * 7 + 3] = 12.0;
        let lines = run(7, 7, &v, 10);
        let ten: Vec<&Line> = lines.iter().filter(|l| l.level == 10).collect();
        assert_eq!(ten.len(), 1, "{lines:?}");
        let ring = &ten[0].points;
        assert_eq!(ring.first(), ring.last(), "the ring is closed");
        let twenty = lines.iter().filter(|l| l.level == 20).count();
        assert_eq!(twenty, 1);
    }

    /// Two opposite corners high and two low is ambiguous: the mean says whether the high ones are
    /// joined across the cell or cut off from each other.
    #[test]
    fn a_saddle_is_split_by_the_mean() {
        let v = vec![20.0, 0.0, 0.0, 20.0];
        // mean 10: on the level, so inside - the two high corners are joined, and each low corner
        // is cut off by its own arc
        let at_ten = run(2, 2, &v, 10).into_iter().filter(|l| l.level == 10).count();
        assert_eq!(at_ten, 2);
        // at 20 the mean is below: the high corners are separate hills, each cut off alone
        let high: Vec<Line> = run(2, 2, &v, 10).into_iter().filter(|l| l.level == 20).collect();
        assert_eq!(high.len(), 2);
        // and a level between that sees the mean the other way round gives the other pairing
        let low = vec![20.0, 0.0, 0.0, 20.0];
        let lines = run(2, 2, &low, 5);
        let at_fifteen: Vec<&Line> = lines.iter().filter(|l| l.level == 15).collect();
        assert_eq!(at_fifteen.len(), 2);
        // 15 > mean 10: the high corners are cut off, so each arc hugs a high corner
        assert!(at_fifteen.iter().any(|l| l.points.iter().all(|p| p.0 < 1.0 && p.1 < 1.0)));
    }

    /// A line that only brushes a corner is a point, not a feature.
    #[test]
    fn a_peak_exactly_on_a_level_is_a_degenerate_line() {
        let v = vec![
            0.0, 0.0, 0.0, //
            0.0, 10.0, 0.0, //
            0.0, 0.0, 0.0,
        ];
        let lines = run(3, 3, &v, 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].points.iter().all(|p| *p == (1.0, 1.0)), "{lines:?}");
    }

    #[test]
    fn missing_data_makes_no_lines_around_it() {
        let nan = f32::NAN;
        let v = vec![
            nan, nan, nan, //
            nan, 5.0, 15.0, //
            nan, 5.0, 15.0,
        ];
        let lines = run(3, 3, &v, 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].points.iter().all(|p| p.0 > 1.0 && p.1 >= 1.0), "{lines:?}");
    }

    /// The two cells either side of an edge must agree, or a line would stop in the middle of a
    /// grid. A value exactly on a level is the case that breaks a careless `>`.
    #[test]
    fn a_value_exactly_on_a_level_does_not_break_the_line() {
        let v = vec![
            0.0, 10.0, 20.0, //
            0.0, 10.0, 20.0, //
            0.0, 10.0, 20.0,
        ];
        let lines = run(3, 3, &v, 10);
        let ten: Vec<&Line> = lines.iter().filter(|l| l.level == 10).collect();
        assert_eq!(ten.len(), 1, "{lines:?}");
        assert_eq!(ten[0].points.len(), 3);
    }

    #[test]
    fn levels_that_are_not_wanted_are_not_traced() {
        let v: Vec<f32> = (0..3).flat_map(|_| [0.0, 10.0, 20.0, 30.0]).collect();
        let mut out = Vec::new();
        Marcher::new().contours(&Grid { w: 4, h: 3, v: &v }, 10, &|l| l % 20 == 0, &mut out);
        assert_eq!(out.iter().map(|l| l.level).collect::<Vec<_>>(), vec![20]);
    }

    #[test]
    fn negative_elevations_have_levels_too() {
        let v: Vec<f32> = (0..3).flat_map(|_| [-25.0, -5.0, 15.0]).collect();
        let lines = run(3, 3, &v, 10);
        assert_eq!(lines.iter().map(|l| l.level).collect::<Vec<_>>(), vec![-20, -10, 0, 10]);
    }

    /// The scratch is reused tile after tile, so stale links from one grid must not leak into the
    /// next.
    #[test]
    fn the_scratch_space_can_be_reused() {
        let v: Vec<f32> = (0..4).flat_map(|_| [0.0, 10.0, 20.0, 30.0]).collect();
        let mut marcher = Marcher::new();
        let mut first = Vec::new();
        marcher.contours(&Grid { w: 4, h: 4, v: &v }, 10, &|_| true, &mut first);
        let mut second = Vec::new();
        marcher.contours(&Grid { w: 4, h: 4, v: &v }, 10, &|_| true, &mut second);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }
}
