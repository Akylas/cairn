//! Composing elevation sources, the way `sources.json` describes them.
//!
//! `sources.json` lists sources **lowest priority first** - the generator's own convention
//! (`build_terrain_rgb.py`: "Sources are listed lowest priority first"). So the repository's
//! file names `tilezen` before `ignrge5`, and the IGN 5 m raster is the one that should win
//! wherever it has data, with the 1-arcsecond `.hgt` tiles filling in beyond the border.
//!
//! Reading that order the other way round is silent and plausible: every sample still returns a
//! sensible elevation, just from the coarser source, and the output looks fine until compared.
//!
//! Boundaries are feathered the way the generator does it. Its `composite()` builds
//! `weight = box_blur(valid) * valid` and mixes `out * (1 - weight) + source * weight`, so a
//! higher-priority source fades in *inside* its own coverage: zero at its data boundary, rising
//! to one `blur` metres further in. Multiplying by `valid` truncates the ramp so it never bleeds
//! past the edge, because past the edge there is nothing to fade towards.
//!
//! A box blur of a 0/1 mask is just the valid fraction of the neighbourhood, so the same shape
//! is produced here by probing coverage on a ring around each point rather than by convolving an
//! array - this pipeline samples per point and never materialises the mask. The generator runs
//! its blur twice for a triangular ramp; a single ring is closer to linear, which differs in the
//! middle of the fade and not at either end.

use crate::elevation::Encoding;
use crate::terrain::fetch::Counters;
use crate::terrain::tiles::{Location, TerrainTiles, TileStats, TilesConfig};
use crate::terrain::{geotiff::GeoTiff, hgt::HgtSource, lambert93};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// For `xyz` and `pmtiles` sources: a `{z}/{x}/{y}` template, or a PMTiles archive to read
    /// by byte range. mapterhorn publishes both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Where the data is on disk. A tiled source reached over HTTP names a `url` instead.
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub path: PathBuf,
    /// How elevation is packed into the tiles. Terrarium unless said otherwise, which is what
    /// every public terrain pyramid serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minzoom: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clamp_min: Option<f64>,
    /// Whether missing `.hgt` tiles may be fetched for this source. Defaults to true, matching
    /// `build_terrain_rgb.py`, where only `"download": false` stopped it.
    ///
    /// It matters because a missing tile is not an error anywhere - the renderer simply writes
    /// nothing there, and the hole only shows up as blank terrain much later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<bool>,
    /// Where fetched tiles are kept between runs. Defaults to `.cache/terrain-tiles/<name>`
    /// beside `sources.json`; a tiled source is never read without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Requests in flight at once when this source is prefetched. 24 unless said otherwise;
    /// raise it for an endpoint that welcomes load, lower it for one that does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Anything else the file holds, kept so that saving it from the app does not drop a key
    /// this version has never heard of.
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

fn path_is_empty(path: &PathBuf) -> bool {
    path.as_os_str().is_empty()
}

/// Whether a kind is a tile pyramid rather than a single grid.
pub fn is_tiled(kind: &str) -> bool {
    matches!(kind, "pmtiles" | "xyz" | "tiles")
}

/// Whether a kind reads `.hgt` files, and so needs them downloaded first.
pub fn is_hgt(kind: &str) -> bool {
    matches!(kind, "valhalla" | "hgt")
}

/// Read a `sources.json`, resolving relative paths against its own directory.
pub fn read_specs(path: &Path) -> Result<Vec<SourceSpec>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut specs: Vec<SourceSpec> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let root = path.parent().unwrap_or(Path::new("."));
    for spec in &mut specs {
        if spec.path.is_relative() && !spec.path.as_os_str().is_empty() {
            spec.path = root.join(&spec.path);
        }
        match &spec.cache_dir {
            Some(dir) if dir.is_relative() => spec.cache_dir = Some(root.join(dir)),
            // a tiled source with no cache would ask a public endpoint for every tile of every
            // run, so one is given rather than left off. A file on this disk is its own cache.
            None if is_tiled(&spec.kind) && spec.url.is_some() => {
                spec.cache_dir =
                    Some(root.join(".cache/terrain-tiles").join(cache_name(&spec.name)))
            }
            _ => {}
        }
    }
    Ok(specs)
}

/// A `sources.json` as written, with nothing resolved: relative paths stay relative and no cache
/// directory is filled in. This is what an editor must show and save back, or opening the file
/// and closing it again would rewrite every path in it as an absolute one.
pub fn read_specs_raw(path: &Path) -> Result<Vec<SourceSpec>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // no file yet is an empty list, not an error: saving is what creates it
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write the list back, in the layout the file has always had, replacing it in one step so a
/// build that opens it at that moment sees the old list or the new one and never half of either.
pub fn write_specs(path: &Path, specs: &[SourceSpec]) -> Result<()> {
    let problems = validate(specs);
    if let Some(first) = problems.first() {
        return Err(anyhow!("not saved: {}", first.message));
    }
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    specs.serialize(&mut serializer)?;
    out.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::write(&part, out).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Something wrong with one field of one source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub index: usize,
    pub field: String,
    pub message: String,
}

/// Everything that would make a source list fail or misbehave at the start of a build, found now
/// instead. Errors only: a source whose files are not on this machine is allowed, because a
/// machine without the 44 GB raster is a normal one.
pub fn validate(specs: &[SourceSpec]) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut problem = |index: usize, field: &str, message: String| {
        out.push(Problem { index, field: field.into(), message });
    };
    for (index, spec) in specs.iter().enumerate() {
        let label =
            if spec.name.is_empty() { format!("source {}", index + 1) } else { spec.name.clone() };
        if spec.name.trim().is_empty() {
            problem(index, "name", format!("{label} needs a name"));
        } else if specs[..index].iter().any(|other| other.name == spec.name) {
            problem(index, "name", format!("two sources are called `{}`", spec.name));
        }
        let has_url = spec.url.as_deref().is_some_and(|u| !u.trim().is_empty());
        let has_path = !spec.path.as_os_str().is_empty();
        match spec.kind.as_str() {
            "xyz" | "tiles" => match spec.url.as_deref() {
                Some(url) if url.contains("{z}") && url.contains("{x}") && url.contains("{y}") => {}
                Some(_) => problem(
                    index,
                    "url",
                    format!("{label}: the URL needs {{z}}, {{x}} and {{y}} in it"),
                ),
                None => problem(index, "url", format!("{label} needs a URL template")),
            },
            "pmtiles" => {
                if !has_url && !has_path {
                    problem(index, "url", format!("{label} needs a URL or a file"));
                }
            }
            "raster" | "valhalla" | "hgt" => {
                if !has_path {
                    problem(index, "path", format!("{label} needs a path"));
                }
            }
            other => problem(index, "type", format!("{label}: `{other}` is not a source type")),
        }
        if let Some(encoding) = &spec.encoding {
            if Encoding::parse(encoding).is_none() {
                problem(
                    index,
                    "encoding",
                    format!("{label}: `{encoding}` is not terrarium or mapbox"),
                );
            }
        }
        if let (Some(min), Some(max)) = (spec.minzoom, spec.maxzoom) {
            if min > max {
                problem(
                    index,
                    "minzoom",
                    format!("{label}: min zoom {min} is above max zoom {max}"),
                );
            }
        }
        if spec.tile_size == Some(0) {
            problem(index, "tile_size", format!("{label}: tile size cannot be zero"));
        }
        if let Some(n) = spec.concurrency {
            if !(1..=128).contains(&n) {
                problem(
                    index,
                    "concurrency",
                    format!("{label}: requests in flight must be 1 to 128"),
                );
            }
        }
    }
    out
}

/// What trying a source found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Probe {
    pub ok: bool,
    pub summary: String,
    pub details: Vec<String>,
}

/// Open a source and read one elevation out of it, to say whether it works before a build finds out.
///
/// The point is in the Alps - high, so that "0 m" means a source returning nothing rather than the
/// sea - and the tile cache is left off, so trying a source writes nothing.
pub fn probe(spec: &SourceSpec) -> Probe {
    let problems = validate(std::slice::from_ref(spec));
    if let Some(first) = problems.first() {
        return Probe { ok: false, summary: first.message.clone(), details: Vec::new() };
    }
    let started = std::time::Instant::now();
    let (lon, lat) = (8.0, 46.0);
    let mut details = Vec::new();

    if is_tiled(&spec.kind) {
        let mut spec = spec.clone();
        spec.cache_dir = None;
        let mut tiles = match open_tiles(&spec) {
            Ok(tiles) => tiles,
            Err(e) => {
                return Probe { ok: false, summary: format!("could not open it: {e}"), details }
            }
        };
        let info = tiles.info();
        details.push(format!(
            "{}: z{}-z{}, {}-pixel {} tiles",
            info.kind, info.minzoom, info.maxzoom, info.tile_size, info.extension
        ));
        let value = tiles.sample(lon, lat, 30.0);
        let stats = tiles.stats();
        details.push(format!(
            "{} request(s), {:.0} ms",
            stats.requests,
            started.elapsed().as_secs_f64() * 1000.0
        ));
        return match value {
            Some(metres) => {
                Probe { ok: true, summary: format!("works: {metres:.0} m at 46N 8E"), details }
            }
            None => Probe {
                ok: false,
                summary: match stats.last_error {
                    Some(e) => format!("opened, but reading a tile failed: {e}"),
                    None => "opened, but has no data at 46N 8E (the Alps)".into(),
                },
                details,
            },
        };
    }

    match spec.kind.as_str() {
        "raster" => match GeoTiff::open(&spec.path) {
            Ok(_) => Probe { ok: true, summary: "the raster opens".into(), details },
            Err(e) => Probe { ok: false, summary: format!("could not open it: {e}"), details },
        },
        _ => {
            let files = std::fs::read_dir(&spec.path)
                .map(|d| {
                    d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "hgt")).count()
                })
                .unwrap_or(0);
            if files == 0 {
                Probe {
                    ok: false,
                    summary: format!("no .hgt files in {}", spec.path.display()),
                    details,
                }
            } else {
                Probe { ok: true, summary: format!("{files} .hgt tiles"), details }
            }
        }
    }
}

/// A source name as a directory name, with anything a path would object to replaced.
fn cache_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

enum Backing {
    /// A projected raster. Currently Lambert-93 only, which is what the IGN data uses.
    Raster {
        tiff: Box<GeoTiff>,
        clamp_min: Option<f64>,
    },
    Hgt {
        source: HgtSource,
        clamp_min: Option<f64>,
    },
    /// A raster tile pyramid: PMTiles, or a `{z}/{x}/{y}` endpoint.
    Tiles {
        tiles: Box<TerrainTiles>,
        clamp_min: Option<f64>,
    },
}

fn open_tiles(spec: &SourceSpec) -> Result<TerrainTiles> {
    let encoding = match &spec.encoding {
        Some(raw) => Encoding::parse(raw).ok_or_else(|| anyhow!("unknown encoding `{raw}`"))?,
        None => Encoding::Terrarium,
    };
    let config = TilesConfig {
        encoding,
        tile_size: spec.tile_size.unwrap_or(512),
        minzoom: spec.minzoom,
        maxzoom: spec.maxzoom,
        cache_dir: spec.cache_dir.clone(),
        concurrency: spec.concurrency.unwrap_or(crate::terrain::tiles::DEFAULT_CONCURRENCY),
    };
    let target = match &spec.url {
        Some(url) => url.clone(),
        None => spec.path.display().to_string(),
    };
    if target.is_empty() {
        return Err(anyhow!("neither a path nor a url"));
    }
    match spec.kind.as_str() {
        "xyz" | "tiles" => TerrainTiles::xyz(&target, config),
        _ => TerrainTiles::pmtiles(&Location::parse(&target), config),
    }
}

/// What the render is about to ask of the sources for one output tile.
#[derive(Debug, Clone, Copy)]
pub struct TileQuery {
    /// west, south, east, north in degrees.
    pub bounds: (f64, f64, f64, f64),
    /// Latitude of the tile's centre, which the zoom to read is chosen at.
    pub centre_lat: f64,
    /// Ground size of one output pixel, in metres.
    pub target_m_per_px: f64,
    /// Output pixels per side.
    pub size: u32,
}

pub struct CompositeSource {
    sources: Vec<(String, Backing)>,
}

impl CompositeSource {
    /// Open every source that exists. A missing one is skipped rather than fatal, so a machine
    /// without the 44 GB raster still renders from the fallback.
    pub fn open(specs: &[SourceSpec]) -> Result<(Self, Vec<String>)> {
        let mut sources = Vec::new();
        let mut skipped = Vec::new();
        // kept in file order, lowest priority first, because blending has to accumulate in that
        // direction; the unblended path walks it backwards instead
        for spec in specs.iter() {
            // a source reached over HTTP has nothing on disk to look for
            if spec.url.is_none() && !spec.path.exists() {
                skipped.push(format!("{} ({} missing)", spec.name, spec.path.display()));
                continue;
            }
            match spec.kind.as_str() {
                kind if is_tiled(kind) => match open_tiles(spec) {
                    Ok(tiles) => sources.push((
                        spec.name.clone(),
                        Backing::Tiles { tiles: Box::new(tiles), clamp_min: spec.clamp_min },
                    )),
                    Err(e) => skipped.push(format!("{}: {e}", spec.name)),
                },
                "raster" => match GeoTiff::open(&spec.path) {
                    Ok(tiff) => sources.push((
                        spec.name.clone(),
                        Backing::Raster { tiff: Box::new(tiff), clamp_min: spec.clamp_min },
                    )),
                    Err(e) => skipped.push(format!("{}: {e}", spec.name)),
                },
                // `valhalla` is what the pipeline calls a directory of .hgt tiles
                "valhalla" | "hgt" => sources.push((
                    spec.name.clone(),
                    Backing::Hgt { source: HgtSource::new(&spec.path), clamp_min: spec.clamp_min },
                )),
                other => skipped.push(format!("{}: unsupported type {other}", spec.name)),
            }
        }
        Ok((Self { sources }, skipped))
    }

    /// Pin every tiled source's choice of zoom to the centre of the tile about to be rendered.
    /// See [`TerrainTiles::set_reference_latitude`].
    pub fn begin_tile(&mut self, centre_lat: Option<f64>) {
        for (_, backing) in &mut self.sources {
            if let Backing::Tiles { tiles, .. } = backing {
                tiles.set_reference_latitude(centre_lat);
            }
        }
    }

    /// Whether anything in this tile could be sampled at all.
    ///
    /// Only a tiled source can say so without being sampled, so anything else - a grid, a raster
    /// - answers "maybe", and the render does the work. For a build over mapterhorn alone it is
    /// exact, and it is what stops a tile of open sea costing a quarter of a million lookups to
    /// find out it holds nothing.
    pub fn may_cover(&mut self, query: &TileQuery) -> bool {
        self.begin_tile(Some(query.centre_lat));
        for (_, backing) in &mut self.sources {
            match backing {
                Backing::Tiles { tiles, .. } => {
                    if tiles.covers(query.bounds, query.size, query.target_m_per_px) {
                        return true;
                    }
                }
                _ => return true,
            }
        }
        false
    }

    /// Fetch, ahead of the render, the source tiles these output tiles will read.
    pub fn prefetch(&mut self, queries: &[TileQuery]) {
        for (_, backing) in &mut self.sources {
            let Backing::Tiles { tiles, .. } = backing else { continue };
            let mut keys = std::collections::BTreeSet::new();
            for query in queries {
                tiles.set_reference_latitude(Some(query.centre_lat));
                if let Some((z, x0, x1, y0, y1)) =
                    tiles.tiles_under(query.bounds, query.size, query.target_m_per_px)
                {
                    for x in x0..=x1 {
                        for y in y0..=y1 {
                            keys.insert((z, x, y));
                        }
                    }
                }
            }
            tiles.set_reference_latitude(None);
            let keys: Vec<_> = keys.into_iter().collect();
            tiles.prefetch(&keys);
        }
    }

    /// Live counters of every tiled source, readable while a render holds this source locked.
    pub fn counters(&self) -> Vec<(String, Arc<Counters>)> {
        self.sources
            .iter()
            .rev()
            .filter_map(|(name, backing)| match backing {
                Backing::Tiles { tiles, .. } => Some((name.clone(), tiles.counters())),
                _ => None,
            })
            .collect()
    }

    /// Source names, highest priority first.
    pub fn names(&self) -> Vec<&str> {
        self.sources.iter().rev().map(|(n, _)| n.as_str()).collect()
    }

    /// Elevation from one source, or `None` where it has no data there.
    fn probe(&mut self, index: usize, lon: f64, lat: f64, target_m_per_px: f64) -> Option<f32> {
        let (_, backing) = self.sources.get_mut(index)?;
        let value = match backing {
            Backing::Raster { tiff, clamp_min } => {
                let (easting, northing) = lambert93::from_lonlat(lon, lat);
                let level = tiff.level_for(target_m_per_px);
                tiff.sample(easting, northing, level).map(|v| clamp(v, *clamp_min))
            }
            Backing::Hgt { source, clamp_min } => {
                source.sample(lon, lat).map(|v| clamp(v, *clamp_min))
            }
            Backing::Tiles { tiles, clamp_min } => {
                tiles.sample(lon, lat, target_m_per_px).map(|v| clamp(v, *clamp_min))
            }
        };
        value.filter(|v| v.is_finite())
    }

    /// What each tiled source did, and whether any tile went unread. A hole in a terrain archive
    /// is invisible until someone looks at that part of the map, so it is said out loud instead.
    pub fn reports(&self) -> Vec<(String, TileStats)> {
        self.sources
            .iter()
            .rev()
            .filter_map(|(name, backing)| match backing {
                Backing::Tiles { tiles, .. } => Some((name.clone(), tiles.stats())),
                _ => None,
            })
            .collect()
    }

    /// Fraction of a ring of radius `blur_m` around the point where this source has data.
    ///
    /// Stands in for the generator's box blur of the coverage mask. Zero when the centre itself
    /// is uncovered, matching the `* valid` factor that truncates the ramp at the edge.
    fn coverage_weight(
        &mut self,
        index: usize,
        lon: f64,
        lat: f64,
        target: f64,
        blur_m: f64,
    ) -> f32 {
        if self.probe(index, lon, lat, target).is_none() {
            return 0.0;
        }
        if blur_m <= 0.0 {
            return 1.0;
        }
        const RING: usize = 8;
        let dlat = blur_m / 111_320.0;
        let dlon = dlat / lat.to_radians().cos().max(1e-6);

        let mut covered = 1usize;
        for i in 0..RING {
            let angle = std::f64::consts::TAU * i as f64 / RING as f64;
            let probe_lon = lon + dlon * angle.cos();
            let probe_lat = lat + dlat * angle.sin();
            if self.probe(index, probe_lon, probe_lat, target).is_some() {
                covered += 1;
            }
        }
        covered as f32 / (RING + 1) as f32
    }

    /// Sample with feathered boundaries between sources.
    ///
    /// Accumulates lowest priority first, exactly as `composite()` does: the first covering
    /// source establishes a value, and each better source mixes in by its coverage weight.
    /// `blur_m` of zero reduces this to a hard switch.
    pub fn sample_blended(
        &mut self,
        lon: f64,
        lat: f64,
        target_m_per_px: f64,
        blur_m: f64,
    ) -> Option<f32> {
        let mut out: Option<f32> = None;
        for index in 0..self.sources.len() {
            let Some(value) = self.probe(index, lon, lat, target_m_per_px) else { continue };
            out = Some(match out {
                None => value,
                Some(previous) => {
                    let w = self.coverage_weight(index, lon, lat, target_m_per_px, blur_m);
                    previous * (1.0 - w) + value * w
                }
            });
        }
        out
    }

    /// Sample the highest-priority source covering this point.
    ///
    /// `target_m_per_px` picks which overview of a raster source to read: rendering a z8 tile
    /// from 5 m data would decode thousands of full-resolution tiles per output tile, so the
    /// pyramid is used to keep the read proportional to the output.
    pub fn sample(&mut self, lon: f64, lat: f64, target_m_per_px: f64) -> Option<f32> {
        // backwards, because the list is lowest priority first
        for index in (0..self.sources.len()).rev() {
            if let Some(v) = self.probe(index, lon, lat, target_m_per_px) {
                return Some(v);
            }
        }
        None
    }
}

/// `clamp_min` exists to stop bathymetry and nodata artefacts dragging coastlines below sea
/// level; the pipeline sets it to -10 for the global source.
fn clamp(value: f32, min: Option<f64>) -> f32 {
    match min {
        Some(m) => value.max(m as f32),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat `.hgt` grid at a constant elevation.
    fn flat(size: usize, metres: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(size * size * 2);
        for _ in 0..size * size {
            out.extend_from_slice(&metres.to_be_bytes());
        }
        out
    }

    #[test]
    fn reads_the_repository_source_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sources.json");
        std::fs::write(
            &path,
            r#"[
              {"name":"tilezen","type":"valhalla","path":"./elevation_tiles","clamp_min":-10},
              {"name":"ignrge5","type":"raster","path":"work/out/ign.tif"}
            ]"#,
        )
        .unwrap();
        let specs = read_specs(&path).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].clamp_min, Some(-10.0));
        // relative paths resolve against the file, not the process working directory
        assert_eq!(specs[1].path, dir.path().join("work/out/ign.tif"));
    }

    /// A missing raster must not stop the fallback source from working - not every machine has
    /// the 44 GB file.
    #[test]
    fn missing_sources_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let specs = vec![
            SourceSpec {
                name: "ign".into(),
                kind: "raster".into(),
                path: dir.path().join("absent.tif"),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
            SourceSpec {
                name: "tilezen".into(),
                kind: "valhalla".into(),
                path: dir.path().to_path_buf(),
                clamp_min: Some(-10.0),
                download: None,
                ..Default::default()
            },
        ];
        let (composite, skipped) = CompositeSource::open(&specs).unwrap();
        assert_eq!(composite.names(), vec!["tilezen"]);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("absent.tif"));
    }

    #[test]
    fn unsupported_types_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let specs = vec![SourceSpec {
            name: "weird".into(),
            kind: "netcdf".into(),
            path: dir.path().to_path_buf(),
            clamp_min: None,
            download: None,
            ..Default::default()
        }];
        let (composite, skipped) = CompositeSource::open(&specs).unwrap();
        assert!(composite.names().is_empty());
        assert!(skipped[0].contains("netcdf"));
    }

    /// Opening the file in an editor and saving it must not change what it says: relative paths
    /// stay relative, keys this version does not know are kept, and absent fields stay absent.
    #[test]
    fn saving_what_was_read_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sources.json");
        let original = r##"[
    {
        "name": "mapterhorn",
        "type": "xyz",
        "url": "https://tiles.mapterhorn.com/{z}/{x}/{y}.webp",
        "encoding": "terrarium",
        "maxzoom": 14,
        "clamp_min": -10.0
    },
    {
        "name": "ign",
        "type": "raster",
        "path": "work/out/ign.tif",
        "licence": "etalab"
    }
]
"##;
        std::fs::write(&path, original).unwrap();
        let specs = read_specs_raw(&path).unwrap();
        assert_eq!(specs[1].path, Path::new("work/out/ign.tif"), "not resolved against the file");
        assert!(specs[0].cache_dir.is_none(), "no cache directory is invented");
        write_specs(&path, &specs).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!dir.path().join("sources.json.part").exists());
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_specs_raw(&dir.path().join("none.json")).unwrap().is_empty());
    }

    #[test]
    fn nothing_invalid_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sources.json");
        let bad = vec![SourceSpec {
            name: "x".into(),
            kind: "xyz".into(),
            url: Some("https://example.com/tiles.png".into()),
            ..Default::default()
        }];
        let err = write_specs(&path, &bad).unwrap_err().to_string();
        assert!(err.contains("{z}"), "{err}");
        assert!(!path.exists());
    }

    #[test]
    fn validation_names_the_field_that_is_wrong() {
        let spec = |name: &str, kind: &str| SourceSpec {
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        };
        let found = validate(&[
            spec("a", "xyz"),
            spec("a", "pmtiles"),
            spec("", "raster"),
            spec("c", "netcdf"),
            SourceSpec { encoding: Some("png".into()), ..spec("d", "valhalla") },
            SourceSpec { minzoom: Some(9), maxzoom: Some(3), ..spec("e", "valhalla") },
        ]);
        let fields: Vec<(usize, &str)> =
            found.iter().map(|p| (p.index, p.field.as_str())).collect();
        for expected in [
            (0, "url"),
            (1, "name"),
            (1, "url"),
            (2, "name"),
            (2, "path"),
            (3, "type"),
            (4, "encoding"),
            (5, "minzoom"),
        ] {
            assert!(fields.contains(&expected), "missing {expected:?} in {fields:?}");
        }
        let fine = SourceSpec {
            url: Some("https://t.example/{z}/{x}/{y}.webp".into()),
            ..spec("ok", "xyz")
        };
        assert!(validate(&[fine]).is_empty());
    }

    #[test]
    fn probing_a_directory_without_tiles_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let spec = SourceSpec {
            name: "hgt".into(),
            kind: "valhalla".into(),
            path: dir.path().to_path_buf(),
            ..Default::default()
        };
        let probe = probe(&spec);
        assert!(!probe.ok);
        assert!(probe.summary.contains("no .hgt"), "{}", probe.summary);
    }

    #[test]
    fn clamp_min_lifts_below_sea_level_values() {
        assert_eq!(clamp(-40.0, Some(-10.0)), -10.0);
        assert_eq!(clamp(120.0, Some(-10.0)), 120.0);
        assert_eq!(clamp(-40.0, None), -40.0);
    }

    /// The list is lowest-priority-first, so the *last* entry that covers a point answers it.
    #[test]
    fn last_listed_source_has_priority() {
        let dir = tempfile::tempdir().unwrap();
        let specs = vec![
            SourceSpec {
                name: "low".into(),
                kind: "valhalla".into(),
                path: dir.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
            SourceSpec {
                name: "high".into(),
                kind: "valhalla".into(),
                path: dir.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
        ];
        let (composite, _) = CompositeSource::open(&specs).unwrap();
        assert_eq!(composite.names(), vec!["high", "low"], "highest priority is consulted first");
    }

    /// Two overlapping sources at different elevations: inside the better source's coverage the
    /// blend must reach it fully, and at its edge fall back to the other.
    #[test]
    fn blur_ramps_between_sources() {
        let low = tempfile::tempdir().unwrap();
        let high = tempfile::tempdir().unwrap();
        // low priority covers two degree squares at 100 m
        for name in ["N44E006.hgt", "N44E007.hgt"] {
            std::fs::write(low.path().join(name), flat(9, 100)).unwrap();
        }
        // high priority covers only the western one, at 200 m
        std::fs::write(high.path().join("N44E006.hgt"), flat(9, 200)).unwrap();

        let specs = vec![
            SourceSpec {
                name: "low".into(),
                kind: "valhalla".into(),
                path: low.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
            SourceSpec {
                name: "high".into(),
                kind: "valhalla".into(),
                path: high.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
        ];
        let (mut c, _) = CompositeSource::open(&specs).unwrap();
        let blur = 2000.0;

        // deep inside the better source: fully its value
        let inside = c.sample_blended(6.5, 44.5, 30.0, blur).unwrap();
        assert!((inside - 200.0).abs() < 0.5, "inside gave {inside}");

        // outside it entirely: the fallback, untouched
        let outside = c.sample_blended(7.5, 44.5, 30.0, blur).unwrap();
        assert!((outside - 100.0).abs() < 0.5, "outside gave {outside}");

        // just inside the eastern edge: partway between the two
        let edge = c.sample_blended(6.999, 44.5, 30.0, blur).unwrap();
        assert!(edge > 100.0 && edge < 200.0, "edge gave {edge}, expected a blend");
    }

    /// Zero blur is the old hard switch, and must stay available.
    #[test]
    fn zero_blur_switches_hard() {
        let low = tempfile::tempdir().unwrap();
        let high = tempfile::tempdir().unwrap();
        std::fs::write(low.path().join("N44E006.hgt"), flat(9, 100)).unwrap();
        std::fs::write(high.path().join("N44E006.hgt"), flat(9, 200)).unwrap();
        let specs = vec![
            SourceSpec {
                name: "low".into(),
                kind: "valhalla".into(),
                path: low.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
            SourceSpec {
                name: "high".into(),
                kind: "valhalla".into(),
                path: high.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
        ];
        let (mut c, _) = CompositeSource::open(&specs).unwrap();
        assert_eq!(c.sample_blended(6.5, 44.5, 30.0, 0.0), Some(200.0));
    }

    /// The ramp is truncated at the edge: where the better source has no data at all the weight
    /// is zero, so nothing of it leaks outside its coverage.
    #[test]
    fn weight_is_zero_outside_coverage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("N44E006.hgt"), flat(9, 200)).unwrap();
        let specs = vec![SourceSpec {
            name: "only".into(),
            kind: "valhalla".into(),
            path: dir.path().to_path_buf(),
            clamp_min: None,
            download: None,
            ..Default::default()
        }];
        let (mut c, _) = CompositeSource::open(&specs).unwrap();
        assert_eq!(c.coverage_weight(0, 20.0, 20.0, 30.0, 2000.0), 0.0);
        assert_eq!(
            c.coverage_weight(0, 6.5, 44.5, 30.0, 2000.0),
            1.0,
            "deep inside is full weight"
        );
    }

    #[test]
    fn first_covering_source_wins() {
        let dir = tempfile::tempdir().unwrap();
        let size = 3usize;
        let mut grid = Vec::new();
        for _ in 0..size * size {
            grid.extend_from_slice(&700i16.to_be_bytes());
        }
        std::fs::write(dir.path().join("N44E006.hgt"), grid).unwrap();

        let specs = vec![
            SourceSpec {
                name: "absent-raster".into(),
                kind: "raster".into(),
                path: dir.path().join("nope.tif"),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
            SourceSpec {
                name: "hgt".into(),
                kind: "valhalla".into(),
                path: dir.path().to_path_buf(),
                clamp_min: None,
                download: None,
                ..Default::default()
            },
        ];
        let (mut composite, _) = CompositeSource::open(&specs).unwrap();
        assert_eq!(composite.sample(6.5, 44.5, 30.0), Some(700.0));
        // outside coverage nothing answers
        assert_eq!(composite.sample(20.0, 20.0, 30.0), None);
    }
}
