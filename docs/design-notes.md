# Design notes — the desktop app plan

> Historical: the plan as written when the app was started, with the decisions it settled. Where
> the code has since moved on, [internals.md](internals.md) is current.


Replace the shell/Python pipeline with a desktop app that can run one, some, or all build
steps for any area, expose every step's options as a form, show progress, browse generated
output with sizes, and inspect/compare/validate the resulting tiles — including live routing
against the generated Valhalla package.

## Status

Milestones 1-4 are in, plus a second front end the plan did not call for: `cairn`, a command
line that replaces the shell scripts rather than driving the app. Every step listed in the graph
below runs from either front end.

Departures from the plan as written:

* **4.1** ships the `.hgt` path and the Lambert-93 inverse (hand-rolled, matching pyproj to
  1e-7). The GeoTIFF raster source reads BigTIFF/ZSTD/float-predictor, but compositing IGN RGE
  ALTI into a full run is not wired into the app's terrain step yet.
* **4.3** went further than a port: tile selection reads the `.poly` directly instead of going
  through a quadtree tilemask, so a package can be built for an area nothing has covered before.
* **Not started:** packaging and notarisation.

## Decisions settled

| question | decision | reason |
|---|---|---|
| UI framework | **Tauri 2** (Rust + system webview) | preview needs MapLibre GL JS |
| planetiler | **subprocess**, never embedded | `-Xmx32g` heap, single-use `Planetiler` instance, mmap cleanup on cancel, 89 MB classpath |
| JRE | **downloaded on first run** (Adoptium) | ships small; new planetiler jar without an app release |
| terrain RGB | **port to Rust**, in-process | best progress, removes rasterio/GDAL |
| valhalla routing | **link `libvalhalla.a` via `tyr::actor_t`** | no prime_server, no zmq, no HTTP server |
| valhalla tiles/elevation | **downloaded at runtime** | already proven in Massif Maps |
| valhalla package | **port to Rust** | last Python dependency |
| our schema | **stays Java** | `Route.java` needs OSM relations; custommap has none |
| other schemas | **importable** as prebuilt jar or YAML | decouples app from any one schema |
| everything else | **configurable** | output root, data dirs, tool paths, areas, presets |

### Why the schema stays Java

`planetiler-custommap` has no OSM relation support — no `OsmRelationInfo`, no
`preprocessOsmRelation`. The bundled shortbread sample admits the gap
(`shortbread.yml:347`: `# TODO get min admin level from relations`).

`Route.java` (418 lines) is built entirely on `RouteRelationData` / `OsmRelationInfo`.
Shortbread has no route layer at all — `route` appears once, as the tag value `route: ferry`.
YAML `tile_post_process` supports only `merge_line_strings` and `merge_polygons`, so
`Landcover`'s per-zoom `setPixelToleranceOverrides` and `Route`'s `finish()` symbol-table
writer have no declarative equivalent.

Divergence from upstream is 165 lines across 3 files (`Route` 135, `Landcover` 51,
`OmtLanguageUtils` 16). Cheaper to rebase than to port. And porting to YAML would not remove
the JRE — `--schema=foo.yml` still runs in the same JVM.

## Workspace model

An **area** is the unit of everything: generation, preview, comparison.

```
area = { name, poly_path, bbox?, presets }
```

Output layout, matching what exists today:

```
<output_root>/<area>/<area>[_<kind>].<ext>
  rhone-alpes.mbtiles           basemap (vector)
  rhone-alpes_routes.mbtiles    routes  (vector)
  rhone-alpes_terrain.mbtiles   terrain RGB (raster-dem, terrarium)
  rhone-alpes_hillshade.mbtiles hillshade (raster)
  rhone-alpes.vtiles            valhalla package (gph3)
```

Discovery scans `<output_root>` subdirectories, skipping dotfiles (`.DS_Store` is present).
Classification is by filename suffix, and **unrecognised variants are kept, not hidden** —
`rhone-alpes_mlt.mbtiles`, `rhone-alpes.vtiles.base`, `rhone-alpes_hillshade.mbtiles.old`
all exist today and are exactly the comparison candidates the compare UI needs.

UI: one tab per area, plus an all-areas overview. Generation targets the active area;
preview is scoped to it. Cross-area compare reuses the same swipe UI.

### Configurable settings

Output root, OSM/source data dir, tmp dir, elevation tiles dir, `sources.json` path,
JRE (auto / download / explicit path), planetiler source (bundled jar / custom jar / YAML
schema), valhalla binary dir, per-area poly files, per-step option presets.

## Architecture

```
cairn/
  src-tauri/
    src/
      main.rs
      settings/        # persisted config, area registry
      toolchain/       # locate + download JRE, planetiler jar, valhalla bins
      steps/
        mod.rs         # step graph, dependency resolution, run queue
        planetiler.rs  # subprocess + stdout progress parser
        terrain.rs     # native Rust terrarium encoder
        valhalla.rs    # subprocess build + native package writer
      tileserver/      # axum: /{area}/{artifact}/{z}/{x}/{y}
      elevation/       # terrarium sampling, profile computation
      routing/         # libvalhalla FFI shim, .vtiles unpack
      catalog/         # output walk, mbtiles stats
      progress.rs      # unified event type -> Tauri events
  src-cpp/
    valhalla_shim.cc   # actor_t::route(json) -> json
  src/                 # Svelte + Vite, MapLibre vendored (Tauri CSP blocks CDN)
```

### Unified progress event

```rust
enum StepEvent {
  Started  { step: StepId, area: AreaId },
  Phase    { step: StepId, name: String },          // "osm_pass1", "z12", ...
  Progress { step: StepId, done: u64, total: u64 },
  Log      { step: StepId, line: String },
  Finished { step: StepId, ok: bool, outputs: Vec<PathBuf> },
}
```

Planetiler progress is parsed from stdout. Format (`crates/core/tests/fixtures/planetiler.log`, ANSI stripped):

```
0:00:02 INF [lake_centerlines] -  read: [   22 100%   53/s ] write: [    0    0/s ] 0
```

Stage name from `[...]`, percent from the counter block. Parser must be tolerant — unmatched
lines become `Log`, never fail a build on a format change.

### Step graph

```
download_osm ──┬── basemap
               ├── routes
               └── valhalla_tiles ── valhalla_package
elevation_tiles ─┘

sources.json ──┬── terrain_rgb
               └── hillshade
```

---

## Milestone 1 — Foundation

### 1.1 Spike
Tauri 2 scaffold, Svelte frontend. Toolchain module: detect system Java 21+, else download an
Adoptium JRE for host platform/arch. Run the basemap build as a subprocess with the current
flag set, parse progress, render one bar.

*Done when:* basemap builds from the GUI with a live progress bar, on a machine with no system Java.

### 1.2 Workspace
Settings store. Configurable output root. Area discovery and registration. Tab shell.

### 1.3 Output catalog
Per area: file list with sizes. Per mbtiles: `metadata` table, per-zoom tile count and byte
totals. Side-by-side diff of two builds with deltas. Replaces the ad-hoc `tools/bench/` scripts.

---

## Milestone 2 — Preview

The core value. Everything here reads existing files; no generation needed.

### 2.1 Tile server

axum, `GET /{area}/{artifact}/{z}/{x}/{y}`, reading straight from the mbtiles. Handles the
gzip content-encoding and the TMS row flip (`y = 2^z - 1 - row`). Serves four kinds:

| artifact | MapLibre source | notes |
|---|---|---|
| basemap | `vector` | MVT and MLT both — MLT is supported by MapLibre GL JS |
| routes | `vector` | overlay on basemap to check alignment |
| terrain RGB | `raster-dem` | `encoding: "terrarium"`, `tileSize: 512` |
| hillshade | `raster` | pre-baked |

Terrain previews as a `hillshade` layer with no extra work, since MapLibre decodes terrarium
natively. Also drives 3D terrain if wanted.

### 2.2 Inspect mode

`queryRenderedFeatures` on click: layer name plus every property. This is the direct check on
the attribute work — `name_int` absence, `extent` decimal count, symbol ids, landcover
`subclass` fallback.

### 2.3 Back style

An optional layer *underneath* the inspected source, for comparison. Sources:

- raster XYZ (IGN, OSM, satellite)
- another build of the same area (`_mlt`, `.base`, `.old`)
- the same artifact from another area
- a different vector style over the same tiles

Swipe divider plus opacity slider. Locked zoom/center between the two.

### 2.4 Elevation profile

Input: a drawn line, an imported GPX, or a feature picked from the routes mbtiles.
Sample the terrain mbtiles along it — decode terrarium as `(R*256 + G + B/256) - 32768`,
bilinear between pixels, at a selectable zoom.

Doubles as QA for the terrain build: the `round_digits` ramp shows up as visible stair-stepping
in the profile, so the vertical quantization schedule can be judged directly instead of by
eyeballing hillshade.

---

## Milestone 3 — Generation

### 3.1 Step runner
Run one / some / all, respecting the dependency graph, scoped to the active area. Cancel kills
the subprocess and cleans `data/tmp` (two concurrent planetiler runs sharing tmp already
destroyed each other's sort chunks once — the runner must serialise or isolate tmp per run).

### 3.2 Options forms
Per-step JSON option schema driving a generated form. Planetiler's surface includes our custom
flags — `route_road_tolerance`, `route_extent_digits`, `route_symbol_id`,
`landcover_tolerance_z11_13`, `landcover_drop_redundant_subclass`, `landcover_merge_maxzoom` —
plus stock `--simplify-tolerance*` and `--min-feature-size*`.

Named presets saved to disk. This makes the current "bench variant" workflow first-class.

### 3.3 Schema picker

Verified subcommands in the fork ([Main.java:56-67](../planetiler/planetiler-dist/src/main/java/com/onthegomap/planetiler/Main.java)):

```bash
java -jar planetiler.jar openmaptiles --mbtiles=out.mbtiles
```
```bash
java -jar planetiler.jar custom --schema=path/to/schema.yml
```
```bash
java -jar planetiler.jar verify-schema --schema=path/to/schema.yml
```

Import a **prebuilt jar**, not a source tree — compiling needs a full JDK plus Maven, replacing
the ~50 MB JRE with a ~200 MB toolchain. YAML import needs no toolchain, hot-reloads on save,
and validates with `verify-schema` before running.

---

## Milestone 4 — Native ports and routing

### 4.1 Terrain RGB to Rust

Port `scripts/build_terrain_rgb.py` (877 lines). Highest technical risk in the plan.

- read `sources.json`, composite per macrotile
- reproject EPSG:2154 (Lambert-93) to EPSG:3857, bilinear/cubic resample
- terrarium encode, lossless WebP, write mbtiles via `rusqlite`
- preserve the per-zoom `round_digits` ramp and the `--max-round-digits` trap
  (`if max_round_digits < round_digits: max_round_digits = round_digits`)

Reprojection is the risk: either bind `libproj` (adds back a native dep) or hand-roll the
Lambert Conformal Conic inverse — well-specified, small, dependency-free.

*Acceptance:* per-tile pixel diff against the Python output. Keep the Python path until parity
holds on a full rhone-alpes z5–13 run.

### 4.2 Valhalla routing preview

Link `libvalhalla.a` (30.2 MB, already built) through a small C++ shim exposing
`valhalla::tyr::actor_t::route(json) -> json`. This is the embedding path the mobile bindings
use. No `valhalla_service`, no prime_server, no zmq.

Tile source, two modes:

1. `mjolnir.tile_dir` pointed at `valhalla_tiles/`
2. **unpack the generated `.vtiles` to a temp tile dir and route against that** — validates the
   actual shipped deliverable, not an intermediate

`.vtiles` uses the mbtiles schema exactly — `zoom_level, tile_column, tile_row, tile_data`,
`format=gph3`, gzipped gph blobs, `zoom_level` being the Valhalla hierarchy level. Unpacking is
a short reversal of `build_valhalla_package.py`.

UI: click origin/destination on the map, pick a costing profile, render the route geometry plus
the maneuver list with turn markers. Combine with 4.1's profile sampler to show the elevation
profile of a computed route — which is also the check on grade-aware routing.

### 4.3 Valhalla package to Rust

Port `scripts/build_valhalla_package.py`, keeping the zopfli path and `PRAGMA page_size=4096`
(measured −3.03%: 212.4 MiB vs 219.0 MiB). Directory walk, gzip, sqlite insert.

*Done when:* no Python remains and the 238 MB venv can be deleted.

### 4.4 Packaging

Bundle only the valhalla binaries actually needed (`build_tiles`, `build_config`, `build_admins`,
`build_timezones`, `add_elevation`) as Tauri sidecars, plus the routing shim. macOS: dylib
relocation, codesign, notarize. First-run flow downloads JRE, planetiler jar, elevation tiles.

---

## Risks

| risk | severity | mitigation |
|---|---|---|
| Rust raster reprojection/resampling parity | high | pixel-diff harness vs Python; keep Python until parity |
| `libvalhalla.a` FFI + static-link size/ABI | medium | shim is small and JSON-in/JSON-out; fall back to `valhalla_service` + prime_server if linking fights back |
| macOS notarization of bundled binaries | medium | codesign each sidecar, hardened runtime |
| planetiler log format drift on upstream merges | low | tolerant parser |

## Out of scope

- Windows (Valhalla support is poor)
- tippecanoe (was contours-only; contours are now traced natively, see `crates/core/src/contours`)
- prime_server (avoided by embedding `libvalhalla`)

## Repo issues to fix along the way

- `routeRelationDatas` in `Route.java:100` is a plain `HashMap` written from `processAllOsm`
  across 17 worker threads — data race
- `env.sh` / `scripts/buildAll.sh` reference jar versions 0.5 and 0.7; actual is 0.10.3
- `setup.sh` clones an unversioned `rio-rgbify` (moot once terrain is Rust)
- `--skip_filled_tiles` never evaluated
