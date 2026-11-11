//! Discovering generated output and measuring it.
//!
//! An area is a subdirectory of the output root; artifacts are the files inside it, named
//! `<area>[_<suffix>].<ext>[.<trailing>]`. Real examples from a working tree:
//!
//! ```text
//! rhone-alpes.mbtiles                basemap
//! rhone-alpes_routes.mbtiles         routes
//! rhone-alpes_terrain.mbtiles        terrain RGB
//! rhone-alpes.vtiles                 valhalla package
//! rhone-alpes_mlt.mbtiles            basemap, MLT-encoded
//! rhone-alpes.vtiles.base            an older valhalla package kept for comparison
//! rhone-alpes_hillshade.mbtiles.old  ditto for hillshade
//! .DS_Store                          not an artifact
//! ```
//!
//! Two rules follow from that listing. Unrecognised variants are **kept, not hidden** - they are
//! exactly the files a comparison wants. And classification leans on the `metadata` table rather
//! than the filename, because the metadata is authoritative: `format` alone separates MVT
//! (`pbf`) from MLT (`application/vnd.maplibre-vector-tile`) from raster (`webp`) from routing
//! (`gph3`), and it keeps working for files named anything at all.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileFormat {
    /// Mapbox Vector Tile, `format=pbf`.
    Mvt,
    /// MapLibre Tile, `format=application/vnd.maplibre-vector-tile`.
    Mlt,
    Webp,
    Png,
    Jpeg,
    /// Valhalla routing tiles, `format=gph3`.
    Gph3,
    Other(String),
    Unknown,
}

impl TileFormat {
    /// Public wrapper so other modules can classify a raw `format` string.
    pub fn parse_public(raw: &str) -> Self {
        Self::parse(raw)
    }

    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pbf" | "mvt" => TileFormat::Mvt,
            "application/vnd.maplibre-vector-tile" | "mlt" => TileFormat::Mlt,
            "webp" => TileFormat::Webp,
            "png" => TileFormat::Png,
            "jpg" | "jpeg" => TileFormat::Jpeg,
            "gph3" => TileFormat::Gph3,
            other => TileFormat::Other(other.to_string()),
        }
    }

    pub fn is_vector(&self) -> bool {
        matches!(self, TileFormat::Mvt | TileFormat::Mlt)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Basemap,
    Routes,
    Bathymap,
    TerrainRgb,
    Contours,
    Hillshade,
    ValhallaPackage,
    Unknown,
}

/// A vector layer as advertised in the `json` metadata key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorLayer {
    pub id: String,
    #[serde(default)]
    pub minzoom: Option<u8>,
    #[serde(default)]
    pub maxzoom: Option<u8>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// Where a build came from. Planetiler stamps all of this; other producers stamp none of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub planetiler_version: Option<String>,
    pub githash: Option<String>,
    pub buildtime: Option<String>,
    pub osm_replication_seq: Option<String>,
    pub osm_replication_time: Option<String>,
}

impl Provenance {
    fn from_metadata(meta: &BTreeMap<String, String>) -> Self {
        let get = |k: &str| meta.get(k).cloned();
        Self {
            planetiler_version: get("planetiler:version"),
            githash: get("planetiler:githash"),
            buildtime: get("planetiler:buildtime"),
            osm_replication_seq: get("planetiler:osm:osmosisreplicationseq"),
            osm_replication_time: get("planetiler:osm:osmosisreplicationtime"),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub file_name: String,
    pub area: String,
    /// `_mlt`, `.old`, `.base` - anything distinguishing this from the canonical build.
    pub variant: Option<String>,
    pub size_bytes: u64,
    pub kind: ArtifactKind,
    pub format: TileFormat,
    pub minzoom: Option<u8>,
    pub maxzoom: Option<u8>,
    pub bounds: Option<String>,
    /// `terrarium` or `mapbox` for raster-DEM tilesets; MapLibre needs this exact value.
    pub encoding: Option<String>,
    pub compression: Option<String>,
    pub layers: Vec<VectorLayer>,
    pub provenance: Provenance,
    pub metadata: BTreeMap<String, String>,
    /// Set when the file could not be opened as an mbtiles-shaped SQLite database. The artifact
    /// is still listed - a corrupt or half-written output is worth showing, not hiding.
    pub probe_error: Option<String>,
}

/// Split `<area>[_<suffix>].<ext>[.<trailing>]`.
///
/// Returns `None` when the file does not belong to this area at all.
fn split_name(area: &str, file_name: &str) -> Option<(Option<String>, String, Option<String>)> {
    let rest = file_name.strip_prefix(area)?;
    let (suffix, after) = match rest.find('.') {
        Some(idx) => (&rest[..idx], &rest[idx + 1..]),
        None => (rest, ""),
    };
    let (ext, trailing) = match after.find('.') {
        Some(idx) => (&after[..idx], Some(after[idx + 1..].to_string())),
        None => (after, None),
    };
    let suffix = suffix.trim_start_matches('_');
    let suffix = (!suffix.is_empty()).then(|| suffix.to_string());
    Some((suffix, ext.to_string(), trailing))
}

/// Filename-derived kind, used only when the metadata table is unreadable.
fn kind_from_name(suffix: Option<&str>, ext: &str) -> ArtifactKind {
    match (suffix, ext) {
        (Some("routes"), _) => ArtifactKind::Routes,
        (Some("bathymap"), _) => ArtifactKind::Bathymap,
        (Some("terrain"), _) => ArtifactKind::TerrainRgb,
        (Some("contours"), _) => ArtifactKind::Contours,
        (Some("hillshade"), _) => ArtifactKind::Hillshade,
        (_, "vtiles") => ArtifactKind::ValhallaPackage,
        // PMTiles is not SQLite, so the metadata pass cannot open it and the name is all there
        // is to go on - which is exactly the case this fallback exists for.
        (None, "mbtiles") | (None, "pmtiles") => ArtifactKind::Basemap,
        _ => ArtifactKind::Unknown,
    }
}

/// Does this raster archive hold packed elevation rather than a picture?
///
/// Two generations of the pipeline wrote these. The current script stamps `encoding`; the older
/// rio-rgbify path stamped only `round-digits` (the per-zoom quantisation ramp, e.g.
/// `" 3 4 5 6 7 7 7 7"`) and no encoding at all. Both are terrain RGB, and the older ones are
/// still named `_hillshade` - so the name is exactly the wrong thing to classify on.
fn dem_encoding(metadata: &BTreeMap<String, String>) -> Option<String> {
    if let Some(explicit) = metadata.get("encoding") {
        return Some(explicit.clone());
    }
    // rio-rgbify defaulted to mapbox packing, and that is what these files decode as
    metadata.contains_key("round-digits").then(|| "mapbox".to_string())
}

/// Metadata-derived kind. Preferred, because `format` and `encoding` describe what the file
/// actually is regardless of what somebody named it.
fn kind_from_metadata(
    format: &TileFormat,
    encoding: Option<&str>,
    layers: &[VectorLayer],
) -> Option<ArtifactKind> {
    match format {
        TileFormat::Gph3 => Some(ArtifactKind::ValhallaPackage),
        f if f.is_vector() => {
            // the routes build emits exactly one layer; the basemap emits ~19
            if layers.len() == 1 && layers[0].id == "route" {
                Some(ArtifactKind::Routes)
            } else if layers.len() == 1 && layers[0].fields.contains_key("ele") {
                // one layer of lines carrying elevations is a contour archive, whatever the layer
                // was named - the name is an option of the step
                Some(ArtifactKind::Contours)
            } else if layers.iter().any(|l| l.id == "depth" || l.id == "global_landcover") {
                Some(ArtifactKind::Bathymap)
            } else {
                Some(ArtifactKind::Basemap)
            }
        }
        TileFormat::Webp | TileFormat::Png | TileFormat::Jpeg => Some(match encoding {
            // an elevation encoding is what separates terrain RGB from a plain picture
            Some(_) => ArtifactKind::TerrainRgb,
            None => ArtifactKind::Hillshade,
        }),
        _ => None,
    }
}

fn read_metadata(path: &Path) -> Result<BTreeMap<String, String>> {
    if is_pmtiles(path) {
        return read_pmtiles_metadata(path);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut stmt = conn.prepare("SELECT name, value FROM metadata")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn is_pmtiles(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pmtiles"))
}

/// The same keys an mbtiles keeps in its `metadata` table, out of a PMTiles archive.
///
/// A PMTiles holds two descriptions of itself: a fixed header with the zoom range, the bounds and
/// the tile type, and a free-form JSON blob with everything else. Only the JSON survives a round
/// trip from mbtiles, so the header is read first and the JSON allowed to override it - and
/// `format` is taken from the header's tile type, because a writer that stamped nothing still
/// knows what it wrote.
fn read_pmtiles_metadata(path: &Path) -> Result<BTreeMap<String, String>> {
    use crate::terrain::pmtiles::{Archive, TileType};

    let archive = Archive::open(path)?;
    let header = archive.header();
    let mut meta = BTreeMap::new();
    meta.insert("minzoom".to_string(), header.min_zoom.to_string());
    meta.insert("maxzoom".to_string(), header.max_zoom.to_string());
    let (w, s, e, n) = header.bounds;
    if (w, s, e, n) != (0.0, 0.0, 0.0, 0.0) {
        meta.insert("bounds".to_string(), format!("{w},{s},{e},{n}"));
    }
    let format = match header.tile_type {
        TileType::Mvt => Some("pbf"),
        TileType::Png => Some("png"),
        TileType::Jpeg => Some("jpg"),
        TileType::Webp => Some("webp"),
        TileType::Avif => Some("avif"),
        TileType::Unknown => None,
    };
    if let Some(format) = format {
        meta.insert("format".to_string(), format.to_string());
    }

    for (key, value) in archive.metadata()?.as_object().into_iter().flatten() {
        // `json` and `vector_layers` are objects in a PMTiles and strings in an mbtiles; the rest
        // of the catalog reads them as mbtiles spells them, so they are re-serialised
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if key == "vector_layers" {
            meta.insert("json".to_string(), format!("{{\"vector_layers\":{text}}}"));
            continue;
        }
        meta.insert(key.clone(), text);
    }
    Ok(meta)
}

fn parse_layers(meta: &BTreeMap<String, String>) -> Vec<VectorLayer> {
    let Some(raw) = meta.get("json") else { return Vec::new() };
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        vector_layers: Vec<VectorLayer>,
    }
    serde_json::from_str::<Wrapper>(raw).map(|w| w.vector_layers).unwrap_or_default()
}

/// The same counts out of a PMTiles archive, read from its directories.
///
/// No tile is touched, so this is far cheaper than the mbtiles pass it stands in for. `unique` is
/// what the header already knows: PMTiles dedups by content hash as it writes, so the difference
/// between addressed and unique is the dedup, exactly as `tiles_data` is for a compact mbtiles.
fn pmtiles_stats(path: &Path) -> Result<TileStats> {
    let archive = crate::terrain::pmtiles::Archive::open(path)?;
    let per_zoom: Vec<ZoomStat> = archive
        .zoom_stats()?
        .into_iter()
        .map(|(zoom, tiles, bytes)| ZoomStat { zoom, tiles, bytes })
        .collect();
    Ok(TileStats {
        addressed_tiles: per_zoom.iter().map(|z| z.tiles).sum(),
        addressed_bytes: per_zoom.iter().map(|z| z.bytes).sum(),
        unique_tiles: Some(archive.header().tile_contents_count),
        unique_bytes: Some(archive.header().tile_data_length),
        per_zoom,
    })
}

/// Read everything cheap about one file. Does not touch the `tiles` table - see [`tile_stats`].
pub fn probe(path: &Path, area: &str) -> Artifact {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let (suffix, ext, trailing) =
        split_name(area, &file_name).unwrap_or((None, String::new(), None));

    let mut artifact = Artifact {
        path: path.to_path_buf(),
        file_name,
        area: area.to_string(),
        variant: None,
        size_bytes,
        kind: kind_from_name(suffix.as_deref(), &ext),
        format: TileFormat::Unknown,
        minzoom: None,
        maxzoom: None,
        bounds: None,
        encoding: None,
        compression: None,
        layers: Vec::new(),
        provenance: Provenance::default(),
        metadata: BTreeMap::new(),
        probe_error: None,
    };

    // a suffix that is not a known kind is a variant of one: `_mlt` is still a basemap
    let known_suffix = matches!(
        suffix.as_deref(),
        Some("routes") | Some("terrain") | Some("contours") | Some("hillshade")
    );
    artifact.variant = match (suffix.as_deref(), trailing.as_deref()) {
        (Some(s), Some(t)) if !known_suffix => Some(format!("{s}.{t}")),
        (Some(s), None) if !known_suffix => Some(s.to_string()),
        (_, Some(t)) => Some(t.to_string()),
        _ => None,
    };

    match read_metadata(path) {
        Ok(meta) => {
            artifact.format =
                meta.get("format").map(|f| TileFormat::parse(f)).unwrap_or(TileFormat::Unknown);
            artifact.minzoom = meta.get("minzoom").and_then(|v| v.parse().ok());
            artifact.maxzoom = meta.get("maxzoom").and_then(|v| v.parse().ok());
            artifact.bounds = meta.get("bounds").cloned();
            artifact.encoding = dem_encoding(&meta);
            artifact.compression = meta.get("compression").cloned();
            artifact.layers = parse_layers(&meta);
            artifact.provenance = Provenance::from_metadata(&meta);
            if let Some(kind) =
                kind_from_metadata(&artifact.format, artifact.encoding.as_deref(), &artifact.layers)
            {
                artifact.kind = kind;
            }
            artifact.metadata = meta;
        }
        Err(e) => artifact.probe_error = Some(e.to_string()),
    }
    artifact
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoomStat {
    pub zoom: u8,
    pub tiles: u64,
    /// Sum of blob lengths as addressed. Under `--compact-db` a deduplicated blob is counted
    /// once per address, so this is the logical size, not the bytes on disk.
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TileStats {
    pub addressed_tiles: u64,
    pub addressed_bytes: u64,
    /// `Some` only for `--compact-db` archives, which store each distinct blob once and point
    /// many (z,x,y) at it. Comparing this with `addressed_*` shows what deduplication saved;
    /// summing `per_zoom` bytes will *overstate* the file for exactly that reason.
    pub unique_tiles: Option<u64>,
    pub unique_bytes: Option<u64>,
    pub per_zoom: Vec<ZoomStat>,
}

impl TileStats {
    /// Fraction of addressed tiles that resolve to a shared blob. Zero for classic archives.
    pub fn dedup_ratio(&self) -> f64 {
        match self.unique_tiles {
            Some(unique) if self.addressed_tiles > 0 => {
                1.0 - (unique as f64 / self.addressed_tiles as f64)
            }
            _ => 0.0,
        }
    }
}

/// Count and measure tiles. Walks the whole archive, so it is the expensive call.
pub fn tile_stats(path: &Path) -> Result<TileStats> {
    if is_pmtiles(path) {
        return pmtiles_stats(path);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT zoom_level, count(*), coalesce(sum(length(tile_data)), 0)
         FROM tiles GROUP BY zoom_level ORDER BY zoom_level",
    )?;
    let per_zoom: Vec<ZoomStat> = stmt
        .query_map([], |r| {
            Ok(ZoomStat {
                zoom: r.get::<_, i64>(0)? as u8,
                tiles: r.get::<_, i64>(1)? as u64,
                bytes: r.get::<_, i64>(2)? as u64,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let addressed_tiles = per_zoom.iter().map(|z| z.tiles).sum();
    let addressed_bytes = per_zoom.iter().map(|z| z.bytes).sum();

    // `tiles` is a view over `tiles_shallow`/`tiles_data` in compact archives, a table otherwise
    let compact: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='tiles_data'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);

    let (unique_tiles, unique_bytes) = if compact {
        conn.query_row(
            "SELECT count(*), coalesce(sum(length(tile_data)), 0) FROM tiles_data",
            [],
            |r| Ok((Some(r.get::<_, i64>(0)? as u64), Some(r.get::<_, i64>(1)? as u64))),
        )
        .unwrap_or((None, None))
    } else {
        (None, None)
    };

    Ok(TileStats { addressed_tiles, addressed_bytes, unique_tiles, unique_bytes, per_zoom })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub name: String,
    pub dir: PathBuf,
    pub artifacts: Vec<Artifact>,
}

impl Area {
    /// Artifacts of one kind, canonical build first, then variants.
    pub fn of_kind(&self, kind: ArtifactKind) -> Vec<&Artifact> {
        let mut found: Vec<&Artifact> = self.artifacts.iter().filter(|a| a.kind == kind).collect();
        found.sort_by_key(|a| a.variant.is_some());
        found
    }

    pub fn total_bytes(&self) -> u64 {
        self.artifacts.iter().map(|a| a.size_bytes).sum()
    }
}

/// Scan the output root. Each subdirectory is an area; dotfiles are skipped everywhere.
pub fn discover(output_root: &Path) -> Result<Vec<Area>> {
    let mut areas = Vec::new();
    let entries = match std::fs::read_dir(output_root) {
        Ok(e) => e,
        // an output root that does not exist yet is an empty workspace, not an error
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(areas),
        Err(e) => return Err(e).with_context(|| format!("reading {}", output_root.display())),
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !dir.is_dir() || name.starts_with('.') {
            continue;
        }
        let mut artifacts = Vec::new();
        if let Ok(files) = std::fs::read_dir(&dir) {
            for file in files.flatten() {
                let path = file.path();
                let file_name = file.file_name().to_string_lossy().to_string();
                // `.DS_Store` lives in these directories; so does anything else the OS drops
                if !path.is_file() || file_name.starts_with('.') {
                    continue;
                }
                if split_name(&name, &file_name).is_none() {
                    continue;
                }
                artifacts.push(probe(&path, &name));
            }
        }
        artifacts.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        areas.push(Area { name, dir, artifacts });
    }
    areas.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(areas)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoomDelta {
    pub zoom: u8,
    pub tiles_a: u64,
    pub tiles_b: u64,
    pub bytes_a: u64,
    pub bytes_b: u64,
}

impl ZoomDelta {
    pub fn byte_change_pct(&self) -> Option<f64> {
        (self.bytes_a > 0)
            .then(|| (self.bytes_b as f64 - self.bytes_a as f64) / self.bytes_a as f64 * 100.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetadataDiff {
    pub only_in_a: BTreeMap<String, String>,
    pub only_in_b: BTreeMap<String, String>,
    pub changed: BTreeMap<String, (String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerDiff {
    pub layer: String,
    pub only_in_a_fields: Vec<String>,
    pub only_in_b_fields: Vec<String>,
}

/// Compare two builds. Per-zoom bytes are the useful signal; the metadata diff explains *why*
/// they differ (a different planetiler githash, a different OSM snapshot, changed layer fields).
pub fn diff_zooms(a: &TileStats, b: &TileStats) -> Vec<ZoomDelta> {
    let mut zooms: Vec<u8> = a.per_zoom.iter().chain(&b.per_zoom).map(|z| z.zoom).collect();
    zooms.sort_unstable();
    zooms.dedup();
    zooms
        .into_iter()
        .map(|zoom| {
            let find = |s: &TileStats| s.per_zoom.iter().find(|z| z.zoom == zoom).cloned();
            let (za, zb) = (find(a), find(b));
            ZoomDelta {
                zoom,
                tiles_a: za.as_ref().map_or(0, |z| z.tiles),
                tiles_b: zb.as_ref().map_or(0, |z| z.tiles),
                bytes_a: za.map_or(0, |z| z.bytes),
                bytes_b: zb.map_or(0, |z| z.bytes),
            }
        })
        .collect()
}

/// Metadata keys that differ. `json` is excluded - it is the whole layer schema and belongs in
/// [`diff_layers`], not in a key-value comparison.
pub fn diff_metadata(a: &Artifact, b: &Artifact) -> MetadataDiff {
    let mut diff = MetadataDiff::default();
    for (key, va) in &a.metadata {
        if key == "json" {
            continue;
        }
        match b.metadata.get(key) {
            None => {
                diff.only_in_a.insert(key.clone(), va.clone());
            }
            Some(vb) if vb != va => {
                diff.changed.insert(key.clone(), (va.clone(), vb.clone()));
            }
            Some(_) => {}
        }
    }
    for (key, vb) in &b.metadata {
        if key != "json" && !a.metadata.contains_key(key) {
            diff.only_in_b.insert(key.clone(), vb.clone());
        }
    }
    diff
}

/// Per-layer field differences - the direct check on attribute work like dropping `name_int`.
pub fn diff_layers(a: &Artifact, b: &Artifact) -> Vec<LayerDiff> {
    let mut ids: Vec<&str> = a.layers.iter().chain(&b.layers).map(|l| l.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();

    ids.into_iter()
        .filter_map(|id| {
            let fields = |art: &Artifact| {
                art.layers
                    .iter()
                    .find(|l| l.id == id)
                    .map(|l| l.fields.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
            };
            let (fa, fb) = (fields(a), fields(b));
            let only_a: Vec<String> = fa.iter().filter(|f| !fb.contains(f)).cloned().collect();
            let only_b: Vec<String> = fb.iter().filter(|f| !fa.contains(f)).cloned().collect();
            (!only_a.is_empty() || !only_b.is_empty()).then(|| LayerDiff {
                layer: id.to_string(),
                only_in_a_fields: only_a,
                only_in_b_fields: only_b,
            })
        })
        .collect()
}

/// Delete one output file and the sidecars that belong to it, returning the bytes freed.
///
/// `root` is the configured output root and the path must be inside it. The path arrives from the
/// front end, and a delete that removes whatever it is handed is one typo from being a problem.
pub fn delete_artifact(root: &Path, path: &Path) -> anyhow::Result<u64> {
    let root = root.canonicalize().with_context(|| format!("output root {}", root.display()))?;
    let target = path.canonicalize().with_context(|| format!("{}", path.display()))?;
    if !target.starts_with(&root) {
        anyhow::bail!("{} is outside the output root", target.display());
    }
    if !target.is_file() {
        anyhow::bail!("{} is not a file", target.display());
    }

    let mut freed = 0u64;
    // an mbtiles leaves -journal/-wal/-shm beside it; removing the archive and leaving those
    // behind means the next open finds a half-written database
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let sidecar = if suffix.is_empty() {
            target.clone()
        } else {
            PathBuf::from(format!("{}{suffix}", target.display()))
        };
        if sidecar.is_file() {
            freed += std::fs::metadata(&sidecar).map(|m| m.len()).unwrap_or(0);
            std::fs::remove_file(&sidecar)
                .with_context(|| format!("removing {}", sidecar.display()))?;
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    /// A delete command that removes whatever path it is handed is one typo from being a problem,
    /// so the guard is the part worth testing.
    #[test]
    fn delete_refuses_anything_outside_the_output_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("precious.mbtiles");
        std::fs::write(&victim, b"do not delete me").unwrap();

        let err = super::delete_artifact(root.path(), &victim).unwrap_err();
        assert!(err.to_string().contains("outside the output root"), "{err}");
        assert!(victim.is_file(), "the file must still be there");
    }

    #[test]
    fn delete_takes_the_sidecars_with_the_archive() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("rhone-alpes.mbtiles");
        std::fs::write(&db, vec![0u8; 100]).unwrap();
        std::fs::write(root.path().join("rhone-alpes.mbtiles-journal"), vec![0u8; 20]).unwrap();
        std::fs::write(root.path().join("rhone-alpes.mbtiles-wal"), vec![0u8; 30]).unwrap();
        // a different archive must be left alone
        let other = root.path().join("rhone-alpes_routes.mbtiles");
        std::fs::write(&other, vec![0u8; 10]).unwrap();

        let freed = super::delete_artifact(root.path(), &db).unwrap();
        assert_eq!(freed, 150);
        assert!(!db.exists());
        assert!(!root.path().join("rhone-alpes.mbtiles-journal").exists());
        assert!(!root.path().join("rhone-alpes.mbtiles-wal").exists());
        assert!(other.is_file(), "an unrelated archive must survive");
    }

    #[test]
    fn delete_refuses_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("nested");
        std::fs::create_dir(&dir).unwrap();
        let err = super::delete_artifact(root.path(), &dir).unwrap_err();
        assert!(err.to_string().contains("not a file"), "{err}");
        assert!(dir.is_dir());
    }

    use super::*;

    /// Build a `--compact-db` style archive: `tiles_shallow` + `tiles_data` + a `tiles` view.
    fn compact_db(path: &Path, meta: &[(&str, &str)], tiles: &[(u8, u32, u32, usize)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (name text, value text);
             CREATE TABLE tiles_shallow (zoom_level integer, tile_column integer,
               tile_row integer, tile_data_id integer,
               primary key(zoom_level, tile_column, tile_row)) without rowid;
             CREATE TABLE tiles_data (tile_data_id integer primary key, tile_data blob);
             CREATE VIEW tiles AS SELECT tiles_shallow.zoom_level AS zoom_level,
               tiles_shallow.tile_column AS tile_column, tiles_shallow.tile_row AS tile_row,
               tiles_data.tile_data AS tile_data
               FROM tiles_shallow JOIN tiles_data
               ON tiles_shallow.tile_data_id = tiles_data.tile_data_id;",
        )
        .unwrap();
        for (k, v) in meta {
            conn.execute("INSERT INTO metadata VALUES (?, ?)", (k, v)).unwrap();
        }
        // one blob per distinct size, shared by every tile asking for that size
        let mut blob_ids: BTreeMap<usize, i64> = BTreeMap::new();
        for (z, x, y, size) in tiles {
            let id = match blob_ids.get(size) {
                Some(id) => *id,
                None => {
                    let id = blob_ids.len() as i64 + 1;
                    conn.execute("INSERT INTO tiles_data VALUES (?, ?)", (id, vec![7u8; *size]))
                        .unwrap();
                    blob_ids.insert(*size, id);
                    id
                }
            };
            conn.execute("INSERT INTO tiles_shallow VALUES (?, ?, ?, ?)", (z, x, y, id)).unwrap();
        }
    }

    fn classic_db(path: &Path, meta: &[(&str, &str)], tiles: &[(u8, u32, u32, usize)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (name text, value text);
             CREATE TABLE tiles (zoom_level integer, tile_column integer,
               tile_row integer, tile_data blob);",
        )
        .unwrap();
        for (k, v) in meta {
            conn.execute("INSERT INTO metadata VALUES (?, ?)", (k, v)).unwrap();
        }
        for (z, x, y, size) in tiles {
            conn.execute("INSERT INTO tiles VALUES (?, ?, ?, ?)", (z, x, y, vec![7u8; *size]))
                .unwrap();
        }
    }

    const ROUTE_JSON: &str = r#"{"vector_layers":[{"id":"route","minzoom":5,"maxzoom":14,"fields":{"name":"String","extent":"String","symbol":"Number"}}]}"#;

    #[test]
    fn splits_every_real_filename() {
        let a = "rhone-alpes";
        assert_eq!(split_name(a, "rhone-alpes.mbtiles"), Some((None, "mbtiles".into(), None)));
        assert_eq!(
            split_name(a, "rhone-alpes_routes.mbtiles"),
            Some((Some("routes".into()), "mbtiles".into(), None))
        );
        assert_eq!(
            split_name(a, "rhone-alpes.vtiles.base"),
            Some((None, "vtiles".into(), Some("base".into())))
        );
        assert_eq!(
            split_name(a, "rhone-alpes_hillshade.mbtiles.old"),
            Some((Some("hillshade".into()), "mbtiles".into(), Some("old".into())))
        );
        // a file belonging to another area is not ours
        assert_eq!(split_name(a, "corsica.mbtiles"), None);
    }

    /// `format` is authoritative: an MLT basemap is a basemap even though nothing in its name
    /// says so, and the `_mlt` part becomes a variant rather than a kind.
    #[test]
    fn classifies_mlt_as_a_basemap_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes_mlt.mbtiles");
        compact_db(
            &path,
            &[
                ("format", "application/vnd.maplibre-vector-tile"),
                ("minzoom", "0"),
                ("maxzoom", "14"),
            ],
            &[(0, 0, 0, 10)],
        );
        let art = probe(&path, "rhone-alpes");
        assert_eq!(art.kind, ArtifactKind::Basemap);
        assert_eq!(art.format, TileFormat::Mlt);
        assert_eq!(art.variant.as_deref(), Some("mlt"));
    }

    /// The routes build is the one that emits a single `route` layer.
    #[test]
    fn classifies_routes_by_its_single_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes_routes.mbtiles");
        compact_db(&path, &[("format", "pbf"), ("json", ROUTE_JSON)], &[(5, 0, 0, 10)]);
        let art = probe(&path, "rhone-alpes");
        assert_eq!(art.kind, ArtifactKind::Routes);
        assert_eq!(art.layers.len(), 1);
        assert!(art.layers[0].fields.contains_key("extent"));
    }

    /// The older pipeline wrote terrain RGB under a `_hillshade` name with no `encoding` key at
    /// all - only `round-digits`, the per-zoom quantisation ramp. Classifying on the name would
    /// call these pictures and render them as flat imagery instead of elevation.
    #[test]
    fn round_digits_marks_an_unlabelled_terrain_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes_hillshade.mbtiles");
        compact_db(
            &path,
            &[("format", "webp"), ("round-digits", " 3 4 5 6 7 7 7 7"), ("minzoom", "5")],
            &[(5, 0, 0, 10)],
        );
        let art = probe(&path, "rhone-alpes");
        assert_eq!(art.kind, ArtifactKind::TerrainRgb, "named hillshade, actually terrain");
        assert_eq!(art.encoding.as_deref(), Some("mapbox"), "rio-rgbify packed these as mapbox");
    }

    /// An explicit `encoding` always wins over the inference.
    #[test]
    fn explicit_encoding_beats_the_round_digits_guess() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes_terrain.mbtiles");
        compact_db(
            &path,
            &[("format", "webp"), ("encoding", "terrarium"), ("round-digits", " 8 9")],
            &[(5, 0, 0, 10)],
        );
        assert_eq!(probe(&path, "rhone-alpes").encoding.as_deref(), Some("terrarium"));
    }

    /// An `encoding` key is what separates terrain RGB from a plain picture raster - both are
    /// webp, and MapLibre needs the exact value to decode elevation.
    #[test]
    fn encoding_separates_terrain_from_hillshade() {
        let dir = tempfile::tempdir().unwrap();
        let terrain = dir.path().join("rhone-alpes_terrain.mbtiles");
        compact_db(&terrain, &[("format", "webp"), ("encoding", "terrarium")], &[(5, 0, 0, 10)]);
        assert_eq!(probe(&terrain, "rhone-alpes").kind, ArtifactKind::TerrainRgb);

        let hill = dir.path().join("rhone-alpes_hillshade.mbtiles");
        compact_db(&hill, &[("format", "webp")], &[(5, 0, 0, 10)]);
        let art = probe(&hill, "rhone-alpes");
        assert_eq!(art.kind, ArtifactKind::Hillshade);
        assert_eq!(art.encoding, None);
    }

    #[test]
    fn classifies_valhalla_package() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes.vtiles");
        compact_db(&path, &[("format", "gph3"), ("type", "routing")], &[(0, 45, 33, 100)]);
        assert_eq!(probe(&path, "rhone-alpes").kind, ArtifactKind::ValhallaPackage);
    }

    /// The accounting trap. Under `--compact-db` one blob can serve many (z,x,y), so summing
    /// the `tiles` view overstates what is on disk. Both numbers have to be reported.
    #[test]
    fn separates_addressed_from_unique_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes.mbtiles");
        // four addresses, two distinct blobs (100 and 200 bytes)
        compact_db(
            &path,
            &[("format", "pbf")],
            &[(5, 0, 0, 100), (5, 1, 0, 100), (6, 0, 0, 200), (6, 1, 0, 100)],
        );
        let stats = tile_stats(&path).unwrap();
        assert_eq!(stats.addressed_tiles, 4);
        assert_eq!(stats.addressed_bytes, 500, "100+100+200+100 as addressed");
        assert_eq!(stats.unique_tiles, Some(2));
        assert_eq!(stats.unique_bytes, Some(300), "only two blobs actually stored");
        assert_eq!(stats.dedup_ratio(), 0.5);
    }

    #[test]
    fn per_zoom_breakdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes.mbtiles");
        compact_db(&path, &[("format", "pbf")], &[(5, 0, 0, 100), (6, 0, 0, 200), (6, 1, 0, 300)]);
        let stats = tile_stats(&path).unwrap();
        assert_eq!(
            stats.per_zoom,
            vec![
                ZoomStat { zoom: 5, tiles: 1, bytes: 100 },
                ZoomStat { zoom: 6, tiles: 2, bytes: 500 },
            ]
        );
    }

    /// Classic archives store blobs inline, so there is nothing to deduplicate and no honest
    /// unique count to report.
    #[test]
    fn classic_archive_reports_no_unique_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes.mbtiles");
        classic_db(&path, &[("format", "pbf")], &[(5, 0, 0, 100), (5, 1, 0, 100)]);
        let stats = tile_stats(&path).unwrap();
        assert_eq!(stats.addressed_bytes, 200);
        assert_eq!(stats.unique_tiles, None);
        assert_eq!(stats.dedup_ratio(), 0.0);
    }

    /// A file that is not a database at all still gets listed - a truncated or half-written
    /// output is worth seeing, not silently dropping.
    #[test]
    fn unreadable_file_is_listed_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rhone-alpes.mbtiles");
        std::fs::write(&path, b"not a database").unwrap();
        let art = probe(&path, "rhone-alpes");
        assert!(art.probe_error.is_some());
        assert_eq!(art.kind, ArtifactKind::Basemap, "falls back to the filename");
    }

    #[test]
    fn discovery_skips_dotfiles_and_foreign_files() {
        let root = tempfile::tempdir().unwrap();
        let area = root.path().join("rhone-alpes");
        std::fs::create_dir_all(&area).unwrap();
        compact_db(&area.join("rhone-alpes.mbtiles"), &[("format", "pbf")], &[(5, 0, 0, 10)]);
        compact_db(
            &area.join("rhone-alpes_routes.mbtiles"),
            &[("format", "pbf"), ("json", ROUTE_JSON)],
            &[(5, 0, 0, 10)],
        );
        std::fs::write(area.join(".DS_Store"), b"junk").unwrap();
        std::fs::write(area.join("notes.txt"), b"unrelated").unwrap();
        std::fs::create_dir_all(root.path().join(".hidden-area")).unwrap();

        let areas = discover(root.path()).unwrap();
        assert_eq!(areas.len(), 1, "hidden directories are not areas");
        assert_eq!(areas[0].name, "rhone-alpes");
        let names: Vec<&str> = areas[0].artifacts.iter().map(|a| a.file_name.as_str()).collect();
        assert_eq!(names, vec!["rhone-alpes.mbtiles", "rhone-alpes_routes.mbtiles"]);
    }

    #[test]
    fn missing_output_root_is_an_empty_workspace() {
        let root = tempfile::tempdir().unwrap();
        assert!(discover(&root.path().join("never-created")).unwrap().is_empty());
    }

    /// The direct check on attribute work: dropping `name_int` from a layer shows up here.
    #[test]
    fn layer_diff_reports_dropped_fields() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("rhone-alpes.mbtiles");
        let b = dir.path().join("rhone-alpes_trimmed.mbtiles");
        compact_db(
            &a,
            &[
                ("format", "pbf"),
                (
                    "json",
                    r#"{"vector_layers":[{"id":"place","fields":{"name":"String","name_int":"String"}}]}"#,
                ),
            ],
            &[(5, 0, 0, 10)],
        );
        compact_db(
            &b,
            &[
                ("format", "pbf"),
                ("json", r#"{"vector_layers":[{"id":"place","fields":{"name":"String"}}]}"#),
            ],
            &[(5, 0, 0, 10)],
        );
        let diff = diff_layers(&probe(&a, "rhone-alpes"), &probe(&b, "rhone-alpes"));
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].layer, "place");
        assert_eq!(diff[0].only_in_a_fields, vec!["name_int"]);
        assert!(diff[0].only_in_b_fields.is_empty());
    }

    #[test]
    fn metadata_diff_surfaces_provenance_changes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("rhone-alpes.mbtiles");
        let b = dir.path().join("rhone-alpes_v2.mbtiles");
        compact_db(&a, &[("format", "pbf"), ("planetiler:githash", "aaa")], &[(5, 0, 0, 10)]);
        compact_db(&b, &[("format", "pbf"), ("planetiler:githash", "bbb")], &[(5, 0, 0, 10)]);
        let (pa, pb) = (probe(&a, "rhone-alpes"), probe(&b, "rhone-alpes"));
        assert_eq!(pa.provenance.githash.as_deref(), Some("aaa"));
        let diff = diff_metadata(&pa, &pb);
        assert_eq!(
            diff.changed.get("planetiler:githash"),
            Some(&("aaa".to_string(), "bbb".to_string()))
        );
    }

    #[test]
    fn zoom_diff_aligns_missing_zooms() {
        let a = TileStats {
            addressed_tiles: 1,
            addressed_bytes: 100,
            unique_tiles: None,
            unique_bytes: None,
            per_zoom: vec![ZoomStat { zoom: 5, tiles: 1, bytes: 100 }],
        };
        let b = TileStats {
            addressed_tiles: 2,
            addressed_bytes: 250,
            unique_tiles: None,
            unique_bytes: None,
            per_zoom: vec![
                ZoomStat { zoom: 5, tiles: 1, bytes: 50 },
                ZoomStat { zoom: 6, tiles: 1, bytes: 200 },
            ],
        };
        let deltas = diff_zooms(&a, &b);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].byte_change_pct(), Some(-50.0));
        assert_eq!(deltas[1].bytes_a, 0, "zoom missing from A reads as zero, not absent");
        assert_eq!(deltas[1].byte_change_pct(), None);
    }

    /// PMTiles cannot be opened as SQLite, so the name is the only classifier there is.
    #[test]
    fn a_pmtiles_basemap_is_recognised_by_name() {
        assert_eq!(kind_from_name(None, "pmtiles"), ArtifactKind::Basemap);
        assert_eq!(kind_from_name(Some("bathymap"), "pmtiles"), ArtifactKind::Bathymap);
        assert_eq!(kind_from_name(Some("bathymap"), "mbtiles"), ArtifactKind::Bathymap);
    }

    /// The bathymap has two vector layers, so the "more than one layer means basemap" rule
    /// would otherwise claim it.
    #[test]
    fn depth_layers_identify_a_bathymap() {
        let layers = vec![
            VectorLayer {
                id: "global_landcover".into(),
                minzoom: None,
                maxzoom: None,
                fields: Default::default(),
            },
            VectorLayer {
                id: "depth".into(),
                minzoom: None,
                maxzoom: None,
                fields: Default::default(),
            },
        ];
        assert_eq!(
            kind_from_metadata(&TileFormat::Mvt, None, &layers),
            Some(ArtifactKind::Bathymap)
        );
    }
}
