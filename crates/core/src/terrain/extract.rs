//! A local copy of the part of PMTiles archives that the areas being built need.
//!
//! mapterhorn publishes its pyramid as one 355 GB archive for z0-12 and, for anything finer, one
//! archive per z6 tile of the world - `6-33-22.pmtiles` and its neighbours, each hundreds of
//! gigabytes. Nobody wants those on their disk to build one region. `pmtiles extract` cuts a region
//! out of one archive, but cuts a *new file* each time, and an extract for `france` does not contain
//! the one for `rhone-alpes` as a view - so growing an area by hand means storing the overlap twice.
//!
//! This keeps one file instead. A source in `sources.json` that names `extract_from` archives has
//! its local `path` grown to cover each area as it is built: the tiles the area needs and the file
//! does not have are fetched from whichever archive holds them by byte range, and the file is
//! rewritten as the union of what it had and what it has now. An area that is already covered costs
//! nothing, and not a request - which is what the `.coverage.json` beside the file is for.
//!
//! An origin is a URL, or a URL template with `{x}` and `{y}`: the archive that holds a tile is then
//! the one named by the tile's ancestor at z6, which is how mapterhorn names its regional
//! archives, and only the archives an area touches are ever opened.
//!
//! The rewrite goes through a second file and is moved into place at the end, so a failure leaves
//! the old one alone, and the work is journalled the way a terrain build is: an interrupted
//! extract continues from its last finished batch.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::archive::{ArchiveMeta, Commit, ResumeMode, TerrainArchive, TileKind};
use super::build::Event;
use super::pmtiles::{self, Archive, Compression, TileType};
use super::source::SourceSpec;
use super::tiles::Upstream;
use crate::poly::Polygon;
use crate::steps::archive::ArchiveFormat;

/// Tiles this close in the archive are fetched as one range. mapterhorn's archives are in Hilbert
/// order, so tiles that are neighbours on the map are mostly neighbours in the file, and a
/// request per tile is the slowest way to read them.
const GAP: u64 = 64 * 1024;
/// The largest single range request.
const MAX_RANGE: u64 = 8 * 1024 * 1024;
/// Tiles copied from the old file between two commits.
const COPY_BATCH: usize = 512;
/// Range requests in flight when the source does not say.
const DEFAULT_CONCURRENCY: usize = 16;
/// Where a source with no `maxzoom` stops. mapterhorn's planet archive ends at 12.
const DEFAULT_MAXZOOM: u8 = 12;
/// The zoom a `{x}`/`{y}` template is indexed at: mapterhorn names its regional archives by their
/// z6 tile.
const DEFAULT_GRID_ZOOM: u8 = 6;

/// What one extract is asked to cover.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub bounds: (f64, f64, f64, f64),
    /// The area's own outline. With one, tiles that only the bounding box reaches are left out,
    /// which at fine zooms is more than half of them for an irregular region.
    pub shape: Option<&'a Polygon>,
    /// A ceiling on the zoom, under the source's own: a graph build reads nothing finer than z12,
    /// so it has no business fetching the gigabytes above that.
    pub maxzoom: Option<u8>,
}

/// One box an extract was grown to cover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Area {
    bounds: [f64; 4],
    minzoom: u8,
    maxzoom: u8,
    /// Which outline the tiles were chosen by, `None` for the whole box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shape: Option<u64>,
}

impl Area {
    /// Whether everything `inner` asks for is in what `self` covers.
    fn contains(&self, inner: &Area) -> bool {
        self.minzoom <= inner.minzoom
            && self.maxzoom >= inner.maxzoom
            && self.bounds[0] <= inner.bounds[0]
            && self.bounds[1] <= inner.bounds[1]
            && self.bounds[2] >= inner.bounds[2]
            && self.bounds[3] >= inner.bounds[3]
            // a box covers any outline inside it; an outline covers only itself
            && (self.shape.is_none() || self.shape == inner.shape)
    }
}

/// What a file is known to cover, kept beside it.
///
/// Without this every run would have to ask the servers about every tile of the area to learn that
/// nothing is missing. Absent or unreadable, it only means that question is asked once more.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Coverage {
    source: String,
    areas: Vec<Area>,
}

fn sidecar(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".coverage.json");
    name.into()
}

fn extending(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".extending");
    name.into()
}

impl Coverage {
    fn load(target: &Path) -> Self {
        std::fs::read_to_string(sidecar(target))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, target: &Path) -> Result<()> {
        let path = sidecar(target);
        let part = path.with_extension("json.part");
        std::fs::write(&part, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&part, &path).with_context(|| format!("writing {}", path.display()))
    }

    fn covers(&self, source: &str, area: &Area) -> bool {
        self.source == source && self.areas.iter().any(|have| have.contains(area))
    }

    fn add(&mut self, source: &str, area: Area) {
        if self.source != source {
            self.source = source.to_string();
            self.areas.clear();
        }
        // a box inside one already held says nothing new
        if self.covers(source, &area) {
            return;
        }
        self.areas.retain(|have| !area.contains(have));
        self.areas.push(area);
    }
}

/// The box a request is rounded out to: a tenth of a degree, which makes two areas that differ by
/// a hair the same request. Rounding to whole degrees would, at z15, fetch a ring of ground the
/// size of the area again.
pub fn snap(bounds: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    const LIMIT: f64 = 85.0511;
    let down = |v: f64| (v * 10.0 + 1e-6).floor() / 10.0;
    let up = |v: f64| (v * 10.0 - 1e-6).ceil() / 10.0;
    (
        down(bounds.0).max(-180.0),
        down(bounds.1).max(-LIMIT),
        up(bounds.2).min(180.0),
        up(bounds.3).min(LIMIT),
    )
}

/// The `(west, south, east, north)` of a tile.
fn tile_rect(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let lon = |x: f64| x / n * 360.0 - 180.0;
    let lat = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
    (lon(x as f64), lat(y as f64 + 1.0), lon(x as f64 + 1.0), lat(y as f64))
}

/// Every tile a render over `bounds` can read, at each zoom: the ones under the box - or, given a
/// shape, under the shape - and one more ring around them. The ring is the terrain's own tile
/// buffer and the neighbours bilinear filtering reaches across an edge for; leaving it out puts a
/// seam at the border of the area.
pub fn needed_tiles(
    bounds: (f64, f64, f64, f64),
    shape: Option<&Polygon>,
    minzoom: u8,
    maxzoom: u8,
) -> Vec<(u8, u32, u32)> {
    let (west, south, east, north) = bounds;
    let mut out = Vec::new();
    for z in minzoom..=maxzoom {
        let n = (1u64 << z) as f64;
        let x_of = |lon: f64| (lon + 180.0) / 360.0 * n;
        let y_of = |lat: f64| {
            let r = lat.to_radians();
            (1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n
        };
        let last = (n - 1.0) as u32;
        let clamp = |v: f64| v.floor().clamp(0.0, last as f64) as u32;
        let (x0, x1) = (clamp(x_of(west)), clamp(x_of(east)));
        let (y0, y1) = (clamp(y_of(north)), clamp(y_of(south)));
        let ring = |v: u32, d: i64| (i64::from(v) + d).clamp(0, i64::from(last)) as u32;

        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for x in x0..=x1 {
            for y in y0..=y1 {
                let inside = shape.is_none_or(|shape| {
                    let (w, s, e, n) = tile_rect(z, x, y);
                    shape.intersects_rect(w, s, e, n)
                });
                if !inside {
                    continue;
                }
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        seen.insert((ring(x, dx), ring(y, dy)));
                    }
                }
            }
        }
        out.extend(seen.into_iter().map(|(x, y)| (z, x, y)));
    }
    out
}

/// What an extract did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The file already covered the area; nothing was read from the servers.
    pub covered: bool,
    /// Tiles kept from the file as it was.
    pub kept: u64,
    /// Tiles fetched.
    pub fetched: u64,
    pub fetched_bytes: u64,
    /// `false` when it was stopped: what was done is kept, and a rerun continues.
    pub finished: bool,
}

/// One range request, and the tiles it holds.
struct Group {
    /// Which archive: an index into [`Origins::ups`].
    origin: usize,
    start: u64,
    end: u64,
    tiles: Vec<(u8, u32, u32, u64, u32)>,
}

enum Work {
    /// Tiles from the file as it was: `(tile id, offset, length)`.
    Copy(Vec<(u64, u64, u32)>),
    /// Range requests to one archive, made together.
    Fetch(Vec<Group>),
}

fn fnv(hash: u64, value: u64) -> u64 {
    let mut hash = hash;
    for byte in value.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One entry of `extract_from`.
enum Origin {
    /// An archive at one URL.
    Plain(usize),
    /// A template: one archive per tile of the grid zoom, opened when a tile under it is wanted.
    Grid { template: String, zoom: u8, opened: HashMap<(u32, u32), Option<usize>> },
}

/// The archives tiles are copied from, opened as they are needed.
struct Archives {
    origins: Vec<Origin>,
    ups: Vec<Upstream>,
    kind: Option<TileKind>,
}

impl Archives {
    fn open(urls: &[String], emit: &mut dyn FnMut(Event)) -> Result<Self> {
        let mut this = Self { origins: Vec::new(), ups: Vec::new(), kind: None };
        for url in urls {
            if url.contains("{x}") || url.contains("{y}") {
                this.origins.push(Origin::Grid {
                    template: url.clone(),
                    zoom: DEFAULT_GRID_ZOOM,
                    opened: HashMap::new(),
                });
            } else {
                emit(Event::Phase(format!("Reading {url}")));
                let up = Upstream::open(url)?;
                let index = this.adopt(up, url)?;
                this.origins.push(Origin::Plain(index));
            }
        }
        Ok(this)
    }

    /// Check an archive is one that can be copied from, and keep it.
    fn adopt(&mut self, up: Upstream, url: &str) -> Result<usize> {
        let header = up.header();
        let kind = match header.tile_type {
            TileType::Webp => TileKind::Webp,
            TileType::Png => TileKind::Png,
            other => bail!("{url} holds {other:?} tiles; an elevation extract wants webp or png"),
        };
        if header.tile_compression != Compression::None {
            bail!("{url} compresses its tiles; only uncompressed raster tiles are copied");
        }
        match self.kind {
            Some(have) if have != kind => {
                bail!("{url} holds a different kind of tile than the others")
            }
            _ => self.kind = Some(kind),
        }
        self.ups.push(up);
        Ok(self.ups.len() - 1)
    }

    /// The archive that holds `z/x/y`, with where the tile's bytes are in it.
    fn locate(&mut self, z: u8, x: u32, y: u32) -> Result<Option<(usize, u64, u64)>> {
        for i in 0..self.origins.len() {
            let index = match &mut self.origins[i] {
                Origin::Plain(index) => *index,
                Origin::Grid { template, zoom, opened } => {
                    if z < *zoom {
                        continue;
                    }
                    let shift = z - *zoom;
                    let key = (x >> shift, y >> shift);
                    let cached = opened.get(&key).copied();
                    match cached {
                        Some(Some(index)) => index,
                        Some(None) => continue,
                        None => {
                            let url = template
                                .replace("{x}", &key.0.to_string())
                                .replace("{y}", &key.1.to_string());
                            let opened_now = match Upstream::open_optional(&url)? {
                                Some(up) => {
                                    let slot = self.adopt(up, &url)?;
                                    Some(slot)
                                }
                                // an archive that is not there is an area with nothing in it
                                None => None,
                            };
                            if let Origin::Grid { opened, .. } = &mut self.origins[i] {
                                opened.insert(key, opened_now);
                            }
                            match opened_now {
                                Some(index) => index,
                                None => continue,
                            }
                        }
                    }
                }
            };
            let header = self.ups[index].header();
            if z < header.min_zoom || z > header.max_zoom {
                continue;
            }
            if let Some((offset, length)) = self.ups[index].locate(z, x, y)? {
                return Ok(Some((index, offset, length)));
            }
        }
        Ok(None)
    }
}

/// Make `spec.path` cover what `request` asks, fetching what it lacks from `spec.extract_from`.
///
/// `cancelled` is read between batches. A source with no `extract_from` is not this function's
/// business and is left alone.
pub fn ensure(
    spec: &SourceSpec,
    request: &Request,
    cancelled: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Event),
) -> Result<Report> {
    let urls = spec.extract_from.as_ref().map(|o| o.urls()).unwrap_or_default();
    if urls.is_empty() {
        return Ok(Report { covered: true, finished: true, ..Default::default() });
    }
    let source = urls.join("\n");
    let target = &spec.path;
    let b = snap(request.bounds);
    let minzoom = spec.minzoom.unwrap_or(0);
    let maxzoom = spec.maxzoom.unwrap_or(DEFAULT_MAXZOOM).min(request.maxzoom.unwrap_or(u8::MAX));
    if minzoom > maxzoom {
        return Ok(Report { covered: true, finished: true, ..Default::default() });
    }
    let area = Area {
        bounds: [b.0, b.1, b.2, b.3],
        minzoom,
        maxzoom,
        shape: request.shape.map(|s| s.fingerprint()),
    };

    let mut coverage = Coverage::load(target);
    if target.is_file() && coverage.covers(&source, &area) {
        return Ok(Report { covered: true, finished: true, ..Default::default() });
    }

    let mut origins = Archives::open(&urls, emit)?;

    let local = if target.is_file() {
        Some(Archive::open(target).with_context(|| format!("reading {}", target.display()))?)
    } else {
        None
    };

    // which of the tiles this area needs are missing here and present there
    let wanted = {
        let mut tiles = needed_tiles(b, request.shape, minzoom, maxzoom);
        tiles.sort_by_key(|&(z, x, y)| pmtiles::zxy_to_tile_id(z, x, y));
        tiles
    };
    emit(Event::Log(format!(
        "extract: {} tiles to place over {:.1},{:.1},{:.1},{:.1}, z{minzoom}-{maxzoom}{}",
        wanted.len(),
        b.0,
        b.1,
        b.2,
        b.3,
        if request.shape.is_some() { ", inside the area's outline" } else { "" }
    )));
    // (origin, offset, length, z, x, y)
    let mut missing: Vec<(usize, u64, u64, u8, u32, u32)> = Vec::new();
    for (i, &(z, x, y)) in wanted.iter().enumerate() {
        if cancelled() {
            return Ok(Report::default());
        }
        if local.as_ref().map(|l| l.contains(z, x, y)).transpose()?.unwrap_or(false) {
            continue;
        }
        if let Some((origin, offset, length)) = origins.locate(z, x, y)? {
            missing.push((origin, offset, length, z, x, y));
        }
        if i % 20_000 == 0 && i > 0 {
            emit(Event::Log(format!("extract: looked up {i} of {}", wanted.len())));
        }
    }

    if missing.is_empty() {
        // nothing under the area that the file lacks: either it was built for it, or the servers
        // have nothing there, and either way the answer is now known
        if target.is_file() {
            coverage.add(&source, area);
            coverage.save(target)?;
        }
        return Ok(Report { covered: true, finished: true, ..Default::default() });
    }
    let kind = origins.kind.ok_or_else(|| anyhow!("none of the archives could be opened"))?;

    // the work, in a fixed order, so that a resume means the same thing as it did when it stopped
    let old = match &local {
        Some(local) => {
            let mut all = local.addresses()?;
            // sequential reads of the old file, not hops around it
            all.sort_unstable_by_key(|a| a.1);
            all
        }
        None => Vec::new(),
    };
    missing.sort_unstable_by_key(|m| (m.0, m.1));
    let mut works: Vec<Work> = old.chunks(COPY_BATCH).map(|c| Work::Copy(c.to_vec())).collect();
    let concurrency = spec.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1);
    let mut groups: Vec<Group> = Vec::new();
    for &(origin, offset, length, z, x, y) in &missing {
        let end = offset + length;
        match groups.last_mut() {
            Some(g)
                if g.origin == origin
                    && offset <= g.end + GAP
                    && end.max(g.end) - g.start <= MAX_RANGE =>
            {
                g.end = g.end.max(end);
                g.tiles.push((z, x, y, offset, length as u32));
            }
            _ => groups.push(Group {
                origin,
                start: offset,
                end,
                tiles: vec![(z, x, y, offset, length as u32)],
            }),
        }
    }
    let requests = groups.len();
    // a batch is range requests to one archive, made together
    let mut batch: Vec<Group> = Vec::new();
    for group in groups {
        if batch.last().is_some_and(|last| last.origin != group.origin) {
            works.push(Work::Fetch(std::mem::take(&mut batch)));
        }
        batch.push(group);
        if batch.len() == concurrency {
            works.push(Work::Fetch(std::mem::take(&mut batch)));
        }
    }
    if !batch.is_empty() {
        works.push(Work::Fetch(batch));
    }

    let total_tiles = (old.len() + missing.len()) as u64;
    let mut identity = fnv(0xcbf2_9ce4_8422_2325, old.len() as u64);
    for m in &missing {
        identity = fnv(identity, m.1 ^ (u64::from(m.3) << 56) ^ ((m.0 as u64) << 44));
    }
    let local_stamp = std::fs::metadata(target)
        .ok()
        .map(|m| {
            let secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}:{secs}", m.len())
        })
        .unwrap_or_default();
    let identity = format!("extract:{minzoom}-{maxzoom}:{identity:016x}:{local_stamp}");

    let union = {
        let (mut w, mut s, mut e, mut n) = (b.0, b.1, b.2, b.3);
        if let Some(local) = &local {
            let h = local.header().bounds;
            if h.2 > h.0 && h.3 > h.1 {
                (w, s, e, n) = (w.min(h.0), s.min(h.1), e.max(h.2), n.max(h.3));
            }
        }
        (w, s, e, n)
    };
    let meta = ArchiveMeta {
        kind,
        minzoom: local.as_ref().map(|l| l.header().min_zoom.min(minzoom)).unwrap_or(minzoom),
        maxzoom: local.as_ref().map(|l| l.header().max_zoom.max(maxzoom)).unwrap_or(maxzoom),
        bounds: union,
        pairs: vec![
            ("name".into(), "mapterhorn".into()),
            ("format".into(), if kind == TileKind::Png { "png" } else { "webp" }.into()),
            ("type".into(), "baselayer".into()),
            ("description".into(), "an extract of terrain tile archives".into()),
            ("source".into(), urls.join(" ")),
            ("minzoom".into(), minzoom.to_string()),
            ("maxzoom".into(), maxzoom.to_string()),
            ("bounds".into(), format!("{},{},{},{}", union.0, union.1, union.2, union.3)),
        ],
    };

    let part = extending(target);
    let (mut archive, resumed) = match TerrainArchive::open_meta(
        &part,
        ArchiveFormat::PMTiles,
        &meta,
        &identity,
        ResumeMode::Auto,
    ) {
        Ok(opened) => opened,
        // a leftover from a different request is not this one's to continue, and not an
        // error worth stopping for: the next run would only ask for it to be discarded
        Err(_) => {
            TerrainArchive::discard_partial(&part, ArchiveFormat::PMTiles)?;
            TerrainArchive::open_meta(
                &part,
                ArchiveFormat::PMTiles,
                &meta,
                &identity,
                ResumeMode::Fresh,
            )?
        }
    };
    let first =
        resumed.as_ref().map(|r| r.commit.next_batch as usize).unwrap_or(0).min(works.len());
    if let Some(r) = &resumed {
        emit(Event::Log(format!(
            "extract: continuing from batch {} of {} ({} tiles already written)",
            first,
            works.len(),
            r.tiles
        )));
    }

    emit(Event::Phase("Extracting".into()));
    emit(Event::Log(format!(
        "extract: {} tiles kept, {} to fetch in {requests} requests from {} archive(s) -> {}",
        old.len(),
        missing.len(),
        origins.ups.len(),
        target.display()
    )));
    let mut report = Report::default();
    let mut done: u64 = works[..first]
        .iter()
        .map(|w| match w {
            Work::Copy(c) => c.len() as u64,
            Work::Fetch(g) => g.iter().map(|g| g.tiles.len() as u64).sum(),
        })
        .sum();

    for (index, work) in works.iter().enumerate().skip(first) {
        if cancelled() {
            emit(Event::Log(
                "extract: stopped; run it again to continue from the last finished batch".into(),
            ));
            return Ok(report);
        }
        match work {
            Work::Copy(tiles) => {
                let local = local.as_ref().ok_or_else(|| anyhow!("a copy batch with no file"))?;
                for &(id, offset, length) in tiles {
                    let (z, x, y) = pmtiles::tile_id_to_zxy(id);
                    let bytes = local.blob(offset, u64::from(length))?;
                    archive.add(z, x, y, &bytes)?;
                }
                report.kept += tiles.len() as u64;
                done += tiles.len() as u64;
            }
            Work::Fetch(groups) => {
                let up = &origins.ups[groups[0].origin];
                let url = up.url().to_string();
                let requests: Vec<_> =
                    groups.iter().map(|g| (url.clone(), Some((g.start, g.end - 1)))).collect();
                let replies = up.fetcher().get_many(requests, groups.len());
                for (group, reply) in groups.iter().zip(replies) {
                    let bytes = reply
                        .with_context(|| {
                            format!("fetching bytes {}-{} of {url}", group.start, group.end)
                        })?
                        .ok_or_else(|| anyhow!("{url} has nothing at {}", group.start))?;
                    if (bytes.len() as u64) < group.end - group.start {
                        bail!(
                            "{url} answered {} bytes for a range of {}",
                            bytes.len(),
                            group.end - group.start
                        );
                    }
                    for &(z, x, y, offset, length) in &group.tiles {
                        let from = (offset - group.start) as usize;
                        archive.add(z, x, y, &bytes[from..from + length as usize])?;
                        report.fetched += 1;
                        report.fetched_bytes += u64::from(length);
                        done += 1;
                    }
                }
            }
        }
        archive.commit(&Commit {
            zoom: 0,
            next_batch: (index + 1) as u32,
            wanted_count: works.len() as u64,
            wanted_hash: 0,
        })?;
        emit(Event::Progress {
            label: "extract".into(),
            percent: ((done * 100) / total_tiles.max(1)).min(100) as u8,
        });
    }

    archive.finish()?;
    drop(local);
    std::fs::rename(&part, target).with_context(|| format!("replacing {}", target.display()))?;
    coverage.add(&source, area);
    coverage.save(target)?;
    report.finished = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;

    use crate::terrain::source::Origins as OriginList;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};

    /// A small archive standing in for the planet: every tile z0-z4, each holding its own address.
    fn write_upstream(path: &Path) {
        let meta = ArchiveMeta {
            kind: TileKind::Webp,
            minzoom: 0,
            maxzoom: 4,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            pairs: vec![("name".into(), "upstream".into())],
        };
        let (mut archive, _) =
            TerrainArchive::open_meta(path, ArchiveFormat::PMTiles, &meta, "", ResumeMode::Fresh)
                .unwrap();
        for z in 0..=4u8 {
            for x in 0..(1u32 << z) {
                for y in 0..(1u32 << z) {
                    archive.add(z, x, y, format!("{z}/{x}/{y}").as_bytes()).unwrap();
                }
            }
        }
        archive.finish().unwrap();
    }

    /// Serves a file the way an archive host does: `Range` answered with 206.
    async fn serve_file(bytes: Arc<Vec<u8>>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        async fn handler(State(bytes): State<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
            let range = headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
            match range {
                Some((from, to)) if from < bytes.len() => {
                    let to = to.min(bytes.len() - 1);
                    (StatusCode::PARTIAL_CONTENT, bytes[from..=to].to_vec()).into_response()
                }
                _ => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(handler).with_state(bytes);
        (addr, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    fn run(spec: &SourceSpec, bounds: (f64, f64, f64, f64)) -> Report {
        run_with(spec, bounds, None, None)
    }

    fn run_with(
        spec: &SourceSpec,
        bounds: (f64, f64, f64, f64),
        shape: Option<&Polygon>,
        maxzoom: Option<u8>,
    ) -> Report {
        ensure(spec, &Request { bounds, shape, maxzoom }, &|| false, &mut |_| {}).unwrap()
    }

    fn tile_text(archive: &Archive, z: u8, x: u32, y: u32) -> Option<String> {
        archive.tile(z, x, y).unwrap().map(|b| String::from_utf8(b).unwrap())
    }

    /// The whole contract: an area is fetched into a file that holds the tiles it needs and not
    /// the rest; asking again is free; a bigger area grows the same file, keeping what it had; and
    /// once covered, not a request is made - the server is gone and it still answers.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_grows_to_cover_each_area_and_is_never_asked_twice() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.pmtiles");
        write_upstream(&upstream);
        let (addr, server) = serve_file(Arc::new(std::fs::read(&upstream).unwrap())).await;

        let spec = SourceSpec {
            name: "m".into(),
            kind: "pmtiles".into(),
            path: dir.path().join("data/m.pmtiles"),
            extract_from: Some(OriginList::One(format!("http://{addr}/planet.pmtiles"))),
            maxzoom: Some(4),
            ..Default::default()
        };
        let small = (6.0, 45.0, 7.0, 46.0);
        let big = (-5.0, 40.0, 10.0, 50.0);

        let first = {
            let spec = spec.clone();
            tokio::task::spawn_blocking(move || run(&spec, small)).await.unwrap()
        };
        assert!(first.finished && !first.covered, "{first:?}");
        assert_eq!(first.kept, 0);
        assert!(first.fetched > 0);

        let local = Archive::open(&spec.path).unwrap();
        for (z, x, y) in needed_tiles(snap(small), None, 0, 4) {
            assert_eq!(tile_text(&local, z, x, y), Some(format!("{z}/{x}/{y}")), "{z}/{x}/{y}");
        }
        // z4 spans 16 tiles across, and 6 E is column 8: the far corner was never asked for
        assert_eq!(tile_text(&local, 4, 0, 0), None);
        drop(local);

        let again = {
            let spec = spec.clone();
            tokio::task::spawn_blocking(move || run(&spec, small)).await.unwrap()
        };
        assert!(again.covered && again.fetched == 0, "{again:?}");

        let grown = {
            let spec = spec.clone();
            tokio::task::spawn_blocking(move || run(&spec, big)).await.unwrap()
        };
        assert!(grown.finished && !grown.covered, "{grown:?}");
        assert!(grown.kept >= first.fetched, "everything it had is kept: {grown:?}");
        let local = Archive::open(&spec.path).unwrap();
        for (z, x, y) in needed_tiles(snap(small), None, 0, 4).into_iter().chain(needed_tiles(
            snap(big),
            None,
            0,
            4,
        )) {
            assert_eq!(tile_text(&local, z, x, y), Some(format!("{z}/{x}/{y}")), "{z}/{x}/{y}");
        }
        drop(local);

        server.abort();
        let offline = {
            let spec = spec.clone();
            tokio::task::spawn_blocking(move || run(&spec, big)).await.unwrap()
        };
        assert!(offline.covered, "a covered area must not need the network: {offline:?}");
        assert!(!extending(&spec.path).exists(), "no scratch file is left behind");
    }

    /// A stopped extract leaves the file it was growing exactly as it was, and the next run
    /// finishes the job.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_extract_leaves_the_old_file_alone_and_a_rerun_finishes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.pmtiles");
        write_upstream(&upstream);
        let (addr, server) = serve_file(Arc::new(std::fs::read(&upstream).unwrap())).await;
        let spec = SourceSpec {
            name: "m".into(),
            kind: "pmtiles".into(),
            path: dir.path().join("m.pmtiles"),
            extract_from: Some(OriginList::One(format!("http://{addr}/planet.pmtiles"))),
            maxzoom: Some(4),
            ..Default::default()
        };
        let small = (6.0, 45.0, 7.0, 46.0);
        let big = (-5.0, 40.0, 10.0, 50.0);

        let (s1, s2) = (spec.clone(), spec.clone());
        tokio::task::spawn_blocking(move || run(&s1, small)).await.unwrap();
        let before = std::fs::read(&spec.path).unwrap();

        let stopped = tokio::task::spawn_blocking(move || {
            let stop = AtomicBool::new(false);
            let mut emit = |event: Event| {
                if matches!(&event, Event::Phase(p) if p == "Extracting") {
                    stop.store(true, Ordering::SeqCst);
                }
            };
            let request = Request { bounds: big, shape: None, maxzoom: None };
            ensure(&s2, &request, &|| stop.load(Ordering::SeqCst), &mut emit).unwrap()
        })
        .await
        .unwrap();
        assert!(!stopped.finished, "{stopped:?}");
        assert_eq!(std::fs::read(&spec.path).unwrap(), before, "the old file is untouched");

        let s3 = spec.clone();
        let done = tokio::task::spawn_blocking(move || run(&s3, big)).await.unwrap();
        assert!(done.finished, "{done:?}");
        let local = Archive::open(&spec.path).unwrap();
        for (z, x, y) in needed_tiles(snap(big), None, 0, 4) {
            assert_eq!(tile_text(&local, z, x, y), Some(format!("{z}/{x}/{y}")));
        }
        server.abort();
    }

    #[test]
    fn the_box_is_rounded_out_to_a_tenth_of_a_degree() {
        assert_eq!(snap((4.72, 44.1, 7.21, 46.99)), (4.7, 44.1, 7.3, 47.0));
        assert_eq!(snap((-181.0, -90.0, 181.0, 90.0)), (-180.0, -85.0511, 180.0, 85.0511));
    }

    /// One ring around the tiles under the box, at every zoom, and never past the edge of the world.
    #[test]
    fn a_ring_of_tiles_is_added_around_the_area() {
        let tiles = needed_tiles((6.0, 45.0, 7.0, 46.0), None, 5, 5);
        let xs: Vec<u32> = tiles.iter().map(|t| t.1).collect();
        // z5 is 32 tiles across and 6..7 E is x 16.5..16.6, all in column 16: so 15 to 17
        assert_eq!((xs.iter().min(), xs.iter().max()), (Some(&15), Some(&17)));
        assert!(tiles.iter().all(|t| t.0 == 5));

        let whole = needed_tiles((-180.0, -85.0511, 180.0, 85.0511), None, 2, 2);
        assert_eq!(whole.len(), 16, "z2 is 4x4 and the ring cannot add tiles that do not exist");
    }

    #[test]
    fn coverage_answers_only_for_what_it_was_grown_to() {
        let mut c = Coverage::default();
        let area = |b: [f64; 4], z: u8| Area { bounds: b, minzoom: 0, maxzoom: z, shape: None };
        c.add("u", area([0.0, 40.0, 10.0, 50.0], 12));
        assert!(c.covers("u", &area([2.0, 42.0, 8.0, 48.0], 12)));
        assert!(c.covers("u", &area([2.0, 42.0, 8.0, 48.0], 10)));
        assert!(!c.covers("u", &area([2.0, 42.0, 12.0, 48.0], 12)), "reaches past the east edge");
        assert!(!c.covers("u", &area([2.0, 42.0, 8.0, 48.0], 13)), "deeper than it went");
        assert!(!c.covers("other", &area([2.0, 42.0, 8.0, 48.0], 12)), "a different archive");

        // a box inside one already held is not kept; a bigger one replaces it
        c.add("u", area([2.0, 42.0, 8.0, 48.0], 12));
        assert_eq!(c.areas.len(), 1);
        c.add("u", area([-5.0, 35.0, 20.0, 60.0], 12));
        assert_eq!(c.areas.len(), 1);
        assert_eq!(c.areas[0].bounds, [-5.0, 35.0, 20.0, 60.0]);
    }

    /// Several files served by name; anything not in the list is a 404, as a region with no
    /// archive is.
    async fn serve_files(
        files: HashMap<String, Arc<Vec<u8>>>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        async fn handler(
            State(files): State<Arc<HashMap<String, Arc<Vec<u8>>>>>,
            uri: axum::http::Uri,
            headers: HeaderMap,
        ) -> Response {
            let Some(bytes) = files.get(uri.path()) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let range = headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
            match range {
                Some((from, to)) if from < bytes.len() => {
                    let to = to.min(bytes.len() - 1);
                    (StatusCode::PARTIAL_CONTENT, bytes[from..=to].to_vec()).into_response()
                }
                _ => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(handler).with_state(Arc::new(files));
        (addr, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    fn write_tiles(path: &Path, minzoom: u8, maxzoom: u8, tiles: &[(u8, u32, u32)]) {
        let meta = ArchiveMeta {
            kind: TileKind::Webp,
            minzoom,
            maxzoom,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            pairs: vec![("name".into(), "upstream".into())],
        };
        let (mut archive, _) =
            TerrainArchive::open_meta(path, ArchiveFormat::PMTiles, &meta, "", ResumeMode::Fresh)
                .unwrap();
        for &(z, x, y) in tiles {
            archive.add(z, x, y, format!("{z}/{x}/{y}").as_bytes()).unwrap();
        }
        archive.finish().unwrap();
    }

    /// What mapterhorn does: the planet archive stops at z6 here, and anything finer lives in an
    /// archive per z6 tile - of which one does not exist, because that region is empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn finer_tiles_come_from_the_regional_archive_that_holds_them() {
        let dir = tempfile::tempdir().unwrap();
        let area = (6.0, 45.0, 6.3, 45.2);
        let wanted = needed_tiles(snap(area), None, 7, 8);
        let mut regions: Vec<(u32, u32)> =
            wanted.iter().map(|&(z, x, y)| (x >> (z - 6), y >> (z - 6))).collect();
        regions.sort();
        regions.dedup();
        assert!(regions.len() >= 2, "the test needs an area that spans regions: {regions:?}");
        let empty = regions[0];

        let mut files: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        let planet: Vec<(u8, u32, u32)> = (0..=6u8)
            .flat_map(|z| (0..1u32 << z).flat_map(move |x| (0..1u32 << z).map(move |y| (z, x, y))))
            .collect();
        let path = dir.path().join("planet.pmtiles");
        write_tiles(&path, 0, 6, &planet);
        files.insert("/planet.pmtiles".into(), Arc::new(std::fs::read(&path).unwrap()));
        for &(x6, y6) in regions.iter().filter(|r| **r != empty) {
            let mut tiles = Vec::new();
            for z in 7..=8u8 {
                let n = 1u32 << (z - 6);
                for dx in 0..n {
                    for dy in 0..n {
                        tiles.push((z, (x6 << (z - 6)) + dx, (y6 << (z - 6)) + dy));
                    }
                }
            }
            let path = dir.path().join(format!("6-{x6}-{y6}.pmtiles"));
            write_tiles(&path, 7, 8, &tiles);
            files.insert(format!("/6-{x6}-{y6}.pmtiles"), Arc::new(std::fs::read(&path).unwrap()));
        }
        let (addr, server) = serve_files(files).await;

        let spec = SourceSpec {
            name: "m".into(),
            kind: "pmtiles".into(),
            path: dir.path().join("m.pmtiles"),
            extract_from: Some(OriginList::Many(vec![
                format!("http://{addr}/planet.pmtiles"),
                format!("http://{addr}/6-{{x}}-{{y}}.pmtiles"),
            ])),
            maxzoom: Some(8),
            ..Default::default()
        };
        let s = spec.clone();
        let report = tokio::task::spawn_blocking(move || run(&s, area)).await.unwrap();
        assert!(report.finished && report.fetched > 0, "{report:?}");

        let local = Archive::open(&spec.path).unwrap();
        assert_eq!(local.header().max_zoom, 8);
        for (z, x, y) in needed_tiles(snap(area), None, 0, 8) {
            let region = (x >> z.saturating_sub(6), y >> z.saturating_sub(6));
            let expected =
                if z >= 7 && region == empty { None } else { Some(format!("{z}/{x}/{y}")) };
            assert_eq!(tile_text(&local, z, x, y), expected, "{z}/{x}/{y}");
        }
        drop(local);

        server.abort();
        let s = spec.clone();
        let again = tokio::task::spawn_blocking(move || run(&s, area)).await.unwrap();
        assert!(again.covered, "{again:?}");
    }

    /// A graph build asks for z12 at most, and a terrain build for the zoom it renders: asking for
    /// less first must leave the finer tiles to be added later, not be taken as having them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_zoom_cap_fetches_less_and_the_rest_is_added_later() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.pmtiles");
        write_upstream(&upstream);
        let (addr, server) = serve_file(Arc::new(std::fs::read(&upstream).unwrap())).await;
        let spec = SourceSpec {
            name: "m".into(),
            kind: "pmtiles".into(),
            path: dir.path().join("m.pmtiles"),
            extract_from: Some(OriginList::One(format!("http://{addr}/planet.pmtiles"))),
            maxzoom: Some(4),
            ..Default::default()
        };
        let area = (6.0, 45.0, 7.0, 46.0);

        let s = spec.clone();
        let shallow =
            tokio::task::spawn_blocking(move || run_with(&s, area, None, Some(2))).await.unwrap();
        assert!(shallow.finished && !shallow.covered);
        let local = Archive::open(&spec.path).unwrap();
        assert!(tile_text(&local, 2, 2, 1).is_some() || tile_text(&local, 2, 2, 2).is_some());
        assert!(needed_tiles(snap(area), None, 4, 4)
            .iter()
            .all(|&(z, x, y)| tile_text(&local, z, x, y).is_none()));
        drop(local);

        let s = spec.clone();
        let full = tokio::task::spawn_blocking(move || run(&s, area)).await.unwrap();
        assert!(full.finished && !full.covered, "a cap is not coverage: {full:?}");
        assert!(full.kept > 0);
        let local = Archive::open(&spec.path).unwrap();
        for (z, x, y) in needed_tiles(snap(area), None, 0, 4) {
            assert_eq!(tile_text(&local, z, x, y), Some(format!("{z}/{x}/{y}")));
        }
        server.abort();
    }

    /// An outline keeps the tiles it touches and drops the corners of its bounding box.
    #[test]
    fn an_outline_drops_the_corners_of_the_box() {
        // a thin band from the south-west to the north-east of a 8 x 8 degree box
        let band = Polygon::from_str(
            "band\n1\n   0.0 40.0\n   0.4 40.0\n   8.4 48.0\n   8.0 48.0\n   0.0 40.0\nEND\nEND\n",
        )
        .unwrap();
        let bounds = band.bounds();
        let with = needed_tiles(bounds, Some(&band), 11, 11);
        let without = needed_tiles(bounds, None, 11, 11);
        assert!(with.len() * 3 < without.len(), "{} of {}", with.len(), without.len());
        // the middle of the band is kept; the far north-west corner of the box is not
        let at = |lon: f64, lat: f64| {
            let n = 2048.0;
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let r = lat.to_radians();
            let y =
                ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
            (11u8, x, y)
        };
        assert!(with.contains(&at(4.2, 44.0)));
        assert!(!with.contains(&at(0.5, 47.5)));
    }
}
