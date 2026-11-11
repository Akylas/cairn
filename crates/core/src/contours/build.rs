//! The contour build loop, shared by `cairn contours` and the desktop app.
//!
//! The shape of [`crate::terrain::build`] on purpose, because it has the same three needs and the
//! same answers: a build over a region is long enough to be interrupted, so a batch is committed
//! once written and a restart continues from the last one; the sources may be a network, so the
//! tiles the next batch reads are fetched while this one renders; and nobody can tell a slow build
//! from a hung one without it saying what it is doing, so it reports every few seconds.
//!
//! What is different is the unit. A terrain tile is a fixed grid of pixels; here a tile is a grid
//! chosen per zoom ([`super::grid_for`]), traced into lines, and a tile with no line in it is not
//! written at all - so the count of tiles written is not the count visited.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use rayon::prelude::*;

use super::tileset::{self, TileFilter};
use super::{march::Marcher, ContourOptions};
use crate::poly::Polygon;
use crate::steps::archive::ArchiveFormat;
use crate::terrain::archive::{ArchiveMeta, Commit, ResumeMode, TerrainArchive, TileKind};
use crate::terrain::build::{
    fingerprint, fnv1a, hash_file, ms, report, snapshot, totals, Event, Outcome, Snapshot, Spent,
    FNV_OFFSET, MANY_REQUESTS, REPORT_EVERY,
};
use crate::terrain::fetch::Counters;
use crate::terrain::render::{self, EncodedTile, RenderPool};
use crate::terrain::source::{CompositeSource, SourceSpec};

pub struct Job<'a> {
    pub specs: &'a [SourceSpec],
    pub opts: &'a ContourOptions,
    pub bounds: (f64, f64, f64, f64),
    pub shape: Option<&'a Polygon>,
    /// The `.poly` the shape came from, whose contents are part of what makes two builds the same.
    pub shape_file: Option<&'a Path>,
    /// Only tiles this archive also has are rendered.
    pub only_tiles_of: Option<&'a Path>,
    pub archive_format: ArchiveFormat,
    pub output: &'a Path,
    pub name: &'a str,
    pub mode: ResumeMode,
    /// Tiles per batch; [`BATCH`] unless a test wants small ones.
    pub batch: usize,
}

/// Everything that decides what the tiles look like, as one string.
///
/// Two runs may share a partial archive only when this is equal; what only changes how fast tiles
/// arrive - the number of workers, request concurrency - is left out on purpose.
pub fn identity(job: &Job) -> String {
    let mut text = format!(
        "contours-rev{};batch{};{:?};{};bounds{:x},{:x},{:x},{:x};shape{};",
        super::RENDER_REVISION,
        job.batch,
        job.opts,
        job.archive_format.extension(),
        job.bounds.0.to_bits(),
        job.bounds.1.to_bits(),
        job.bounds.2.to_bits(),
        job.bounds.3.to_bits(),
        hash_file(job.shape_file)
    );
    if let Some(path) = job.only_tiles_of {
        text.push_str(&format!("only{};", tileset::fingerprint(path)));
    }
    for spec in job.specs {
        text.push_str(&format!(
            "[{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}]",
            spec.name,
            spec.kind,
            spec.path.display(),
            spec.url,
            spec.encoding,
            spec.tile_size,
            spec.minzoom,
            spec.maxzoom,
            spec.clamp_min,
            spec.download
        ));
    }
    format!("{:016x}", fnv1a(FNV_OFFSET, text.as_bytes()))
}

/// The tiles of one zoom the build will visit, in the order it visits them.
///
/// Column-major, so each worker walks a run of tiles that sit next to each other and its sources'
/// caches keep hitting.
pub fn wanted_tiles(job: &Job, filter: Option<&TileFilter>, zoom: u8) -> Vec<(u32, u32)> {
    let (x0, y0, x1, y1) = render::tile_range(zoom, job.bounds);
    (x0..=x1)
        .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            if let Some(filter) = filter {
                if !filter.contains(zoom, x, y) {
                    return false;
                }
            }
            let Some(shape) = job.shape else { return true };
            let (w, s, e, n) = render::tile_bounds(zoom, x, y);
            shape.intersects_rect(w, s, e, n)
        })
        .collect()
}

/// What the archive says about itself. The `tilemask` is the area's shape, as the app that reads
/// these asks for.
pub fn archive_meta(job: &Job) -> ArchiveMeta {
    let opts = job.opts;
    let layer = serde_json::json!({
        "vector_layers": [{
            "id": opts.layer,
            "description": format!("Contour lines every {} m", opts.interval),
            "minzoom": opts.minzoom,
            "maxzoom": opts.maxzoom,
            "fields": { "ele": "Number", "div": "Number" },
        }]
    });
    let mut pairs = vec![
        ("name", job.name.to_string()),
        ("format", "pbf".to_string()),
        ("type", "overlay".to_string()),
        ("version", "1".to_string()),
        ("description", format!("Contour lines every {} m", opts.interval)),
        ("generator", format!("cairn {}", env!("CARGO_PKG_VERSION"))),
        ("minzoom", opts.minzoom.to_string()),
        ("maxzoom", opts.maxzoom.to_string()),
        ("bounds", format!("{},{},{},{}", job.bounds.0, job.bounds.1, job.bounds.2, job.bounds.3)),
        ("json", layer.to_string()),
    ];
    if let Some(shape) = job.shape {
        pairs.push(("tilemask", crate::valhalla::tilemask::for_polygon(shape, opts.polyzoom)));
    }
    ArchiveMeta {
        kind: TileKind::Mvt,
        minzoom: opts.minzoom,
        maxzoom: opts.maxzoom,
        bounds: job.bounds,
        pairs: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

/// One worker's scratch space: the contouring buffers live as long as its thread does.
struct Scratch(std::cell::RefCell<Marcher>);

thread_local! {
    static SCRATCH: Scratch = Scratch(std::cell::RefCell::new(Marcher::new()));
}

/// Render one tile: sample, trace, encode. `None` for a tile with no line in it.
pub fn render_one(
    source: &mut CompositeSource,
    zoom: u8,
    x: u32,
    y: u32,
    grid: &super::GridSpec,
    opts: &ContourOptions,
) -> Option<Vec<u8>> {
    let values = super::sample_grid(source, zoom, x, y, grid, opts.blur_m)?;
    SCRATCH.with(|scratch| {
        let mut marcher = scratch.0.borrow_mut();
        super::render_tile(&mut marcher, &values, grid, zoom, opts)
    })
}

/// Run the build. Returns when the archive is finished or `cancelled` says stop.
pub fn run(
    job: &Job,
    cancelled: &(dyn Fn() -> bool + Sync),
    emit: &mut dyn FnMut(Event),
) -> Result<Outcome> {
    job.opts.check()?;
    let (probe, skipped) = CompositeSource::open(job.specs)?;
    for note in &skipped {
        emit(Event::Log(format!("skipped {note}")));
    }
    emit(Event::Log(format!("sources, highest priority first: {:?}", probe.names())));
    if probe.names().is_empty() {
        bail!("no usable elevation sources");
    }
    drop(probe);

    let filter = match job.only_tiles_of {
        Some(path) => {
            let filter = TileFilter::open(path, job.opts.minzoom, job.opts.maxzoom)?;
            emit(Event::Log(format!("keeping only the tiles {} has", path.display())));
            Some(filter)
        }
        None => None,
    };

    let identity = identity(job);
    let meta = archive_meta(job);
    let (mut archive, resumed) =
        TerrainArchive::open_meta(job.output, job.archive_format, &meta, &identity, job.mode)?;
    let resume_point = resumed.as_ref().map(|r| r.commit.clone());
    let mut written = resumed.as_ref().map(|r| r.tiles).unwrap_or(0);
    if let Some(r) = &resumed {
        emit(Event::Log(format!(
            "resuming at z{} batch {}: {} tiles, {:.1} MB already written",
            r.commit.zoom,
            r.commit.next_batch,
            r.tiles,
            r.bytes as f64 / 1_048_576.0
        )));
    }

    // no more renderers than there are tiles to render at the busiest zoom: opening one reads a
    // GeoTIFF's directory, and a three-tile build must not pay for eighteen of them
    let biggest = {
        let (x0, y0, x1, y1) = render::tile_range(job.opts.maxzoom, job.bounds);
        ((x1 - x0 + 1) as usize) * ((y1 - y0 + 1) as usize)
    };
    let pool = RenderPool::new(biggest, || Ok(CompositeSource::open(job.specs)?.0))?;
    emit(Event::Log(format!("{} render threads", pool.len())));
    let counters: Vec<(String, Arc<Counters>)> =
        pool.all_sources().filter_map(|s| s.lock().ok()).flat_map(|s| s.counters()).collect();

    let centre = super::centre_latitude(job.bounds);
    let mut last_report = Instant::now();
    let mut last_snapshot: Snapshot = snapshot(&counters).0;
    let mut spent = Spent::default();
    let visited_since_report = AtomicU64::new(0);
    let written_since_report = AtomicU64::new(0);
    let started = Instant::now();
    let batch_size = job.batch.max(1);

    for zoom in job.opts.minzoom..=job.opts.maxzoom {
        if matches!(&resume_point, Some(c) if zoom < c.zoom) {
            continue;
        }
        if cancelled() {
            return Ok(stopped(&pool, written));
        }
        let grid = super::grid_for(job.opts, zoom, centre);
        let wanted = wanted_tiles(job, filter.as_ref(), zoom);
        let (count, hash) = (wanted.len() as u64, fingerprint(&wanted));
        let first_batch = match &resume_point {
            Some(c) if c.zoom == zoom => {
                if c.wanted_count != count || c.wanted_hash != hash {
                    bail!(
                        "z{zoom} has {count} tiles to render now and had {} when this build was \
                         interrupted, so its batches no longer line up. Start over to discard it.",
                        c.wanted_count
                    );
                }
                c.next_batch as usize
            }
            _ => 0,
        };
        let batches: Vec<&[(u32, u32)]> = wanted.chunks(batch_size).collect();
        emit(Event::Phase(format!("z{zoom}")));
        emit(Event::Log(format!(
            "z{zoom}: {count} tiles, a {0}x{0} grid each ({1:.1} m between samples)",
            grid.side(),
            grid_metres(zoom, centre, &grid)
        )));
        if first_batch >= batches.len() {
            continue;
        }

        let queries = |tiles: &[(u32, u32)]| -> Vec<_> {
            tiles.iter().map(|&(x, y)| super::tile_query(zoom, x, y, &grid)).collect()
        };
        // the first batch has nothing to overlap with
        if let Some(first) = batches.get(first_batch) {
            let began = Instant::now();
            pool.prefetch_queries(&queries(first));
            spent.waiting += ms(began);
        }

        for index in first_batch..batches.len() {
            if cancelled() {
                return Ok(stopped(&pool, written));
            }
            let batch = batches[index];
            let began = Instant::now();
            let mut waited = 0;
            let pool_ref = &pool;
            let rendered: Result<Vec<EncodedTile>> = std::thread::scope(|scope| {
                let ahead = batches.get(index + 1).map(|next| {
                    let next_queries = queries(next);
                    scope.spawn(move || pool_ref.prefetch_queries(&next_queries))
                });
                let done: Result<Vec<Option<EncodedTile>>> = batch
                    .par_iter()
                    .map(|&(x, y)| {
                        let bytes = pool.with_source(|source| {
                            render_one(source, zoom, x, y, &grid, job.opts)
                        })?;
                        Ok(bytes.map(|bytes| (x, y, bytes)))
                    })
                    .collect();
                // whatever the prefetch still has to do is time the workers could not use
                let waiting = Instant::now();
                if let Some(handle) = ahead {
                    let _ = handle.join();
                }
                waited = ms(waiting);
                Ok(done?.into_iter().flatten().collect())
            });
            let rendered = rendered?;
            spent.render += ms(began).saturating_sub(waited);
            spent.waiting += waited;

            let writing = Instant::now();
            for (x, y, bytes) in &rendered {
                archive.add(zoom, *x, *y, bytes)?;
            }
            archive.commit(&Commit {
                zoom,
                next_batch: index as u32 + 1,
                wanted_count: count,
                wanted_hash: hash,
            })?;
            spent.archive += ms(writing);
            spent.batches += 1;
            spent.tiles += batch.len() as u64;
            written += rendered.len() as u64;
            visited_since_report.fetch_add(batch.len() as u64, Ordering::Relaxed);
            written_since_report.fetch_add(rendered.len() as u64, Ordering::Relaxed);

            let done = ((index + 1) * batch_size).min(wanted.len());
            let percent = ((done as u64 * 100) / count.max(1)).min(100) as u8;
            emit(Event::Progress { label: format!("z{zoom}"), percent });

            if last_report.elapsed() >= REPORT_EVERY {
                let (now, in_flight) = snapshot(&counters);
                emit(Event::Log(report(
                    zoom,
                    percent,
                    last_report.elapsed(),
                    &spent,
                    &last_snapshot,
                    &now,
                    in_flight,
                    written_since_report.swap(0, Ordering::Relaxed),
                )));
                visited_since_report.store(0, Ordering::Relaxed);
                last_snapshot = now;
                last_report = Instant::now();
                spent = Spent::default();
            }
        }
    }

    archive.finish()?;

    let stats = totals(&pool);
    let requests: u64 = stats.iter().map(|(_, s)| s.requests).sum();
    for (name, s) in &stats {
        emit(Event::Log(format!("{name}: {s}")));
        if s.unreadable > 0 {
            emit(Event::Log(format!(
                "warning: {name} left {} tiles unread; this archive has holes",
                s.unreadable
            )));
        }
    }
    if requests >= MANY_REQUESTS {
        emit(Event::Log(format!(
            "{requests} requests went to a public tile service. For a build this size, read an \
             extract of its PMTiles archive from disk instead (see docs/pipeline-reference.md); it \
             is faster and costs the service nothing."
        )));
    }
    emit(Event::Log(format!("traced {written} tiles in {:.0}s", started.elapsed().as_secs_f64())));
    Ok(Outcome { finished: true, written, stats })
}

fn stopped(pool: &RenderPool, written: u64) -> Outcome {
    Outcome { finished: false, written, stats: totals(pool) }
}

/// The ground distance between two samples at the middle of the job, for the log.
fn grid_metres(zoom: u8, centre_lat: f64, grid: &super::GridSpec) -> f64 {
    40_075_016.686 * centre_lat.to_radians().cos() / f64::from(1u32 << zoom) / grid.cells as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::archive::TerrainArchive;
    use std::sync::atomic::AtomicUsize;

    /// A degree square that rises to the north-east in a way that is not flat anywhere, so a
    /// wrong tile is a different tile.
    fn hgt(dir: &Path) {
        let size = 101usize;
        let mut grid = Vec::new();
        for row in 0..size {
            for col in 0..size {
                // 1000 m at the south-west corner, 20 m of rise per row going north and a ripple
                let rise = (size - 1 - row) as f64 * 20.0 + col as f64 * 6.0;
                let ripple = ((row as f64 / 6.0).sin() * (col as f64 / 5.0).cos()) * 40.0;
                grid.extend_from_slice(&((1000.0 + rise + ripple) as i16).to_be_bytes());
            }
        }
        std::fs::write(dir.join("N44E006.hgt"), grid).unwrap();
    }

    fn specs(dir: &Path) -> Vec<SourceSpec> {
        vec![SourceSpec {
            name: "hgt".into(),
            kind: "valhalla".into(),
            path: dir.to_path_buf(),
            ..Default::default()
        }]
    }

    fn opts() -> ContourOptions {
        ContourOptions { minzoom: 11, maxzoom: 12, ..Default::default() }
    }

    fn job<'a>(
        specs: &'a [SourceSpec],
        opts: &'a ContourOptions,
        output: &'a Path,
        format: ArchiveFormat,
    ) -> Job<'a> {
        Job {
            specs,
            opts,
            bounds: (6.30, 44.30, 6.45, 44.45),
            shape: None,
            shape_file: None,
            only_tiles_of: None,
            archive_format: format,
            output,
            name: "t_contours",
            mode: ResumeMode::Auto,
            // small, so a zoom spans several batches
            batch: 2,
        }
    }

    fn quiet() -> impl FnMut(Event) {
        |_| {}
    }

    fn tiles(path: &Path) -> Vec<(u32, u32, u32, Vec<u8>)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut q = conn
            .prepare(
                "SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles ORDER BY 1,2,3",
            )
            .unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn it_builds_an_archive_a_reader_can_use() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let (specs, opts) = (specs(sources.path()), opts());
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("c.mbtiles");

        let outcome =
            run(&job(&specs, &opts, &path, ArchiveFormat::MBTiles), &|| false, &mut quiet())
                .unwrap();
        assert!(outcome.finished && outcome.written > 2, "{}", outcome.written);

        let conn = rusqlite::Connection::open(&path).unwrap();
        let meta = |key: &str| -> String {
            conn.query_row("SELECT value FROM metadata WHERE name = ?", [key], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(meta("format"), "pbf");
        assert_eq!(meta("minzoom"), "11");
        assert_eq!(meta("maxzoom"), "12");
        assert!(meta("json").contains("\"id\":\"contour\""));

        let rows = tiles(&path);
        assert!(rows.iter().all(|r| r.0 == 11 || r.0 == 12));
        // every tile is a gzipped tile whose lines are 10 m multiples at the right importance
        for (_, _, _, data) in &rows {
            assert_eq!(&data[..2], &[0x1f, 0x8b]);
            let layer = super::super::mvt::decode::layer(&gunzip(data));
            assert_eq!((layer.name.as_str(), layer.extent), ("contour", 4096));
            assert!(!layer.features.is_empty());
            for f in &layer.features {
                assert_eq!(f.ele % 10, 0, "lines are on the interval: {}", f.ele);
                assert!(f.div >= 100, "at z11-12 only the 100 m lines and up are drawn: {}", f.ele);
                assert_eq!(f.div, i64::from(super::super::division(f.ele, 10)));
                assert!(f.points.len() >= 2);
            }
        }
    }

    /// The two tiles either side of a shared edge trace the same line through it, to the unit:
    /// the lattice they sample is the same, so a break would be a bug in how it is anchored.
    #[test]
    fn neighbouring_tiles_meet_on_their_shared_edge() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = specs(sources.path());
        let opts = ContourOptions { simplify: 0.0, interval: 10, ..opts() };
        let (mut source, _) = CompositeSource::open(&specs).unwrap();
        let grid = super::super::grid_for(&opts, 14, 44.4);
        // two tiles side by side in the middle of the square, at 6.4 E and 44.4 N
        let n = f64::from(1u32 << 14);
        let x = ((6.4 + 180.0) / 360.0 * n) as u32;
        let y =
            ((1.0 - 44.4f64.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
        let z = 14u8;
        let side = grid.side();
        let left = super::super::sample_grid(&mut source, z, x, y, &grid, 0.0).unwrap();
        let right = super::super::sample_grid(&mut source, z, x + 1, y, &grid, 0.0).unwrap();
        // the left tile's last real column is the right tile's first, and its margin overlaps
        for row in 0..side {
            for k in 0..=2 * grid.margin {
                let l = left[row * side + grid.margin + grid.cells + k.min(grid.margin)];
                let r = right[row * side + grid.margin + k.min(grid.margin)];
                if k <= grid.margin {
                    assert_eq!(l, r, "row {row}, {k} cells past the edge");
                }
            }
        }
    }

    #[test]
    fn a_tile_with_nothing_under_it_is_not_written() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = specs(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("c.mbtiles");
        let mut far = job(&specs, &opts, &path, ArchiveFormat::MBTiles);
        // a degree east of the only .hgt there is
        far.bounds = (7.30, 44.30, 7.45, 44.45);
        let outcome = run(&far, &|| false, &mut quiet()).unwrap();
        assert_eq!(outcome.written, 0);
        assert!(tiles(&path).is_empty());
    }

    /// Run a build that is stopped after a few batches, then run it again to the end, and compare
    /// with one that was never interrupted.
    fn interrupted_equals_uninterrupted(format: ArchiveFormat) {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let (specs, opts) = (specs(sources.path()), opts());
        let out = tempfile::tempdir().unwrap();
        let ext = format.extension();
        let whole = out.path().join(format!("whole.{ext}"));
        let broken = out.path().join(format!("broken.{ext}"));

        let outcome = run(&job(&specs, &opts, &whole, format), &|| false, &mut quiet()).unwrap();
        assert!(outcome.finished && outcome.written > 4, "{}", outcome.written);

        let checks = AtomicUsize::new(0);
        let stop = || checks.fetch_add(1, Ordering::Relaxed) >= 4;
        let first = run(&job(&specs, &opts, &broken, format), &stop, &mut quiet()).unwrap();
        assert!(!first.finished);
        assert!(!broken.exists(), "an interrupted build must not leave something that looks built");
        assert!(TerrainArchive::partial_info(&broken, format).is_some());

        let mut log = Vec::new();
        let second = run(&job(&specs, &opts, &broken, format), &|| false, &mut |e| {
            if let Event::Log(line) = e {
                log.push(line);
            }
        })
        .unwrap();
        assert!(second.finished);
        assert!(log.iter().any(|l| l.starts_with("resuming at z")), "{log:?}");
        assert_eq!(second.written, outcome.written);

        match format {
            ArchiveFormat::MBTiles => assert_eq!(tiles(&whole), tiles(&broken)),
            ArchiveFormat::PMTiles => {
                assert_eq!(std::fs::read(&whole).unwrap(), std::fs::read(&broken).unwrap())
            }
        }
    }

    #[test]
    fn an_interrupted_mbtiles_build_finishes_as_if_it_had_not_been() {
        interrupted_equals_uninterrupted(ArchiveFormat::MBTiles);
    }

    #[test]
    fn an_interrupted_pmtiles_build_finishes_as_if_it_had_not_been() {
        interrupted_equals_uninterrupted(ArchiveFormat::PMTiles);
    }

    #[test]
    fn changed_settings_are_refused_not_mixed_in() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = specs(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("c.mbtiles");
        let checks = AtomicUsize::new(0);
        let stop = || checks.fetch_add(1, Ordering::Relaxed) >= 3;
        let first =
            run(&job(&specs, &opts, &path, ArchiveFormat::MBTiles), &stop, &mut quiet()).unwrap();
        assert!(!first.finished);

        let other = ContourOptions { interval: 20, ..opts.clone() };
        let refused =
            run(&job(&specs, &other, &path, ArchiveFormat::MBTiles), &|| false, &mut quiet());
        assert!(refused.is_err(), "different settings must not continue the build");

        let mut fresh = job(&specs, &other, &path, ArchiveFormat::MBTiles);
        fresh.mode = ResumeMode::Fresh;
        assert!(run(&fresh, &|| false, &mut quiet()).unwrap().finished);
    }

    #[test]
    fn only_the_tiles_of_another_archive_are_rendered() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = specs(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();

        // a "basemap" with exactly one of the tiles
        let all = out.path().join("all.mbtiles");
        run(&job(&specs, &opts, &all, ArchiveFormat::MBTiles), &|| false, &mut quiet()).unwrap();
        let every = tiles(&all);
        assert!(every.len() > 1);
        let keep = &every[0];
        let base = out.path().join("base.mbtiles");
        let conn = rusqlite::Connection::open(&base).unwrap();
        conn.execute_batch(
            "CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob)",
        )
        .unwrap();
        conn.execute("INSERT INTO tiles VALUES (?, ?, ?, x'00')", (keep.0, keep.1, keep.2))
            .unwrap();
        drop(conn);

        let narrowed = out.path().join("narrow.mbtiles");
        let mut only = job(&specs, &opts, &narrowed, ArchiveFormat::MBTiles);
        only.only_tiles_of = Some(&base);
        run(&only, &|| false, &mut quiet()).unwrap();
        let rows = tiles(&narrowed);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1, rows[0].2), (keep.0, keep.1, keep.2));
        assert_eq!(rows[0].3, keep.3, "and the tile is the same tile");
    }

    #[test]
    fn a_shape_puts_its_tilemask_in_the_archive_and_limits_the_tiles() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = specs(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();
        let shape = Polygon::from_str(
            "s\n1\n 6.30 44.30\n 6.36 44.30\n 6.36 44.36\n 6.30 44.36\n 6.30 44.30\nEND\nEND\n",
        )
        .unwrap();
        let (all, small) = (out.path().join("all.mbtiles"), out.path().join("small.mbtiles"));
        run(&job(&specs, &opts, &all, ArchiveFormat::MBTiles), &|| false, &mut quiet()).unwrap();
        let mut shaped = job(&specs, &opts, &small, ArchiveFormat::MBTiles);
        shaped.shape = Some(&shape);
        run(&shaped, &|| false, &mut quiet()).unwrap();
        assert!(tiles(&small).len() < tiles(&all).len());
        let conn = rusqlite::Connection::open(&small).unwrap();
        let mask: String = conn
            .query_row("SELECT value FROM metadata WHERE name = 'tilemask'", [], |r| r.get(0))
            .unwrap();
        assert!(!mask.is_empty());
    }

    /// What the catalog - and so the app's output tab and the tile server - makes of a finished
    /// archive, in both containers.
    #[test]
    fn the_catalog_knows_a_contour_archive_for_what_it_is() {
        use crate::catalog::{self, ArtifactKind, TileFormat};
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let (specs, opts) = (specs(sources.path()), opts());
        let out = tempfile::tempdir().unwrap();
        for format in [ArchiveFormat::MBTiles, ArchiveFormat::PMTiles] {
            let path = out.path().join(format!("alps_contours.{}", format.extension()));
            run(&job(&specs, &opts, &path, format), &|| false, &mut quiet()).unwrap();
            let artifact = catalog::probe(&path, "alps");
            assert_eq!(artifact.kind, ArtifactKind::Contours, "{format:?}");
            assert_eq!(artifact.format, TileFormat::Mvt, "{format:?}");
            assert_eq!(artifact.variant, None, "`_contours` is a kind, not a variant");
            assert_eq!((artifact.minzoom, artifact.maxzoom), (Some(11), Some(12)));
            assert_eq!(artifact.layers.len(), 1);
            assert_eq!(artifact.layers[0].id, "contour");
            assert!(artifact.layers[0].fields.contains_key("ele"));
            assert!(artifact.layers[0].fields.contains_key("div"));
            assert!(artifact.probe_error.is_none(), "{:?}", artifact.probe_error);
        }
    }

    /// A PMTiles reader finds the layers where TileJSON puts them, and the tiles say they are
    /// gzipped vector tiles.
    #[test]
    fn a_pmtiles_archive_says_what_its_tiles_are() {
        use crate::terrain::pmtiles::{Archive, Compression, TileType};
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let (specs, opts) = (specs(sources.path()), opts());
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("c.pmtiles");
        run(&job(&specs, &opts, &path, ArchiveFormat::PMTiles), &|| false, &mut quiet()).unwrap();
        let archive = Archive::open(&path).unwrap();
        assert_eq!(archive.header().tile_type, TileType::Mvt);
        assert_eq!(archive.header().tile_compression, Compression::Gzip);
        let meta = archive.metadata().unwrap();
        assert_eq!(meta["vector_layers"][0]["id"], "contour");
        assert!(meta.get("json").is_none(), "TileJSON has the layers at the top level");
    }

    #[test]
    fn the_identity_tracks_what_changes_the_lines_and_not_the_speed() {
        let sources = tempfile::tempdir().unwrap();
        let mut specs = specs(sources.path());
        let opts = opts();
        let out = Path::new("/tmp/x.mbtiles");
        let base = identity(&job(&specs, &opts, out, ArchiveFormat::MBTiles));

        specs[0].concurrency = Some(99);
        assert_eq!(base, identity(&job(&specs, &opts, out, ArchiveFormat::MBTiles)));

        let finer = ContourOptions { interval: 5, ..opts.clone() };
        assert_ne!(base, identity(&job(&specs, &finer, out, ArchiveFormat::MBTiles)));
        assert_ne!(base, identity(&job(&specs, &opts, out, ArchiveFormat::PMTiles)));
    }
}
