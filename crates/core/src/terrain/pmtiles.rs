//! Reading PMTiles v3 archives.
//!
//! The `pmtiles` crate this workspace already depends on is pulled in with its `write` feature
//! only, and its reader is async - which the terrain sampler is not (see [`super::fetch`]). The
//! format's read path is small enough to state directly, and doing so keeps the whole of it
//! synchronous and range-addressed, which is what reading a 350 GB planet archive over HTTP
//! needs: only the directories and the tiles actually sampled are ever transferred.
//!
//! Layout, in one paragraph: a fixed 127-byte header names the root directory, the leaf directory
//! block and the tile data block. A directory is a column-oriented run of varints - all tile ids
//! (delta-encoded), then all run lengths, then all lengths, then all offsets - sorted by tile id.
//! An entry with `run_length == 0` is not a tile but a pointer to a leaf directory, which is how
//! an archive with a hundred million tiles keeps its root small. Tile ids are positions along a
//! Hilbert curve, with each zoom's ids following the whole of the zoom above, so a single sorted
//! integer key addresses the entire pyramid and neighbouring tiles tend to be neighbouring bytes.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

pub const HEADER_LEN: usize = 127;
const MAGIC: &[u8] = b"PMTiles";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Unknown,
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl Compression {
    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Compression::None,
            2 => Compression::Gzip,
            3 => Compression::Brotli,
            4 => Compression::Zstd,
            _ => Compression::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileType {
    Unknown,
    Mvt,
    Png,
    Jpeg,
    Webp,
    Avif,
}

impl TileType {
    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => TileType::Mvt,
            2 => TileType::Png,
            3 => TileType::Jpeg,
            4 => TileType::Webp,
            5 => TileType::Avif,
            _ => TileType::Unknown,
        }
    }

    /// What a cached tile of this type is called on disk.
    pub fn extension(self) -> &'static str {
        match self {
            TileType::Mvt => "mvt",
            TileType::Png => "png",
            TileType::Jpeg => "jpg",
            TileType::Webp => "webp",
            TileType::Avif => "avif",
            TileType::Unknown => "bin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    /// How many tiles the archive addresses, counting a deduplicated one once per address.
    pub addressed_tiles_count: u64,
    /// How many distinct tile blobs it actually stores.
    pub tile_contents_count: u64,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// west, south, east, north
    pub bounds: (f64, f64, f64, f64),
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn e7_at(bytes: &[u8], at: usize) -> f64 {
    i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as f64 / 1e7
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            bail!("a PMTiles header is {HEADER_LEN} bytes, got {}", bytes.len());
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            bail!("not a PMTiles archive (bad magic)");
        }
        if bytes[7] != 3 {
            bail!("PMTiles v{} is not supported; this reads v3", bytes[7]);
        }
        Ok(Self {
            root_offset: u64_at(bytes, 8),
            root_length: u64_at(bytes, 16),
            metadata_offset: u64_at(bytes, 24),
            metadata_length: u64_at(bytes, 32),
            leaf_offset: u64_at(bytes, 40),
            leaf_length: u64_at(bytes, 48),
            tile_data_offset: u64_at(bytes, 56),
            tile_data_length: u64_at(bytes, 64),
            addressed_tiles_count: u64_at(bytes, 72),
            tile_contents_count: u64_at(bytes, 88),
            internal_compression: Compression::from_byte(bytes[97]),
            tile_compression: Compression::from_byte(bytes[98]),
            tile_type: TileType::from_byte(bytes[99]),
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: (e7_at(bytes, 102), e7_at(bytes, 106), e7_at(bytes, 110), e7_at(bytes, 114)),
        })
    }
}

/// One directory entry: either a tile, or - when `run_length` is zero - a leaf directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    /// Relative to `tile_data_offset` for a tile, to `leaf_offset` for a leaf directory.
    pub offset: u64,
    pub length: u32,
    /// How many consecutive tile ids share this entry. Zero marks a leaf directory.
    pub run_length: u32,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| anyhow!("a PMTiles directory ended in the middle of a varint"))?;
        *at += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("a PMTiles varint is longer than 64 bits");
        }
    }
}

/// Decode a directory. The four columns are stored one after another, not interleaved.
pub fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut at = 0usize;
    let count = varint(bytes, &mut at)? as usize;
    // a corrupt length would otherwise ask for a terabyte of entries before failing
    if count > bytes.len() * 4 {
        bail!("a PMTiles directory claims {count} entries in {} bytes", bytes.len());
    }
    let mut entries = vec![Entry { tile_id: 0, offset: 0, length: 0, run_length: 0 }; count];

    let mut last = 0u64;
    for entry in entries.iter_mut() {
        last += varint(bytes, &mut at)?;
        entry.tile_id = last;
    }
    for entry in entries.iter_mut() {
        entry.run_length = varint(bytes, &mut at)? as u32;
    }
    for entry in entries.iter_mut() {
        entry.length = varint(bytes, &mut at)? as u32;
    }
    for index in 0..count {
        let raw = varint(bytes, &mut at)?;
        // zero is the "immediately after the previous one" shorthand, which is what a clustered
        // archive stores for almost every entry
        entries[index].offset = if raw == 0 {
            let previous = index
                .checked_sub(1)
                .ok_or_else(|| anyhow!("the first PMTiles entry has no previous offset"))?;
            entries[previous].offset + entries[previous].length as u64
        } else {
            raw - 1
        };
    }
    Ok(entries)
}

/// The entry covering `tile_id`: an exact hit, a run that spans it, or the leaf directory that
/// would hold it.
pub fn find(entries: &[Entry], tile_id: u64) -> Option<Entry> {
    let mut low = 0i64;
    let mut high = entries.len() as i64 - 1;
    while low <= high {
        let middle = (low + high) / 2;
        let entry = entries[middle as usize];
        match tile_id.cmp(&entry.tile_id) {
            std::cmp::Ordering::Less => high = middle - 1,
            std::cmp::Ordering::Greater => low = middle + 1,
            std::cmp::Ordering::Equal => return Some(entry),
        }
    }
    // `high` now points at the last entry that starts before the id
    if high < 0 {
        return None;
    }
    let entry = entries[high as usize];
    if entry.run_length == 0 {
        return Some(entry);
    }
    (tile_id - entry.tile_id < entry.run_length as u64).then_some(entry)
}

fn rotate(n: u64, x: &mut u64, y: &mut u64, rx: u64, ry: u64) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

/// Position of a tile along the Hilbert curve, offset by every tile in the zooms above it.
///
/// Getting the offset wrong reads a plausible tile from the wrong zoom, which is the kind of
/// mistake that looks like blurry terrain rather than like a bug.
pub fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    // sum of 4^i for i < z, the number of tiles in all zooms above this one
    let base = ((1u64 << (2 * z as u64)) - 1) / 3;
    let n = 1u64 << z;
    let (mut x, mut y) = (x as u64, y as u64);
    let mut distance = 0u64;
    let mut side = n / 2;
    while side > 0 {
        let rx = u64::from(x & side > 0);
        let ry = u64::from(y & side > 0);
        distance += side * side * ((3 * rx) ^ ry);
        rotate(n, &mut x, &mut y, rx, ry);
        side /= 2;
    }
    base + distance
}

/// Undo whatever an archive compressed a directory or a tile with.
pub fn decompress(compression: Compression, bytes: &[u8]) -> Result<Vec<u8>> {
    match compression {
        // an unknown compression is left alone: webp and png are stored raw, and saying so is
        // optional in archives written by older tools
        Compression::None | Compression::Unknown => Ok(bytes.to_vec()),
        Compression::Gzip => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
            Ok(out)
        }
        Compression::Zstd => Ok(zstd::decode_all(bytes)?),
        Compression::Brotli => bail!("brotli-compressed PMTiles are not supported"),
    }
}

/// A PMTiles archive on disk, open for reading.
///
/// This is the whole-archive counterpart to the pieces above, and it is what the tile server
/// reads: the catalog needs an archive's metadata and the server needs its tiles, and neither can
/// open a PMTiles as SQLite. Terrain's own reader stays separate because it also reads archives
/// over HTTP by byte range, which a served file never is.
///
/// Every method takes `&self`, because the server holds one of these behind an `Arc` and answers
/// requests from several threads at once.
pub struct Archive {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    header: Header,
    root: Vec<Entry>,
    /// Leaf directories already read, keyed by where they sit. An archive with millions of tiles
    /// keeps most of its index out here, and one leaf answers for a whole run of neighbours.
    leaves: Mutex<HashMap<LeafKey, Arc<Vec<Entry>>>>,
}

/// A leaf directory's offset and length, relative to the leaf block.
type LeafKey = (u64, u64);

impl Archive {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let header = Header::parse(&read_at(&mut file, 0, HEADER_LEN as u64)?)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw = read_at(&mut file, header.root_offset, header.root_length)?;
        let root = parse_directory(&decompress(header.internal_compression, &raw)?)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            header,
            root,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The archive's JSON metadata: the same keys an mbtiles keeps in its `metadata` table.
    pub fn metadata(&self) -> Result<serde_json::Value> {
        if self.header.metadata_length == 0 {
            return Ok(serde_json::Value::Object(Default::default()));
        }
        let raw = self.range(self.header.metadata_offset, self.header.metadata_length)?;
        let bytes = decompress(self.header.internal_compression, &raw)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Object(Default::default())))
    }

    /// One tile, exactly as stored. Left compressed: the server sniffs it and sets
    /// `Content-Encoding` rather than gzipping it again on the way out.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let Some((offset, length)) = self.locate(z, x, y)? else { return Ok(None) };
        self.range(offset, length).map(Some)
    }

    /// Whether the archive has this tile, from its directories alone - nothing is read.
    pub fn contains(&self, z: u8, x: u32, y: u32) -> Result<bool> {
        Ok(self.locate(z, x, y)?.is_some())
    }

    /// Every tile the archive addresses, as `(zoom, tiles, bytes)` per zoom.
    ///
    /// Read from the directories alone - no tile is touched. A run of identical tiles counts once
    /// per address and its bytes once per address too, which is what an mbtiles reports for the
    /// same archive under `--compact-db`.
    pub fn zoom_stats(&self) -> Result<Vec<(u8, u64, u64)>> {
        let mut per_zoom: HashMap<u8, (u64, u64)> = HashMap::new();
        let mut count = |entries: &[Entry]| {
            for entry in entries {
                if entry.run_length == 0 {
                    continue;
                }
                let zoom = zoom_of(entry.tile_id);
                let slot = per_zoom.entry(zoom).or_insert((0, 0));
                slot.0 += u64::from(entry.run_length);
                slot.1 += u64::from(entry.length) * u64::from(entry.run_length);
            }
        };
        count(&self.root);
        for entry in &self.root {
            if entry.run_length == 0 {
                count(&self.leaf(entry.offset, u64::from(entry.length))?);
            }
        }
        let mut out: Vec<(u8, u64, u64)> =
            per_zoom.into_iter().map(|(z, (n, b))| (z, n, b)).collect();
        out.sort_unstable();
        Ok(out)
    }

    /// Where a tile's bytes are, following one leaf directory when the root defers to it.
    fn locate(&self, z: u8, x: u32, y: u32) -> Result<Option<(u64, u64)>> {
        let id = zxy_to_tile_id(z, x, y);
        let Some(entry) = find(&self.root, id) else { return Ok(None) };
        let entry = if entry.run_length == 0 {
            let leaf = self.leaf(entry.offset, u64::from(entry.length))?;
            match find(&leaf, id) {
                // a leaf pointing at another leaf is legal in the format but written by nothing
                // this pipeline meets, and following it blindly could loop
                Some(found) if found.run_length > 0 => found,
                _ => return Ok(None),
            }
        } else {
            entry
        };
        Ok(Some((self.header.tile_data_offset + entry.offset, u64::from(entry.length))))
    }

    fn leaf(&self, offset: u64, length: u64) -> Result<Arc<Vec<Entry>>> {
        if let Ok(cache) = self.leaves.lock() {
            if let Some(hit) = cache.get(&(offset, length)) {
                return Ok(hit.clone());
            }
        }
        let raw = self.range(self.header.leaf_offset + offset, length)?;
        let entries =
            Arc::new(parse_directory(&decompress(self.header.internal_compression, &raw)?)?);
        if let Ok(mut cache) = self.leaves.lock() {
            cache.insert((offset, length), entries.clone());
        }
        Ok(entries)
    }

    fn range(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow!("{} was left locked by a panic", self.path.display()))?;
        read_at(&mut file, offset, length)
    }
}

fn read_at(file: &mut std::fs::File, offset: u64, length: u64) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0u8; length as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// Which zoom a tile id belongs to. Each zoom's ids follow the whole of the zoom above.
pub fn zoom_of(tile_id: u64) -> u8 {
    let mut zoom = 0u8;
    while zoom < 31 {
        let next = ((1u64 << (2 * (zoom as u64 + 1))) - 1) / 3;
        if tile_id < next {
            return zoom;
        }
        zoom += 1;
    }
    zoom
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference ids from the PMTiles specification. A wrong curve reads the wrong tile
    /// everywhere but the origin, where every ordering agrees.
    #[test]
    fn tile_ids_follow_the_specification() {
        assert_eq!(zxy_to_tile_id(0, 0, 0), 0);
        assert_eq!(zxy_to_tile_id(1, 0, 0), 1);
        assert_eq!(zxy_to_tile_id(1, 0, 1), 2);
        assert_eq!(zxy_to_tile_id(1, 1, 1), 3);
        assert_eq!(zxy_to_tile_id(1, 1, 0), 4);
        assert_eq!(zxy_to_tile_id(2, 0, 0), 5);
    }

    /// Every id in a zoom is distinct and lands inside that zoom's block.
    #[test]
    fn a_zoom_maps_onto_its_own_block_of_ids() {
        let mut seen = std::collections::HashSet::new();
        let base = ((1u64 << 6) - 1) / 3; // z3 starts after z0..z2
        for x in 0..8u32 {
            for y in 0..8u32 {
                let id = zxy_to_tile_id(3, x, y);
                assert!((base..base + 64).contains(&id), "z3/{x}/{y} gave {id}");
                assert!(seen.insert(id), "{id} was produced twice");
            }
        }
    }

    fn write_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                return;
            }
        }
    }

    fn directory(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, entries.len() as u64);
        let mut last = 0;
        for e in entries {
            write_varint(&mut out, e.tile_id - last);
            last = e.tile_id;
        }
        for e in entries {
            write_varint(&mut out, e.run_length as u64);
        }
        for e in entries {
            write_varint(&mut out, e.length as u64);
        }
        for (index, e) in entries.iter().enumerate() {
            let contiguous = index > 0
                && e.offset == entries[index - 1].offset + entries[index - 1].length as u64;
            write_varint(&mut out, if contiguous { 0 } else { e.offset + 1 });
        }
        out
    }

    #[test]
    fn directories_round_trip() {
        let entries = vec![
            Entry { tile_id: 0, offset: 0, length: 10, run_length: 1 },
            Entry { tile_id: 1, offset: 10, length: 20, run_length: 2 },
            Entry { tile_id: 9, offset: 500, length: 7, run_length: 1 },
        ];
        assert_eq!(parse_directory(&directory(&entries)).unwrap(), entries);
    }

    #[test]
    fn find_walks_runs_and_leaves() {
        let entries = vec![
            Entry { tile_id: 10, offset: 0, length: 10, run_length: 3 },
            Entry { tile_id: 20, offset: 10, length: 10, run_length: 0 },
        ];
        assert_eq!(find(&entries, 10).unwrap().offset, 0);
        // inside the run of three that starts at 10
        assert_eq!(find(&entries, 12).unwrap().tile_id, 10);
        // past the run, before the next entry: nothing
        assert!(find(&entries, 13).is_none());
        // a leaf answers for anything at or after it
        assert_eq!(find(&entries, 999).unwrap().run_length, 0);
        // before the first entry
        assert!(find(&entries, 1).is_none());
    }

    /// An archive written by the `pmtiles` crate, read back through [`Archive`]: the tiles, the
    /// metadata the catalog classifies on, and the per-zoom counts the output tab shows.
    #[test]
    fn an_archive_reads_back_its_tiles_metadata_and_counts() {
        use ::pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType as WriterType};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PmTilesWriter::new(WriterType::Webp)
            .tile_compression(Compression::None)
            .min_zoom(3)
            .max_zoom(5)
            .bounds(6.0, 45.0, 7.0, 46.0)
            .metadata(r#"{"encoding":"terrarium","name":"a_terrain"}"#)
            .create(file)
            .unwrap();
        let mut written = vec![
            (TileCoord::new(5, 16, 11).unwrap(), b"tile-one".to_vec()),
            (TileCoord::new(5, 16, 12).unwrap(), b"tile-two".to_vec()),
            (TileCoord::new(3, 4, 2).unwrap(), b"coarse".to_vec()),
        ];
        written.sort_by_key(|(coord, _)| u64::from(TileId::from(*coord)));
        for (coord, bytes) in &written {
            writer.add_raw_tile(*coord, bytes).unwrap();
        }
        writer.finalize().unwrap();

        let archive = Archive::open(&path).unwrap();
        assert_eq!(archive.header().tile_type, TileType::Webp);
        assert_eq!((archive.header().min_zoom, archive.header().max_zoom), (3, 5));
        assert_eq!(archive.metadata().unwrap()["encoding"], "terrarium");

        // XYZ in, stored bytes out - no row flipping, unlike mbtiles
        assert_eq!(archive.tile(5, 16, 11).unwrap().as_deref(), Some(&b"tile-one"[..]));
        assert_eq!(archive.tile(5, 16, 12).unwrap().as_deref(), Some(&b"tile-two"[..]));
        assert_eq!(archive.tile(3, 4, 2).unwrap().as_deref(), Some(&b"coarse"[..]));
        // a tile the archive does not hold is absent, not an error
        assert_eq!(archive.tile(5, 0, 0).unwrap(), None);

        assert_eq!(archive.zoom_stats().unwrap(), vec![(3, 1, 6), (5, 2, 16)]);
    }

    /// Tile ids carry no zoom of their own; it is recovered from which block they fall in, and
    /// getting that wrong files a zoom's tiles under its neighbour.
    #[test]
    fn zooms_are_recovered_from_tile_ids() {
        for z in 0..8u8 {
            for (x, y) in [(0, 0), ((1 << z) - 1, (1 << z) - 1)] {
                assert_eq!(zoom_of(zxy_to_tile_id(z, x, y)), z, "z{z}/{x}/{y}");
            }
        }
    }

    #[test]
    fn headers_are_checked_before_they_are_believed() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[..MAGIC.len()].copy_from_slice(MAGIC);
        bytes[7] = 2;
        assert!(Header::parse(&bytes).unwrap_err().to_string().contains("v2"));
        bytes[7] = 3;
        assert!(Header::parse(&bytes).is_ok());
        assert!(Header::parse(&bytes[..10]).is_err());
        assert!(Header::parse(&[0u8; HEADER_LEN]).unwrap_err().to_string().contains("magic"));
    }
}
