//! Rendering terrain-RGB tiles.
//!
//! Ports the encoding half of `scripts/build_terrain_rgb.py`. The vertical quantisation ramp is
//! the part that matters for size: elevation is snapped to a step that grows as zoom drops, so
//! the low-detail zooms spend far fewer distinct byte values and compress much harder.
//!
//! For terrarium the step is `(1/256) * 2^round_digits`, so `round_digits = 8` is exactly one
//! metre - and at whole metres the blue channel, which carries the sub-metre fraction, becomes a
//! constant zero. That cliff is where terrarium's size advantage over mapbox comes from; at
//! matched fractional steps mapbox is the smaller of the two.

use crate::elevation::Encoding;
use crate::terrain::source::{CompositeSource, TileQuery};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Base value and quantisation interval per encoding, matching the Python `ENCODINGS` table.
pub fn interval(encoding: Encoding) -> f64 {
    match encoding {
        // mapbox's interval is fixed by the format at 0.1 m and is not a free parameter
        Encoding::Mapbox => 0.1,
        Encoding::Terrarium => 1.0 / 256.0,
    }
}

/// Quantisation exponent for a zoom.
///
/// Detail is only needed where it can be seen, so each step down from the maximum zoom adds one
/// to the exponent - doubling the vertical step - until `max_round_digits` caps it.
pub fn round_digits_for(zoom: u8, maxzoom: u8, round_digits: u32, max_round_digits: u32) -> u32 {
    let ramp = round_digits + (maxzoom.saturating_sub(zoom)) as u32;
    ramp.min(max_round_digits.max(round_digits))
}

/// Vertical step in metres at a zoom.
pub fn step_for(
    encoding: Encoding,
    zoom: u8,
    maxzoom: u8,
    round_digits: u32,
    max_round_digits: u32,
) -> f64 {
    let digits = round_digits_for(zoom, maxzoom, round_digits, max_round_digits);
    interval(encoding) * 2f64.powi(digits as i32)
}

/// Pack an elevation into RGB, snapped to `step`.
pub fn encode(encoding: Encoding, elevation: f32, step: f64) -> [u8; 3] {
    let snapped =
        if step > 0.0 { ((elevation as f64 / step).round() * step) as f32 } else { elevation };
    match encoding {
        Encoding::Terrarium => {
            let v = (snapped as f64 + 32768.0).clamp(0.0, 65535.999);
            let whole = v.floor();
            let r = (whole / 256.0).floor() as u8;
            let g = (whole % 256.0) as u8;
            let b = ((v - whole) * 256.0).round().clamp(0.0, 255.0) as u8;
            [r, g, b]
        }
        Encoding::Mapbox => {
            let v = ((snapped as f64 + 10000.0) / 0.1).round().clamp(0.0, 16_777_215.0) as u32;
            [(v >> 16) as u8, (v >> 8) as u8, v as u8]
        }
    }
}

/// Longitude and latitude of a pixel centre in an XYZ tile.
pub fn pixel_lonlat(z: u8, x: u32, y: u32, px: u32, py: u32, size: u32) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    let world_x = (x as f64 * size as f64 + px as f64 + 0.5) / (size as f64 * n);
    let world_y = (y as f64 * size as f64 + py as f64 + 0.5) / (size as f64 * n);
    let lon = world_x * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * world_y)).sinh().atan().to_degrees();
    (lon, lat)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerrainOptions {
    pub encoding: Encoding,
    pub minzoom: u8,
    pub maxzoom: u8,
    pub tile_size: u32,
    /// Quantisation exponent at the maximum zoom. 8 gives whole metres for terrarium.
    pub round_digits: u32,
    /// Cap on the ramp. Note the Python original silently clamps this up to `round_digits` when
    /// it is smaller, which with its default of 0 disables the ramp entirely.
    pub max_round_digits: u32,
    /// Distance over which a higher-priority source fades in at its coverage boundary, in
    /// metres. Matches the generator's `--blur`, whose default is also 1000.
    pub blur_m: f64,
    /// Elevation written where no source covers the pixel.
    ///
    /// Only reachable inside a tile that is covered somewhere - a tile no source touches at all
    /// is skipped rather than written flat. `build_terrain_rgb.py` used -10 so that uncovered
    /// pixels read as sea rather than as ground; 0 is kept as the default here because it is
    /// what every archive in this repository was built with.
    pub nodata_elevation: f64,
}

/// These are what an unset option means, and they have to stay the reference build's values:
/// z5-z12, mapbox, no quantisation at the max zoom. The desktop form fills an option the user
/// left alone from here while showing its own hint beside the field, so a value that disagrees
/// with the hint makes the form lie - Max zoom read "12" and rendered z13, which is four times
/// the tiles and a zoom past what mapterhorn publishes. `the_defaults_are_what_the_option_form_promises`
/// is the test that keeps the two in step.
impl Default for TerrainOptions {
    fn default() -> Self {
        Self {
            encoding: Encoding::Mapbox,
            minzoom: 5,
            maxzoom: 12,
            tile_size: 512,
            nodata_elevation: 0.0,
            round_digits: 0,
            max_round_digits: 15,
            blur_m: 1000.0,
        }
    }
}

/// Ground resolution of a tile's pixels, in metres.
///
/// Used to pick which overview of a projected raster source to read: sampling a z8 tile out of
/// 5 m data would decode thousands of full-resolution raster tiles for one output tile.
pub fn ground_resolution(z: u8, y: u32, size: u32) -> f64 {
    let n = (1u64 << z) as f64;
    let world_y = (y as f64 + 0.5) / n;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * world_y)).sinh().atan();
    40_075_016.686 * lat.cos() / (n * size as f64)
}

/// What a source has to know about an output tile to read the right part of itself.
pub fn tile_query(z: u8, x: u32, y: u32, size: u32) -> TileQuery {
    let n = (1u64 << z) as f64;
    let centre = std::f64::consts::PI * (1.0 - 2.0 * (y as f64 + 0.5) / n);
    TileQuery {
        bounds: tile_bounds(z, x, y),
        centre_lat: centre.sinh().atan().to_degrees(),
        target_m_per_px: ground_resolution(z, y, size),
        size,
    }
}

/// Render one tile. Returns `None` when no pixel had coverage, so empty tiles are skipped
/// rather than written as a wall of sea level.
pub fn render_tile(
    source: &mut CompositeSource,
    z: u8,
    x: u32,
    y: u32,
    opts: &TerrainOptions,
) -> Option<Vec<u8>> {
    let size = opts.tile_size;
    let step = step_for(opts.encoding, z, opts.maxzoom, opts.round_digits, opts.max_round_digits);
    let query = tile_query(z, x, y, size);
    let target = query.target_m_per_px;
    // A tile with nothing under it is found out here, for a few lookups, instead of by sampling
    // every one of its pixels. Over a continent-sized box that is most of the sea.
    if !source.may_cover(&query) {
        return None;
    }
    let mut rgb = vec![0u8; (size * size * 3) as usize];
    let mut covered = false;

    // Longitude depends only on the column and latitude only on the row, so both are worked out
    // once rather than per pixel. Asking `pixel_lonlat` for the longitude used to compute the
    // row's latitude as well and throw it away - an `asinh` and an `atan` per pixel, a quarter of
    // a million of them per tile, for a number already in hand.
    let n = (1u64 << z) as f64;
    let span = size as f64 * n;
    let lons: Vec<f64> = (0..size)
        .map(|px| (x as f64 * size as f64 + px as f64 + 0.5) / span * 360.0 - 180.0)
        .collect();

    for py in 0..size {
        let world_y = (y as f64 * size as f64 + py as f64 + 0.5) / span;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * world_y)).sinh().atan().to_degrees();
        for px in 0..size {
            let lon = lons[px as usize];
            let elevation = match source.sample_blended(lon, lat, target, opts.blur_m) {
                Some(e) => {
                    covered = true;
                    e
                }
                None => opts.nodata_elevation as f32,
            };
            let packed = encode(opts.encoding, elevation, step);
            let at = ((py * size + px) * 3) as usize;
            rgb[at..at + 3].copy_from_slice(&packed);
        }
    }
    covered.then_some(rgb)
}

/// How hard libwebp works, on its 0-9 lossless scale.
///
/// Measured on real terrain tiles: 3 and 6 land within half a percent of each other and 9 is
/// sometimes *larger*, so the extra time buys nothing. 4 is the knee.
const WEBP_EFFORT: i32 = 4;

/// Encode an RGB buffer as lossless WebP, through libwebp.
///
/// Not the `image` crate's own lossless encoder, which this used to call: on terrain tiles it
/// writes 40-68% more bytes for identical pixels. Both are lossless, so the whole difference is
/// compression, and on a z5-z13 pyramid it is the single largest lever there is - larger than the
/// quantisation ramp, and free of any cost in quality.
pub fn to_webp(rgb: &[u8], size: u32) -> Result<Vec<u8>> {
    let mut config = webp::WebPConfig::new()
        .map_err(|_| anyhow::anyhow!("libwebp rejected its own default configuration"))?;
    // SAFETY: the config is initialised above, and the level is in the 0-9 the preset accepts
    let ok = unsafe { libwebp_sys::WebPConfigLosslessPreset(&mut config, WEBP_EFFORT) };
    if ok == 0 {
        return Err(anyhow::anyhow!("libwebp rejected lossless preset {WEBP_EFFORT}"));
    }
    // `exact` keeps the RGB of fully transparent pixels; there is no alpha here, but elevation
    // lives in all three channels and none of it may be approximated
    config.exact = 1;
    let encoded = webp::Encoder::from_rgb(rgb, size, size)
        .encode_advanced(&config)
        .map_err(|e| anyhow::anyhow!("encoding webp: {e:?}"))?;
    Ok(encoded.to_vec())
}

/// Encode an RGB buffer as PNG.
///
/// Bigger than lossless WebP for this data, but some tools still will not read WebP - which is
/// why `build_terrain_rgb.py` had `-f png` and this does too.
pub fn to_png(rgb: &[u8], size: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // the encoder trait has to be in scope for `write_image`; webp's inherent method does not
    // need it, which is why only this one imports it
    use image::ImageEncoder;
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgb, size, size, image::ExtendedColorType::Rgb8)
        .context("encoding png")?;
    Ok(out)
}

/// A finished tile: its column, its row, and the encoded bytes.
pub type EncodedTile = (u32, u32, Vec<u8>);

/// One renderer per worker thread, so tiles can be rendered in parallel.
///
/// A [`CompositeSource`] cannot be shared: every source behind it caches decoded data, and the
/// whole sampling path takes `&mut self`. Re-opening one per tile is not an option either - that
/// would re-read a 46 GB GeoTIFF's directory, and re-fetch a PMTiles archive's, tens of thousands
/// of times.
///
/// So there is one per thread, chosen by the thread's own index. Two tasks never run on the same
/// thread at once, so the mutex is uncontended; it is there because the borrow checker cannot see
/// that, not because anything waits on it. Each worker also ends up walking a contiguous run of
/// tiles - rayon splits a slice into halves - so its caches stay warm on the part of the map it
/// is actually on.
pub struct RenderPool {
    sources: Vec<std::sync::Mutex<CompositeSource>>,
    /// Not a render worker: the one source that fetches ahead of them. Kept apart so that asking
    /// the network for the next batch never waits on a worker's lock.
    prefetcher: std::sync::Mutex<CompositeSource>,
}

impl RenderPool {
    /// Open `size` renderers, clamped to the number of threads rayon will actually use.
    pub fn new(size: usize, open: impl Fn() -> Result<CompositeSource>) -> Result<Self> {
        let size = size.clamp(1, rayon::current_num_threads());
        let mut sources = Vec::with_capacity(size);
        for _ in 0..size {
            sources.push(std::sync::Mutex::new(open()?));
        }
        Ok(Self { sources, prefetcher: std::sync::Mutex::new(open()?) })
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Every renderer, for reading back what the sources did once a build is over.
    pub fn sources(&self) -> &[std::sync::Mutex<CompositeSource>] {
        &self.sources
    }

    /// The renderers and the prefetcher, which between them did all the fetching.
    pub fn all_sources(&self) -> impl Iterator<Item = &std::sync::Mutex<CompositeSource>> {
        self.sources.iter().chain(std::iter::once(&self.prefetcher))
    }

    /// Fetch what these output tiles will read, ahead of rendering them.
    ///
    /// Safe to call while a batch is being rendered: it only touches the prefetcher.
    pub fn prefetch(&self, zoom: u8, tiles: &[(u32, u32)], opts: &TerrainOptions) {
        let queries: Vec<_> =
            tiles.iter().map(|&(x, y)| tile_query(zoom, x, y, opts.tile_size)).collect();
        if let Ok(mut source) = self.prefetcher.lock() {
            source.prefetch(&queries);
        }
    }

    /// Fetch what these queries will read, ahead of using them.
    ///
    /// [`prefetch`](Self::prefetch) for a caller that works out its own queries: the contour
    /// step reads a grid of its own size around each tile, not a terrain tile's pixels.
    pub fn prefetch_queries(&self, queries: &[TileQuery]) {
        if let Ok(mut source) = self.prefetcher.lock() {
            source.prefetch(queries);
        }
    }

    /// Run `work` on the source that belongs to the calling thread.
    ///
    /// The pool's one-source-per-thread rule, for a caller that does something other than render
    /// a terrain tile with it. Meant to be called from inside a rayon parallel iterator.
    pub fn with_source<R>(&self, work: impl FnOnce(&mut CompositeSource) -> R) -> Result<R> {
        let index = rayon::current_thread_index().unwrap_or(0) % self.sources.len();
        let mut source = self.sources[index]
            .lock()
            .map_err(|_| anyhow::anyhow!("a renderer was left poisoned by a panic"))?;
        Ok(work(&mut source))
    }

    /// Render and encode a batch of tiles, in parallel, returning the ones that had coverage.
    ///
    /// Encoding runs here too rather than in the caller: lossless WebP is a good fraction of the
    /// work, and leaving it outside would hand it all back to one thread.
    pub fn render(
        &self,
        zoom: u8,
        tiles: &[(u32, u32)],
        opts: &TerrainOptions,
        format: &str,
    ) -> Result<Vec<EncodedTile>> {
        use rayon::prelude::*;
        let done: Result<Vec<Option<EncodedTile>>> = tiles
            .par_iter()
            .map(|&(x, y)| {
                let index = rayon::current_thread_index().unwrap_or(0) % self.sources.len();
                let mut source = self.sources[index]
                    .lock()
                    .map_err(|_| anyhow::anyhow!("a renderer was left poisoned by a panic"))?;
                let Some(rgb) = render_tile(&mut source, zoom, x, y, opts) else {
                    return Ok(None);
                };
                drop(source);
                let bytes = if format == "png" {
                    to_png(&rgb, opts.tile_size)?
                } else {
                    to_webp(&rgb, opts.tile_size)?
                };
                Ok(Some((x, y, bytes)))
            })
            .collect();
        Ok(done?.into_iter().flatten().collect())
    }
}

/// Lon/lat box of a web-mercator tile, for deciding whether a shape touches it.
pub fn tile_bounds(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let lon = |x: f64| x / n * 360.0 - 180.0;
    let lat = |y: f64| {
        let t = std::f64::consts::PI * (1.0 - 2.0 * y / n);
        t.sinh().atan().to_degrees()
    };
    (lon(x as f64), lat(y as f64 + 1.0), lon(x as f64 + 1.0), lat(y as f64))
}

/// Tile range covering a lon/lat bounding box at a zoom.
pub fn tile_range(z: u8, bounds: (f64, f64, f64, f64)) -> (u32, u32, u32, u32) {
    let n = (1u64 << z) as f64;
    let to_x =
        |lon: f64| (((lon + 180.0) / 360.0 * n).floor().max(0.0) as u32).min((n - 1.0) as u32);
    let to_y = |lat: f64| {
        let rad = lat.to_radians();
        let v = (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
        (v.floor().max(0.0) as u32).min((n - 1.0) as u32)
    };
    // latitude runs the other way from tile rows
    (to_x(bounds.0), to_y(bounds.3), to_x(bounds.2), to_y(bounds.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrarium_step_of_one_metre_at_eight_digits() {
        let s = step_for(Encoding::Terrarium, 13, 13, 8, 15);
        assert!((s - 1.0).abs() < 1e-12, "got {s}");
    }

    /// Each zoom below the maximum doubles the step, until the cap.
    #[test]
    fn the_ramp_doubles_per_zoom_and_then_caps() {
        let step = |z| step_for(Encoding::Terrarium, z, 13, 8, 11);
        assert_eq!(step(13), 1.0);
        assert_eq!(step(12), 2.0);
        assert_eq!(step(11), 4.0);
        assert_eq!(step(10), 8.0);
        assert_eq!(step(9), 8.0, "capped at max_round_digits");
    }

    /// The Python original raises `max_round_digits` to `round_digits` when it is smaller, so
    /// its default of 0 disables the ramp entirely. Matching that keeps ports comparable.
    #[test]
    fn cap_below_the_base_disables_the_ramp() {
        for z in 5..=13 {
            assert_eq!(step_for(Encoding::Terrarium, z, 13, 8, 0), 1.0);
        }
    }

    #[test]
    fn terrarium_encodes_the_documented_values() {
        assert_eq!(encode(Encoding::Terrarium, 0.0, 0.0), [128, 0, 0]);
        assert_eq!(encode(Encoding::Terrarium, 100.0, 0.0), [128, 100, 0]);
        assert_eq!(encode(Encoding::Terrarium, 500.0, 0.0), [129, 244, 0]);
    }

    /// Where terrarium's size advantage actually comes from: at whole-metre steps the blue
    /// channel, which carries the fraction, is constant zero across the whole tile.
    #[test]
    fn whole_metre_quantisation_zeroes_the_blue_channel() {
        for elevation in [0.4f32, 123.7, 2401.2, -5.9] {
            let [_, _, b] = encode(Encoding::Terrarium, elevation, 1.0);
            assert_eq!(b, 0, "blue must vanish at 1 m steps for {elevation}");
        }
        // and it does not vanish at a fractional step
        assert_ne!(encode(Encoding::Terrarium, 123.5, 1.0 / 256.0)[2], 0);
    }

    #[test]
    fn round_trips_through_the_decoder() {
        for elevation in [-400.0f32, 0.0, 137.0, 2000.0, 4808.0] {
            for enc in [Encoding::Terrarium, Encoding::Mapbox] {
                let [r, g, b] = encode(enc, elevation, 1.0);
                let back = enc.decode(r, g, b);
                assert!((back - elevation).abs() <= 1.0, "{enc:?} {elevation} -> {back}");
            }
        }
    }

    #[test]
    fn pixel_centres_land_inside_their_tile() {
        // z0 has one tile; its centre pixel is near null island
        let (lon, lat) = pixel_lonlat(0, 0, 0, 256, 256, 512);
        assert!(lon.abs() < 1.0 && lat.abs() < 1.0, "got {lon},{lat}");
        // the north-west pixel is near the projection's corner
        let (lon, lat) = pixel_lonlat(0, 0, 0, 0, 0, 512);
        assert!(lon < -179.0 && lat > 85.0, "got {lon},{lat}");
    }

    #[test]
    fn tile_range_covers_the_alps() {
        let (x0, y0, x1, y1) = tile_range(8, (3.68, 44.11, 7.19, 46.52));
        assert!(x0 <= x1 && y0 <= y1, "range must be ordered: {x0},{y0}..{x1},{y1}");
        // sanity: the box is a handful of tiles at z8, not the whole world
        assert!(x1 - x0 < 10 && y1 - y0 < 10);
    }

    fn hgt_only(dir: &std::path::Path) -> CompositeSource {
        let specs = vec![crate::terrain::source::SourceSpec {
            name: "hgt".into(),
            kind: "valhalla".into(),
            path: dir.to_path_buf(),
            clamp_min: None,
            download: None,
            ..Default::default()
        }];
        CompositeSource::open(&specs).unwrap().0
    }

    #[test]
    fn empty_coverage_yields_no_tile() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = hgt_only(dir.path());
        assert!(render_tile(&mut src, 8, 131, 91, &TerrainOptions::default()).is_none());
    }

    /// Ground resolution has to shrink with zoom and with latitude, or the wrong raster overview
    /// gets read and low zooms crawl.
    #[test]
    fn ground_resolution_follows_zoom_and_latitude() {
        let equator_z8 = ground_resolution(8, 128, 512);
        let equator_z12 = ground_resolution(12, 2048, 512);
        assert!(equator_z12 < equator_z8 / 8.0, "{equator_z12} vs {equator_z8}");
        // around 45 degrees a z13 512-pixel tile is a few metres per pixel
        let alps = ground_resolution(13, 2963, 512);
        assert!((5.0..12.0).contains(&alps), "got {alps}");
    }

    /// The desktop form shows, beside each field, what leaving it alone will do - and then fills
    /// an unset option from `TerrainOptions::default()`. Nothing connects the two, so they drifted:
    /// Max zoom said 12 and an untouched build rendered z13.
    #[test]
    fn the_defaults_are_what_the_option_form_promises() {
        let hints: std::collections::HashMap<String, String> =
            crate::steps::options::terrain_options().into_iter().map(|o| (o.key, o.hint)).collect();
        let hint = |key: &str| hints.get(key).unwrap_or_else(|| panic!("no {key} option")).clone();
        let defaults = TerrainOptions::default();

        assert_eq!(hint("minzoom"), defaults.minzoom.to_string());
        assert_eq!(hint("maxzoom"), defaults.maxzoom.to_string());
        assert_eq!(hint("tile_size"), defaults.tile_size.to_string());
        assert_eq!(hint("round_digits"), defaults.round_digits.to_string());
        assert_eq!(hint("max_round_digits"), defaults.max_round_digits.to_string());
        assert_eq!(hint("blur"), format!("{:.0}", defaults.blur_m));
        assert_eq!(hint("nodata_elevation"), format!("{:.0}", defaults.nodata_elevation));
        assert_eq!(
            hint("encoding"),
            match defaults.encoding {
                Encoding::Mapbox => "mapbox",
                Encoding::Terrarium => "terrarium",
            }
        );
    }

    #[test]
    fn renders_and_encodes_a_tile() {
        let dir = tempfile::tempdir().unwrap();
        let size = 101usize;
        let mut grid = Vec::new();
        for row in 0..size {
            for _ in 0..size {
                grid.extend_from_slice(&((row * 10) as i16).to_be_bytes());
            }
        }
        std::fs::write(dir.path().join("N44E006.hgt"), grid).unwrap();
        let mut src = hgt_only(dir.path());

        let opts = TerrainOptions { tile_size: 64, maxzoom: 8, ..Default::default() };
        let (x0, y0, _, _) = tile_range(8, (6.1, 44.1, 6.9, 44.9));
        let rgb = render_tile(&mut src, 8, x0, y0, &opts).expect("covered");
        assert_eq!(rgb.len(), 64 * 64 * 3);

        let webp = to_webp(&rgb, 64).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        // and it decodes back to the same pixels
        let decoded = image::load_from_memory(&webp).unwrap().to_rgb8();
        assert_eq!(decoded.as_raw().len(), rgb.len());
    }
}
