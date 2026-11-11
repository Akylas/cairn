//! Declarative option schema for build steps.
//!
//! The point is that the GUI never hard-codes a form: it renders whatever these definitions
//! say, and the same definitions turn the collected values back into a command line. That keeps
//! the form and the argv in step, and makes the argv testable without running anything.
//!
//! Every flag name here was read out of the sources rather than remembered - the stock ones from
//! `PlanetilerConfig`, the custom ones from the fork's `Route`/`Landcover` layers. Planetiler's
//! `Arguments` treats `-` and `_` in a flag name as equivalent, so `--simplify-tolerance` and
//! `--simplify_tolerance` reach the same setting.
//!
//! Defaults are deliberately *absent*. An option carries a `hint` describing what planetiler
//! does when the flag is omitted, but the schema never asserts that value - so an unset option
//! emits nothing and planetiler's own default stands, instead of this file's guess about it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OptionKind {
    Bool,
    Int { min: Option<i64>, max: Option<i64> },
    Float { min: Option<f64>, max: Option<f64> },
    Text,
    Choice { choices: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionDef {
    /// Stable key used in presets and in the values map.
    pub key: String,
    /// Flag as passed to planetiler, without the leading `--`.
    pub flag: String,
    pub label: String,
    pub help: String,
    pub group: String,
    pub kind: OptionKind,
    /// What happens when the option is left unset. Documentation only - never emitted.
    pub hint: String,
}

fn opt(
    key: &str,
    flag: &str,
    label: &str,
    group: &str,
    kind: OptionKind,
    help: &str,
    hint: &str,
) -> OptionDef {
    OptionDef {
        key: key.into(),
        flag: flag.into(),
        label: label.into(),
        group: group.into(),
        kind,
        help: help.into(),
        hint: hint.into(),
    }
}

fn float(min: f64) -> OptionKind {
    OptionKind::Float { min: Some(min), max: None }
}

/// Options shared by every planetiler-driven step.
pub fn planetiler_common() -> Vec<OptionDef> {
    vec![
        opt(
            "simplify_tolerance",
            "simplify-tolerance",
            "Simplify tolerance",
            "Geometry",
            float(0.0),
            "Douglas-Peucker tolerance in tile pixels below max zoom. Removes vertices; never \
             deletes a feature, so lowering detail here costs shape fidelity but not content.",
            "planetiler's own default applies",
        ),
        opt(
            "simplify_tolerance_at_max_zoom",
            "simplify-tolerance-at-max-zoom",
            "Simplify tolerance (max zoom)",
            "Geometry",
            float(0.0),
            "Same, applied only at the maximum zoom.",
            "falls back to the value below max zoom",
        ),
        opt(
            "min_feature_size",
            "min-feature-size",
            "Min feature size",
            "Geometry",
            float(0.0),
            "Deletion threshold in tile pixels below max zoom. SQUARED for polygons, so this is \
             a minimum area - raising it drops small polygons entirely rather than simplifying \
             them, which is why it reads as missing forest and grass rather than coarser forest.",
            "planetiler's own default applies",
        ),
        opt(
            "min_feature_size_at_max_zoom",
            "min-feature-size-at-max-zoom",
            "Min feature size (max zoom)",
            "Geometry",
            float(0.0),
            "Same, applied only at the maximum zoom.",
            "falls back to the value below max zoom",
        ),
        opt(
            "maxzoom",
            "maxzoom",
            "Max zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Highest zoom rendered. The top zoom dominates output size - z14 is about 71% of a \
             rhone-alpes basemap.",
            "14",
        ),
        opt(
            "minzoom",
            "minzoom",
            "Min zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Lowest zoom rendered.",
            "0",
        ),
        opt(
            "nodemap_type",
            "nodemap-type",
            "Node map",
            "Performance",
            OptionKind::Choice {
                choices: vec!["sparsearray".into(), "sortedtable".into(), "array".into()],
            },
            "How OSM node locations are held. `sparsearray` is the low-memory choice and is what \
             makes a 16 GB machine viable.",
            "planetiler picks based on input size",
        ),
        opt(
            "parallel_tmp_io",
            "parallel-tmp-io",
            "Parallel temp IO",
            "Performance",
            OptionKind::Bool,
            "Read and write sort chunks in parallel.",
            "off",
        ),
        opt(
            "max_point_buffer",
            "max-point-buffer",
            "Max point buffer",
            "Geometry",
            float(0.0),
            "Caps how far outside its own edge a tile carries POINT features. Layers such as \
             `place` declare a 256px buffer, which is nine times the tile's own area - capping it \
             at 4 is worth tens of MB. Applies to points only; lines and polygons keep their \
             layer's buffer.",
            "no cap, every layer's own buffer applies",
        ),
        opt(
            "mlt_shared_dict",
            "mlt-shared-dict",
            "MLT shared dictionary",
            "Output",
            OptionKind::Bool,
            "Share the string dictionary across the archive.",
            "off",
        ),
        opt(
            "transportation_name_limit_merge",
            "transportation-name-limit-merge",
            "Limit name merge",
            "Layers",
            OptionKind::Bool,
            "Restrict merging of transportation_name features.",
            "off",
        ),
        opt(
            "area_poly",
            "area_poly",
            "Clip to the area boundary",
            "Tiles",
            OptionKind::Bool,
            "Clip to the area's own .poly rather than the extract's bounding box, downloading it \
             from Geofabrik if it is not already beside the extract. Without it a build writes \
             half-filled tiles all around the bounding box. An explicit clip shape wins over this.",
            "off",
        ),
        opt(
            "compact_db",
            "compact-db",
            "Compact archive",
            "Output",
            OptionKind::Bool,
            "Store each distinct tile blob once behind a `tiles` view. Measured on the current \
             rhone-alpes output this deduplicates only 0.3% of tiles, so the indirection is \
             close to free but also close to pointless.",
            "off",
        ),
        opt(
            "skip_filled_tiles",
            "skip-filled-tiles",
            "Skip filled tiles",
            "Output",
            OptionKind::Bool,
            "Omit tiles whose content is entirely covered by their parent.",
            "off",
        ),
        opt(
            "languages",
            "languages",
            "Languages",
            "Output",
            OptionKind::Text,
            "Comma-separated name languages to keep. Empty drops all localised names.",
            "all languages",
        ),
        opt(
            "polygon",
            "polygon",
            "Clip polygon",
            "Output",
            OptionKind::Text,
            "Path to a .poly clipping the build to a shape.",
            "the extract's own bbox",
        ),
    ]
}

/// Terrain-RGB options.
///
/// These are the renderer's own knobs, not planetiler flags - `flag` names the CLI flag so the
/// same values can be shown as an `cairn terrain` command line. The step used to be handed
/// `TerrainOptions::default()` regardless of what the form said.
pub fn terrain_options() -> Vec<OptionDef> {
    vec![
        pmtiles_option(),
        opt("minzoom", "minzoom", "Min zoom", "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) }, "Lowest zoom rendered.", "5"),
        opt("maxzoom", "maxzoom", "Max zoom", "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Highest zoom rendered, and the zoom the quantisation ramp is anchored to.", "12"),
        opt("encoding", "encoding", "Encoding", "Packing",
            OptionKind::Choice { choices: vec!["terrarium".into(), "mapbox".into()] },
            "How elevation is packed into RGB. terrarium is 1 m per step at round-digits 8;              mapbox is 0.1 m, which is what the older `_hillshade` archives use.",
            "mapbox"),
        opt("round_digits", "round-digits", "Round digits", "Packing",
            OptionKind::Int { min: Some(0), max: Some(16) },
            "Quantisation exponent at the maximum zoom. The step is `interval * 2^round_digits`,              so raising it coarsens elevation and compresses far better.",
            "0"),
        opt("max_round_digits", "max-round-digits", "Max round digits", "Packing",
            OptionKind::Int { min: Some(0), max: Some(16) },
            "Cap on the per-zoom ramp: lower zooms quantise more coarsely, up to this.",
            "15"),
        opt("tile_size", "tile-size", "Tile size", "Output",
            OptionKind::Int { min: Some(256), max: Some(1024) }, "Pixels per side.", "512"),
        opt("format", "format", "Format", "Output",
            OptionKind::Choice { choices: vec!["webp".into(), "png".into()] },
            "Lossless WebP is much smaller; PNG is for tools that will not read WebP.", "webp"),
        opt("blur", "blur", "Source blend", "Sources",
            float(0.0),
            "Metres over which a higher-priority source fades in at its coverage boundary, so              the seam between IGN and tilezen data is a ramp rather than a step.",
            "1000"),
        opt("nodata_elevation", "nodata-elevation", "No-data elevation", "Sources",
            OptionKind::Float { min: None, max: None },
            "Elevation written where no source covers a pixel. build_terrain_rgb.py used -10 so              uncovered pixels read as sea; every archive in this repository was built with 0.",
            "0"),
        opt("download_elevation", "no-elevation-download", "Fetch missing tiles", "Sources",
            OptionKind::Bool,
            "Download any .hgt tile this render needs and does not have. A missing tile is \
             otherwise silent: the renderer writes nothing there and the archive comes out with \
             a hole.",
            "on"),
        opt("area_poly", "area_poly", "Clip to the area boundary", "Sources",
            OptionKind::Bool,
            "Clip to the area's own .poly rather than the extract's bounding box, downloading it \
             from Geofabrik if it is not already beside the extract. Without it a build writes \
             half-filled tiles all around the bounding box. An explicit clip shape wins over this.",
            "off"),
        opt("fresh", "fresh", "Start over", "Output",
            OptionKind::Bool,
            "Discard a partial build left by an interrupted run and begin again. Without it a run \
             continues one made with the same settings, and refuses one made with different \
             settings rather than mixing the two.",
            "continue a matching partial build"),
        opt("poly_shape", "poly-shape", "Clip shape", "Sources",
            OptionKind::Text,
            "Path to an osmosis .poly. Only tiles touching the shape are written.",
            "the whole bounding box"),
        opt("tile_buffer", "tile-buffer", "Tile buffer", "Sources",
            OptionKind::Int { min: Some(0), max: Some(8) },
            "Ring of extra tiles around the shape. 3D renderers backfill a DEM tile's 1px border              from its neighbours, so without a ring there is a seam where coverage stops.",
            "1"),
        opt("bounds", "bounds", "Bounds", "Sources",
            OptionKind::Text, "west,south,east,north.",
            "the shape's bounds, else the area's basemap bounds"),
    ]
}

/// Contour options: the old `build_contours.sh`'s flags, and what the new pipeline adds to them.
pub fn contour_options() -> Vec<OptionDef> {
    vec![
        pmtiles_option(),
        opt(
            "minzoom",
            "minzoom",
            "Min zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Lowest zoom written. Below it the map has no lines at all, which is the point: the \
             lines at a low zoom are a dense wash. The zoom filter on `div` still applies.",
            "11",
        ),
        opt(
            "maxzoom",
            "maxzoom",
            "Max zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Highest zoom written. The top zoom is most of the archive and most of the time.",
            "14",
        ),
        opt(
            "interval",
            "interval",
            "Interval",
            "Lines",
            OptionKind::Int { min: Some(1), max: Some(1000) },
            "Metres between two lines. Every line carries `ele`, and `div`: the largest of \
             1000, 500, 250, 200, 100, 50, 20 and 10 that its elevation is a multiple of, which \
             the zoom filter and a style's heavier index lines read.",
            "10",
        ),
        opt(
            "layer",
            "layer",
            "Layer name",
            "Lines",
            OptionKind::Text,
            "Name of the vector layer the lines are in.",
            "contour",
        ),
        opt(
            "name",
            "name",
            "Archive name",
            "Output",
            OptionKind::Text,
            "The `name` in the archive's metadata, which tile servers show.",
            "<area>_contours",
        ),
        opt(
            "simplify",
            "simplify",
            "Simplification",
            "Lines",
            OptionKind::Float { min: Some(0.0), max: None },
            "Douglas-Peucker tolerance in tile units, of 4096 to a tile - tippecanoe's `-S`. 0 \
             keeps every vertex of the traced grid, which is larger and no more accurate than \
             the elevation it came from.",
            "3",
        ),
        opt(
            "resolution",
            "resolution",
            "Grid spacing",
            "Lines",
            OptionKind::Float { min: Some(1.0), max: None },
            "The finest the elevation is sampled for a tile, in metres between samples. Lower \
             traces finer detail out of better data - set 5 for a 5 m source - at a cost that \
             grows with the square. Below the ground resolution of the source it only draws the \
             same slope through more points.",
            "10",
        ),
        opt(
            "max_grid",
            "max-grid",
            "Max grid",
            "Lines",
            OptionKind::Int { min: Some(8), max: Some(2048) },
            "The most samples along one side of a tile, however fine the spacing asks for. It \
             is what bounds the low zooms, where a tile is tens of kilometres across.",
            "256",
        ),
        opt(
            "blur",
            "blur",
            "Source blend",
            "Sources",
            float(0.0),
            "Metres over which a higher-priority source fades in at its coverage boundary, so \
             the seam between two sources is a slope and not a cliff the lines pile up on. The \
             same value as the terrain step, so the two agree. 0 is a hard switch, and the \
             fastest.",
            "1000",
        ),
        opt(
            "download_elevation",
            "no-elevation-download",
            "Fetch missing tiles",
            "Sources",
            OptionKind::Bool,
            "Download any .hgt tile this build needs and does not have. A missing tile is \
             otherwise silent: there are no lines there, and the archive has a hole.",
            "on",
        ),
        opt(
            "area_poly",
            "area_poly",
            "Clip to the area boundary",
            "Sources",
            OptionKind::Bool,
            "Only write the tiles the area's own .poly touches, downloading it from Geofabrik if \
             it is not already beside the extract. An explicit clip shape wins over this.",
            "off",
        ),
        opt(
            "poly_shape",
            "poly-shape",
            "Clip shape",
            "Sources",
            OptionKind::Text,
            "Path to an osmosis .poly. Only tiles touching the shape are written, and its tile \
             mask is stored in the archive's metadata.",
            "the whole bounding box",
        ),
        opt(
            "bounds",
            "bounds",
            "Bounds",
            "Sources",
            OptionKind::Text,
            "west,south,east,north.",
            "the shape's bounds, else the area's basemap bounds",
        ),
        opt(
            "polyzoom",
            "polyzoom",
            "Tile mask zoom",
            "Sources",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Zoom of the quadtree `tilemask` stored in the metadata when there is a clip shape. \
             It starts four zooms lower and goes up until the mask is long enough to be worth \
             having. The app that reads the archive uses it to tell which positions it covers.",
            "11",
        ),
        opt(
            "match_basemap",
            "match-basemap",
            "Same tiles as the basemap",
            "Output",
            OptionKind::Bool,
            "Render only the tiles the area's basemap also has. An app that draws contours \
             between the layers of the basemap merges the two tiles of one position, and a \
             contour tile with no basemap tile under it is never drawn. This is what \
             `filter_tiles_from_other_mbtiles.py` did afterwards - here nothing is rendered to \
             be thrown away. Ignored, with a note, when the area has no basemap yet.",
            "off",
        ),
        opt(
            "fresh",
            "fresh",
            "Start over",
            "Output",
            OptionKind::Bool,
            "Discard a partial build left by an interrupted run and begin again. Without it a run \
             continues one made with the same settings, and refuses one made with different \
             settings rather than mixing the two.",
            "continue a matching partial build",
        ),
    ]
}

/// Valhalla package options.
pub fn package_options() -> Vec<OptionDef> {
    vec![
        opt(
            "compression",
            "compression",
            "Compression",
            "Output",
            OptionKind::Choice { choices: vec!["zopfli".into(), "zlib".into()] },
            "Both emit ordinary gzip. zopfli is about 3% smaller and much slower.",
            "zopfli",
        ),
        opt(
            "poly",
            "poly",
            "Tile selection shape",
            "Tiles",
            OptionKind::Text,
            "Path to an osmosis .poly. Every graph tile the shape touches is packed. Left empty, \
             the tiles are those of the package already built for this area, and when there is \
             none, those under the area's own boundary - the .poly beside the OSM extract, \
             downloaded from Geofabrik if it is missing. Never every tile in the graph.",
            "the existing package's tiles, else the area's boundary",
        ),
        opt(
            "levels",
            "levels",
            "Hierarchy levels",
            "Tiles",
            OptionKind::Text,
            "Comma-separated Valhalla levels to include.",
            "0,1,2",
        ),
    ]
}

/// Basemap-only options, including the fork's landcover work.
pub fn basemap_options() -> Vec<OptionDef> {
    let mut defs = planetiler_common();
    defs.push(pmtiles_option());
    defs.extend([
        opt("exclude_layers", "exclude_layers", "Exclude layers", "Layers",
            OptionKind::Text, "Comma-separated layers to leave out. The basemap excludes `route`.", "none"),
        opt("only_layers", "only_layers", "Only layers", "Layers",
            OptionKind::Text, "Comma-separated allow-list.", "all layers"),
        opt("transportation_z13_paths", "transportation_z13_paths", "Paths at z13", "Layers",
            OptionKind::Bool, "Keep paths down to z13.", "off"),
        opt("poi_custom_ranks", "poi_custom_ranks", "Outdoor POI ranks", "Layers",
            OptionKind::Bool,
            "Order POI labels by our outdoor ranks - pharmacy, drinking water and bakery ahead of \
             schools, viewpoints last - looked up by subclass then class, instead of \
             OpenMapTiles' order by class. Changes which label wins a crowded spot, and the \
             `rank` a style filters on.",
            "OpenMapTiles' ranks"),
        opt("poi_trees", "poi_trees", "Trees", "Layers",
            OptionKind::Bool,
            "Emit `natural=tree` in the poi layer from z14 as `class=tree`. Unnamed trees come as \
             one MultiPoint per tile with no `rank`, so a style needs a rule that draws MultiPoints \
             and does not filter them on rank. About +0.3% of tile bytes on rhone-alpes \
             (354k trees, up to 7.9k in one city tile).",
            "off"),
        opt("poi_landmarks", "poi_landmarks", "Landmarks", "Layers",
            OptionKind::Bool,
            "Emit from z14 in the poi layer: power towers (`power_tower`), aerialway pylons \
             (`pylon`), masts, wind turbines, wayside crosses and shrines, crosses, cairns, stones \
             and rocks. Unnamed ones come as one MultiPoint per class per tile with no `rank`. \
             About +0.23% of tile bytes on rhone-alpes (59k points).",
            "off"),
        opt("power_lines", "power_lines", "Power lines", "Layers",
            OptionKind::Bool,
            "Emit overhead transmission lines (`power=line`) from z14 in a new `power` layer as \
             `class=line`. Distribution lines and underground cables stay out. About +0.11% of \
             tile bytes on rhone-alpes (9,400 km).",
            "off"),
        opt("poi_guideposts", "poi_guideposts", "Guideposts", "Layers",
            OptionKind::Bool,
            "Emit hiking guideposts from z14 in the poi layer as `class=guidepost`, without their \
             name, as one MultiPoint per tile with no `rank`. About +0.09% of tile bytes on \
             rhone-alpes (28k guideposts).",
            "off"),
        opt("landcover_tolerance_z11_13", "landcover_tolerance_z11_13", "Landcover tolerance z11-13", "Landcover",
            float(0.0),
            "Overrides landcover simplification for z11-13 only. Must exceed the global \
             simplify-tolerance to have any effect - a smaller value simplifies LESS and makes \
             the file bigger.",
            "the layer's own factor applies"),
        opt("landcover_drop_redundant_subclass", "landcover_drop_redundant_subclass", "Drop redundant subclass", "Landcover",
            OptionKind::Bool,
            "Omit `subclass` where it equals `class`. Small win - gzip already collapses the \
             repetition - and merging falls back to `class` so wood/grass still merge.",
            "off"),
        opt("landcover_merge_maxzoom", "landcover_merge_maxzoom", "Merge landcover at max zoom", "Landcover",
            OptionKind::Bool, "Extend polygon merging to z14.", "merging stops at z13"),
        opt("water_pool_tolerance", "water_pool_tolerance", "Swimming pool tolerance", "Water",
            float(0.0),
            "Extra max-zoom simplification for swimming pools, which carry about 99 vertices each \
             for a shape a pixel or two across. Pools also switch to Douglas-Peucker here: the \
             layer's usual Visvalingam-Whyatt drops lowest-area vertices first and deletes 78% of \
             pools at 1px, where Douglas-Peucker keeps 97.9% of them and still removes 39% of the \
             vertices. 1 is the measured sweet spot; past it the curve flattens.",
            "no extra simplification"),
        opt("drop_redundant_name_int", "drop_redundant_name_int", "Drop duplicate international name", "Names",
            OptionKind::Bool,
            "Omit `name_int` where it is an exact copy of `name`, which it usually is under an \
             empty `languages`. Worth about -0.9% of tile bytes, spread over 13 layers and \
             concentrated in transportation_name.",
            "off"),
        opt("drop_duplicate_names", "drop_duplicate_names", "Drop repeated names", "Names",
            OptionKind::Bool,
            "Omit a `name:<lang>` that copies `name`, and `name_int` where another name tag \
             already carries the same string. Under `languages=fr,en` a French feature with an \
             English name arrives as four tags holding two strings; this leaves two. Not a size \
             win - only about 3% of named features carry any translation. Needs a style whose \
             fallback reaches `name` before `name_int`, or a French reader lands on the English \
             name instead of the local one.",
            "off"),
        opt("transportation_surface_detail", "transportation_surface_detail", "Road surface detail", "Layers",
            OptionKind::Bool,
            "Emit `surface_detail`, OSM's raw surface value or the tracktype grade, on every road \
             class. The stock `surface` attribute only fires on path and track and only ever says \
             `paved`, so unpaved and unknown are indistinguishable. Costs about +0.8% of tile \
             bytes and reaches 73% of tracks. Values are raw OSM, so a style needs a default \
             branch: 75 of the 133 that appear are tagging noise.",
            "off"),
        opt("transportation_surface_detail_minzoom", "transportation_surface_detail_minzoom", "Surface detail min zoom", "Layers",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Lowest zoom carrying `surface_detail`. z13 adds roughly 0.9 MB on rhone-alpes and \
             z12 a further 0.2 MB, to label roads that are hairlines at those zooms.",
            "14"),
        opt("transportation_cycleway", "transportation_cycleway", "Road cycle infrastructure", "Layers",
            OptionKind::Bool,
            "Emit `cycleway_l` and `cycleway_r`, the cycle infrastructure on the road itself for \
             each side relative to the line direction (1 shared lane, 2 painted or advisory lane, \
             3 physically separated: a track, or a lane with a kerb, bollard or flex post, 4 a \
             separate path mapped as its own way), and `shoulder=1`. Absent means none or \
             unknown. Lines carrying `cycleway_l`/`cycleway_r` are no longer merged, since their \
             sides depend on the line direction. Cost not measured yet; it touches a small share \
             of roads.",
            "off"),
        opt("transportation_cycleway_minzoom", "transportation_cycleway_minzoom", "Cycleway min zoom", "Layers",
            OptionKind::Int { min: Some(0), max: Some(15) },
            "Lowest zoom carrying `cycleway_l`, `cycleway_r` and `shoulder`.",
            "13"),
    ]);
    defs
}

/// Route-layer options from the fork.
pub fn routes_options() -> Vec<OptionDef> {
    let mut defs = planetiler_common();
    defs.push(pmtiles_option());
    defs.extend([
        opt(
            "only_layers",
            "only_layers",
            "Only layers",
            "Layers",
            OptionKind::Text,
            "Set to `route` for a routes-only build.",
            "all layers",
        ),
        opt(
            "route_road_tolerance",
            "route_road_tolerance",
            "Match road simplification",
            "Routes",
            OptionKind::Bool,
            "Simplify routes with the same tolerance the transportation layer uses \
             (`tolerance * 0.5`), so a route and the track it follows stay aligned at every zoom.",
            "routes use the plain tolerance and drift from roads",
        ),
        opt(
            "route_extent_digits",
            "route_extent_digits",
            "Extent decimals",
            "Routes",
            OptionKind::Int { min: Some(0), max: Some(9) },
            "Decimal places kept in the `extent` attribute. Extent is unique per relation, so \
             unlike duplicated attributes it does not compress away - trimming it is the single \
             biggest route-tile saving.",
            "3",
        ),
        opt(
            "route_symbol_id",
            "route_symbol_id",
            "Symbols as ids",
            "Routes",
            OptionKind::Bool,
            "Emit `osmc:symbol` as an integer id and write the lookup table alongside.",
            "the full symbol string is stored on every feature",
        ),
        opt(
            "route_symbol_table",
            "route_symbol_table",
            "Symbol table path",
            "Routes",
            OptionKind::Text,
            "Where the symbol id table is written. Regenerate it with every routes build: ids are \
             assigned from the sorted set of symbols in the extract, so a table from a different \
             extract will not line up.",
            "route_symbols.json",
        ),
        // route_slim_attrs, route_drop_extent and route_min_length were removed from planetiler:
        // each worked by taking data out of route tiles, and the route layer should only ever get
        // cheaper to encode, never lose content.
    ]);
    defs
}

/// The archive-format switch, shared by every step whose tool can write both.
///
/// One key, one meaning, so a preset that sets it once applies everywhere it is offered.
fn pmtiles_option() -> OptionDef {
    opt(
        crate::steps::archive::PMTILES_KEY,
        "",
        "PMTiles output",
        "Output",
        OptionKind::Bool,
        "Write a single-file PMTiles archive instead of MBTiles. The tool decides from the \
         output extension, so this only renames the file it was going to write - the tiles are \
         identical - except for terrain, which has no external tool and writes both containers \
         itself. Two things it costs: cairn's own tile server reads SQLite, so the map preview \
         and `cairn serve` cannot show a PMTiles archive, and the catalog can only classify it \
         by filename because there is no metadata table to open.",
        "MBTiles",
    )
}

/// Options for `scripts/make_bathymap.py`.
///
/// Unlike the planetiler schema these `flag` names take a separate value rather than `=`, which
/// is why `bathymap::script_args` renders them and `to_args` does not. `pmtiles` carries an
/// empty flag because the job turns it into an output path, not a switch.
///
/// The hints are the script's own defaults, and they are the measured ones - every number here
/// was chosen against a global build and a render, not guessed.
pub fn bathymap_options() -> Vec<OptionDef> {
    vec![
        opt(
            "extent",
            "",
            "Extent",
            "Zooms",
            OptionKind::Choice { choices: vec!["area".into(), "global".into()] },
            "`area` builds only the selected area's bounding box; `global` builds the whole \
             world. The layer is meant to be global - it is what fills the zooms below the \
             basemap, and it does not read the OSM extract - so one global build serves every \
             area. `area` is for trying settings without waiting half an hour, and needs the \
             area's basemap to have been built so there are bounds to read.",
            "global",
        ),
        opt(
            "min_zoom",
            "min-zoom",
            "Min zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(14) },
            "Lowest zoom built.",
            "0",
        ),
        opt(
            "max_zoom",
            "max-zoom",
            "Max zoom",
            "Zooms",
            OptionKind::Int { min: Some(0), max: Some(14) },
            "Highest zoom built. This dominates the size: on a global build z7 alone was 61% of \
             the file and z6 a further 22%. If the basemap takes over at z8 the client can \
             overzoom z6 for free, so 6 is worth trying before any other size lever.",
            "7",
        ),
        opt(
            "landcover_smooth_pixels",
            "landcover-smooth-pixels",
            "Landcover smoothing",
            "Landcover",
            float(0.0),
            "Radius of the close-then-open pass that welds neighbouring patches into zones and \
             shaves the tendrils, in tile units at each zoom. 0 disables it and leaves the raw \
             patchwork - at z7 that is confetti rather than landcover.",
            "12",
        ),
        opt(
            "landcover_simplify_pixels",
            "landcover-simplify-pixels",
            "Landcover simplification",
            "Landcover",
            float(0.0),
            "Douglas-Peucker tolerance, applied to every class at once through a coverage \
             simplification so classes that share a border keep sharing it. Simplifying classes \
             separately opens slivers between them. 4 px keeps 100% of the area for 75% of the \
             vertices.",
            "4",
        ),
        opt(
            "landcover_min_zone_pixels",
            "landcover-min-zone-pixels",
            "Smallest landcover zone",
            "Landcover",
            float(0.0),
            "Smallest zone kept, squared tile units. What is dropped is not a hole: the zoom \
             below fills it, which is what makes an aggressive value safe here.",
            "4096",
        ),
        opt(
            "bathymetry_simplify_pixels",
            "bathymetry-simplify-pixels",
            "Depth simplification",
            "Depth",
            float(0.0),
            "Douglas-Peucker tolerance for the isobaths. Unconditionally safe because the depth \
             polygons are nested rather than abutting, so nothing shares an edge that could come \
             apart.",
            "14",
        ),
        opt(
            "bathymetry_min_zone_pixels",
            "bathymetry-min-zone-pixels",
            "Smallest depth polygon",
            "Depth",
            float(0.0),
            "Smallest depth polygon kept, squared tile units. Smoothness comes from the \
             tolerance above; this only decides what is too small to draw, and a depth polygon \
             is a real basin rather than speckle. At 16384 the Mediterranean lost a fifth of its \
             3000 m contour at z2.",
            "1024",
        ),
        opt(
            "detail",
            "detail",
            "Tile detail",
            "Output",
            OptionKind::Int { min: Some(8), max: Some(14) },
            "Coordinate resolution inside a tile, as a power of two. Tippecanoe's own default is \
             12, which is eight units per screen pixel at 512px. Measured on a global build: 12 \
             gave 115 MB, 11 gave 101 MB, 10 gave 87 MB.",
            "11",
        ),
        opt(
            "chunk_zoom",
            "chunk-zoom",
            "Processing grid",
            "Performance",
            OptionKind::Int { min: Some(0), max: Some(14) },
            "Zoom of the grid the landcover is dissolved on. Unset picks the coarsest grid that \
             both fits in memory and keeps the workers busy. 0 forces a single seamless chunk \
             and no parallelism - a regional build went from 18 s to 296 s that way.",
            "chosen from the region and the job count",
        ),
        opt(
            "jobs",
            "jobs",
            "Worker processes",
            "Performance",
            OptionKind::Int { min: Some(1), max: Some(64) },
            "Processes building landcover chunks. A global build was 2h20m at 1 and 25 min at 16.",
            "cores minus two, capped at 16",
        ),
        opt(
            "refresh_source",
            "refresh-source",
            "Re-fetch the landcover extract",
            "Sources",
            OptionKind::Bool,
            "Download the regional landcover container again rather than reusing the cached one. \
             Natural Earth is always cached; this is only the VersaTiles extract.",
            "reuse whatever is cached",
        ),
        pmtiles_option(),
    ]
}

/// The options one step accepts.
///
/// The single mapping from step to schema. It used to exist twice - once in the Tauri command
/// and once in `cairn options` - and the two had already drifted: the CLI knew about basemap
/// and routes only, so `cairn options terrain` said no such step while the GUI happily rendered
/// its form.
pub fn for_step(step: crate::steps::StepId) -> Vec<OptionDef> {
    use crate::steps::StepId;
    match step {
        StepId::Basemap => basemap_options(),
        StepId::Routes => routes_options(),
        StepId::Bathymap => bathymap_options(),
        StepId::TerrainRgb => terrain_options(),
        StepId::Contours => contour_options(),
        StepId::ValhallaPackage => package_options(),
        // the download and the two Valhalla binaries take paths and bounds, which are settings
        // rather than per-run choices
        StepId::DownloadOsm
        | StepId::ElevationTiles
        | StepId::ValhallaAdmins
        | StepId::ValhallaTiles => Vec::new(),
    }
}

/// Render a values map into planetiler arguments.
///
/// Only keys actually present are emitted, so an untouched form adds nothing to the command line
/// and planetiler's own defaults stand. Unknown keys are ignored rather than guessed at.
pub fn to_args(defs: &[OptionDef], values: &BTreeMap<String, Value>) -> Vec<String> {
    let mut args = Vec::new();
    for def in defs {
        // An empty flag is an option the runner acts on itself rather than passing through -
        // `pmtiles` picks an output path. Emitting it would hand planetiler `--=true`.
        if def.flag.is_empty() {
            continue;
        }
        let Some(value) = values.get(&def.key) else { continue };
        let rendered = match value {
            Value::Null => continue,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => {
                // an empty string is meaningful for `languages` (drop all names), so it is
                // emitted rather than skipped
                s.clone()
            }
            other => other.to_string(),
        };
        args.push(format!("--{}={}", def.flag, rendered));
    }
    args
}

/// Look a definition up by key.
pub fn find<'a>(defs: &'a [OptionDef], key: &str) -> Option<&'a OptionDef> {
    defs.iter().find(|d| d.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn emits_only_what_was_set() {
        let defs = basemap_options();
        let args = to_args(&defs, &values(&[("simplify_tolerance", json!(0.7))]));
        assert_eq!(args, vec!["--simplify-tolerance=0.7"]);
    }

    /// An untouched form must add nothing, so planetiler's defaults stand rather than this
    /// schema's guesses about them.
    #[test]
    fn empty_values_emit_no_arguments() {
        assert!(to_args(&basemap_options(), &values(&[])).is_empty());
    }

    #[test]
    fn renders_the_measured_flag_set() {
        let defs = basemap_options();
        let args = to_args(
            &defs,
            &values(&[
                ("simplify_tolerance", json!(0.70)),
                ("simplify_tolerance_at_max_zoom", json!(0.25)),
                ("min_feature_size_at_max_zoom", json!(0.25)),
                ("landcover_tolerance_z11_13", json!(1.05)),
                ("landcover_drop_redundant_subclass", json!(true)),
                ("landcover_merge_maxzoom", json!(true)),
            ]),
        );
        assert!(args.contains(&"--simplify-tolerance=0.7".to_string()));
        assert!(args.contains(&"--simplify-tolerance-at-max-zoom=0.25".to_string()));
        assert!(args.contains(&"--landcover_tolerance_z11_13=1.05".to_string()));
        assert!(args.contains(&"--landcover_drop_redundant_subclass=true".to_string()));
        assert_eq!(args.len(), 6);
    }

    #[test]
    fn booleans_emit_explicit_false() {
        let args = to_args(&basemap_options(), &values(&[("compact_db", json!(false))]));
        assert_eq!(args, vec!["--compact-db=false"]);
    }

    /// `--languages=` with nothing after it is how localised names are dropped, so an empty
    /// string must survive rather than being treated as unset.
    #[test]
    fn empty_string_is_still_emitted() {
        let args = to_args(&basemap_options(), &values(&[("languages", json!(""))]));
        assert_eq!(args, vec!["--languages="]);
    }

    #[test]
    fn null_is_treated_as_unset() {
        assert!(
            to_args(&basemap_options(), &values(&[("simplify_tolerance", json!(null))])).is_empty()
        );
    }

    /// `pmtiles` renames the output; it is not a planetiler flag, and emitting it produced
    /// `--=true`, which planetiler rejects.
    #[test]
    fn the_archive_switch_is_never_passed_through() {
        let args = to_args(&basemap_options(), &values(&[("pmtiles", json!(true))]));
        assert!(args.is_empty(), "got {args:?}");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        assert!(to_args(&basemap_options(), &values(&[("not_a_flag", json!(1))])).is_empty());
    }

    #[test]
    fn route_options_carry_the_fork_flags() {
        let defs = routes_options();
        for key in ["route_road_tolerance", "route_extent_digits", "route_symbol_id"] {
            assert!(find(&defs, key).is_some(), "missing {key}");
        }
        let args = to_args(&defs, &values(&[("route_extent_digits", json!(2))]));
        assert_eq!(args, vec!["--route_extent_digits=2"]);
    }

    #[test]
    fn every_key_is_unique_within_a_step() {
        for defs in [basemap_options(), routes_options()] {
            let mut keys: Vec<&str> = defs.iter().map(|d| d.key.as_str()).collect();
            let before = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), before, "duplicate option key");
        }
    }

    /// Operational flags: cairn supplies these itself, so a preset has no business setting them.
    const RUNNER_OWNED: &[&str] = &[
        "area",
        "mbtiles",
        "polygon",
        "jar",
        "download",
        "force",
        "tmpdir",
        "loginterval",
        "schema",
        // resolves into --polygon, which is itself runner-owned
        "area_poly",
    ];

    /// `0.70` in the doc and `0.7` from to_args are the same flag, so compare numbers as numbers.
    fn canonical(flag: &str) -> String {
        match flag.split_once('=') {
            Some((k, v)) => match v.parse::<f64>() {
                Ok(n) => format!("{k}={n}"),
                Err(_) => flag.to_string(),
            },
            None => flag.to_string(),
        }
    }

    /// Pull the flag set out of one of the reference pipeline's build command lines.
    fn readme_flags(marker: &str) -> Vec<String> {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/pipeline-reference.md"),
        )
        .expect("docs/pipeline-reference.md should be two levels above crates/core");
        // first match wins: the README gives an area build and a parent-area variant that differ
        // only by --skip_filled_tiles, and the area build is the one the presets mirror
        let lines: Vec<&str> = readme.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.contains(marker) && l.contains("PLANETILER_JAR"))
            .unwrap_or_else(|| {
                panic!("no build command containing `{marker}` in docs/pipeline-reference.md")
            });
        // commands continue onto following lines with a trailing backslash
        let mut line = String::new();
        for l in &lines[start..] {
            line.push_str(l.trim_end_matches('\\'));
            line.push(' ');
            if !l.trim_end().ends_with('\\') {
                break;
            }
        }
        let line = line.as_str();
        let mut flags: Vec<String> = line
            .split_whitespace()
            .filter(|t| t.starts_with('-') && !t.starts_with("-Xmx"))
            .map(|t| {
                let t = t.trim_start_matches('-');
                // a bare boolean flag in the README is the same as `=true` from to_args
                if t.contains('=') {
                    t.to_string()
                } else {
                    format!("{t}=true")
                }
            })
            .filter(|t| {
                let key = t.split('=').next().unwrap_or_default().replace('-', "_");
                !RUNNER_OWNED.contains(&key.as_str())
            })
            // the README quotes the empty languages value; to_args does not
            .map(|t| t.replace("=\"\"", "="))
            .map(|t| canonical(&t))
            .collect();
        flags.sort();
        flags
    }

    /// The presets exist to reproduce the reference pipeline's builds. Nothing enforced that, so the two drifted:
    /// the basemap preset was missing max-point-buffer, mlt-shared-dict, transportation_z13_paths,
    /// compact-db and transportation-name-limit-merge, and carried a simplify_tolerance the README
    /// never set. Missing max-point-buffer alone is tens of MB, because the place layer declares a
    /// 256px buffer.
    #[test]
    fn measured_presets_match_the_readme_build_commands() {
        for (marker, step, defs) in [
            ("${AREA}.mbtiles", crate::steps::StepId::Basemap, basemap_options()),
            ("_routes.mbtiles", crate::steps::StepId::Routes, routes_options()),
        ] {
            let preset = crate::presets::builtin()
                .into_iter()
                .find(|p| p.step == step && p.name == "measured")
                .expect("a `measured` preset for this step");
            let mut got: Vec<String> = to_args(&defs, &preset.values)
                .iter()
                .map(|a| canonical(a.trim_start_matches('-')))
                // the same runner-owned keys are dropped from both sides, or area_poly would
                // look like drift against the reference's explicit --polygon
                .filter(|t| {
                    let key = t.split('=').next().unwrap_or_default().replace('-', "_");
                    !RUNNER_OWNED.contains(&key.as_str())
                })
                .collect();
            got.sort();
            assert_eq!(
                got,
                readme_flags(marker),
                "`measured` preset for {step:?} has drifted from docs/pipeline-reference.md"
            );
        }
    }

    /// to_args silently ignores keys it does not recognise, so a preset naming an option that does
    /// not exist - a typo, or a flag removed from planetiler - would quietly render nothing at all.
    #[test]
    fn every_preset_key_is_a_real_option() {
        for preset in crate::presets::builtin() {
            let defs = match preset.step {
                crate::steps::StepId::Basemap => basemap_options(),
                crate::steps::StepId::Routes => routes_options(),
                _ => continue,
            };
            let known: Vec<&str> = defs.iter().map(|d| d.key.as_str()).collect();
            for key in preset.values.keys() {
                assert!(
                    known.contains(&key.as_str()),
                    "preset `{}` ({:?}) sets `{key}`, which is not an option for that step",
                    preset.name,
                    preset.step
                );
            }
        }
    }

    /// Every step with a form must have a schema behind it, and every step without one must
    /// have an empty schema rather than somebody else's.
    #[test]
    fn every_step_maps_to_its_own_schema() {
        use crate::steps::{StepId, ALL_STEPS};
        for step in ALL_STEPS {
            let defs = for_step(step);
            let expected_empty = matches!(
                step,
                StepId::DownloadOsm
                    | StepId::ElevationTiles
                    | StepId::ValhallaAdmins
                    | StepId::ValhallaTiles
            );
            assert_eq!(defs.is_empty(), expected_empty, "{step:?}");
        }
        assert_ne!(for_step(StepId::Bathymap).len(), 0);
    }

    /// The archive switch is offered exactly where the tool can honour it.
    #[test]
    fn the_archive_switch_is_offered_where_it_works() {
        use crate::steps::{archive, StepId, ALL_STEPS};
        for step in ALL_STEPS {
            let offered = find(&for_step(step), archive::PMTILES_KEY).is_some();
            assert_eq!(offered, archive::supports_pmtiles(step), "{step:?}");
        }
    }
}
