//! Which tiles another archive has, for keeping two archives tile for tile alike.
//!
//! The app that draws contours merges the contour tile of a position with the basemap tile of the
//! same position. A contour tile where the basemap has none is bytes nothing will ever draw, and
//! the old pipeline cut them with `filter_tiles_from_other_mbtiles.py` after the build. Knowing
//! the list beforehand means they are never rendered at all.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::terrain::pmtiles::Archive;

/// The tiles of an archive, as a membership test.
pub enum TileFilter {
    /// Read up front: an mbtiles is a table, and one query is quicker than a lookup per tile.
    Set(HashSet<(u8, u32, u32)>),
    /// A PMTiles answers from its directories, which are already in memory once it is open.
    Directory(Box<Archive>),
}

impl TileFilter {
    /// Open the tiles of an archive between two zooms.
    pub fn open(path: &Path, minzoom: u8, maxzoom: u8) -> Result<Self> {
        if crate::catalog::is_pmtiles(path) {
            return Ok(TileFilter::Directory(Box::new(Archive::open(path)?)));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut statement = conn.prepare(
            "SELECT zoom_level, tile_column, tile_row FROM tiles \
             WHERE zoom_level BETWEEN ? AND ?",
        )?;
        let rows = statement.query_map([minzoom, maxzoom], |r| {
            Ok((r.get::<_, u8>(0)?, r.get::<_, u32>(1)?, r.get::<_, u32>(2)?))
        })?;
        let mut tiles = HashSet::new();
        for row in rows {
            let (z, x, tms_row) = row?;
            // mbtiles rows count up from the south; every tile here is addressed from the north
            tiles.insert((z, x, (1u32 << z) - 1 - tms_row));
        }
        Ok(TileFilter::Set(tiles))
    }

    pub fn contains(&self, z: u8, x: u32, y: u32) -> bool {
        match self {
            TileFilter::Set(tiles) => tiles.contains(&(z, x, y)),
            // an unreadable directory is no reason to drop a tile the other archive may have
            TileFilter::Directory(archive) => archive.contains(z, x, y).unwrap_or(true),
        }
    }
}

/// Where an area's basemap is, when it has one built.
///
/// An mbtiles wins when both exist, as it does everywhere else the pipeline looks for the
/// canonical build.
pub fn basemap_path(area_dir: &Path, area: &str) -> Option<std::path::PathBuf> {
    ["mbtiles", "pmtiles"]
        .into_iter()
        .map(|ext| area_dir.join(format!("{area}.{ext}")))
        .find(|path| path.is_file())
}

/// A string that changes when the archive does, for the identity of a build that read it.
pub fn fingerprint(path: &Path) -> String {
    let meta = std::fs::metadata(path).ok();
    let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}:{len}:{modified}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_mbtiles_is_read_from_the_north() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.mbtiles");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob);",
        )
        .unwrap();
        // z3 x5 y2 from the north is row 7 - 2 = 5 from the south
        conn.execute("INSERT INTO tiles VALUES (3, 5, 5, x'00')", []).unwrap();
        conn.execute("INSERT INTO tiles VALUES (9, 1, 1, x'00')", []).unwrap();
        drop(conn);

        let filter = TileFilter::open(&path, 3, 5).unwrap();
        assert!(filter.contains(3, 5, 2));
        assert!(!filter.contains(3, 5, 5), "the southern row is not the same tile");
        assert!(!filter.contains(9, 1, 510), "out of the asked zooms");
    }

    #[test]
    fn the_basemap_is_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        assert!(basemap_path(dir.path(), "alps").is_none());
        std::fs::write(dir.path().join("alps_routes.mbtiles"), b"x").unwrap();
        assert!(basemap_path(dir.path(), "alps").is_none(), "the routes are not the basemap");
        std::fs::write(dir.path().join("alps.pmtiles"), b"x").unwrap();
        assert!(basemap_path(dir.path(), "alps").unwrap().ends_with("alps.pmtiles"));
        std::fs::write(dir.path().join("alps.mbtiles"), b"x").unwrap();
        assert!(basemap_path(dir.path(), "alps").unwrap().ends_with("alps.mbtiles"));
    }
}
