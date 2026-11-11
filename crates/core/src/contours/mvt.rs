//! Mapbox Vector Tile encoding for one line layer, and the line simplification that precedes it.
//!
//! Hand-written rather than pulled from a crate: a layer of lines with two integer attributes is
//! a dozen protobuf fields, and the encoder is the hot path of the contour step. Doing it here
//! also means every tile is built in one buffer, straight from the lines, with no intermediate
//! feature objects.
//!
//! The format, in the parts this uses (vector_tile.proto, version 2):
//!
//! ```text
//! Tile    { Layer layers = 3; }
//! Layer   { uint32 version = 15; string name = 1; Feature features = 2;
//!           string keys = 3; Value values = 4; uint32 extent = 5; }
//! Feature { packed uint32 tags = 2; GeomType type = 3; packed uint32 geometry = 4; }
//! Value   { sint64 sint_value = 6; }
//! ```
//!
//! A geometry is a run of commands: `MoveTo` and `LineTo`, each a `(count << 3) | id` followed by
//! `count` zigzag-encoded `(dx, dy)` pairs, relative to the previous point.

use std::io::Write;

use flate2::write::GzEncoder;
use flate2::Compression;

const LINESTRING: u64 = 2;
const MOVE_TO: u32 = 1;
const LINE_TO: u32 = 2;

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn zigzag32(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// A length-delimited field: tag, length, bytes.
fn field(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    varint(out, u64::from(number << 3 | 2));
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// One line with its two attributes, in tile units.
#[derive(Debug, Clone, PartialEq)]
pub struct LineFeature {
    pub ele: i64,
    pub div: i64,
    pub points: Vec<(i32, i32)>,
}

/// Encode one layer of line features as a tile, uncompressed.
pub fn encode_tile(layer: &str, extent: u32, features: &[LineFeature]) -> Vec<u8> {
    // The attribute tables. A tile has a handful of distinct elevations and fewer divisions, so
    // a linear lookup is faster than hashing and keeps the order - and so the bytes - stable.
    let mut values: Vec<i64> = Vec::new();
    let index_of = |value: i64, values: &mut Vec<i64>| -> u32 {
        match values.iter().position(|v| *v == value) {
            Some(at) => at as u32,
            None => {
                values.push(value);
                (values.len() - 1) as u32
            }
        }
    };

    let mut body = Vec::new();
    varint(&mut body, 120); // version
    varint(&mut body, 2);
    field(&mut body, 1, layer.as_bytes());

    let mut scratch = Vec::new();
    let mut geometry = Vec::new();
    for feature in features {
        let ele = index_of(feature.ele, &mut values);
        let div = index_of(feature.div, &mut values);

        scratch.clear();
        // tags: key 0 (`ele`) with its value, key 1 (`div`) with its value
        let mut tags = Vec::new();
        for tag in [0u32, ele, 1, div] {
            varint(&mut tags, u64::from(tag));
        }
        field(&mut scratch, 2, &tags);
        varint(&mut scratch, 24);
        varint(&mut scratch, LINESTRING);

        geometry.clear();
        let (mut x, mut y) = (0i32, 0i32);
        let command = |id: u32, count: usize, geometry: &mut Vec<u8>| {
            varint(geometry, u64::from((count as u32) << 3 | id));
        };
        if let Some((first, rest)) = feature.points.split_first() {
            command(MOVE_TO, 1, &mut geometry);
            varint(&mut geometry, u64::from(zigzag32(first.0 - x)));
            varint(&mut geometry, u64::from(zigzag32(first.1 - y)));
            (x, y) = *first;
            command(LINE_TO, rest.len(), &mut geometry);
            for point in rest {
                varint(&mut geometry, u64::from(zigzag32(point.0 - x)));
                varint(&mut geometry, u64::from(zigzag32(point.1 - y)));
                (x, y) = *point;
            }
        }
        field(&mut scratch, 4, &geometry);
        field(&mut body, 2, &scratch);
    }

    field(&mut body, 3, b"ele");
    field(&mut body, 3, b"div");
    for value in &values {
        let mut encoded = Vec::new();
        varint(&mut encoded, 48);
        varint(&mut encoded, zigzag(*value));
        field(&mut body, 4, &encoded);
    }
    varint(&mut body, 40);
    varint(&mut body, u64::from(extent));

    let mut tile = Vec::with_capacity(body.len() + 8);
    field(&mut tile, 3, &body);
    tile
}

/// Gzip a tile, the way every vector tile archive in this pipeline stores them.
///
/// Level 6, as planetiler writes them: the contour tiles are small and numerous, and the last few
/// percent a higher level finds costs more than the render it follows.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 2 + 32), Compression::new(6));
    // writing to a Vec cannot fail
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

/// Douglas-Peucker: drop the vertices that move the line by no more than `tolerance`.
///
/// `tolerance` is in tile units. The first and last points always stay, so a line keeps its ends -
/// and a closed ring, whose ends coincide, keeps that point, which is the one place it is cut.
pub fn simplify(points: &[(i32, i32)], tolerance: f64) -> Vec<(i32, i32)> {
    let n = points.len();
    if n < 3 || tolerance <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    let limit = tolerance * tolerance;
    while let Some((from, to)) = stack.pop() {
        if to <= from + 1 {
            continue;
        }
        let (mut worst, mut at) = (0.0f64, from);
        for i in from + 1..to {
            let d = distance_squared(points[i], points[from], points[to]);
            if d > worst {
                worst = d;
                at = i;
            }
        }
        if worst > limit {
            keep[at] = true;
            stack.push((from, at));
            stack.push((at, to));
        }
    }
    points.iter().zip(&keep).filter(|(_, k)| **k).map(|(p, _)| *p).collect()
}

/// Squared distance from `p` to the segment `a`-`b`.
fn distance_squared(p: (i32, i32), a: (i32, i32), b: (i32, i32)) -> f64 {
    let (px, py) = (f64::from(p.0), f64::from(p.1));
    let (ax, ay) = (f64::from(a.0), f64::from(a.1));
    let (dx, dy) = (f64::from(b.0) - ax, f64::from(b.1) - ay);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / length).clamp(0.0, 1.0)
    };
    let (cx, cy) = (ax + t * dx - px, ay + t * dy - py);
    cx * cx + cy * cy
}

#[cfg(test)]
pub(crate) mod decode {
    //! Just enough of a reader to check what the encoder wrote: one layer of line features.

    use super::LineFeature;

    pub struct Layer {
        pub name: String,
        pub extent: u32,
        pub keys: Vec<String>,
        pub values: Vec<i64>,
        pub features: Vec<LineFeature>,
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl<'a> Reader<'a> {
        fn varint(&mut self) -> u64 {
            let (mut value, mut shift) = (0u64, 0);
            loop {
                let byte = self.bytes[self.at];
                self.at += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        }

        fn bytes(&mut self) -> &'a [u8] {
            let len = self.varint() as usize;
            let out = &self.bytes[self.at..self.at + len];
            self.at += len;
            out
        }

        fn done(&self) -> bool {
            self.at >= self.bytes.len()
        }
    }

    fn unzigzag(v: u64) -> i64 {
        ((v >> 1) as i64) ^ -((v & 1) as i64)
    }

    pub fn layer(tile: &[u8]) -> Layer {
        let mut outer = Reader { bytes: tile, at: 0 };
        assert_eq!(outer.varint(), 3 << 3 | 2, "a tile holds layers");
        let body = outer.bytes();
        assert!(outer.done(), "exactly one layer");

        let mut r = Reader { bytes: body, at: 0 };
        let mut layer = Layer {
            name: String::new(),
            extent: 0,
            keys: vec![],
            values: vec![],
            features: vec![],
        };
        let mut raw_features: Vec<(Vec<u32>, Vec<u32>)> = vec![];
        while !r.done() {
            let tag = r.varint();
            match (tag >> 3, tag & 7) {
                (15, 0) => assert_eq!(r.varint(), 2),
                (1, 2) => layer.name = String::from_utf8(r.bytes().to_vec()).unwrap(),
                (3, 2) => layer.keys.push(String::from_utf8(r.bytes().to_vec()).unwrap()),
                (5, 0) => layer.extent = r.varint() as u32,
                (4, 2) => {
                    let mut v = Reader { bytes: r.bytes(), at: 0 };
                    assert_eq!(v.varint(), 6 << 3);
                    layer.values.push(unzigzag(v.varint()));
                }
                (2, 2) => {
                    let mut f = Reader { bytes: r.bytes(), at: 0 };
                    let (mut tags, mut geometry) = (vec![], vec![]);
                    while !f.done() {
                        let t = f.varint();
                        match (t >> 3, t & 7) {
                            (2, 2) => {
                                let mut p = Reader { bytes: f.bytes(), at: 0 };
                                while !p.done() {
                                    tags.push(p.varint() as u32);
                                }
                            }
                            (3, 0) => assert_eq!(f.varint(), 2, "lines only"),
                            (4, 2) => {
                                let mut p = Reader { bytes: f.bytes(), at: 0 };
                                while !p.done() {
                                    geometry.push(p.varint() as u32);
                                }
                            }
                            other => panic!("unexpected feature field {other:?}"),
                        }
                    }
                    raw_features.push((tags, geometry));
                }
                other => panic!("unexpected layer field {other:?}"),
            }
        }

        for (tags, geometry) in raw_features {
            let value_of = |key: &str| {
                let k = layer.keys.iter().position(|n| n == key).unwrap() as u32;
                let pair = tags.chunks(2).find(|p| p[0] == k).unwrap();
                layer.values[pair[1] as usize]
            };
            let mut points = vec![];
            let (mut x, mut y) = (0i64, 0i64);
            let mut at = 0;
            while at < geometry.len() {
                let (id, count) = (geometry[at] & 7, (geometry[at] >> 3) as usize);
                at += 1;
                assert!(id == 1 || id == 2);
                for _ in 0..count {
                    x += unzigzag(u64::from(geometry[at]));
                    y += unzigzag(u64::from(geometry[at + 1]));
                    at += 2;
                    points.push((x as i32, y as i32));
                }
            }
            layer.features.push(LineFeature { ele: value_of("ele"), div: value_of("div"), points });
        }
        layer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn feature(ele: i64, div: i64, points: &[(i32, i32)]) -> LineFeature {
        LineFeature { ele, div, points: points.to_vec() }
    }

    #[test]
    fn a_tile_reads_back_as_it_was_written() {
        let features = vec![
            feature(1230, 10, &[(0, 0), (100, 50), (4096, 4000), (-80, 4176)]),
            feature(1240, 10, &[(5, 5), (6, 7)]),
            feature(1500, 500, &[(1, 1), (2, 2), (3, 1)]),
            feature(-10, 10, &[(10, 10), (20, 20)]),
        ];
        let decoded = decode::layer(&encode_tile("contour", 4096, &features));
        assert_eq!(decoded.name, "contour");
        assert_eq!(decoded.extent, 4096);
        assert_eq!(decoded.keys, vec!["ele", "div"]);
        assert_eq!(decoded.features, features);
        // 1230, 10, 1240, 1500, 500, -10: each stored once however many features use it
        assert_eq!(decoded.values.len(), 6, "{:?}", decoded.values);
    }

    #[test]
    fn gzip_round_trips() {
        let tile = encode_tile("contour", 4096, &[feature(10, 10, &[(0, 0), (1, 1)])]);
        let mut back = Vec::new();
        GzDecoder::new(&gzip(&tile)[..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, tile);
        assert_eq!(&gzip(&tile)[..2], &[0x1f, 0x8b], "an mbtiles reader sniffs this");
    }

    /// The same tile has to compress to the same bytes, or an interrupted build that is resumed
    /// would not equal one that was not.
    #[test]
    fn gzip_is_deterministic() {
        let tile = encode_tile("contour", 4096, &[feature(10, 10, &[(0, 0), (1, 1)])]);
        assert_eq!(gzip(&tile), gzip(&tile));
    }

    #[test]
    fn simplify_drops_what_is_within_tolerance() {
        let line = [(0, 0), (50, 2), (100, 0), (150, 40), (200, 0)];
        // (50, 2) is 2 off the line to its neighbours and goes at a tolerance of 3; (150, 40) is
        // 40 off and stays, and with it (100, 0), which is far from the new line to (150, 40)
        assert_eq!(simplify(&line, 3.0), vec![(0, 0), (100, 0), (150, 40), (200, 0)]);
        assert_eq!(simplify(&line, 100.0), vec![(0, 0), (200, 0)]);
        assert_eq!(simplify(&line, 0.0), line.to_vec(), "0 turns it off");
        assert_eq!(simplify(&line[..2], 100.0), line[..2].to_vec(), "two points stay");
    }

    #[test]
    fn simplify_keeps_the_point_where_a_ring_closes() {
        let ring = [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
        let simplified = simplify(&ring, 3.0);
        assert_eq!(simplified.first(), simplified.last());
        assert_eq!(simplified.len(), 5, "a square has nothing to drop");
    }
}
