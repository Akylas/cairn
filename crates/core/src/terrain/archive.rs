//! Where rendered terrain tiles go: an MBTiles, or a PMTiles.
//!
//! The terrain step is the one that writes its own archive rather than handing tiles to an
//! external tool, so unlike the planetiler and tippecanoe steps it cannot get PMTiles by naming
//! the output differently. This is that second writer, behind the same three calls, so the
//! render loops do not know or care which one they are feeding.
//!
//! It also ends the duplication that was there before: `create_archive` plus a prepared INSERT
//! plus a final CREATE INDEX existed once in the CLI and once in the Tauri command, and the two
//! had to be kept in step by hand.
//!
//! # Resuming
//!
//! A terrain build over a continent is hours long, and it used to be all-or-nothing: the index of
//! what had been written lived in memory, so stopping it - or the machine doing so - left a
//! scratch file nobody could read and the next run began again at zero. Both writers now keep
//! their progress on disk, committed a batch at a time:
//!
//! * PMTiles appends each tile's `(z, x, y, offset, length)` to a journal beside the scratch
//!   file, followed by a commit record naming how far the render has got. On the next run
//!   everything after the last commit is thrown away and the rest is taken as written.
//! * MBTiles writes a batch inside one transaction together with a progress row, so the two
//!   cannot disagree after a crash.
//!
//! Neither is trusted blindly. The journal carries an identity - a hash of everything that
//! decides what the tiles look like - and a partial build is only continued by a run with the
//! same identity; a different one is refused rather than silently mixed in.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use pmtiles::{Compression, PmTilesStreamWriter, PmTilesWriter, TileCoord, TileType};
use rusqlite::Connection;

use super::render::TerrainOptions;
use crate::elevation::Encoding;
use crate::steps::archive::ArchiveFormat;

/// Whether a build may pick up where an interrupted one stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumeMode {
    /// Continue a partial build that matches, start a new one when there is none, and refuse
    /// when there is one that does not match.
    #[default]
    Auto,
    /// Throw away whatever partial build is there.
    Fresh,
}

/// How far a render has got, as the writer last committed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub zoom: u8,
    /// Batches of this zoom that are done; the next one to render is this index.
    pub next_batch: u32,
    /// The size and fingerprint of the zoom's tile list, so a resume can tell that it is still
    /// walking the same list the batches were numbered in.
    pub wanted_count: u64,
    pub wanted_hash: u64,
}

/// What an archive found when it was opened on top of an interrupted build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resumed {
    pub commit: Commit,
    /// Tiles already in the archive.
    pub tiles: u64,
    /// Bytes of tile data already written.
    pub bytes: u64,
}

/// A partial build on disk, for showing to someone who is deciding whether to resume it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PartialInfo {
    pub zoom: u8,
    pub tiles: u64,
    pub bytes: u64,
}

/// One tile archive being written.
pub enum TerrainArchive {
    MBTiles(Box<MBTilesArchive>),
    PMTiles(Box<PMTilesArchive>),
}

impl TerrainArchive {
    /// Create the archive from nothing, with the metadata a terrain reader needs.
    pub fn create(
        path: &Path,
        format: ArchiveFormat,
        name: &str,
        opts: &TerrainOptions,
        bounds: (f64, f64, f64, f64),
        image_format: &str,
    ) -> Result<Self> {
        Ok(Self::open(path, format, name, opts, bounds, image_format, "", ResumeMode::Fresh)?.0)
    }

    /// Open the archive, continuing an interrupted build when there is one that matches
    /// `identity`.
    ///
    /// The second half of the answer says where the render must pick up; `None` means a build
    /// from the start.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        path: &Path,
        format: ArchiveFormat,
        name: &str,
        opts: &TerrainOptions,
        bounds: (f64, f64, f64, f64),
        image_format: &str,
        identity: &str,
        mode: ResumeMode,
    ) -> Result<(Self, Option<Resumed>)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if mode == ResumeMode::Fresh {
            Self::discard_partial(path, format)?;
        }
        let (archive, resumed) = match format {
            ArchiveFormat::MBTiles => {
                let (a, r) =
                    MBTilesArchive::open(path, name, opts, bounds, image_format, identity)?;
                (TerrainArchive::MBTiles(Box::new(a)), r)
            }
            ArchiveFormat::PMTiles => {
                let (a, r) =
                    PMTilesArchive::open(path, name, opts, bounds, image_format, identity, mode)?;
                (TerrainArchive::PMTiles(Box::new(a)), r)
            }
        };
        // an earlier finished archive gives way to the one now being made - but only once this
        // one has opened, so a refusal above leaves it where it was
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok((archive, resumed))
    }

    /// Add one tile, addressed the way the renderer thinks: XYZ, y counting from the north.
    pub fn add(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        match self {
            TerrainArchive::MBTiles(a) => a.add(z, x, y, data),
            TerrainArchive::PMTiles(a) => a.add(z, x, y, data),
        }
    }

    /// Make everything added so far survive an interruption, and say how far the render is.
    pub fn commit(&mut self, commit: &Commit) -> Result<()> {
        match self {
            TerrainArchive::MBTiles(a) => a.commit(commit),
            TerrainArchive::PMTiles(a) => a.commit(commit),
        }
    }

    /// Close the archive. Not optional: PMTiles writes nothing usable until this runs.
    pub fn finish(self) -> Result<()> {
        match self {
            TerrainArchive::MBTiles(a) => a.finish(),
            TerrainArchive::PMTiles(a) => a.finish(),
        }
    }

    /// Delete whatever an interrupted build left, without touching a finished archive.
    pub fn discard_partial(path: &Path, format: ArchiveFormat) -> Result<()> {
        let remove = |p: PathBuf| -> Result<()> {
            match std::fs::remove_file(&p) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
            }
        };
        match format {
            ArchiveFormat::PMTiles => {
                remove(pm_spill_path(path))?;
                remove(pm_journal_path(path))?;
                remove(pm_tmp_path(path))?;
            }
            ArchiveFormat::MBTiles => {
                let partial = mb_partial_path(path);
                remove(with_suffix(&partial, "-wal"))?;
                remove(with_suffix(&partial, "-shm"))?;
                remove(partial)?;
            }
        }
        Ok(())
    }

    /// What an interrupted build has written, if there is one to be seen.
    pub fn partial_info(path: &Path, format: ArchiveFormat) -> Option<PartialInfo> {
        match format {
            ArchiveFormat::PMTiles => pm_partial_info(path),
            ArchiveFormat::MBTiles => mb_partial_info(path),
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn encoding_name(encoding: Encoding) -> &'static str {
    match encoding {
        Encoding::Terrarium => "terrarium",
        Encoding::Mapbox => "mapbox",
    }
}

/// The metadata pairs both containers carry, in one place so they cannot disagree.
fn metadata_pairs(
    name: &str,
    opts: &TerrainOptions,
    bounds: (f64, f64, f64, f64),
    image_format: &str,
) -> Vec<(&'static str, String)> {
    let encoding = encoding_name(opts.encoding);
    vec![
        ("name", name.to_string()),
        ("format", image_format.to_string()),
        ("type", "baselayer".into()),
        ("version", "1".into()),
        ("description", format!("{encoding} terrain rgb")),
        // MapLibre needs this exact key to decode elevation from the tiles
        ("encoding", encoding.into()),
        ("minzoom", opts.minzoom.to_string()),
        ("maxzoom", opts.maxzoom.to_string()),
        ("bounds", format!("{},{},{},{}", bounds.0, bounds.1, bounds.2, bounds.3)),
    ]
}

fn mismatch(tiles: u64) -> anyhow::Error {
    anyhow!(
        "a partial build with {tiles} tiles is already here, and it was started with different \
         settings, so it cannot be continued. Run again with the settings it was started with, or \
         start over (--fresh in the CLI, \"Start over\" in the app) to discard it."
    )
}

// ---------------------------------------------------------------------------------------------
// MBTiles

fn mb_partial_path(path: &Path) -> PathBuf {
    with_suffix(path, ".partial")
}

/// An MBTiles archive, written into `<name>.partial` and renamed when it is complete, so an
/// unfinished one is never mistaken for a built one.
pub struct MBTilesArchive {
    conn: Connection,
    path: PathBuf,
    partial: PathBuf,
    in_transaction: bool,
}

impl MBTilesArchive {
    fn open(
        path: &Path,
        name: &str,
        opts: &TerrainOptions,
        bounds: (f64, f64, f64, f64),
        image_format: &str,
        identity: &str,
    ) -> Result<(Self, Option<Resumed>)> {
        let partial = mb_partial_path(path);
        if partial.exists() {
            if let Some((conn, resumed)) = Self::resume(&partial, identity)? {
                return Ok((
                    Self { conn, path: path.to_path_buf(), partial, in_transaction: false },
                    Some(resumed),
                ));
            }
        }
        TerrainArchive::discard_partial(path, ArchiveFormat::MBTiles)?;

        let conn = Connection::open(&partial)?;
        // WAL, so that a batch is durable once committed and an interrupted one rolls back; a
        // journal-less database could be left half-written by exactly the interruption this
        // exists to survive
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE metadata (name text, value text);
             CREATE TABLE tiles (zoom_level integer, tile_column integer,
               tile_row integer, tile_data blob);
             CREATE TABLE cairn_progress (identity text, zoom integer, next_batch integer,
               wanted_count integer, wanted_hash integer);",
        )?;
        for (key, value) in metadata_pairs(name, opts, bounds, image_format) {
            conn.execute("INSERT INTO metadata VALUES (?, ?)", (key, value))?;
        }
        conn.execute(
            "INSERT INTO cairn_progress (identity) VALUES (?)",
            rusqlite::params![identity],
        )?;
        Ok((Self { conn, path: path.to_path_buf(), partial, in_transaction: false }, None))
    }

    /// `None` when the file is not a usable partial build (and so should be replaced), an error
    /// when it is one that belongs to different settings.
    fn resume(partial: &Path, identity: &str) -> Result<Option<(Connection, Resumed)>> {
        let Ok(conn) = Connection::open(partial) else { return Ok(None) };
        type Row = (String, Option<i64>, Option<i64>, Option<i64>, Option<i64>);
        let row: rusqlite::Result<Row> = conn.query_row(
            "SELECT identity, zoom, next_batch, wanted_count, wanted_hash FROM cairn_progress",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        );
        let Ok((known, zoom, next_batch, count, hash)) = row else { return Ok(None) };
        let tiles: i64 =
            conn.query_row("SELECT COUNT(*) FROM tiles", [], |r| r.get(0)).unwrap_or(0);
        if known != identity {
            return Err(mismatch(tiles as u64));
        }
        // nothing committed yet: there is nothing worth keeping
        let (Some(zoom), Some(next_batch), Some(count), Some(hash)) =
            (zoom, next_batch, count, hash)
        else {
            return Ok(None);
        };
        let bytes: i64 = conn
            .query_row("SELECT COALESCE(SUM(LENGTH(tile_data)), 0) FROM tiles", [], |r| r.get(0))
            .unwrap_or(0);
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(Some((
            conn,
            Resumed {
                commit: Commit {
                    zoom: zoom as u8,
                    next_batch: next_batch as u32,
                    wanted_count: count as u64,
                    wanted_hash: hash as u64,
                },
                tiles: tiles as u64,
                bytes: bytes as u64,
            },
        )))
    }

    fn add(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        if !self.in_transaction {
            self.conn.execute_batch("BEGIN")?;
            self.in_transaction = true;
        }
        // mbtiles rows count up from the south; the renderer counts down from the north
        let tms_row = (1u32 << z) - 1 - y;
        self.conn
            .prepare_cached("INSERT INTO tiles VALUES (?, ?, ?, ?)")?
            .execute(rusqlite::params![z, x, tms_row, data])?;
        Ok(())
    }

    fn commit(&mut self, commit: &Commit) -> Result<()> {
        if !self.in_transaction {
            self.conn.execute_batch("BEGIN")?;
        }
        self.conn.execute(
            "UPDATE cairn_progress SET zoom = ?, next_batch = ?, wanted_count = ?, wanted_hash = ?",
            rusqlite::params![
                commit.zoom,
                commit.next_batch,
                commit.wanted_count as i64,
                commit.wanted_hash as i64
            ],
        )?;
        self.conn.execute_batch("COMMIT")?;
        self.in_transaction = false;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.in_transaction {
            self.conn.execute_batch("COMMIT")?;
        }
        self.conn.execute_batch(
            "DROP TABLE cairn_progress;
             CREATE UNIQUE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row);
             PRAGMA wal_checkpoint(TRUNCATE);
             PRAGMA journal_mode=DELETE;",
        )?;
        self.conn.close().map_err(|(_, e)| e).context("closing the archive")?;
        std::fs::rename(&self.partial, &self.path)
            .with_context(|| format!("moving {} into place", self.partial.display()))?;
        let _ = std::fs::remove_file(with_suffix(&self.partial, "-wal"));
        let _ = std::fs::remove_file(with_suffix(&self.partial, "-shm"));
        Ok(())
    }
}

fn mb_partial_info(path: &Path) -> Option<PartialInfo> {
    let partial = mb_partial_path(path);
    if !partial.exists() {
        return None;
    }
    let conn =
        Connection::open_with_flags(&partial, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let zoom: Option<i64> =
        conn.query_row("SELECT zoom FROM cairn_progress", [], |r| r.get(0)).ok()?;
    let tiles: i64 = conn.query_row("SELECT COUNT(*) FROM tiles", [], |r| r.get(0)).ok()?;
    Some(PartialInfo {
        zoom: zoom? as u8,
        tiles: tiles as u64,
        bytes: std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------------------------
// PMTiles

fn pm_spill_path(path: &Path) -> PathBuf {
    path.with_extension("pmtiles.spill")
}

fn pm_journal_path(path: &Path) -> PathBuf {
    path.with_extension("pmtiles.resume")
}

fn pm_tmp_path(path: &Path) -> PathBuf {
    path.with_extension("pmtiles.tmp")
}

const JOURNAL_MAGIC: &[u8; 8] = b"CAIRNRS1";
const TILE_RECORD: u8 = 1;
const COMMIT_RECORD: u8 = 2;
/// tag, z, x, y, offset, length
const TILE_RECORD_LEN: usize = 1 + 1 + 4 + 4 + 8 + 4;
/// tag, zoom, next batch, count, hash, scratch length
const COMMIT_RECORD_LEN: usize = 1 + 1 + 4 + 8 + 8 + 8;

/// A PMTiles archive, assembled at the end from tiles spilled to a temporary file.
///
/// PMTiles addresses tiles by Hilbert id and wants them written in ascending order - a reader
/// fetching a run of neighbouring tiles then gets one contiguous range instead of a scatter.
/// The render loops walk zoom, then column, then row, which is not that order, and holding a
/// whole terrain pyramid in memory to sort it is not an option: a rhone-alpes z5-12 archive is
/// hundreds of megabytes.
///
/// So the tiles go to a scratch file as they are rendered, only their id, offset and length are
/// kept, and `finish` sorts those and copies the bytes back out in order. That is the same
/// shape tippecanoe uses for its own PMTiles output, and it costs one extra pass over the tile
/// bytes plus a transient copy on disk.
///
/// The same list is appended to a journal as it grows, which is what lets an interrupted build be
/// continued: see the module notes.
pub struct PMTilesArchive {
    path: PathBuf,
    spill_path: PathBuf,
    journal_path: PathBuf,
    spill: BufWriter<File>,
    journal: BufWriter<File>,
    entries: Vec<Spilled>,
    offset: u64,
    writer: PmTilesWriter,
}

struct Spilled {
    id: u64,
    coord: TileCoord,
    offset: u64,
    len: u32,
}

/// What a journal said, up to its last complete commit.
struct Recovered {
    identity: String,
    entries: Vec<Spilled>,
    commit: Option<Commit>,
    /// Bytes of the scratch file the commit covers.
    spill_len: u64,
    /// Bytes of the journal up to and including the commit.
    journal_len: u64,
}

fn read_journal(path: &Path, spill_have: u64) -> Option<Recovered> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 10 || &bytes[..8] != JOURNAL_MAGIC {
        return None;
    }
    let identity_len = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let identity = String::from_utf8(bytes.get(10..10 + identity_len)?.to_vec()).ok()?;
    let mut at = 10 + identity_len;

    let mut recovered = Recovered {
        identity,
        entries: Vec::new(),
        commit: None,
        spill_len: 0,
        journal_len: at as u64,
    };
    let mut pending: Vec<Spilled> = Vec::new();
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    // a record cut short, or one that does not parse, ends the journal: it is what a process
    // killed mid-write leaves, and everything after the last commit is discarded anyway
    loop {
        match bytes.get(at) {
            Some(&TILE_RECORD) if bytes.len() >= at + TILE_RECORD_LEN => {
                let record = &bytes[at..at + TILE_RECORD_LEN];
                let Ok(coord) = TileCoord::new(record[1], u32_at(record, 2), u32_at(record, 6))
                else {
                    break;
                };
                pending.push(Spilled {
                    id: u64::from(pmtiles::TileId::from(coord)),
                    coord,
                    offset: u64_at(record, 10),
                    len: u32_at(record, 18),
                });
                at += TILE_RECORD_LEN;
            }
            Some(&COMMIT_RECORD) if bytes.len() >= at + COMMIT_RECORD_LEN => {
                let record = &bytes[at..at + COMMIT_RECORD_LEN];
                let spill_len = u64_at(record, 22);
                // a commit that claims more scratch bytes than exist cannot be believed
                if spill_len > spill_have {
                    break;
                }
                recovered.entries.append(&mut pending);
                recovered.commit = Some(Commit {
                    zoom: record[1],
                    next_batch: u32_at(record, 2),
                    wanted_count: u64_at(record, 6),
                    wanted_hash: u64_at(record, 14),
                });
                recovered.spill_len = spill_len;
                at += COMMIT_RECORD_LEN;
                recovered.journal_len = at as u64;
            }
            _ => break,
        }
    }
    Some(recovered)
}

impl PMTilesArchive {
    fn open(
        path: &Path,
        name: &str,
        opts: &TerrainOptions,
        bounds: (f64, f64, f64, f64),
        image_format: &str,
        identity: &str,
        mode: ResumeMode,
    ) -> Result<(Self, Option<Resumed>)> {
        let tile_type = match image_format {
            "png" => TileType::Png,
            _ => TileType::Webp,
        };

        // The metadata is JSON here rather than a table. `encoding` has to survive that move or
        // MapLibre cannot decode the elevation, which is the whole point of the archive.
        let metadata: serde_json::Map<String, serde_json::Value> =
            metadata_pairs(name, opts, bounds, image_format)
                .into_iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v)))
                .collect();

        let writer = PmTilesWriter::new(tile_type)
            // WebP and PNG are already compressed; gzipping them again costs time and grows
            // the file
            .tile_compression(Compression::None)
            .min_zoom(opts.minzoom)
            .max_zoom(opts.maxzoom)
            .bounds(bounds.0, bounds.1, bounds.2, bounds.3)
            .metadata(&serde_json::Value::Object(metadata).to_string());

        let spill_path = pm_spill_path(path);
        let journal_path = pm_journal_path(path);

        if mode == ResumeMode::Auto && journal_path.exists() && spill_path.exists() {
            let have = std::fs::metadata(&spill_path)?.len();
            if let Some(found) = read_journal(&journal_path, have) {
                if found.identity != identity {
                    return Err(mismatch(found.entries.len() as u64));
                }
                if let Some(commit) = found.commit.clone() {
                    // Anything after the commit is a batch that was cut off: the journal and the
                    // scratch file are both cut back to it, and the batch is rendered again.
                    OpenOptions::new()
                        .write(true)
                        .open(&journal_path)?
                        .set_len(found.journal_len)?;
                    let mut spill = OpenOptions::new().read(true).write(true).open(&spill_path)?;
                    spill.set_len(found.spill_len)?;
                    spill.seek(SeekFrom::Start(found.spill_len))?;
                    let journal = OpenOptions::new().append(true).open(&journal_path)?;
                    let resumed = Resumed {
                        commit,
                        tiles: found.entries.len() as u64,
                        bytes: found.spill_len,
                    };
                    return Ok((
                        Self {
                            path: path.to_path_buf(),
                            spill_path,
                            journal_path,
                            spill: BufWriter::new(spill),
                            journal: BufWriter::new(journal),
                            entries: found.entries,
                            offset: found.spill_len,
                            writer,
                        },
                        Some(resumed),
                    ));
                }
            }
        }

        // Read *and* write: the tiles go in here as they are rendered and come back out in
        // tile-id order at the end, so a write-only handle fails with EBADF on the way back.
        let spill = BufWriter::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&spill_path)
                .with_context(|| format!("creating {}", spill_path.display()))?,
        );
        let mut journal = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&journal_path)
                .with_context(|| format!("creating {}", journal_path.display()))?,
        );
        journal.write_all(JOURNAL_MAGIC)?;
        journal.write_all(&(identity.len() as u16).to_le_bytes())?;
        journal.write_all(identity.as_bytes())?;
        journal.flush()?;

        Ok((
            Self {
                path: path.to_path_buf(),
                spill_path,
                journal_path,
                spill,
                journal,
                entries: Vec::new(),
                offset: 0,
                writer,
            },
            None,
        ))
    }

    fn add(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        let coord =
            TileCoord::new(z, x, y).map_err(|e| anyhow!("{z}/{x}/{y} is not a valid tile: {e}"))?;
        self.spill.write_all(data)?;
        self.journal.write_all(&[TILE_RECORD, z])?;
        self.journal.write_all(&x.to_le_bytes())?;
        self.journal.write_all(&y.to_le_bytes())?;
        self.journal.write_all(&self.offset.to_le_bytes())?;
        self.journal.write_all(&(data.len() as u32).to_le_bytes())?;
        self.entries.push(Spilled {
            id: u64::from(pmtiles::TileId::from(coord)),
            coord,
            offset: self.offset,
            len: data.len() as u32,
        });
        self.offset += data.len() as u64;
        Ok(())
    }

    fn commit(&mut self, commit: &Commit) -> Result<()> {
        // the scratch bytes first: a commit must never claim more than is on disk
        self.spill.flush()?;
        self.spill.get_ref().sync_data()?;
        self.journal.write_all(&[COMMIT_RECORD, commit.zoom])?;
        self.journal.write_all(&commit.next_batch.to_le_bytes())?;
        self.journal.write_all(&commit.wanted_count.to_le_bytes())?;
        self.journal.write_all(&commit.wanted_hash.to_le_bytes())?;
        self.journal.write_all(&self.offset.to_le_bytes())?;
        self.journal.flush()?;
        self.journal.get_ref().sync_data()?;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.spill.flush()?;
        let mut spill = self.spill.into_inner().context("flushing the tile scratch file")?;

        self.entries.sort_by_key(|e| e.id);

        // Written beside the real name and moved into place, so a build interrupted here leaves
        // the scratch file and journal to try again from, not a truncated archive that looks
        // finished.
        let tmp = pm_tmp_path(&self.path);
        let out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut writer: PmTilesStreamWriter<File> = self
            .writer
            .create(out)
            .map_err(|e| anyhow!("starting {}: {e}", self.path.display()))?;

        let mut buffer = Vec::new();
        for entry in &self.entries {
            spill.seek(SeekFrom::Start(entry.offset))?;
            buffer.resize(entry.len as usize, 0);
            spill.read_exact(&mut buffer)?;
            // already the final bytes, and the header says so - the writer must not touch them
            writer
                .add_raw_tile(entry.coord, &buffer)
                .map_err(|e| anyhow!("writing a tile: {e}"))?;
        }

        writer.finalize().map_err(|e| anyhow!("finishing {}: {e}", self.path.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("moving {} into place", self.path.display()))?;

        drop(spill);
        let _ = std::fs::remove_file(&self.spill_path);
        let _ = std::fs::remove_file(&self.journal_path);
        Ok(())
    }
}

fn pm_partial_info(path: &Path) -> Option<PartialInfo> {
    let have = std::fs::metadata(pm_spill_path(path)).ok()?.len();
    let found = read_journal(&pm_journal_path(path), have)?;
    let commit = found.commit?;
    Some(PartialInfo {
        zoom: commit.zoom,
        tiles: found.entries.len() as u64,
        bytes: found.spill_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TerrainOptions {
        TerrainOptions {
            minzoom: 2,
            maxzoom: 3,
            encoding: Encoding::Terrarium,
            ..Default::default()
        }
    }

    fn sample_tiles() -> Vec<(u8, u32, u32, Vec<u8>)> {
        // deliberately not in Hilbert order, and with one duplicate payload to exercise the
        // writer's deduplication
        vec![
            (3, 5, 2, b"tile-a".to_vec()),
            (2, 1, 1, b"tile-b".to_vec()),
            (3, 1, 1, b"tile-a".to_vec()),
            (2, 0, 0, b"tile-c".to_vec()),
        ]
    }

    #[test]
    fn mbtiles_round_trips_every_tile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mbtiles");
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::MBTiles,
            "t",
            &opts(),
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap();
        for (z, x, y, data) in sample_tiles() {
            archive.add(z, x, y, &data).unwrap();
        }
        archive.finish().unwrap();

        let conn = Connection::open(&path).unwrap();
        for (z, x, y, data) in sample_tiles() {
            let tms = (1u32 << z) - 1 - y;
            let got: Vec<u8> = conn
                .query_row(
                    "SELECT tile_data FROM tiles WHERE zoom_level=? AND tile_column=? AND tile_row=?",
                    (z, x, tms),
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(got, data, "{z}/{x}/{y}");
        }
        let encoding: String = conn
            .query_row("SELECT value FROM metadata WHERE name='encoding'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(encoding, "terrarium");
    }

    /// The header has to say PMTiles v3, and the tiles have to be there. Anything less and the
    /// step would be writing a file nothing can read - which is exactly what happened when the
    /// bathymap's temporary file hid its extension from tippecanoe.
    #[test]
    fn pmtiles_writes_a_readable_v3_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pmtiles");
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::PMTiles,
            "t",
            &opts(),
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap();
        for (z, x, y, data) in sample_tiles() {
            archive.add(z, x, y, &data).unwrap();
        }
        archive.finish().unwrap();

        // Read the header against the v3 byte layout rather than through the crate that wrote
        // it: a writer and its own reader agreeing proves nothing about what a browser will see.
        let bytes = std::fs::read(&path).unwrap();
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());

        assert_eq!(&bytes[0..7], b"PMTiles", "magic");
        assert_eq!(bytes[7], 3, "spec version");
        assert_eq!(u64_at(72), 4, "addressed tiles");
        assert_eq!(u64_at(88), 3, "distinct payloads - the duplicate is stored once");
        assert_eq!(bytes[96], 1, "clustered: tiles written in tile-id order");
        assert_eq!(bytes[98], 1, "tile compression: none, the tiles are already WebP");
        assert_eq!(bytes[99], 4, "tile type: WebP");
        assert_eq!(bytes[100], 2, "min zoom");
        assert_eq!(bytes[101], 3, "max zoom");

        // and the metadata a terrain reader actually needs survived the move to JSON
        let meta_off = u64_at(24) as usize;
        let meta_len = u64_at(32) as usize;
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(&bytes[meta_off..meta_off + meta_len])
            .read_to_end(&mut json)
            .expect("metadata is gzipped, per the internal compression byte");
        let meta: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(meta["encoding"], "terrarium");
        assert_eq!(meta["format"], "webp");
    }

    /// The scratch file is transient; leaving it behind would double the archive on disk.
    #[test]
    fn the_spill_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pmtiles");
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::PMTiles,
            "t",
            &opts(),
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap();
        archive.add(2, 1, 1, b"x").unwrap();
        archive.finish().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("spill"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// Moved here with the writer. It is the check that the archive says enough about itself
    /// for the catalog and the viewer to know what it holds.
    #[test]
    fn archive_metadata_matches_what_the_viewer_needs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t_terrain.mbtiles");
        // named rather than defaulted: this checks that what the options say reaches the
        // metadata, so it must not move when the defaults do
        let opts = TerrainOptions {
            encoding: Encoding::Terrarium,
            maxzoom: 13,
            ..TerrainOptions::default()
        };
        TerrainArchive::create(
            &path,
            ArchiveFormat::MBTiles,
            "t_terrain",
            &opts,
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap()
        .finish()
        .unwrap();

        let art = crate::catalog::probe(&path, "t");
        assert_eq!(art.kind, crate::catalog::ArtifactKind::TerrainRgb);
        assert_eq!(art.encoding.as_deref(), Some("terrarium"));
        assert_eq!(art.maxzoom, Some(13));
    }

    /// The catalog reads a PMTiles archive's own metadata now, so it must come back with the
    /// same facts an mbtiles would give - not just the kind its name implies.
    #[test]
    fn a_pmtiles_terrain_archive_is_classified_from_its_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t_terrain.pmtiles");
        let opts = TerrainOptions {
            minzoom: 4,
            maxzoom: 7,
            encoding: Encoding::Terrarium,
            ..Default::default()
        };
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::PMTiles,
            "t_terrain",
            &opts,
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap();
        archive.add(4, 8, 5, b"tile").unwrap();
        archive.finish().unwrap();

        let art = crate::catalog::probe(&path, "t");
        assert_eq!(art.probe_error, None, "a PMTiles must not read back as unreadable");
        assert_eq!(art.kind, crate::catalog::ArtifactKind::TerrainRgb);
        assert_eq!(art.encoding.as_deref(), Some("terrarium"), "the DEM encoding has to survive");
        assert_eq!((art.minzoom, art.maxzoom), (Some(4), Some(7)));
        assert_eq!(art.format, crate::catalog::TileFormat::Webp);

        let stats = crate::catalog::tile_stats(&path).unwrap();
        assert_eq!(stats.addressed_tiles, 1);
        assert_eq!(stats.per_zoom, vec![crate::catalog::ZoomStat { zoom: 4, tiles: 1, bytes: 4 }]);
    }

    /// Classification must still work from the name alone, for an archive whose metadata says
    /// nothing useful.
    #[test]
    fn a_pmtiles_terrain_archive_is_still_classified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t_terrain.pmtiles");
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::PMTiles,
            "t_terrain",
            &opts(),
            (3.0, 44.0, 7.0, 46.0),
            "webp",
        )
        .unwrap();
        archive.add(2, 1, 1, b"tile").unwrap();
        archive.finish().unwrap();

        let art = crate::catalog::probe(&path, "t");
        assert_eq!(art.kind, crate::catalog::ArtifactKind::TerrainRgb);
    }

    /// A small archive fits its whole index in the root directory. Past about 16 kB of entries
    /// the writer has to spill into leaf directories, and that is a different code path with a
    /// different header - one a terrain pyramid reaches easily and the tests above never would.
    #[test]
    fn a_large_archive_uses_leaf_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.pmtiles");
        let opts = TerrainOptions { minzoom: 0, maxzoom: 8, ..opts() };
        let mut archive = TerrainArchive::create(
            &path,
            ArchiveFormat::PMTiles,
            "big",
            &opts,
            (-180.0, -85.0, 180.0, 85.0),
            "webp",
        )
        .unwrap();

        // distinct payloads, so nothing deduplicates and the index really does get large
        let mut written = 0u64;
        for x in 0..256u32 {
            for y in 0..64u32 {
                archive.add(8, x, y, format!("tile-{x}-{y}").as_bytes()).unwrap();
                written += 1;
            }
        }
        archive.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        assert_eq!(&bytes[0..7], b"PMTiles");
        assert_eq!(u64_at(72), written, "addressed tiles");
        assert_eq!(u64_at(88), written, "every payload distinct");
        assert!(u64_at(48) > 0, "leaf directory section is empty, so no leaves were written");
        assert_eq!(bytes[96], 1, "still clustered");
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    fn opts() -> TerrainOptions {
        TerrainOptions {
            minzoom: 3,
            maxzoom: 4,
            encoding: Encoding::Terrarium,
            ..Default::default()
        }
    }

    const BOUNDS: (f64, f64, f64, f64) = (3.0, 44.0, 7.0, 46.0);

    /// Sixty distinct tiles in three batches of twenty, as a render would hand them over.
    fn batches() -> Vec<Vec<(u8, u32, u32, Vec<u8>)>> {
        (0..3u32)
            .map(|b| {
                (0..20u32)
                    .map(|i| {
                        let n = b * 20 + i;
                        (
                            4u8,
                            n % 16,
                            n / 16,
                            format!("tile-{n}-{}", "x".repeat(n as usize)).into_bytes(),
                        )
                    })
                    .collect()
            })
            .collect()
    }

    fn commit(next_batch: u32) -> Commit {
        Commit { zoom: 4, next_batch, wanted_count: 60, wanted_hash: 0xfeed }
    }

    fn open(
        path: &Path,
        format: ArchiveFormat,
        identity: &str,
    ) -> Result<(TerrainArchive, Option<Resumed>)> {
        TerrainArchive::open(path, format, "t", &opts(), BOUNDS, "webp", identity, ResumeMode::Auto)
    }

    fn tiles_of(path: &Path) -> Vec<(u8, u32, u32, Vec<u8>)> {
        let conn = Connection::open(path).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles ORDER BY 1, 2, 3",
            )
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// The whole point: stop after one committed batch with a second one half-written, come back,
    /// finish - and get exactly the archive an uninterrupted run would have.
    #[test]
    fn a_pmtiles_build_resumes_to_the_same_archive() {
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.pmtiles");
        let broken = dir.path().join("broken.pmtiles");
        let all = batches();

        let (mut a, resumed) = open(&whole, ArchiveFormat::PMTiles, "id").unwrap();
        assert!(resumed.is_none());
        for (i, batch) in all.iter().enumerate() {
            for (z, x, y, d) in batch {
                a.add(*z, *x, *y, d).unwrap();
            }
            a.commit(&commit(i as u32 + 1)).unwrap();
        }
        a.finish().unwrap();

        let (mut a, _) = open(&broken, ArchiveFormat::PMTiles, "id").unwrap();
        for (z, x, y, d) in &all[0] {
            a.add(*z, *x, *y, d).unwrap();
        }
        a.commit(&commit(1)).unwrap();
        // half of the next batch, never committed - what a killed process leaves
        for (z, x, y, d) in &all[1][..7] {
            a.add(*z, *x, *y, d).unwrap();
        }
        drop(a);

        let (mut a, resumed) = open(&broken, ArchiveFormat::PMTiles, "id").unwrap();
        let resumed = resumed.expect("the build should resume");
        assert_eq!(resumed.commit, commit(1));
        assert_eq!(resumed.tiles, 20, "the cut-off batch is not counted");
        for (i, batch) in all.iter().enumerate().skip(1) {
            for (z, x, y, d) in batch {
                a.add(*z, *x, *y, d).unwrap();
            }
            a.commit(&commit(i as u32 + 1)).unwrap();
        }
        a.finish().unwrap();

        assert_eq!(std::fs::read(&whole).unwrap(), std::fs::read(&broken).unwrap());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("spill") || n.contains("resume") || n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// A journal that ends in the middle of a record is what a hard kill leaves, and the build
    /// has to carry on from the commit before it.
    #[test]
    fn a_torn_journal_resumes_from_its_last_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pmtiles");
        let all = batches();
        let (mut a, _) = open(&path, ArchiveFormat::PMTiles, "id").unwrap();
        for (z, x, y, d) in &all[0] {
            a.add(*z, *x, *y, d).unwrap();
        }
        a.commit(&commit(1)).unwrap();
        for (z, x, y, d) in &all[1][..3] {
            a.add(*z, *x, *y, d).unwrap();
        }
        drop(a);

        let journal = pm_journal_path(&path);
        let len = std::fs::metadata(&journal).unwrap().len();
        // cut the last record in half
        OpenOptions::new().write(true).open(&journal).unwrap().set_len(len - 9).unwrap();

        let (_, resumed) = open(&path, ArchiveFormat::PMTiles, "id").unwrap();
        assert_eq!(resumed.unwrap().tiles, 20);
        let info = TerrainArchive::partial_info(&path, ArchiveFormat::PMTiles).unwrap();
        assert_eq!((info.zoom, info.tiles), (4, 20));
    }

    /// Settings that changed since the partial build was started must not be quietly mixed into
    /// it - and neither may the partial build be quietly thrown away.
    #[test]
    fn a_partial_build_with_other_settings_is_refused_and_kept() {
        for format in [ArchiveFormat::PMTiles, ArchiveFormat::MBTiles] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("t.{}", format.extension()));
            let (mut a, _) = open(&path, format, "one").unwrap();
            a.add(4, 1, 1, b"abc").unwrap();
            a.commit(&commit(1)).unwrap();
            drop(a);

            let refused = open(&path, format, "two").err().expect("different settings are refused");
            assert!(refused.to_string().contains("different settings"), "{refused}");
            assert!(TerrainArchive::partial_info(&path, format).is_some(), "{format:?} was lost");

            // and the same settings still resume
            let (_, resumed) = open(&path, format, "one").unwrap();
            assert_eq!(resumed.unwrap().tiles, 1);
        }
    }

    #[test]
    fn fresh_discards_a_partial_build() {
        for format in [ArchiveFormat::PMTiles, ArchiveFormat::MBTiles] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("t.{}", format.extension()));
            let (mut a, _) = open(&path, format, "one").unwrap();
            a.add(4, 1, 1, b"abc").unwrap();
            a.commit(&commit(1)).unwrap();
            drop(a);

            let (_, resumed) = TerrainArchive::open(
                &path,
                format,
                "t",
                &opts(),
                BOUNDS,
                "webp",
                "two",
                ResumeMode::Fresh,
            )
            .unwrap();
            assert!(resumed.is_none());
            assert!(TerrainArchive::partial_info(&path, format).is_none());
        }
    }

    /// Same as the PMTiles case, for the SQLite writer: a batch is in the file or it is not.
    #[test]
    fn an_mbtiles_build_resumes_without_duplicating_a_tile() {
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.mbtiles");
        let broken = dir.path().join("broken.mbtiles");
        let all = batches();

        let (mut a, _) = open(&whole, ArchiveFormat::MBTiles, "id").unwrap();
        for (i, batch) in all.iter().enumerate() {
            for (z, x, y, d) in batch {
                a.add(*z, *x, *y, d).unwrap();
            }
            a.commit(&commit(i as u32 + 1)).unwrap();
        }
        a.finish().unwrap();

        let (mut a, _) = open(&broken, ArchiveFormat::MBTiles, "id").unwrap();
        for (z, x, y, d) in &all[0] {
            a.add(*z, *x, *y, d).unwrap();
        }
        a.commit(&commit(1)).unwrap();
        for (z, x, y, d) in &all[1][..7] {
            a.add(*z, *x, *y, d).unwrap();
        }
        drop(a);
        assert!(!broken.exists(), "an unfinished archive must not look finished");

        let (mut a, resumed) = open(&broken, ArchiveFormat::MBTiles, "id").unwrap();
        let resumed = resumed.expect("the build should resume");
        assert_eq!(resumed.commit, commit(1));
        assert_eq!(resumed.tiles, 20, "the uncommitted rows rolled back");
        for (i, batch) in all.iter().enumerate().skip(1) {
            for (z, x, y, d) in batch {
                a.add(*z, *x, *y, d).unwrap();
            }
            a.commit(&commit(i as u32 + 1)).unwrap();
        }
        a.finish().unwrap();

        assert_eq!(tiles_of(&whole), tiles_of(&broken));
        assert_eq!(tiles_of(&broken).len(), 60);
        let conn = Connection::open(&broken).unwrap();
        let progress: rusqlite::Result<i64> =
            conn.query_row("SELECT COUNT(*) FROM cairn_progress", [], |r| r.get(0));
        assert!(progress.is_err(), "the progress table is working state, not part of the archive");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("partial") || n.ends_with("-wal") || n.ends_with("-shm"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// A partial archive with no batch committed has nothing worth continuing from.
    #[test]
    fn nothing_committed_means_a_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mbtiles");
        let (mut a, _) = open(&path, ArchiveFormat::MBTiles, "id").unwrap();
        a.add(4, 1, 1, b"abc").unwrap();
        drop(a);
        let (_, resumed) = open(&path, ArchiveFormat::MBTiles, "id").unwrap();
        assert!(resumed.is_none());
    }
}
