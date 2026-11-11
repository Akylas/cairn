//! The terrain render loop, shared by `cairn terrain` and the desktop app.
//!
//! Each of them used to carry its own copy of this loop - zoom, then batch, then write - and the
//! two had to be kept in step by hand. It is also where the three things that make a long build
//! bearable have to meet, which is why they are here and not in either front end:
//!
//! * **prefetching**: while a batch renders, the source tiles the next one will read are fetched
//!   in parallel, so a build over a network source spends its time rendering instead of waiting;
//! * **resuming**: a batch is committed to the archive once it is written, so an interrupted
//!   build continues from its last batch rather than from the start;
//! * **saying what it is doing**: every few seconds, what the network and the workers did.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::archive::{Commit, ResumeMode, TerrainArchive};
use super::fetch::Counters;
use super::render::{self, RenderPool, TerrainOptions};
use super::source::{CompositeSource, SourceSpec};
use super::tiles::TileStats;
use crate::poly::Polygon;
use crate::steps::archive::ArchiveFormat;

/// Tiles rendered per batch. The batch is what bounds how many encoded tiles are held in memory
/// at once, what the next batch's prefetch is sized by, and what a resume is counted in; big
/// enough that every worker has a contiguous run to walk, small enough that a zoom of thirty
/// thousand tiles is not all resident.
pub const BATCH: usize = 256;

/// Bumped whenever a change alters the bytes a tile is rendered to, so that a partial archive
/// written by an older renderer is not quietly finished by a newer one.
///
/// 2: a tile reads one source zoom throughout, instead of a finer one for the half of its pixels
///    nearer the equator.
pub const RENDER_REVISION: u32 = 2;

/// How often the build says what it is doing.
pub(crate) const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Requests to a remote tile source after which a bulk read of an archive is the kinder way.
pub(crate) const MANY_REQUESTS: u64 = 50_000;

pub struct Job<'a> {
    pub specs: &'a [SourceSpec],
    pub opts: &'a TerrainOptions,
    pub bounds: (f64, f64, f64, f64),
    pub shape: Option<&'a Polygon>,
    /// The `.poly` the shape came from, whose contents are part of what makes two builds the same.
    pub shape_file: Option<&'a Path>,
    pub tile_buffer: u32,
    /// `webp` or `png`.
    pub format: &'a str,
    pub archive_format: ArchiveFormat,
    pub output: &'a Path,
    pub name: &'a str,
    pub mode: ResumeMode,
    /// Tiles per batch; [`BATCH`] unless a test wants small ones.
    pub batch: usize,
}

pub enum Event {
    Log(String),
    Phase(String),
    Progress { label: String, percent: u8 },
}

pub struct Outcome {
    /// Whether the archive was completed. `false` is a build stopped early; what it wrote is
    /// kept, and the next run continues it.
    pub finished: bool,
    /// Tiles in the archive, those a resumed build found already there included.
    pub written: u64,
    /// What each tiled source did, summed over the workers.
    pub stats: Vec<(String, TileStats)>,
}

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a. Not for security: a fingerprint that is the same on every machine and every compiler,
/// which `DefaultHasher` is explicitly not.
pub fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A fingerprint of a file's contents, empty when there is no file.
pub fn hash_file(path: Option<&Path>) -> String {
    match path.and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => format!("{:016x}", fnv1a(FNV_OFFSET, &bytes)),
        None => String::new(),
    }
}

/// Everything that decides what the tiles look like, as one string.
///
/// Two runs may share a partial archive only when this is equal. It is deliberately a list of the
/// things that change the pixels or the batch numbering, and not of things that only change how
/// fast they arrive (workers, request concurrency) or where they are cached.
pub fn identity(job: &Job) -> String {
    let mut text = format!(
        "rev{RENDER_REVISION};batch{};{:?};{};{};buffer{};",
        job.batch,
        job.opts,
        job.format,
        job.archive_format.extension(),
        job.tile_buffer
    );
    text.push_str(&format!(
        "bounds{:x},{:x},{:x},{:x};shape{};",
        job.bounds.0.to_bits(),
        job.bounds.1.to_bits(),
        job.bounds.2.to_bits(),
        job.bounds.3.to_bits(),
        hash_file(job.shape_file)
    ));
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
pub fn wanted_tiles(job: &Job, zoom: u8) -> Vec<(u32, u32)> {
    let (x0, y0, x1, y1) = render::tile_range(zoom, job.bounds);
    (x0..=x1)
        .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let Some(shape) = job.shape else { return true };
            let (w, s, e, n) = render::tile_bounds(zoom, x, y);
            if job.tile_buffer == 0 {
                return shape.intersects_rect(w, s, e, n);
            }
            // the buffer is measured in tiles at this zoom, so the ring is the same width
            // everywhere on the pyramid rather than a fixed distance
            let dx = (e - w) * job.tile_buffer as f64;
            let dy = (n - s) * job.tile_buffer as f64;
            shape.intersects_rect(w - dx, s - dy, e + dx, n + dy)
        })
        .collect()
}

pub(crate) fn fingerprint(tiles: &[(u32, u32)]) -> u64 {
    let mut hash = FNV_OFFSET;
    for (x, y) in tiles {
        hash = fnv1a(hash, &x.to_le_bytes());
        hash = fnv1a(hash, &y.to_le_bytes());
    }
    hash
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Snapshot {
    pub requests: u64,
    pub not_found: u64,
    pub fetch_ms: u64,
    pub retries: u64,
    pub failures: u64,
    pub from_disk: u64,
    pub decoded: u64,
}

pub(crate) fn snapshot(counters: &[(String, Arc<Counters>)]) -> (Snapshot, u64) {
    let mut sum = Snapshot::default();
    let mut in_flight = 0;
    for (_, c) in counters {
        sum.requests += Counters::get(&c.requests);
        sum.not_found += Counters::get(&c.not_found);
        sum.fetch_ms += Counters::get(&c.fetch_ms);
        sum.retries += Counters::get(&c.retries);
        sum.failures += Counters::get(&c.failures);
        sum.from_disk += Counters::get(&c.from_disk);
        sum.decoded += Counters::get(&c.decoded);
        in_flight += Counters::get(&c.in_flight);
    }
    (sum, in_flight)
}

/// Where the time of the batches since the last report went, in milliseconds.
#[derive(Default)]
pub(crate) struct Spent {
    pub batches: u64,
    pub tiles: u64,
    pub render: u64,
    pub waiting: u64,
    pub archive: u64,
}

pub(crate) fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Run the build. Returns when the archive is finished or `cancelled` says stop.
pub fn run(
    job: &Job,
    cancelled: &(dyn Fn() -> bool + Sync),
    emit: &mut dyn FnMut(Event),
) -> Result<Outcome> {
    let (probe, skipped) = CompositeSource::open(job.specs)?;
    for note in &skipped {
        emit(Event::Log(format!("skipped {note}")));
    }
    emit(Event::Log(format!("sources, highest priority first: {:?}", probe.names())));
    if probe.names().is_empty() {
        bail!("no usable elevation sources");
    }
    drop(probe);

    let identity = identity(job);
    let (mut archive, resumed) = TerrainArchive::open(
        job.output,
        job.archive_format,
        job.name,
        job.opts,
        job.bounds,
        job.format,
        &identity,
        job.mode,
    )?;
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

    // One renderer per worker. Opening them costs a GeoTIFF directory read apiece, so no more are
    // made than there are tiles to render - a three-tile build must not pay for eighteen.
    let biggest = {
        let (x0, y0, x1, y1) = render::tile_range(job.opts.maxzoom, job.bounds);
        ((x1 - x0 + 1) as usize) * ((y1 - y0 + 1) as usize)
    };
    let pool = RenderPool::new(biggest, || Ok(CompositeSource::open(job.specs)?.0))?;
    emit(Event::Log(format!("{} render threads", pool.len())));
    let counters: Vec<(String, Arc<Counters>)> =
        pool.all_sources().filter_map(|s| s.lock().ok()).flat_map(|s| s.counters()).collect();

    let mut last_report = Instant::now();
    let mut last_snapshot = snapshot(&counters).0;
    let mut spent = Spent::default();
    let tiles_since_report = AtomicU64::new(0);
    let started = Instant::now();
    let batch_size = job.batch.max(1);

    for zoom in job.opts.minzoom..=job.opts.maxzoom {
        if matches!(&resume_point, Some(c) if zoom < c.zoom) {
            continue;
        }
        if cancelled() {
            return Ok(stopped(&pool, written));
        }
        let wanted = wanted_tiles(job, zoom);
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
        if first_batch >= batches.len() {
            continue;
        }

        // the first batch has nothing to overlap with
        if let Some(first) = batches.get(first_batch) {
            let began = Instant::now();
            pool.prefetch(zoom, first, job.opts);
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
            let rendered = std::thread::scope(|scope| {
                let ahead = batches.get(index + 1).map(|next| {
                    scope.spawn(move || {
                        pool_ref.prefetch(zoom, next, job.opts);
                    })
                });
                let out = pool.render(zoom, batch, job.opts, job.format);
                // Whatever the prefetch still has to do is time the workers could not use: if
                // this is large the network is the limit, if it is zero the render is.
                let waiting = Instant::now();
                if let Some(handle) = ahead {
                    let _ = handle.join();
                }
                waited = ms(waiting);
                out
            })?;
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
            tiles_since_report.fetch_add(rendered.len() as u64, Ordering::Relaxed);

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
                    tiles_since_report.swap(0, Ordering::Relaxed),
                )));
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
    emit(Event::Log(format!("rendered in {:.0}s", started.elapsed().as_secs_f64())));
    Ok(Outcome { finished: true, written, stats })
}

fn stopped(pool: &RenderPool, written: u64) -> Outcome {
    Outcome { finished: false, written, stats: totals(pool) }
}

/// What every worker's copy of each source did, added up - each of them holds its own, and
/// eighteen lines of half a story is worse than one line of the whole one.
pub(crate) fn totals(pool: &RenderPool) -> Vec<(String, TileStats)> {
    let mut out: Vec<(String, TileStats)> = Vec::new();
    for source in pool.all_sources() {
        let Ok(source) = source.lock() else { continue };
        for (name, stats) in source.reports() {
            match out.iter_mut().find(|(known, _)| *known == name) {
                Some((_, total)) => total.merge(&stats),
                None => out.push((name, stats)),
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn report(
    zoom: u8,
    percent: u8,
    elapsed: Duration,
    spent: &Spent,
    before: &Snapshot,
    now: &Snapshot,
    in_flight: u64,
    written: u64,
) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    let requests = now.requests - before.requests;
    let empty = now.not_found - before.not_found;
    let mut line = format!(
        "z{zoom} {percent}%: {:.1} tiles/s written, {:.1} visited/s",
        written as f64 / secs,
        spent.tiles as f64 / secs
    );
    if requests > 0 {
        line.push_str(&format!(
            " | network {:.0} req/s ({} in flight, {}% empty, {} ms each",
            requests as f64 / secs,
            in_flight,
            empty * 100 / requests,
            (now.fetch_ms - before.fetch_ms) / requests
        ));
        if now.retries > before.retries {
            line.push_str(&format!(", {} retried", now.retries - before.retries));
        }
        if now.failures > before.failures {
            line.push_str(&format!(", {} FAILED", now.failures - before.failures));
        }
        line.push(')');
    } else {
        line.push_str(" | network idle");
    }
    line.push_str(&format!(
        " | cache {} read, {} decoded",
        now.from_disk - before.from_disk,
        now.decoded - before.decoded
    ));
    if spent.batches > 0 {
        let per = |total: u64| total as f64 / spent.batches as f64 / 1000.0;
        line.push_str(&format!(
            " | per batch: render {:.1}s, waiting on the network {:.1}s, writing {:.1}s",
            per(spent.render),
            per(spent.waiting),
            per(spent.archive)
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A degree square of terrain that is not flat, so a wrong tile is a different tile.
    fn hgt(dir: &Path) {
        let size = 101usize;
        let mut grid = Vec::new();
        for row in 0..size {
            for col in 0..size {
                grid.extend_from_slice(&((row * 10 + col * 3) as i16).to_be_bytes());
            }
        }
        std::fs::write(dir.join("N44E006.hgt"), grid).unwrap();
    }

    fn spec(dir: &Path) -> Vec<SourceSpec> {
        vec![SourceSpec {
            name: "hgt".into(),
            kind: "valhalla".into(),
            path: dir.to_path_buf(),
            ..Default::default()
        }]
    }

    fn opts() -> TerrainOptions {
        TerrainOptions { minzoom: 8, maxzoom: 10, tile_size: 32, ..Default::default() }
    }

    fn job<'a>(
        specs: &'a [SourceSpec],
        opts: &'a TerrainOptions,
        output: &'a Path,
        format: ArchiveFormat,
    ) -> Job<'a> {
        Job {
            specs,
            opts,
            bounds: (6.05, 44.05, 6.95, 44.95),
            shape: None,
            shape_file: None,
            tile_buffer: 0,
            format: "png",
            archive_format: format,
            output,
            name: "t",
            mode: ResumeMode::Auto,
            // small, so the zooms span several batches
            batch: 2,
        }
    }

    fn quiet() -> impl FnMut(Event) {
        |_| {}
    }

    /// Run a build that is stopped after `stop_after` checks, then run it again to the end, and
    /// compare with one that was never interrupted.
    fn interrupted_equals_uninterrupted(format: ArchiveFormat) {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = spec(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();
        let ext = format.extension();
        let whole = out.path().join(format!("whole.{ext}"));
        let broken = out.path().join(format!("broken.{ext}"));

        let outcome = run(&job(&specs, &opts, &whole, format), &|| false, &mut quiet()).unwrap();
        assert!(outcome.finished);
        assert!(outcome.written > 4, "the test needs several batches, got {}", outcome.written);

        let checks = AtomicUsize::new(0);
        let stop = || checks.fetch_add(1, Ordering::Relaxed) >= 4;
        let first = run(&job(&specs, &opts, &broken, format), &stop, &mut quiet()).unwrap();
        assert!(!first.finished);
        assert!(!broken.exists(), "an interrupted build must not leave something that looks built");
        assert!(
            TerrainArchive::partial_info(&broken, format).is_some(),
            "an interrupted build should keep what it wrote"
        );

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

        let tiles = |path: &Path| -> Vec<(u32, u32, u32, Vec<u8>)> {
            match format {
                ArchiveFormat::MBTiles => {
                    let conn = rusqlite::Connection::open(path).unwrap();
                    let mut q = conn
                        .prepare("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles ORDER BY 1,2,3")
                        .unwrap();
                    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect()
                }
                // the PMTiles bytes are the whole comparison: same tiles, same order, same header
                ArchiveFormat::PMTiles => vec![(0, 0, 0, std::fs::read(path).unwrap())],
            }
        };
        assert_eq!(tiles(&whole), tiles(&broken));
    }

    #[test]
    fn an_interrupted_pmtiles_build_finishes_as_if_it_had_not_been() {
        interrupted_equals_uninterrupted(ArchiveFormat::PMTiles);
    }

    #[test]
    fn an_interrupted_mbtiles_build_finishes_as_if_it_had_not_been() {
        interrupted_equals_uninterrupted(ArchiveFormat::MBTiles);
    }

    #[test]
    fn changed_settings_are_refused_not_mixed_in() {
        let sources = tempfile::tempdir().unwrap();
        hgt(sources.path());
        let specs = spec(sources.path());
        let opts = opts();
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("t.pmtiles");

        let checks = AtomicUsize::new(0);
        let stop = || checks.fetch_add(1, Ordering::Relaxed) >= 3;
        let first =
            run(&job(&specs, &opts, &path, ArchiveFormat::PMTiles), &stop, &mut quiet()).unwrap();
        assert!(!first.finished);

        let other = TerrainOptions { round_digits: 4, ..opts.clone() };
        let refused =
            run(&job(&specs, &other, &path, ArchiveFormat::PMTiles), &|| false, &mut quiet());
        assert!(refused.is_err(), "different settings must not continue the build");

        // starting over is explicit
        let mut fresh = job(&specs, &other, &path, ArchiveFormat::PMTiles);
        fresh.mode = ResumeMode::Fresh;
        assert!(run(&fresh, &|| false, &mut quiet()).unwrap().finished);
    }

    /// The tile list a batch number refers to must be the one it was counted in.
    #[test]
    fn a_changed_tile_list_is_caught_by_its_fingerprint() {
        assert_ne!(fingerprint(&[(1, 2), (3, 4)]), fingerprint(&[(1, 2), (3, 5)]));
        assert_ne!(fingerprint(&[(1, 2), (3, 4)]), fingerprint(&[(3, 4), (1, 2)]));
    }

    #[test]
    fn the_identity_tracks_what_changes_the_pixels_and_not_the_speed() {
        let sources = tempfile::tempdir().unwrap();
        let mut specs = spec(sources.path());
        let opts = opts();
        let out = Path::new("/tmp/x.pmtiles");
        let base = identity(&job(&specs, &opts, out, ArchiveFormat::PMTiles));

        specs[0].concurrency = Some(99);
        assert_eq!(base, identity(&job(&specs, &opts, out, ArchiveFormat::PMTiles)));

        let finer = TerrainOptions { maxzoom: 11, ..opts.clone() };
        assert_ne!(base, identity(&job(&specs, &finer, out, ArchiveFormat::PMTiles)));
        assert_ne!(base, identity(&job(&specs, &opts, out, ArchiveFormat::MBTiles)));
    }

    #[test]
    fn the_report_names_where_the_time_went() {
        let before = Snapshot::default();
        let now = Snapshot { requests: 100, not_found: 60, fetch_ms: 30_000, ..Default::default() };
        let spent = Spent { batches: 4, tiles: 1024, render: 8_000, waiting: 4_000, archive: 400 };
        let line = report(13, 40, Duration::from_secs(10), &spent, &before, &now, 24, 300);
        assert!(line.contains("z13 40%"), "{line}");
        assert!(line.contains("10 req/s"), "{line}");
        assert!(line.contains("60% empty"), "{line}");
        assert!(line.contains("300 ms each"), "{line}");
        assert!(line.contains("waiting on the network 1.0s"), "{line}");
    }
}
