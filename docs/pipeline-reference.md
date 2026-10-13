# Pipeline reference — running the tools by hand

This is the pipeline as a sequence of raw tool invocations. It is the reference for what each
step actually does, and the place to look when a `cairn` run needs explaining or a step has to be
driven manually.

For normal use see the [README](../README.md): `cairn` runs these same steps with the same flags,
and the option defaults in the app and CLI are checked against the commands on this page by
tests, so the two cannot drift apart.

> Note: `build_hillshades.sh` is gone. Shaded relief is drawn from the terrain-RGB tiles, which
> are the same renderer with mapbox packing.

## prepare

These are the commands the original shell and Python pipeline ran. They still work; the scripts
live in [`tools/legacy/`](../tools/legacy/). Run `tools/legacy/setup.sh` once to build the tools,
then `source tools/legacy/env.sh` to put them on `PATH`.


## Building

First download the poly of the area  you want from geofabrick or find the bounds you want

* polyzoom: zoom used to compute wanted tiles from poly-shape. The bigger the slower to compute but also the more defined is your zone
* elevation_tiles: folder where to store elevation_tiles use by valhalla and to generate tiffs


First we need the poly / pbf from that region

```shell
export AREA=italy
python ./tools/legacy/scripts/download-osm.py --poly $AREA
export POLY=$AREA.poly
java -jar $PLANETILER_JAR  --only-download --area=$AREA
valhalla_build_config --mjolnir-tile-dir ${PWD}/valhalla_tiles --mjolnir-tile-extract ${PWD}/valhalla_tiles.tar --mjolnir-timezone ${PWD}/valhalla_tiles/timezones.sqlite --mjolnir-admin ${PWD}/valhalla_tiles/admins.sqlite --additional-data-elevation ${PWD}/elevation_tiles > valhalla.json 
```

## # First generate the mbtiles with Planetiler
You can change the languages parameter to your need ( like `en,fr`)

There you have multiple choices. Either build only using the area you want. But you will end up with half-filled tiles on area bounds
```shell
java -Xmx32g -jar $PLANETILER_JAR  --download --area=${AREA} --languages="" --force --compact-db --transportation-name-limit-merge -exclude_layers=route --nodemap-type=sparsearray --mbtiles=${OUTPUT_DIR}/${AREA}/${AREA}.mbtiles --polygon=$POLY --max-point-buffer=4  --transportation_z13_paths --mlt-shared-dict --parallel-tmp-io --simplify-tolerance=0.70 --simplify-tolerance-at-max-zoom=0.25 --min-feature-size-at-max-zoom=0.25 --landcover_tolerance_z11_13=1.05 --landcover_drop_redundant_subclass=true --landcover_merge_maxzoom=true --drop_redundant_name_int=true --transportation_surface_detail=true --water_pool_tolerance=1 --poi_custom_ranks=true
```
Or build using a "parent" area. For example i will always use europe as i mostly build europe countries

```shell
java -Xmx32g -jar $PLANETILER_JAR  --download --area=europe --languages="" --force --compact-db --transportation-name-limit-merge -exclude_layers=route --nodemap-type=sparsearray --mbtiles=${OUTPUT_DIR}/${AREA}/${AREA}.mbtiles --polygon=$POLY --max-point-buffer=4 --transportation_z13_paths --mlt-shared-dict --parallel-tmp-io --simplify-tolerance=0.70 --simplify-tolerance-at-max-zoom=0.25 --min-feature-size-at-max-zoom=0.25 --landcover_tolerance_z11_13=1.05 --landcover_drop_redundant_subclass=true --landcover_merge_maxzoom=true --drop_redundant_name_int=true --transportation_surface_detail=true --water_pool_tolerance=1 --poi_custom_ranks=true --skip_filled_tiles
```

If you want to generate low level world map:
```shell
java -Xmx32g -jar $PLANETILER_JAR  --download --area=planet --languages=fr,en --force --transportation-name-limit-merge --compact-db --only_layers=place,park,boundary,mountain_peak,transportation,transportation_name,water,waterway,water_name,landcover,landcover_name,landuse --maxzoom=7 --nodemap-type=sparsearray --mbtiles=${OUTPUT_DIR}/world.mbtiles --max-point-buffer=4 --mlt-shared-dict --parallel-tmp-io --simplify-tolerance-at-max-zoom=0.25 --min-feature-size-at-max-zoom=0.25 --landcover_tolerance_z11_13=1.05 --landcover_drop_redundant_subclass=true --landcover_merge_maxzoom=true --drop_redundant_name_int=true --skip_filled_tiles


## # Generate routes mbtiles

```shell
java -Xmx32g -jar $PLANETILER_JAR  --download --area=${AREA} --languages="" --force --compact-db --transportation-name-limit-merge -only_layers=route --nodemap-type=sparsearray --mbtiles=${OUTPUT_DIR}/${AREA}/${AREA}_routes.mbtiles --polygon=$POLY --max-point-buffer=4 --mlt-shared-dict --parallel-tmp-io --route_road_tolerance=true --route_extent_digits=2 --route_symbol_id=true \
--simplify-tolerance-at-max-zoom=0.25 --min-feature-size-at-max-zoom=0.25
```

## # Generate area tif

Now generate the tif of the area you want. It is best to use polyzoom as the min zoom you want for hillshades / contours. This ensure you wont get half filled tiles. Though the process will be slower and the tif bigger
We use a small polyzoom (5) to avoid half filled tiles. You can raise it for faster build but you ll have half filled tiles in hillshades or contours (mostly hillshade as contours is starting at zoom 11 here)
If you want to have more defined tif over (that you would get from another source) you can use the `--overTif` option. It should do it all for you

```shell
./tools/legacy/scripts/generate_tif_from_hgt.sh --poly-shape $POLY --polyzoom 5 --elevation_tiles ./elevation_tiles --output ${AREA}.tif
```

All intermediates are VRT (no pixel written), so the only file created is the output itself.

`--overTif` only contributes **resolution**, never its own projection: the output always stays on the hgt
grid (`EPSG:4326`, same bounds, same `-10` nodata as without `--overTif`), just refined by an integer factor
so the finer pixels survive and both layers land on the exact same grid. A pixel comes from `overTif` where
it has data, from the hgt everywhere else.
That refined grid gets large (rhone-alpes at the IGN 5m resolution is ~29 gigapixel), so the output is
written tiled and `DEFLATE` compressed by default:

| flag | effect |
| --- | --- |
| `--refine <N>` | refine the hgt grid N times instead of the automatic factor (5 for IGN RGE ALTI 5m). `--refine 3` is ~3x smaller and still finer than a z14 tile |
| `--blur <METERS>` | width of the cross fade along the edge of the `overTif` coverage (default 1000, `--blur 0` disables). The two sources disagree there (different resolution, and EGM96 vs NGF-IGN69 vertical datum) and a hard edge shows up as a ridge in the hillshade. Costs ~3x on the final pass |
| `--compress <ALG>` | `LZW`, `DEFLATE` (default), `ZSTD`, ... |
| `--no-compress` | raw output, only if you have the disk for it |
| `--int16` | store elevation as `Int16` (1m resolution) instead of `Float32`. ~7x smaller, but quantizes the sub-meter precision of a 5m source like IGN RGE ALTI |

`GDAL_CACHEMAX=8192` in the environment raises the GDAL block cache (default 2048MB) if you want to throw
more RAM at it. Add overviews afterwards if you intend to open the result in QGIS:
```shell
gdaladdo -r average --config COMPRESS_OVERVIEW DEFLATE ${AREA}.tif
```

## # Terrain RGB tiles (single step, no intermediate tif)

`tools/legacy/scripts/build_terrain_rgb.py` goes straight from the elevation sources to an mbtiles,
mapterhorn style: the max zoom is warped once per macrotile and every lower zoom is a
2x2 average of the tiles already produced, instead of one full pass over the dem per
zoom. No `${AREA}.tif` is built, so the whole `generate_tif_from_hgt.sh` step and its
disk cost disappear when you only want terrain tiles.

Sources are listed lowest priority first, and each one fades into the previous over
`--blur` metres so a better source overrides a coarser one without a step along the
edge of its coverage. `sources.json`:

```json
[
  {"name": "mapterhorn", "type": "xyz", "url": "https://tiles.mapterhorn.com/{z}/{x}/{y}.webp",
   "encoding": "terrarium", "maxzoom": 12, "clamp_min": -10},
  {"name": "ignrge5", "type": "raster", "path": "work_france_ign/out/*.tif"}
]
```

| `type` | meaning |
| --- | --- |
| `valhalla` | the `.hgt` already downloaded for valhalla. Missing tiles are fetched with `valhalla_build_elevation` unless `"download": false`. 1 arc-second, and they carry bathymetry, hence `clamp_min` |
| `raster` | local file, glob or list of globs, in any CRS |
| `xyz` | **cairn only.** A terrain-RGB tile endpoint, `url` being a `{z}/{x}/{y}` template |
| `pmtiles` | **cairn only.** A PMTiles archive of terrain-RGB tiles: `path` for a local one, `url` for one read over HTTP by byte range |
| `mapterhorn` | **python only.** A [mapterhorn source-catalog](https://github.com/mapterhorn/mapterhorn/tree/main/source-catalog) source of raw DEM deliveries - not the tile pyramid above. `path` is a catalog name, a url to a `file_list.txt`, or a local one |

`xyz` and `pmtiles` read an already-built terrain pyramid rather than a DEM, which is what
[mapterhorn](https://mapterhorn.com/data-access/) publishes: terrarium-encoded webp, 512 px,
z0-z12 worldwide, from Copernicus GLO-30 and national lidar where there is any. It replaces
the `.hgt` fallback - same coverage, better data, nothing to download first. The zoom read is
chosen from the output tile's ground resolution, so a z7 tile never decodes z12 tiles.

The same archive can be read either way. These two are interchangeable, and produce
byte-identical output:

```json
{"name": "mapterhorn", "type": "xyz", "url": "https://tiles.mapterhorn.com/{z}/{x}/{y}.webp"}
{"name": "mapterhorn", "type": "pmtiles", "url": "https://download.mapterhorn.com/planet.pmtiles"}
```

The 355 GB planet archive is *not* downloaded: only its directories and the tiles the render
actually samples are transferred. A local `.pmtiles` works the same way, with `path` - which is
how the regional z13-z17 packages (`6-33-22.pmtiles`) are read.

Fetched tiles are cached under `.cache/terrain-tiles/<name>/` beside `sources.json`, keyed by
`z/x/y`, with an empty `.absent` file where the source has no tile. A rebuild of the same area
makes no requests at all; delete the directory to force a refetch. `"cache_dir"` moves it.
Nothing is read from a public endpoint twice, which matters because these are free services -
one alpine build at z5-z12 is a few thousand tiles, and re-fetching that on every run is how a
pipeline gets blocked.

### Reading mapterhorn: endpoint, remote archive or local extract

The three ways of reading mapterhorn cost very different amounts of waiting:

* **`xyz`** asks for every tile separately, the sea included: a tile that does not exist is a
  request that comes back 404, and over a continent most of the requests are those. A build
  prefetches the next batch of tiles with `concurrency` requests in flight (24 unless the source
  says otherwise), which is what makes it bearable, but it is still a request per tile.
* **`pmtiles` with a `url`** reads the planet archive by byte range. Its directory says which tiles
  exist, so a tile that does not exist costs no request. A range request takes as long as a tile
  request, so this is faster only by the requests it saves. It covers z0-z12.
* **`pmtiles` with a `path`** reads a file on this disk, with no network at all and nothing copied
  into the tile cache. For anything larger than one region this is the way: extract yours once with
  [go-pmtiles](https://github.com/protomaps/go-pmtiles) (`--dry-run` first, for the size) and point
  the source at it:

  ```shell
  pmtiles extract --bbox=<west>,<south>,<east>,<north> \
      https://download.mapterhorn.com/planet.pmtiles mapterhorn.pmtiles
  ```

A 256-pixel output reads the zoom *below* its own, because mapterhorn's tiles are 512 pixels: a z13
tile at `--tile-size 256` is read from z12, so the planet archive covers a z13 build. A 512-pixel z13
tile needs z13, which lives in mapterhorn's separate regional `6-x-y.pmtiles` files.

The sources are listed in `sources.json` and edited under Settings, in the app.

### Interrupting and resuming

A terrain build can be stopped - Ctrl-C, the Cancel button, a crash, a closed laptop - and continued.
It writes its progress as it goes: PMTiles keeps a journal beside its scratch file
(`<name>.pmtiles.spill` and `<name>.pmtiles.resume`), MBTiles builds into `<name>.mbtiles.partial`. The
finished archive is only put in place once it is complete, so an unfinished one is never mistaken for
a built one.

Running the same command again continues from the last finished batch of 256 tiles. It does so only
when everything that decides what the tiles look like is the same: the options, the bounds and clip
shape, `sources.json`, the tile buffer. A partial build started with different settings is refused,
and left alone, rather than finished with a mixture. `--fresh` (the "Start over" option in the app)
discards it and begins again.

The same applies across a change of cairn itself: when a release changes how tiles are rendered, a
partial build from the older one is refused for the same reason.

Both are cairn's, not `build_terrain_rgb.py`'s: the python script fails on a `type` it does not
know.

A `mapterhorn` source downloads what the area needs and extracts the archives (`zip`,
`tar`, `7z`, including the split `.7z.001/.002` deliveries, which need the `7z` cli).
The extracted folder is what gets checked first, so the archives can be deleted once
extracted without triggering a new download.

```json
{
  "name": "frrgealti1m", "type": "mapterhorn",
  "path": "https://raw.githubusercontent.com/mapterhorn/mapterhorn/main/source-catalog/frrgealti1metro/file_list.txt",
  "cache": "sources/frrgealti1m", "crs": "EPSG:2154", "allow_full_download": true
}
```

When the filenames carry their coordinates (`glo30`) only the intersecting tiles are
fetched. When they do not (the departement wide `.7z` of RGE ALTI) the area cannot be
known before downloading, so `allow_full_download` has to be set, and the rasters are
filtered on their real bounds once extracted. `crs` assigns a projection to deliveries
that ship without one, like the `.asc` of RGE ALTI.

```shell
python tools/legacy/scripts/build_terrain_rgb.py --sources sources.json --poly-shape $POLY \
  --minzoom 5 --maxzoom 12 --round-digits 0 --encoding mapbox -f webp \
  --blur 1000 --tile-buffer 1 -j 16 -o ${OUTPUT_DIR}/${AREA}/${AREA}_terrain.mbtiles
```

`--encoding terrarium` writes the same tiles with the terrarium base/interval.
Keep `--round-digits 0` for 3d terrain, the quantization is a hillshade tradeoff.
`--max-round-digits` is what makes the quantization grow as the zoom drops, leave it
alone and every zoom quantizes like the max one.

`--tile-buffer 1` writes a ring of tiles around the poly, for 3d terrain renderers:
they backfill the 1px border of a dem tile from its neighbours, and a missing
neighbour is a seam at the edge of the covered area. The ring is a perimeter, so it
costs ~19% more tiles at the max zoom but a lot more at the low zooms (26 -> 52 tiles
at z9 for rhone-alpes), about 28% more bytes overall. Leave it at 0 for shaded relief and
contours. The extra tiles needed to average a zoom from its 4 children are worked out
separately and never written.

## # Then build contours
```shell
./tools/legacy/scripts/build_contours.sh  --minzoom 11 --maxzoom 14 --poly-shape $POLY --output ${OUTPUT_DIR}/${AREA}/${AREA}_contours.mbtiles ${AREA}.tif
```

## # Cleanup contours mbtiles
This steps ensure we have the same tiles in the area mbtiles and terrain/contours. It is important in AlpiMaps as we merge tiles from area and contours to draw contour lines in between the map style. 
```shell
python ./tools/legacy/scripts/filter_tiles_from_other_mbtiles.py --sourcembtiles ${OUTPUT_DIR}/${AREA}/${AREA}.mbtiles ${OUTPUT_DIR}/${AREA}/${AREA}_contours.mbtiles
```
## # build valhalla package

### Download the elevation tiles

`valhalla_build_tiles` bakes elevation into the graph during its `elevation` stage, so the tiles listed in
`additional_data.elevation` of `valhalla.json` (`./elevation_tiles`) must be downloaded **before** building the tiles.
The same folder is reused later by `generate_tif_from_hgt.sh`, so use `-d` (decompressed `.hgt`) here.

Compute the bounds of your poly, then download from Tilezen:
```shell
export BOUNDS=$(python ./tools/legacy/scripts/get_shape_bounds_tile_envelope.py --poly-shape $POLY --minzoom 5 --maxzoom 5)
valhalla_build_elevation -v -d -b $BOUNDS -o ./elevation_tiles
```
Use the same `--minzoom/--maxzoom` value you intend to pass as `--polyzoom` to `generate_tif_from_hgt.sh` (5 here),
so both steps cover the same area and no tile gets downloaded twice.

If you build the valhalla graph for a parent area (see below), download the elevation for that parent area instead,
either with an explicit bbox (`-b '{w},{s},{e},{n}'`) or, once a graph already exists, straight from it:
```shell
valhalla_build_elevation -v -d -t -c valhalla.json
```

### Build the tiles

first build valhalla tiles if you didnt already. In my case i build valhalla tiles for the whole europe to ensure i have all tiles to calculate routes
between europe packages. So i only build valhalla tiles once . So here for my case i replace `$AREA` with `europe`
```shell
valhalla_build_tiles -c valhalla.json data/sources/${AREA//-/_}.osm.pbf
```
Then build valhalla "mbtiles package
```shell
python ./tools/legacy/scripts/build_valhalla_package.py --id $AREA --poly $AREA.poly --polymaxzoom=11 valhalla_tiles ${OUTPUT_DIR}/${AREA}/${AREA}.vtiles
```
