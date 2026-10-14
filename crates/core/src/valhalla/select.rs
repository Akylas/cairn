//! Which graph tiles go into a package.
//!
//! The command line and the app used to disagree about this: the command line could take a shape,
//! a tilemask or an existing package, and the app's step took only an existing one - so building a
//! package for an area that had never had one failed with "build one with the script first". This
//! is the one answer both ask.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use super::{package, tilemask, tiles_covering, GraphTile, BOUNDS, TILE_SIZES};
use crate::poly::Polygon;

/// Everything that can say which tiles to pack.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Osmosis `.poly` to select tiles from. The usual way to build a new package.
    pub poly: Option<PathBuf>,
    /// Base64 quadtree tilemask, as the Python script emitted.
    pub tilemask: Option<String>,
    /// Zoom to expand `tilemask` to.
    pub polymaxzoom: u8,
    /// Hierarchy levels to include.
    pub levels: Vec<u8>,
    /// Take the list from this existing package.
    pub like: Option<PathBuf>,
}

impl Default for Selection {
    fn default() -> Self {
        Self { poly: None, tilemask: None, polymaxzoom: 11, levels: vec![0, 1, 2], like: None }
    }
}

impl Selection {
    /// Whether nothing here names a source of tiles, so the area's own boundary is what is left.
    ///
    /// Asked before the boundary is fetched, because fetching it is a download.
    pub fn needs_area_poly(&self, output: &Path) -> bool {
        self.poly.is_none()
            && self.tilemask.is_none()
            && self.like.is_none()
            && package::tiles_in(output).is_err()
    }
}

/// Parse `0,1,2` into hierarchy levels, refusing what Valhalla does not have.
pub fn parse_levels(raw: &str) -> Result<Vec<u8>> {
    let mut levels = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let level: u8 = part.parse().map_err(|_| anyhow!("`{part}` is not a hierarchy level"))?;
        if level > 2 {
            return Err(anyhow!("Valhalla has levels 0, 1 and 2; `{level}` is not one"));
        }
        if !levels.contains(&level) {
            levels.push(level);
        }
    }
    if levels.is_empty() {
        return Err(anyhow!("no hierarchy levels given"));
    }
    Ok(levels)
}

/// Work out which graph tiles to pack.
///
/// In order of directness: the shape itself, a tilemask string, an existing package (named, or
/// the one this build would replace), and last the area's own boundary. The shape is the one that
/// can describe an area nothing has covered yet, which is what the Python script was needed for;
/// the boundary is what makes that the default instead of something to go and find.
pub fn tiles_for(
    selection: &Selection,
    output: &Path,
    area_poly: Option<&Path>,
) -> Result<Vec<GraphTile>> {
    if let Some(poly) = &selection.poly {
        return Ok(tiles_covering(&Polygon::parse(poly)?, &selection.levels));
    }
    if let Some(mask) = &selection.tilemask {
        return from_mask(mask, selection.polymaxzoom, &selection.levels);
    }
    let reference = selection.like.clone().unwrap_or_else(|| output.to_path_buf());
    if let Ok(tiles) = package::tiles_in(&reference) {
        return Ok(tiles);
    }
    match area_poly {
        Some(poly) => Ok(tiles_covering(&Polygon::parse(poly)?, &selection.levels)),
        None => Err(anyhow!(
            "no tile list: pass a polygon (--poly), a tilemask (--tilemask) or an existing \
             package (--like), or let the area's own boundary be used"
        )),
    }
}

/// The graph tiles under a tilemask.
///
/// The mask is a web-mercator quadtree; every graph tile a mercator tile overlaps counts.
fn from_mask(mask: &str, max_zoom: u8, levels: &[u8]) -> Result<Vec<GraphTile>> {
    let mut tiles: Vec<GraphTile> = Vec::new();
    for quad in tilemask::tiles(mask, max_zoom)? {
        let (west, south, east, north) = mercator_bounds(quad);
        for level in levels {
            let size = TILE_SIZES[(*level).min(2) as usize];
            let x0 = ((west - BOUNDS.0) / size).floor() as u32;
            let x1 = ((east - BOUNDS.0) / size).ceil() as u32;
            let y0 = ((south - BOUNDS.1) / size).floor() as u32;
            let y1 = ((north - BOUNDS.1) / size).ceil() as u32;
            for y in y0..y1.max(y0 + 1) {
                for x in x0..x1.max(x0 + 1) {
                    tiles.push(GraphTile::new(x, y, *level));
                }
            }
        }
    }
    tiles.sort();
    tiles.dedup();
    Ok(tiles)
}

/// Lon/lat box of a web-mercator tile.
fn mercator_bounds(tile: tilemask::QuadTile) -> (f64, f64, f64, f64) {
    let n = 2f64.powi(tile.z as i32);
    let lon = |x: f64| x / n * 360.0 - 180.0;
    let lat = |y: f64| {
        let t = std::f64::consts::PI * (1.0 - 2.0 * y / n);
        t.sinh().atan().to_degrees()
    };
    (lon(tile.x as f64), lat(tile.y as f64 + 1.0), lon(tile.x as f64 + 1.0), lat(tile.y as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &str = "square\n1\n   6.2   44.2\n   6.8   44.2\n   6.8   44.8\n   6.2   44.8\n   6.2   44.2\nEND\nEND\n";

    fn poly_file(dir: &Path) -> PathBuf {
        let path = dir.join("area.poly");
        std::fs::write(&path, SQUARE).unwrap();
        path
    }

    #[test]
    fn levels_are_checked() {
        assert_eq!(parse_levels("0,1,2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_levels(" 2 , 2,0").unwrap(), vec![2, 0]);
        assert!(parse_levels("3").is_err());
        assert!(parse_levels("x").is_err());
        assert!(parse_levels("").is_err());
    }

    /// The regression: an area with no package yet and nothing named used to be a dead end in the
    /// app. Its own boundary is now the answer.
    #[test]
    fn with_nothing_named_the_area_boundary_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("none.vtiles");
        let selection = Selection::default();
        assert!(selection.needs_area_poly(&output));

        let boundary = poly_file(dir.path());
        let tiles = tiles_for(&selection, &output, Some(&boundary)).unwrap();
        assert!(!tiles.is_empty());
        assert!(tiles.iter().any(|t| t.level == 2), "all three levels by default");
        let refused = tiles_for(&selection, &output, None).unwrap_err().to_string();
        assert!(refused.contains("--poly"), "{refused}");
    }

    #[test]
    fn an_explicit_shape_beats_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let shape = poly_file(dir.path());
        let selection = Selection { poly: Some(shape), levels: vec![1], ..Selection::default() };
        let output = dir.path().join("none.vtiles");
        assert!(!selection.needs_area_poly(&output));
        let tiles = tiles_for(&selection, &output, Some(Path::new("/does/not/exist"))).unwrap();
        assert!(tiles.iter().all(|t| t.level == 1));
    }
}
