//! Osmosis `.poly` files - the shape every step in this pipeline clips to.
//!
//! The format is a name line, then one section per ring: a ring name, coordinate pairs, `END`,
//! and a final `END` for the file. A ring whose name starts with `!` is a hole. That is the
//! whole specification, which is why the scripts pass `.poly` around rather than GeoJSON.
//!
//! What the callers need from a shape is not "is this point inside" so much as "does this tile
//! touch it" - a tile that only clips a corner still has to be rendered, or the edge of the
//! covered area comes out ragged.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// A closed ring of `(lon, lat)` points.
pub type Ring = Vec<(f64, f64)>;

/// One `.poly` shape: outer rings, and holes cut out of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polygon {
    pub name: String,
    pub outers: Vec<Ring>,
    pub holes: Vec<Ring>,
}

impl Polygon {
    pub fn parse(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse the text of a `.poly` file.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Self> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let name = lines.next().ok_or_else(|| anyhow!("empty .poly file"))?.to_string();

        let mut shape = Polygon { name, ..Default::default() };
        while let Some(header) = lines.next() {
            if header.eq_ignore_ascii_case("END") {
                break;
            }
            let hole = header.starts_with('!');
            let mut ring: Ring = Vec::new();
            loop {
                let line =
                    lines.next().ok_or_else(|| anyhow!("ring `{header}` is not closed by END"))?;
                if line.eq_ignore_ascii_case("END") {
                    break;
                }
                let mut parts = line.split_whitespace();
                let (Some(lon), Some(lat)) = (parts.next(), parts.next()) else {
                    return Err(anyhow!("`{line}` is not a coordinate pair"));
                };
                ring.push((
                    lon.parse().with_context(|| format!("longitude in `{line}`"))?,
                    lat.parse().with_context(|| format!("latitude in `{line}`"))?,
                ));
            }
            if ring.len() < 3 {
                return Err(anyhow!("ring `{header}` has fewer than three points"));
            }
            if hole {
                shape.holes.push(ring);
            } else {
                shape.outers.push(ring);
            }
        }
        if shape.outers.is_empty() {
            return Err(anyhow!("no outer ring"));
        }
        Ok(shape)
    }

    /// A number that is the same for the same shape and, for practical purposes, not for another.
    /// For telling whether two runs were asked about the same outline.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut feed = |value: u64| {
            for byte in value.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for (marker, rings) in [(1u64, &self.outers), (2, &self.holes)] {
            feed(marker);
            for ring in rings {
                feed(ring.len() as u64);
                for (lon, lat) in ring {
                    feed(lon.to_bits());
                    feed(lat.to_bits());
                }
            }
        }
        hash
    }

    /// `(west, south, east, north)` over every outer ring.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let mut bounds = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (lon, lat) in self.outers.iter().flatten() {
            bounds.0 = bounds.0.min(*lon);
            bounds.1 = bounds.1.min(*lat);
            bounds.2 = bounds.2.max(*lon);
            bounds.3 = bounds.3.max(*lat);
        }
        bounds
    }

    /// Whether a point is inside the shape, holes taken out.
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        let inside_any = |rings: &[Ring]| rings.iter().any(|r| point_in_ring(r, lon, lat));
        inside_any(&self.outers) && !inside_any(&self.holes)
    }

    /// Whether a rectangle touches the shape at all.
    ///
    /// A tile counts when its box overlaps the shape by any amount: containment of a corner,
    /// containment of the whole tile inside a large shape, or an edge merely crossing it. Only
    /// testing corners misses a tile a narrow shape passes straight through, which is how a
    /// valley ends up with a hole in the middle of its terrain.
    pub fn intersects_rect(&self, west: f64, south: f64, east: f64, north: f64) -> bool {
        let (bw, bs, be, bn) = self.bounds();
        if east < bw || west > be || north < bs || south > bn {
            return false;
        }
        // any corner or the centre inside the shape
        for (lon, lat) in [
            (west, south),
            (west, north),
            (east, south),
            (east, north),
            ((west + east) / 2.0, (south + north) / 2.0),
        ] {
            if self.contains(lon, lat) {
                return true;
            }
        }
        // a ring vertex inside the rectangle, or a ring edge crossing one of its sides
        for ring in self.outers.iter() {
            for point in ring {
                if point.0 >= west && point.0 <= east && point.1 >= south && point.1 <= north {
                    return true;
                }
            }
            for pair in ring.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if segment_crosses_rect(a, b, west, south, east, north) {
                    return true;
                }
            }
        }
        false
    }
}

/// Even-odd ray casting. Points exactly on an edge are not worth special-casing: a tile grid
/// never lines up with a hand-drawn boundary to the last bit.
fn point_in_ring(ring: &Ring, lon: f64, lat: f64) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn segment_crosses_rect(
    a: (f64, f64),
    b: (f64, f64),
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) -> bool {
    let sides = [
        ((west, south), (east, south)),
        ((east, south), (east, north)),
        ((east, north), (west, north)),
        ((west, north), (west, south)),
    ];
    sides.iter().any(|(c, d)| segments_cross(a, b, *c, *d))
}

fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        let value = (q.1 - p.1) * (r.0 - q.0) - (q.0 - p.0) * (r.1 - q.1);
        if value.abs() < f64::EPSILON {
            0
        } else if value > 0.0 {
            1
        } else {
            -1
        }
    };
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    o1 != o2 && o3 != o4
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &str =
        "test\n1\n   0.0 0.0\n   0.0 10.0\n   10.0 10.0\n   10.0 0.0\n   0.0 0.0\nEND\nEND\n";

    #[test]
    fn parses_a_single_ring() {
        let shape = Polygon::from_str(SQUARE).unwrap();
        assert_eq!(shape.name, "test");
        assert_eq!(shape.outers.len(), 1);
        assert_eq!(shape.bounds(), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn a_bang_ring_is_a_hole() {
        let text = format!(
            "{}",
            SQUARE.replace(
                "END\nEND",
                "END\n!2\n   4.0 4.0\n   4.0 6.0\n   6.0 6.0\n   6.0 4.0\n   4.0 4.0\nEND\nEND"
            )
        );
        let shape = Polygon::from_str(&text).unwrap();
        assert_eq!(shape.holes.len(), 1);
        assert!(shape.contains(1.0, 1.0));
        assert!(!shape.contains(5.0, 5.0), "the hole is not inside");
    }

    #[test]
    fn points_outside_are_outside() {
        let shape = Polygon::from_str(SQUARE).unwrap();
        assert!(shape.contains(5.0, 5.0));
        assert!(!shape.contains(-1.0, 5.0));
        assert!(!shape.contains(5.0, 11.0));
    }

    /// The case corner tests alone get wrong: a tile the shape passes through without any corner
    /// of either being inside the other.
    #[test]
    fn a_shape_crossing_a_tile_counts_as_touching_it() {
        let band =
            "band\n1\n  -5.0 4.0\n  15.0 4.0\n  15.0 6.0\n  -5.0 6.0\n  -5.0 4.0\nEND\nEND\n";
        let shape = Polygon::from_str(band).unwrap();
        assert!(shape.intersects_rect(0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn a_tile_well_away_from_the_shape_does_not_count() {
        let shape = Polygon::from_str(SQUARE).unwrap();
        assert!(!shape.intersects_rect(20.0, 20.0, 30.0, 30.0));
        assert!(shape.intersects_rect(-1.0, -1.0, 1.0, 1.0));
    }

    #[test]
    fn a_tile_swallowing_the_whole_shape_counts() {
        let shape = Polygon::from_str(SQUARE).unwrap();
        assert!(shape.intersects_rect(-90.0, -90.0, 90.0, 90.0));
    }

    #[test]
    fn an_unterminated_ring_is_an_error() {
        assert!(Polygon::from_str("test\n1\n 0.0 0.0\n 1.0 1.0\n").is_err());
        assert!(Polygon::from_str("").is_err());
    }

    /// The real thing, so the parser is not only tested against its own idea of the format.
    #[test]
    fn reads_the_repository_poly() {
        let path = std::path::Path::new("../../examples/areas/rhone-alpes.poly");
        if !path.exists() {
            return;
        }
        let shape = Polygon::parse(path).unwrap();
        let (w, s, e, n) = shape.bounds();
        assert!(w > 3.0 && e < 8.0 && s > 43.0 && n < 47.0, "bounds look wrong: {w},{s},{e},{n}");
        assert!(shape.contains(5.72, 45.18), "Grenoble is in Rhone-Alpes");
        assert!(!shape.contains(2.35, 48.85), "Paris is not");
    }
}
