# Getting started

A first build, end to end. It uses a small area — Monaco — so each step finishes in minutes and the
files stay small. Swap in any [Geofabrik](https://download.geofabrik.de) area id (`rhone-alpes`,
`bavaria`, `alps`…) once it works.

## 1. Install

Download the app or the standalone `cairn` command line from the
[latest release](https://github.com/Akylas/cairn/releases/latest), or
[build from source](../README.md#building-from-source).

You need **Java 21+** for the basemap and routes steps. Check with `java -version`; the app can also
download a JRE for you from the Build tab. The planetiler jar is fetched on first use.

## 2. Pick a working directory

Everything Cairn reads and writes defaults from one directory (`--repo`, which is the current
directory unless you say otherwise):

```
output/<area>/     the finished archives
data/sources/      downloaded OSM extracts
elevation_tiles/   downloaded elevation tiles
valhalla_tiles/    the raw routing graph, shared between areas
```

```shell
mkdir maps && cd maps
```

Every one of those paths has its own flag (`--output-root`, `--data-dir`, …); `--repo` only supplies the
defaults.

## 3. Build

With the command line, one step at a time:

```shell
cairn download --area monaco        # the OSM extract
cairn basemap  --area monaco        # vector basemap  → output/monaco/monaco.mbtiles
cairn routes   --area monaco        # route relations → output/monaco/monaco_routes.mbtiles
```

Add `--dry-run` to a step to see what it would run without running it, and `cairn options basemap` to see
every option it takes.

For terrain and routing:

```shell
cairn elevation      --area monaco  # the .hgt tiles the graph and the terrain need
cairn terrain        --area monaco  # terrain-RGB
cairn contours       --area monaco  # contour lines, from the same elevation
cairn valhalla-tiles --area monaco  # the routing graph
cairn valhalla-package --area monaco  # → output/monaco/monaco.vtiles
```

`cairn state --area monaco` shows which outputs exist. A step is *built* when the files it produces are on
disk and non-empty; to make one run again, remove its output with `cairn state --area monaco clear <step>`.

**In the app**, open the **Build** tab, choose the area, tick the steps you want, and press *Run*. Steps carry
their dependencies, so ticking only *Valhalla package* also schedules the download, the elevation tiles and
the graph. The plan skips what is built; *Force* runs a step again.

## 4. Look at it

In the app, the **Map** tab opens the area's archives as stacked layers: click a feature to read its
properties, switch terrain between hillshade, raster and 3D, or place two waypoints in **Route** mode to route
on the package you just built.

From the command line:

```shell
cairn catalog --stats                                                # what is built, per area
cairn route --tiles output/monaco/monaco.vtiles --point 7.42,43.73 --point 7.43,43.74
cairn serve                                                          # tiles over HTTP on 127.0.0.1
```

The archives are ordinary files. Point `tileserver-gl`, QGIS or a MapLibre style at
`output/monaco/monaco.mbtiles` and it renders.

## 5. Make it yours

* **Another area** — change `--area`. Cairn resolves it through Geofabrik's index and downloads the matching
  `.poly` to clip to. To clip to a shape of your own, pass it as `--polygon my-region.poly` (basemap, routes), `--poly-shape` (terrain, contours) or `--poly` (package).
* **Smaller or richer tiles** — `cairn options basemap --presets`, then `-o name=value`. See
  [Tuning](../README.md#tuning).
* **Better elevation** — terrain is composed from the sources in [`config/sources.json`](../config/sources.json),
  lowest priority first, blending a better source in over its own coverage. Add your own national DEM there.
* **Anything planetiler can do** — everything after `--` is passed through untouched.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `No Java 21+ found` | install a JDK 21+, or use *Download JRE 21* on the Build tab |
| the basemap step says there is no jar | pass `--jar`, or set a jar URL in Settings; the default is the planetiler fork's `planetiler-latest` release |
| routing is greyed out in the map | there is no `.vtiles` for the area yet, or this build of Cairn was compiled without Valhalla |
| a step was skipped and you wanted it to run | it counts as built because its files exist — use *Force*, or `state clear` |

Still stuck? Open a [discussion](https://github.com/Akylas/cairn/discussions) or, for a bug,
an [issue](https://github.com/Akylas/cairn/issues/new/choose) with the step's log.
