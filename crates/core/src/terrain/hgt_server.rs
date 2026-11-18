//! Elevation for Valhalla, rendered from the terrain sources instead of read from `.hgt` files.
//!
//! The graph build bakes road grades in from a directory of 1°×1° `.hgt` tiles, 26 MB each, and
//! nothing else will do: its sampler reads that format and no other. A European build wants over a
//! hundred gigabytes of them - a second copy of ground the terrain step already has, in a coarser
//! form. Valhalla can, however, fetch a missing tile from a URL, which is a seam the format does
//! not have to be crossed at: this serves `GET /N45/N45E006.hgt` on the loopback interface and renders
//! the answer from the same sources the terrain step reads (a mapterhorn PMTiles extract, say).
//!
//! A tile costs about a second of CPU to render the first time. Valhalla keeps what it fetched, so
//! a tile is asked for once per pass over its area, and a few finished ones are kept here for the
//! neighbours that ask next.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rayon::prelude::*;
use tokio::sync::{oneshot, OnceCell};

use super::source::{CompositeSource, SourceSpec};

/// Samples along one side of a 1-arcsecond tile. Adjacent tiles share their edge row and column.
pub const HGT_DIM: usize = 3601;
pub const HGT_BYTES: usize = HGT_DIM * HGT_DIM * 2;
/// What a tile says where it has no data; Valhalla skips these rather than reading them as 0 m.
const VOID: i16 = i16::MIN;

#[derive(Debug, Clone)]
pub struct HgtOptions {
    /// Ground size of one tile sample, in metres. It picks the zoom a pyramid source is read at.
    /// The grid itself is 1 arcsecond whatever this says, and Valhalla resamples a road's shape
    /// every 60 m before it looks anything up, so there is nothing to gain from asking for less.
    pub target_m: f64,
    /// Rendered tiles kept for a neighbour that asks next. 26 MB each.
    pub keep: usize,
}

impl Default for HgtOptions {
    fn default() -> Self {
        Self { target_m: 30.0, keep: 6 }
    }
}

/// What the server has done, for the log line at the end of a build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HgtStats {
    pub rendered: u64,
    pub served: u64,
    /// Asked for, with nothing under it: open sea, mostly.
    pub empty: u64,
}

type Cell = Arc<OnceCell<Option<Arc<Vec<u8>>>>>;

/// The tiles asked for, and the order they were first asked in.
type Cells = (HashMap<(i32, i32), Cell>, VecDeque<(i32, i32)>);

struct Inner {
    specs: Vec<SourceSpec>,
    options: HgtOptions,
    /// Sources are `&mut` to sample, and each holds decoded tiles; a few are kept warm and lent
    /// out one render task at a time.
    pool: Mutex<Vec<CompositeSource>>,
    cells: tokio::sync::Mutex<Cells>,
    rendered: AtomicU64,
    served: AtomicU64,
    empty: AtomicU64,
}

impl Inner {
    fn borrow(&self) -> Result<CompositeSource> {
        if let Some(source) = self.pool.lock().unwrap().pop() {
            return Ok(source);
        }
        let (source, _) = CompositeSource::open(&self.specs)?;
        Ok(source)
    }

    fn give_back(&self, source: CompositeSource) {
        self.pool.lock().unwrap().push(source);
    }

    /// The tile, rendered once however many threads ask at the same moment.
    async fn tile(self: &Arc<Self>, lon: i32, lat: i32) -> Option<Arc<Vec<u8>>> {
        let cell = {
            let mut guard = self.cells.lock().await;
            let (map, order) = &mut *guard;
            match map.get(&(lon, lat)) {
                Some(cell) => cell.clone(),
                None => {
                    let cell: Cell = Arc::new(OnceCell::new());
                    map.insert((lon, lat), cell.clone());
                    order.push_back((lon, lat));
                    // only finished tiles are worth keeping; one still being rendered is held by
                    // whoever waits on it, so dropping its key here costs at most a second render
                    while order.len() > self.options.keep.max(1) {
                        if let Some(oldest) = order.pop_front() {
                            map.remove(&oldest);
                        }
                    }
                    cell
                }
            }
        };
        let this = self.clone();
        cell.get_or_init(|| async move {
            tokio::task::spawn_blocking(move || this.render(lon, lat)).await.ok().flatten()
        })
        .await
        .clone()
    }

    fn render(&self, lon: i32, lat: i32) -> Option<Arc<Vec<u8>>> {
        let tile = match render_tile(self, lon, lat) {
            Ok(tile) => tile,
            Err(e) => {
                tracing::warn!("elevation tile {lon},{lat}: {e:#}");
                None
            }
        };
        match &tile {
            Some(_) => self.rendered.fetch_add(1, Ordering::Relaxed),
            None => self.empty.fetch_add(1, Ordering::Relaxed),
        };
        tile.map(Arc::new)
    }
}

/// One tile as Valhalla reads it: big-endian `i16`, rows from the north edge down.
///
/// `None` when no source has anything under it, which is how the sea is reported - Valhalla
/// remembers a 404 and does not ask again.
fn render_tile(inner: &Inner, lon: i32, lat: i32) -> Result<Option<Vec<u8>>> {
    let mut out = vec![0u8; HGT_BYTES];
    let centre_lat = lat as f64 + 0.5;
    let threads = rayon::current_num_threads().max(1);
    let rows_per_task = HGT_DIM.div_ceil(threads * 2).max(1);
    let step = 1.0 / (HGT_DIM - 1) as f64;
    let target = inner.options.target_m;

    let any = out
        .par_chunks_mut(rows_per_task * HGT_DIM * 2)
        .enumerate()
        .map(|(band, chunk)| -> Result<bool> {
            let mut source = inner.borrow()?;
            // the zoom is chosen once for the tile, not per pixel: pixels nearer the equator would
            // otherwise sit a hair over a level and read four times the source tiles
            source.begin_tile(Some(centre_lat));
            let mut found = false;
            let first_row = band * rows_per_task;
            for (i, row) in chunk.chunks_exact_mut(HGT_DIM * 2).enumerate() {
                let y = lat as f64 + 1.0 - (first_row + i) as f64 * step;
                for (j, cell) in row.chunks_exact_mut(2).enumerate() {
                    let x = lon as f64 + j as f64 * step;
                    let value = match source.sample(x, y, target) {
                        Some(metres) => {
                            found = true;
                            metres.round().clamp(-1000.0, 9000.0) as i16
                        }
                        None => VOID,
                    };
                    cell.copy_from_slice(&value.to_be_bytes());
                }
            }
            inner.give_back(source);
            Ok(found)
        })
        .try_reduce(|| false, |a, b| Ok(a || b))?;

    Ok(any.then_some(out))
}

/// `N45E006.hgt` as `(lon, lat)` of the south-west corner.
pub fn parse_name(name: &str) -> Option<(i32, i32)> {
    let name = name.strip_suffix(".hgt")?;
    if name.len() != 7 || !name.is_ascii() {
        return None;
    }
    let lat: i32 = name[1..3].parse().ok()?;
    let lon: i32 = name[4..7].parse().ok()?;
    let lat = match &name[..1] {
        "N" => lat,
        "S" => -lat,
        _ => return None,
    };
    let lon = match &name[3..4] {
        "E" => lon,
        "W" => -lon,
        _ => return None,
    };
    ((-90..90).contains(&lat) && (-180..180).contains(&lon)).then_some((lon, lat))
}

/// Valhalla asks for `N45/N45E006.hgt`, the directory being the file's own first three letters.
async fn serve_tile(
    State(inner): State<Arc<Inner>>,
    Path((dir, name)): Path<(String, String)>,
) -> Response {
    let Some((lon, lat)) = parse_name(&name).filter(|_| name.starts_with(&dir)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match inner.tile(lon, lat).await {
        Some(bytes) => {
            inner.served.fetch_add(1, Ordering::Relaxed);
            (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                axum::body::Bytes::copy_from_slice(&bytes),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A running server. Dropping it stops it; [`HgtServer::stop`] waits until it has.
pub struct HgtServer {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HgtServer {
    /// Open the sources and listen on a free loopback port.
    ///
    /// Fails now if no source can be opened, rather than answering 404 to every tile of a build
    /// that would then run for hours with no grades in it.
    pub async fn start(specs: Vec<SourceSpec>, options: HgtOptions) -> Result<Self> {
        let (first, skipped) = CompositeSource::open(&specs)?;
        if first.names().is_empty() {
            return Err(anyhow!(
                "no elevation source could be opened{}",
                if skipped.is_empty() {
                    String::new()
                } else {
                    format!(": {}", skipped.join("; "))
                }
            ));
        }
        let inner = Arc::new(Inner {
            specs,
            options,
            pool: Mutex::new(vec![first]),
            cells: tokio::sync::Mutex::new((HashMap::new(), VecDeque::new())),
            rendered: AtomicU64::new(0),
            served: AtomicU64::new(0),
            empty: AtomicU64::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the elevation server")?;
        let addr = listener.local_addr()?;
        let app = Router::new().route("/:dir/:name", get(serve_tile)).with_state(inner.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self { addr, inner, shutdown: Some(tx), task: Some(task) })
    }

    /// The value for Valhalla's `additional_data.elevation_url`.
    pub fn url_template(&self) -> String {
        format!("http://{}/{{tilePath}}", self.addr)
    }

    pub fn stats(&self) -> HgtStats {
        HgtStats {
            rendered: self.inner.rendered.load(Ordering::Relaxed),
            served: self.inner.served.load(Ordering::Relaxed),
            empty: self.inner.empty.load(Ordering::Relaxed),
        }
    }

    pub async fn stop(mut self) -> HgtStats {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.stats()
    }
}

impl Drop for HgtServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_tile(metres: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(HGT_BYTES);
        for _ in 0..HGT_DIM * HGT_DIM {
            out.extend_from_slice(&metres.to_be_bytes());
        }
        out
    }

    #[test]
    fn names_are_read_the_way_valhalla_writes_them() {
        assert_eq!(parse_name("N45E006.hgt"), Some((6, 45)));
        assert_eq!(parse_name("S01W001.hgt"), Some((-1, -1)));
        assert_eq!(parse_name("N45E006.hgt.gz"), None);
        assert_eq!(parse_name("N4E006.hgt"), None);
        assert_eq!(parse_name("X45E006.hgt"), None);
        assert_eq!(parse_name("N95E006.hgt"), None);
        assert_eq!(parse_name("../../etc.hgt"), None);
    }

    /// The whole contract in one go: a tile that exists comes back as the 26 MB Valhalla expects,
    /// and one that does not is a 404 it can remember.
    #[tokio::test(flavor = "multi_thread")]
    async fn serves_a_tile_from_the_sources_and_404s_the_sea() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("N45E006.hgt"), flat_tile(1234)).unwrap();
        let spec = SourceSpec {
            name: "local".into(),
            kind: "hgt".into(),
            path: dir.path().to_path_buf(),
            ..Default::default()
        };
        let server = HgtServer::start(vec![spec], HgtOptions::default()).await.unwrap();
        let base = server.url_template().replace("{tilePath}", "");

        let ok = reqwest::get(format!("{base}N45/N45E006.hgt")).await.unwrap();
        assert_eq!(ok.status(), 200);
        let body = ok.bytes().await.unwrap();
        assert_eq!(body.len(), HGT_BYTES);
        // the corners and the middle, north-west first: all of it is the one elevation
        for index in [0, HGT_DIM - 1, HGT_DIM * HGT_DIM / 2, HGT_DIM * HGT_DIM - 1] {
            assert_eq!(i16::from_be_bytes([body[index * 2], body[index * 2 + 1]]), 1234);
        }

        let sea = reqwest::get(format!("{base}N10/N10E010.hgt")).await.unwrap();
        assert_eq!(sea.status(), 404);
        let junk = reqwest::get(format!("{base}N45/nonsense")).await.unwrap();
        assert_eq!(junk.status(), 404);

        let stats = server.stop().await;
        assert_eq!(stats, HgtStats { rendered: 1, served: 1, empty: 1 });
    }

    /// Valhalla asks from many threads at once, and each tile is a second of work.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tile_asked_for_twice_at_once_is_rendered_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("N45E006.hgt"), flat_tile(10)).unwrap();
        let spec = SourceSpec {
            name: "local".into(),
            kind: "hgt".into(),
            path: dir.path().to_path_buf(),
            ..Default::default()
        };
        let server = HgtServer::start(vec![spec], HgtOptions::default()).await.unwrap();
        let url = format!("{}N45/N45E006.hgt", server.url_template().replace("{tilePath}", ""));
        let (a, b, c) = tokio::join!(reqwest::get(&url), reqwest::get(&url), reqwest::get(&url));
        for reply in [a, b, c] {
            assert_eq!(reply.unwrap().status(), 200);
        }
        assert_eq!(server.stop().await.rendered, 1);
    }

    #[tokio::test]
    async fn refuses_to_start_with_nothing_to_read() {
        let spec = SourceSpec {
            name: "gone".into(),
            kind: "pmtiles".into(),
            path: "/nonexistent/mapterhorn.pmtiles".into(),
            ..Default::default()
        };
        let err = HgtServer::start(vec![spec], HgtOptions::default()).await.err().unwrap();
        assert!(err.to_string().contains("mapterhorn.pmtiles"), "{err}");
    }
}
