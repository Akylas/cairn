//! Contour lines, from the elevation sources straight to vector tiles.
//!
//! This replaces `build_contours.sh`, which ran five programs over a GeoTIFF the area had to be
//! built into first: `gdal_translate` to cut it, `gdal_contour` to trace it, `ogr2ogr` twice to
//! reproject and relabel the lines, and tippecanoe to cut them into tiles, followed by a Python
//! script for the tile mask. Every stage wrote the whole area to disk for the next to read back.
//!
//! Here a tile is one unit of work and nothing is written but the archive. A worker samples the
//! elevation under a tile into a grid, traces it with marching squares ([`march`]), simplifies
//! the lines and encodes the tile ([`mvt`]); workers run in parallel over the tiles of a zoom,
//! and the finished tiles go to the archive a batch at a time ([`build`]). There is no GeoTIFF
//! to build first and no GDAL: the sources are the ones the terrain step reads, so the lines and
//! the hillshade come from the same ground.
//!
//! What the script produced is kept: a layer `contour` with `ele` and `div` on each line, a ten
//! metre interval, zooms 11 to 14, tippecanoe's simplification and tile buffer, the zoom filter
//! on `div`, the clip to the area's shape and the `tilemask` in the metadata. See [`keeps`] for
//! the filter and [`ContourOptions`] for the rest.

pub mod build;
pub mod march;
pub mod mvt;
pub mod tileset;

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::terrain::source::{CompositeSource, TileQuery};

/// Tile units across a tile, as tippecanoe's default detail (12) gives.
pub const EXTENT: u32 = 4096;

/// How far past the tile edge a line is kept, in tile units. Tippecanoe's default of 5 out of 256.
pub const BUFFER: u32 = 80;

/// Bumped whenever a change alters the bytes a tile is rendered to, so that a partial archive
/// written by an older renderer is not quietly finished by a newer one.
pub const RENDER_REVISION: u32 = 1;

/// Everything about the lines themselves, as opposed to where they are drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContourOptions {
    pub minzoom: u8,
    pub maxzoom: u8,
    /// Metres between two lines.
    pub interval: u32,
    /// The vector layer's name.
    pub layer: String,
    /// Douglas-Peucker tolerance in tile units (of [`EXTENT`]). Tippecanoe's `-S 3`.
    pub simplify: f64,
    /// The finest the elevation grid is sampled, in metres between samples. The grid is never
    /// finer than the ground under it can justify: sampling 30 m data every 5 m only draws the
    /// same slope through four times the points.
    pub resolution_m: f64,
    /// The most samples along one side of a tile, however fine `resolution_m` asks for.
    pub max_grid: u32,
    /// Metres over which a higher-priority source fades in; see the terrain option of the name.
    pub blur_m: f64,
    /// Zoom at which the shape's tile mask is stored in the archive's metadata.
    pub polyzoom: u8,
}

impl Default for ContourOptions {
    fn default() -> Self {
        Self {
            minzoom: 11,
            maxzoom: 14,
            interval: 10,
            layer: "contour".into(),
            simplify: 3.0,
            resolution_m: 10.0,
            max_grid: 256,
            blur_m: 1000.0,
            polyzoom: 11,
        }
    }
}

impl ContourOptions {
    /// Read the options out of a step's collected form values.
    ///
    /// An absent key leaves the default standing - the same rule every step's form follows - so
    /// an untouched form builds what `cairn contours` builds with no flags.
    pub fn from_values(values: &BTreeMap<String, Value>) -> Result<Self> {
        let mut opts = Self::default();
        let num = |key: &str| values.get(key).and_then(Value::as_f64);
        if let Some(v) = num("minzoom") {
            opts.minzoom = v as u8;
        }
        if let Some(v) = num("maxzoom") {
            opts.maxzoom = v as u8;
        }
        if let Some(v) = num("interval") {
            opts.interval = v as u32;
        }
        if let Some(v) = values.get("layer").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            opts.layer = v.to_string();
        }
        if let Some(v) = num("simplify") {
            opts.simplify = v;
        }
        if let Some(v) = num("resolution") {
            opts.resolution_m = v;
        }
        if let Some(v) = num("max_grid") {
            opts.max_grid = v as u32;
        }
        if let Some(v) = num("blur") {
            opts.blur_m = v;
        }
        if let Some(v) = num("polyzoom") {
            opts.polyzoom = v as u8;
        }
        opts.check()?;
        Ok(opts)
    }

    /// Refuse what cannot work, with the reason, before a build starts rather than during it.
    pub fn check(&self) -> Result<()> {
        if self.interval == 0 {
            return Err(anyhow!("the contour interval must be at least 1 m"));
        }
        if self.minzoom > self.maxzoom {
            return Err(anyhow!("min zoom {} is above max zoom {}", self.minzoom, self.maxzoom));
        }
        if self.maxzoom > 15 {
            return Err(anyhow!(
                "max zoom {} is past 15, where a tile is a few metres wide",
                self.maxzoom
            ));
        }
        if self.max_grid < 8 {
            return Err(anyhow!(
                "a grid of {} samples a side is too coarse to trace",
                self.max_grid
            ));
        }
        if self.resolution_m <= 0.0 {
            return Err(anyhow!("the sample resolution must be above 0 m"));
        }
        if self.layer.is_empty() {
            return Err(anyhow!("the layer needs a name"));
        }
        Ok(())
    }
}

/// The ladder `div` is read from: the largest of these that divides the elevation.
const LADDER: [u32; 8] = [1000, 500, 250, 200, 100, 50, 20, 10];

/// How important a line is, as the largest round number its elevation is a multiple of.
///
/// This is the `div` the shell script computed in SQL - 1000 for a multiple of 1000, then 500,
/// 250, 200, 100, 50, 20, and 10 for anything else - which styles read to draw index contours
/// heavier. It is the whole of what decides which lines survive at a low zoom ([`keeps`]).
///
/// The script's last rung was always 10 because its interval was; with another interval the last
/// rung is that interval, and a rung below the interval can never apply to a line on it.
pub fn division(elevation: i64, interval: u32) -> u32 {
    for rung in LADDER {
        if rung >= interval && elevation.rem_euclid(i64::from(rung)) == 0 {
            return rung;
        }
    }
    interval
}

/// Whether a line of importance `div` is drawn at `zoom`.
///
/// Tippecanoe was handed this as a `-j` filter on `$zoom` and `div`, and this is that filter
/// verbatim:
///
/// | zoom | kept |
/// | --- | --- |
/// | 0-6 | `div >= 1000` |
/// | 7 | `div >= 500` |
/// | 8-9 | `div >= 200`, except `div == 500` |
/// | 10-12 | `div >= 100` |
/// | 13 | `div >= 50` |
/// | 14 and up | everything |
///
/// The exception at zoom 8 and 9 reads as a slip - it drops the 500 m lines while keeping the
/// 250 m ones - but it is what every archive built so far contains, and the rule is a table so it
/// stays one line to change. With the default minimum zoom of 11 it never applies.
pub fn keeps(zoom: u8, div: u32) -> bool {
    match zoom {
        0..=6 => div >= 1000,
        7 => div >= 500,
        8..=9 => div >= 200 && div != 500,
        10..=12 => div >= 100,
        13 => div >= 50,
        _ => true,
    }
}

/// The elevation grid one tile is traced from, and how it maps onto the tile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridSpec {
    /// Cells along one side of the tile proper.
    pub cells: usize,
    /// Extra cells on each side, out into the buffer, so a line leaves the tile by its own
    /// grid and not by being cut.
    pub margin: usize,
}

impl GridSpec {
    /// Samples along one side, margin included.
    pub fn side(&self) -> usize {
        self.cells + 2 * self.margin + 1
    }

    /// Tile units per cell.
    pub fn units_per_cell(&self) -> f64 {
        f64::from(EXTENT) / self.cells as f64
    }
}

/// The grid for a zoom, from the latitude the job is centred on.
///
/// One grid size per zoom for the whole job, not one per tile row: two tiles that share an edge
/// must put their samples at the same places along it, or the lines meet with a small step where
/// they cross. The cost is that cells are not quite the requested size away from the centre
/// latitude, which over an area the size of a region is a few percent.
pub fn grid_for(opts: &ContourOptions, zoom: u8, centre_lat: f64) -> GridSpec {
    let tile_m = 40_075_016.686 * centre_lat.to_radians().cos() / f64::from(1u32 << zoom);
    let cells = ((tile_m / opts.resolution_m).round() as u32).clamp(8, opts.max_grid) as usize;
    let margin = (f64::from(BUFFER) / (f64::from(EXTENT) / cells as f64)).ceil().max(1.0) as usize;
    GridSpec { cells, margin }
}

/// The latitude to size grids at: the middle of the job, towards the equator if it straddles it.
pub fn centre_latitude(bounds: (f64, f64, f64, f64)) -> f64 {
    if bounds.1 <= 0.0 && bounds.3 >= 0.0 {
        0.0
    } else {
        // the middle of the span, not of the nearer edge, so a polar area is not sized for the pole
        (bounds.1 + bounds.3) / 2.0
    }
}

fn lat_of(world_y: f64) -> f64 {
    (std::f64::consts::PI * (1.0 - 2.0 * world_y)).sinh().atan().to_degrees()
}

/// What the sources have to know to read the grid of one tile.
pub fn tile_query(zoom: u8, x: u32, y: u32, grid: &GridSpec) -> TileQuery {
    let n = f64::from(1u32 << zoom);
    let span = grid.margin as f64 / grid.cells as f64;
    let (west, east) = (
        (f64::from(x) - span) / n * 360.0 - 180.0,
        (f64::from(x) + 1.0 + span) / n * 360.0 - 180.0,
    );
    let (north, south) =
        (lat_of((f64::from(y) - span) / n), lat_of((f64::from(y) + 1.0 + span) / n));
    let centre_lat = lat_of((f64::from(y) + 0.5) / n);
    let tile_m = 40_075_016.686 * centre_lat.to_radians().cos() / n;
    TileQuery {
        bounds: (west, south, east, north),
        centre_lat,
        target_m_per_px: tile_m / grid.cells as f64,
        size: grid.side() as u32,
    }
}

/// Sample the grid under a tile. `None` when no source has anything there.
///
/// The samples sit on a lattice that is global - `cells` to a tile, anchored at the tile's own
/// corner - so a neighbour, which has the same `cells`, samples exactly the same points along the
/// shared edge and traces exactly the same line through them.
pub fn sample_grid(
    source: &mut CompositeSource,
    zoom: u8,
    x: u32,
    y: u32,
    grid: &GridSpec,
    blur_m: f64,
) -> Option<Vec<f32>> {
    let query = tile_query(zoom, x, y, grid);
    // also pins the tiled sources' choice of zoom to this tile, which sampling relies on
    if !source.may_cover(&query) {
        return None;
    }
    let side = grid.side();
    let n = f64::from(1u32 << zoom);
    let at = |index: usize, origin: u32| {
        (f64::from(origin) + (index as f64 - grid.margin as f64) / grid.cells as f64) / n
    };
    let lons: Vec<f64> = (0..side).map(|i| at(i, x) * 360.0 - 180.0).collect();
    let mut values = vec![f32::NAN; side * side];
    let mut covered = false;
    for j in 0..side {
        let lat = lat_of(at(j, y));
        for (i, lon) in lons.iter().enumerate() {
            if let Some(elevation) = source.sample_blended(*lon, lat, query.target_m_per_px, blur_m)
            {
                values[j * side + i] = elevation;
                covered = true;
            }
        }
    }
    covered.then_some(values)
}

/// Trace one tile's lines and encode them, gzipped. `None` for a tile with no line in it.
pub fn render_tile(
    marcher: &mut march::Marcher,
    values: &[f32],
    grid: &GridSpec,
    zoom: u8,
    opts: &ContourOptions,
) -> Option<Vec<u8>> {
    let side = grid.side();
    let mut lines = Vec::new();
    let interval = i64::from(opts.interval);
    marcher.contours(
        &march::Grid { w: side, h: side, v: values },
        interval,
        &|level| keeps(zoom, division(level, opts.interval)),
        &mut lines,
    );

    let scale = grid.units_per_cell();
    let origin = grid.margin as f64;
    let extent = i32::try_from(EXTENT).unwrap_or(4096);
    let mut features = Vec::with_capacity(lines.len());
    for line in &lines {
        let mut points: Vec<(i32, i32)> = Vec::with_capacity(line.points.len());
        for (gx, gy) in &line.points {
            let point = (
                ((f64::from(*gx) - origin) * scale).round() as i32,
                ((f64::from(*gy) - origin) * scale).round() as i32,
            );
            if points.last() != Some(&point) {
                points.push(point);
            }
        }
        // a line that is only in the buffer is drawn by the tile it is really in
        let touches = points.iter().any(|p| p.0 >= 0 && p.0 <= extent && p.1 >= 0 && p.1 <= extent)
            || points.windows(2).any(|w| crosses_tile(w[0], w[1], extent));
        if !touches {
            continue;
        }
        let points = mvt::simplify(&points, opts.simplify);
        if points.len() < 2 {
            continue;
        }
        features.push(mvt::LineFeature {
            ele: line.level,
            div: i64::from(division(line.level, opts.interval)),
            points,
        });
    }
    if features.is_empty() {
        return None;
    }
    Some(mvt::gzip(&mvt::encode_tile(&opts.layer, EXTENT, &features)))
}

/// Whether a segment with both ends outside the tile still passes through it.
fn crosses_tile(a: (i32, i32), b: (i32, i32), extent: i32) -> bool {
    // the segment's box overlaps the tile and neither end is on the far side of a single edge
    let (lo_x, hi_x) = (a.0.min(b.0), a.0.max(b.0));
    let (lo_y, hi_y) = (a.1.min(b.1), a.1.max(b.1));
    if hi_x < 0 || lo_x > extent || hi_y < 0 || lo_y > extent {
        return false;
    }
    // a segment spanning the tile corner to corner has both ends outside it; cross-products
    // against the four corners say whether they all fall on one side of the line
    let side = |px: i32, py: i32| -> i64 {
        i64::from(b.0 - a.0) * i64::from(py - a.1) - i64::from(b.1 - a.1) * i64::from(px - a.0)
    };
    let signs = [side(0, 0), side(extent, 0), side(0, extent), side(extent, extent)];
    !(signs.iter().all(|s| *s > 0) || signs.iter().all(|s| *s < 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn division_is_the_script_ladder() {
        // from the SQL: 1000, 500, 250, 200, 100, 50, 20, else 10
        let cases = [
            (0, 1000),
            (2000, 1000),
            (500, 500),
            (1500, 500),
            (250, 250),
            (750, 250),
            (200, 200),
            (600, 200),
            (100, 100),
            (300, 100),
            (50, 50),
            (150, 50),
            (20, 20),
            (40, 20),
            (10, 10),
            (30, 10),
            (-10, 10),
            (-1000, 1000),
        ];
        for (elevation, want) in cases {
            assert_eq!(division(elevation, 10), want, "{elevation} m");
        }
    }

    #[test]
    fn division_with_another_interval() {
        // a 20 m interval has nothing at 10, and a line at 40 is still a multiple of 20
        assert_eq!(division(40, 20), 20);
        assert_eq!(division(100, 20), 100);
        // 25 m: only multiples of 25 exist, and the 250 rung still applies to some of them
        assert_eq!(division(25, 25), 25);
        assert_eq!(division(250, 25), 250);
        // a finer interval gets a rung of its own below the ladder
        assert_eq!(division(15, 5), 5);
        assert_eq!(division(10, 5), 10);
    }

    /// The `-j` filter handed to tippecanoe, as a table.
    #[test]
    fn the_zoom_filter_is_the_scripts() {
        let kept = |zoom: u8| -> Vec<u32> {
            [10, 20, 50, 100, 200, 250, 500, 1000].into_iter().filter(|d| keeps(zoom, *d)).collect()
        };
        assert_eq!(kept(0), vec![1000]);
        assert_eq!(kept(6), vec![1000]);
        assert_eq!(kept(7), vec![500, 1000]);
        assert_eq!(kept(8), vec![200, 250, 1000], "500 is the script's exception");
        assert_eq!(kept(9), vec![200, 250, 1000]);
        assert_eq!(kept(10), vec![100, 200, 250, 500, 1000]);
        assert_eq!(kept(11), vec![100, 200, 250, 500, 1000]);
        assert_eq!(kept(12), vec![100, 200, 250, 500, 1000]);
        assert_eq!(kept(13), vec![50, 100, 200, 250, 500, 1000]);
        assert_eq!(kept(14), vec![10, 20, 50, 100, 200, 250, 500, 1000]);
        assert_eq!(kept(15).len(), 8);
    }

    #[test]
    fn the_defaults_are_the_scripts() {
        let d = ContourOptions::default();
        assert_eq!((d.minzoom, d.maxzoom, d.interval, d.polyzoom), (11, 14, 10, 11));
        assert_eq!(d.layer, "contour");
        assert_eq!(d.simplify, 3.0, "tippecanoe -S 3");
        assert_eq!(
            f64::from(BUFFER) / f64::from(EXTENT),
            5.0 / 256.0,
            "tippecanoe's default buffer"
        );
    }

    #[test]
    fn options_are_checked() {
        let mut o = ContourOptions { interval: 0, ..Default::default() };
        assert!(o.check().is_err());
        o = ContourOptions { minzoom: 14, maxzoom: 11, ..Default::default() };
        assert!(o.check().is_err());
        assert!(ContourOptions::default().check().is_ok());
    }

    #[test]
    fn values_override_only_what_they_name() {
        let values: BTreeMap<String, Value> =
            [("maxzoom".to_string(), Value::from(13)), ("interval".to_string(), Value::from(20))]
                .into();
        let o = ContourOptions::from_values(&values).unwrap();
        assert_eq!((o.maxzoom, o.interval, o.minzoom), (13, 20, 11));
    }

    #[test]
    fn the_grid_follows_the_zoom_and_stops_at_its_cap() {
        let o = ContourOptions::default();
        let at = |zoom| grid_for(&o, zoom, 45.0).cells;
        // a z14 tile is 1.7 km across at 45 degrees: one sample per 10 m
        assert!((165..=175).contains(&at(14)), "{}", at(14));
        assert_eq!(at(11), 256, "capped");
        assert!(at(14) < at(13));
        let g = grid_for(&o, 14, 45.0);
        assert!(g.margin as f64 * g.units_per_cell() >= f64::from(BUFFER));
        assert_eq!(g.side(), g.cells + 2 * g.margin + 1);
    }

    #[test]
    fn a_segment_through_a_tile_counts_and_one_beside_it_does_not() {
        let e = 4096;
        assert!(crosses_tile((-100, 2000), (5000, 2000), e), "straight through");
        assert!(crosses_tile((-100, -100), (5000, 5000), e), "corner to corner");
        assert!(!crosses_tile((-100, -50), (-50, 5000), e), "alongside");
        assert!(!crosses_tile((-100, -100), (-10, -10), e), "in the buffer");
        // past the corner: the box overlaps the tile's but the line goes by it
        assert!(!crosses_tile((-2000, 3000), (3000, 8000), e));
    }

    #[test]
    fn centre_latitude_of_an_area() {
        assert_eq!(centre_latitude((5.0, 44.0, 7.0, 46.0)), 45.0);
        assert_eq!(centre_latitude((5.0, -10.0, 7.0, 10.0)), 0.0);
    }
}
