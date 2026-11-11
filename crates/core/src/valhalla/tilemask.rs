//! The base64 quadtree tilemask that says which tiles a package carries.
//!
//! The mask is a bitstream walked depth-first over a web-mercator quadtree. Each node spends two
//! bits: the first says whether four children follow, the second whether the node itself is
//! inside the area. A node marked inside with no children means the whole subtree is inside.
//!
//! [`tiles`] reads one and [`for_polygon`] writes one - the latter is `generate_poly_tilemask.py`,
//! which stamped a `tilemask` into the metadata of every contour archive so the app could tell
//! which positions a package covers without opening it.

use anyhow::{anyhow, Result};
use base64::Engine;

/// One quadtree node's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuadTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

struct BitReader {
    bits: Vec<u8>,
    at: usize,
}

impl BitReader {
    fn next(&mut self) -> Result<u8> {
        let bit = *self
            .bits
            .get(self.at)
            .ok_or_else(|| anyhow!("tilemask ended early at bit {}", self.at))?;
        self.at += 1;
        Ok(bit)
    }
}

/// Expand a mask into the mercator tiles it covers, descending to `max_zoom`.
pub fn tiles(mask: &str, max_zoom: u8) -> Result<Vec<QuadTile>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(mask.trim())
        .map_err(|e| anyhow!("tilemask is not base64: {e}"))?;
    let bits: Vec<u8> =
        bytes.iter().flat_map(|byte| (0..8).map(move |i| (byte >> (7 - i)) & 1)).collect();
    if bits.is_empty() {
        return Ok(Vec::new());
    }

    let mut reader = BitReader { bits, at: 0 };
    let mut out = Vec::new();
    walk(&mut reader, QuadTile { z: 0, x: 0, y: 0 }, max_zoom, &mut out)?;
    Ok(out)
}

fn walk(
    reader: &mut BitReader,
    tile: QuadTile,
    max_zoom: u8,
    out: &mut Vec<QuadTile>,
) -> Result<()> {
    let has_children = reader.next()? == 1;
    let inside = reader.next()? == 1;
    if inside {
        out.push(tile);
    }
    if has_children {
        for dy in 0..2 {
            for dx in 0..2 {
                let child = QuadTile { z: tile.z + 1, x: tile.x * 2 + dx, y: tile.y * 2 + dy };
                walk(reader, child, max_zoom, out)?;
            }
        }
    } else if inside && tile.z < max_zoom {
        // an inside leaf stands for its whole subtree, so fill it in down to max_zoom
        for dy in 0..2 {
            for dx in 0..2 {
                fill(
                    QuadTile { z: tile.z + 1, x: tile.x * 2 + dx, y: tile.y * 2 + dy },
                    max_zoom,
                    out,
                );
            }
        }
    }
    Ok(())
}

fn fill(tile: QuadTile, max_zoom: u8, out: &mut Vec<QuadTile>) {
    if tile.z > max_zoom {
        return;
    }
    out.push(tile);
    for dy in 0..2 {
        for dx in 0..2 {
            fill(QuadTile { z: tile.z + 1, x: tile.x * 2 + dx, y: tile.y * 2 + dy }, max_zoom, out);
        }
    }
}

/// Encode the tiles for which `inside(z, x, y)` holds, down to `max_zoom`.
///
/// Rows count up from the south here, as the Python that wrote these masks did and as the
/// archives they describe do (TMS); [`tiles`] hands them back as they were written.
pub fn encode_set(inside: &dyn Fn(u8, u32, u32) -> bool, max_zoom: u8) -> String {
    fn build(inside: &dyn Fn(u8, u32, u32) -> bool, z: u8, x: u32, y: u32, max: u8) -> Vec<u8> {
        if !inside(z, x, y) {
            return vec![0, 0];
        }
        if z == max {
            return vec![0, 1];
        }
        let mut children = Vec::new();
        for dy in 0..2 {
            for dx in 0..2 {
                children.extend(build(inside, z + 1, x * 2 + dx, y * 2 + dy, max));
            }
        }
        // four whole children are the parent, whole: nothing to say about them
        if children == [0, 1, 0, 1, 0, 1, 0, 1] {
            return vec![0, 1];
        }
        let mut bits = vec![1, 1];
        bits.extend(children);
        bits
    }

    let mut bits = build(inside, 0, 0, 0, max_zoom);
    bits.resize(bits.len().div_ceil(24) * 24, 0);
    let bytes: Vec<u8> =
        bits.chunks(8).map(|byte| byte.iter().fold(0u8, |acc, bit| acc << 1 | bit)).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The mask of a shape, at the finest zoom up to `polyzoom` that makes it worth having.
///
/// A small mask is a coarse one: at a low zoom a region is a handful of tiles and the mask says
/// little. So it starts four zooms below `polyzoom` and goes up until the mask is 512 characters,
/// or `polyzoom` is reached - the rule `generate_poly_tilemask.py` followed.
pub fn for_polygon(shape: &crate::poly::Polygon, polyzoom: u8) -> String {
    let inside = |z: u8, x: u32, tms_row: u32| {
        let (west, south, east, north) =
            crate::terrain::render::tile_bounds(z, x, (1u32 << z) - 1 - tms_row);
        shape.intersects_rect(west, south, east, north)
    };
    let mut mask = String::new();
    for zoom in polyzoom.saturating_sub(4)..=polyzoom {
        mask = encode_set(&inside, zoom);
        if mask.len() >= 512 {
            break;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(bits: &[u8]) -> String {
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, bit) in bits.iter().enumerate() {
            bytes[i / 8] |= bit << (7 - i % 8);
        }
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn empty_mask_yields_nothing() {
        assert!(tiles("", 3).unwrap().is_empty());
    }

    /// A root marked outside with no children covers nothing.
    #[test]
    fn outside_root_covers_nothing() {
        assert!(tiles(&encode(&[0, 0]), 3).unwrap().is_empty());
    }

    /// A root marked inside with no children stands for the entire pyramid below it.
    #[test]
    fn inside_leaf_expands_to_max_zoom() {
        let got = tiles(&encode(&[0, 1]), 1).unwrap();
        assert_eq!(got.len(), 5, "root plus its four children");
        assert!(got.contains(&QuadTile { z: 0, x: 0, y: 0 }));
        assert!(got.contains(&QuadTile { z: 1, x: 1, y: 1 }));
    }

    #[test]
    fn descends_into_explicit_children() {
        // root has children and is itself outside; first child inside, rest outside
        let bits = [1, 0, /*c0*/ 0, 1, /*c1*/ 0, 0, /*c2*/ 0, 0, /*c3*/ 0, 0];
        let got = tiles(&encode(&bits), 1).unwrap();
        assert_eq!(got, vec![QuadTile { z: 1, x: 0, y: 0 }]);
    }

    #[test]
    fn truncated_mask_is_an_error() {
        // claims children but supplies none
        assert!(tiles(&encode(&[1, 1]), 2).is_err());
    }

    #[test]
    fn a_whole_world_collapses_to_its_root() {
        // bits 0,1 then padding: 0100_0000 and two zero bytes
        assert_eq!(encode_set(&|_, _, _| true, 5), "QAAA");
        assert_eq!(encode_set(&|_, _, _| false, 5), "AAAA");
    }

    /// What is written is what is read back, tile for tile.
    #[test]
    fn encoding_round_trips_through_the_decoder() {
        // the south-west quarter of the world at every zoom, plus one lone tile at z4: a set
        // where some subtrees are whole, some partial and most empty
        let inside = |z: u8, x: u32, y: u32| {
            let half = (1u32 << z) / 2;
            z == 0 || (x < half && y < half) || (z <= 4 && x == (1 << z) - 1 && y == (1 << z) - 1)
        };
        let mask = encode_set(&inside, 4);
        let mut decoded: Vec<(u8, u32, u32)> =
            tiles(&mask, 4).unwrap().into_iter().map(|t| (t.z, t.x, t.y)).collect();
        decoded.sort_unstable();
        let mut wanted: Vec<(u8, u32, u32)> = (0..=4u8)
            .flat_map(|z| (0..1u32 << z).flat_map(move |x| (0..1u32 << z).map(move |y| (z, x, y))))
            .filter(|&(z, x, y)| inside(z, x, y))
            .collect();
        wanted.sort_unstable();
        assert_eq!(decoded, wanted);
        assert!(wanted.len() > 50, "a partial tree, not the root alone");
    }

    #[test]
    fn a_shape_gives_a_mask_over_its_own_tiles() {
        let shape = crate::poly::Polygon::from_str(
            "sq\n1\n 6.0 44.0\n 8.0 44.0\n 8.0 46.0\n 6.0 46.0\n 6.0 44.0\nEND\nEND\n",
        )
        .unwrap();
        let mask = for_polygon(&shape, 8);
        let decoded = tiles(&mask, 8).unwrap();
        assert!(!decoded.is_empty());
        // rows count from the south, so a tile at z8 over 45N, 7E is in the southern half of the
        // world's rows, and the same tile numbered from the north is not in the mask
        let n = 256u32;
        let x = ((7.0 + 180.0) / 360.0 * n as f64) as u32;
        let rad = 45f64.to_radians();
        let north_row = ((1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0
            * n as f64) as u32;
        let tms_row = n - 1 - north_row;
        assert!(decoded.iter().any(|t| t.z == 8 && t.x == x && t.y == tms_row));
        assert!(!decoded.iter().any(|t| t.z == 8 && t.x == x && t.y == north_row));
    }

    #[test]
    fn rejects_non_base64() {
        assert!(tiles("!!!not base64!!!", 2).is_err());
    }
}
