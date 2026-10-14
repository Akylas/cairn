//! Valhalla routing packages.
//!
//! A package is an mbtiles-shaped SQLite file (`zoom_level, tile_column, tile_row, tile_data`,
//! `format=gph3`) holding gzipped Valhalla graph tiles. The `zoom_level` column is Valhalla's
//! hierarchy level, not a map zoom, and the column/row are its graph tile coordinates.

pub mod package;
pub mod routing;
pub mod select;
pub mod tilemask;

/// Valhalla's three hierarchy levels and the degree size of a tile at each.
pub const TILE_SIZES: [f64; 3] = [4.0, 1.0, 0.25];

/// Geographic bounds Valhalla tiles the world over.
pub const BOUNDS: (f64, f64, f64, f64) = (-180.0, -90.0, 180.0, 90.0);

/// One graph tile: column, row, level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphTile {
    pub x: u32,
    pub y: u32,
    pub level: u8,
}

impl GraphTile {
    pub fn new(x: u32, y: u32, level: u8) -> Self {
        Self { x, y, level }
    }

    /// Relative path of the `.gph` file holding this tile.
    ///
    /// The id is `y * columns + x`, rendered as zero-padded three-digit groups, most significant
    /// first, prefixed by the level: level 2 tile id 1_036_800 becomes `2/001/036/800.gph`.
    ///
    /// The Python original divides with `/=`, which is float division in Python 3; it survives
    /// only because the ids in use stay small enough for a double to hold exactly. Integer
    /// division is used here, which agrees on every representable id and does not degrade.
    pub fn path(self) -> String {
        let size = TILE_SIZES[self.level.min(2) as usize];
        let columns = ((BOUNDS.2 - BOUNDS.0) / size) as u64;
        let mut id = self.y as u64 * columns + self.x as u64;

        let groups = self.level.max(1) as usize + 1;
        let mut parts = vec![String::new(); groups];
        for slot in parts.iter_mut().rev() {
            *slot = format!("{:03}", id % 1000);
            id /= 1000;
        }
        format!("{}/{}.gph", self.level, parts.join("/"))
    }

    /// `(west, south, east, north)` of this tile.
    pub fn bounds(self) -> (f64, f64, f64, f64) {
        let size = TILE_SIZES[self.level.min(2) as usize];
        let west = BOUNDS.0 + self.x as f64 * size;
        let south = BOUNDS.1 + self.y as f64 * size;
        (west, south, west + size, south + size)
    }
}

/// Every graph tile a shape touches, at each hierarchy level.
///
/// This is what `--poly` replaces `--like` with: the tile list comes from the area itself rather
/// than from an older package, so a package can be built for a shape nothing has covered before.
/// A tile counts when it overlaps the shape at all - dropping a tile the area only clips is how
/// routes fail at a border.
pub fn tiles_covering(shape: &crate::poly::Polygon, levels: &[u8]) -> Vec<GraphTile> {
    let (west, south, east, north) = shape.bounds();
    let mut tiles = Vec::new();
    for level in levels {
        let size = TILE_SIZES[(*level).min(2) as usize];
        let x0 = ((west - BOUNDS.0) / size).floor().max(0.0) as u32;
        let x1 = ((east - BOUNDS.0) / size).ceil() as u32;
        let y0 = ((south - BOUNDS.1) / size).floor().max(0.0) as u32;
        let y1 = ((north - BOUNDS.1) / size).ceil() as u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let tile = GraphTile::new(x, y, *level);
                let (tw, ts, te, tn) = tile.bounds();
                if shape.intersects_rect(tw, ts, te, tn) {
                    tiles.push(tile);
                }
            }
        }
    }
    tiles.sort();
    tiles.dedup();
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile's box has to line up with the id, or a poly selects tiles next to the area.
    #[test]
    fn tile_bounds_follow_the_id() {
        // level 1 is one degree; tile (185, 135) starts at 5E 45N
        assert_eq!(GraphTile::new(185, 135, 1).bounds(), (5.0, 45.0, 6.0, 46.0));
    }

    #[test]
    fn a_shape_selects_the_tiles_it_touches() {
        let shape = crate::poly::Polygon::from_str(
            "box\n1\n  5.1 45.1\n  5.1 45.9\n  5.9 45.9\n  5.9 45.1\n  5.1 45.1\nEND\nEND\n",
        )
        .unwrap();
        // wholly inside one level-1 tile, and one level-0 tile
        assert_eq!(tiles_covering(&shape, &[1]), vec![GraphTile::new(185, 135, 1)]);
        assert_eq!(tiles_covering(&shape, &[0]), vec![GraphTile::new(46, 33, 0)]);
    }

    /// A shape straddling a tile edge needs both tiles, or routing stops at the seam.
    #[test]
    fn a_shape_across_an_edge_selects_both_tiles() {
        let shape = crate::poly::Polygon::from_str(
            "box\n1\n  4.9 45.1\n  4.9 45.9\n  5.9 45.9\n  5.9 45.1\n  4.9 45.1\nEND\nEND\n",
        )
        .unwrap();
        let tiles = tiles_covering(&shape, &[1]);
        assert_eq!(tiles, vec![GraphTile::new(184, 135, 1), GraphTile::new(185, 135, 1)]);
    }

    #[test]
    fn level_zero_path_has_two_groups() {
        // level 0 tiles are 4 degrees, so 90 columns
        assert_eq!(GraphTile::new(45, 33, 0).path(), "0/003/015.gph");
    }

    #[test]
    fn level_two_path_has_three_groups() {
        // 0.25 degree tiles: 1440 columns, so id = 720 * 1440 + 0
        assert_eq!(GraphTile::new(0, 720, 2).path(), "2/001/036/800.gph");
    }

    #[test]
    fn level_one_path_has_two_groups() {
        // 1 degree tiles: 360 columns
        assert_eq!(GraphTile::new(5, 3, 1).path(), "1/001/085.gph");
    }

    #[test]
    fn id_zero_is_all_zeroes() {
        assert_eq!(GraphTile::new(0, 0, 2).path(), "2/000/000/000.gph");
        assert_eq!(GraphTile::new(0, 0, 0).path(), "0/000/000.gph");
    }
}
