<script>
  import { invoke } from "./api.js";
  import { onMount, onDestroy, tick } from "svelte";
  import { Map as MapLibreMap, NavigationControl, ScaleControl } from "maplibre-gl";
  import Compare from "@maplibre/maplibre-gl-compare";
  import "maplibre-gl/dist/maplibre-gl.css";
  import "@maplibre/maplibre-gl-compare/dist/maplibre-gl-compare.css";
  import {
    makeSource, addSourceToMap, removeSourceFromMap, applyVisibility, applyOpacity,
    applyOrder, boundsOf, baseStyle, BACKDROPS, setAllLayers,
  } from "./sources.js";
  import { tileGrid, pointToTile, fetchTileJson } from "./tiledebug.js";
  import SourcePanel from "./SourcePanel.svelte";
  import StyleEditor from "./StyleEditor.svelte";
  import Profile from "./Profile.svelte";
  import { COSTING_MODELS, readTrip, tripToGeoJson, formatDuration } from "./valhalla.js";
  import { MAP_MODES, TERRAIN_MODE_HELP } from "./modes.js";

  /// Which area to open on. The Build tab hands over the one it has just finished, so the map
  /// shows what was actually built rather than whichever area sorts first.
  let { area: wantedArea = "" } = $props();

  let base = $state("");
  let areas = $state([]);
  let areaName = $state("");
  let backdrop = $state("");
  let error = $state("");
  let geomFilter = $state("all");
  let comparing = $state(false);
  let leftOpen = $state(true);
  let rightOpen = $state(false);
  let showTileGrid = $state(false);
  let drawerOpen = $state(true);
  let showHelp = $state(false);
  let view = $state({ lng: 5.7, lat: 45.4, zoom: 8 });
  let styleApplied = $state(false);

  let mainSources = $state([]);
  let secondarySources = $state([]);

  let mode = $state("inspect");
  let drawn = $state([]);
  let profile = $state(null);
  let profileBusy = $state(false);
  let inspected = $state(null);
  let tileDump = $state(null);

  let valhallaBuilt = $state(false);
  let routingInfo = $state(null);
  let routePackage = $state("");
  let costing = $state("pedestrian");
  let trip = $state(null);
  let routeBusy = $state(false);

  let containerEl, mainEl, secondaryEl, shellEl;
  let mainMap = null, secondaryMap = null, compare = null, resizeObserver = null;

  let area = $derived(areas.find((a) => a.name === areaName));
  let artifacts = $derived(area?.artifacts ?? []);
  let renderable = $derived(artifacts.filter((a) => !a.probe_error && a.kind !== "valhalla_package"));

  function canonicalFirst(list) {
    return [...list].sort((a, b) => (a.variant ? 1 : 0) - (b.variant ? 1 : 0));
  }
  let terrainArt = $derived(canonicalFirst(renderable.filter((a) => a.kind === "terrain_rgb"))[0]);

  // Availability follows the data, not just the build. A build with Valhalla linked still cannot
  // route an area that has no routing package, and offering the mode anyway only produces an
  // error after the user has placed two waypoints.
  let packages = $derived(canonicalFirst(artifacts.filter((a) => a.kind === "valhalla_package")));
  let hasRoutingTiles = $derived(packages.length > 0);
  let canRoute = $derived(valhallaBuilt && hasRoutingTiles);
  let canProfile = $derived(!!terrainArt);

  // matched on area *and* file, the way a source's own id is built: two areas can hold files of
  // the same name, and matching on the name alone would call one of them already loaded
  const loaded = (list, artifact) =>
    list.some((s) => s.area === artifact.area && s.file === artifact.file_name);
  let addableMain = $derived(renderable.filter((a) => !loaded(mainSources, a)));
  let addableSecondary = $derived(renderable.filter((a) => !loaded(secondarySources, a)));

  function waitForBox(el) {
    return new Promise((resolve) => {
      if (el?.clientWidth && el?.clientHeight) return resolve();
      const ro = new ResizeObserver(() => {
        if (el.clientWidth && el.clientHeight) { ro.disconnect(); resolve(); }
      });
      ro.observe(el);
    });
  }

  /**
   * Run `fn` once the style is loaded, however long that takes.
   *
   * The `styledata` event alone is not enough - it can fire before the style is actually
   * complete - so this polls as well. There is deliberately no short deadline: a window that is
   * hidden or occluded gets no animation frames, so the style can take far longer than any
   * timeout worth setting, and giving up leaves the layer panel empty with nothing to retry it.
   * The poll stops when the map is torn down.
   */
  function whenStyleReady(map, fn) {
    if (!map) return;
    if (map.isStyleLoaded()) { fn(); return; }
    let done = false;
    const stop = () => { done = true; clearInterval(timer); map.off("styledata", attempt); };
    const attempt = () => {
      if (done) return;
      if (map._removed) { stop(); return; }
      let ready = false;
      try { ready = map.isStyleLoaded(); } catch { stop(); return; }
      if (!ready) return;
      stop();
      fn();
    };
    const timer = setInterval(attempt, 150);
    map.on("styledata", attempt);
  }

  onMount(async () => {
    try {
      base = await invoke("start_tiles");
      areas = await invoke("list_areas");
      areaName = areas.find((a) => a.name === wantedArea)?.name ?? areas[0]?.name ?? "";
      routingInfo = await invoke("routing_status");
      valhallaBuilt = routingInfo?.available ?? false;
    } catch (err) {
      error = String(err);
      return;
    }

    await waitForBox(containerEl);
    mainMap = newMap(mainEl);
    secondaryMap = newMap(secondaryEl);
    mainMap.on("click", onMapClick);
    mainMap.on("moveend", refreshTileGrid);
    mainMap.on("move", () => {
      const c = mainMap.getCenter();
      view = { lng: c.lng, lat: c.lat, zoom: mainMap.getZoom() };
    });

    // The map has to follow the window, not a fixed height: the shell is flex and the panels
    // collapse, so the container changes size without the window ever being resized.
    resizeObserver = new ResizeObserver(scheduleResize);
    resizeObserver.observe(containerEl);
    if (import.meta.env.DEV) { window.__main = mainMap; window.__secondary = secondaryMap; }

    await addDefaults();
  });

  onDestroy(() => {
    resizeObserver?.disconnect();
    compare?.remove();
    mainMap?.remove();
    secondaryMap?.remove();
  });

  function newMap(container) {
    const map = new MapLibreMap({
      container, style: baseStyle(backdrop), center: [5.7, 45.4], zoom: 8,
      attributionControl: { compact: true }, maxPitch: 85,
    });
    map.addControl(new NavigationControl({ visualizePitch: true }), "top-right");
    map.addControl(new ScaleControl({ unit: "metric" }), "bottom-left");
    return map;
  }

  /**
   * Release 3D terrain before replacing a style.
   *
   * `setStyle` removes every source, including the DEM the terrain points at. Left attached, the
   * new style never finishes loading and the map wedges silently - no error, no tiles, and
   * every later call fails with "Style is not done loading".
   */
  function releaseTerrain(map) {
    if (!map) return;
    // deliberately not gated on isStyleLoaded: the moment this matters most is mid-load, which
    // is exactly when that check would skip it and leave the map to wedge
    try {
      map.setTerrain(null);
    } catch {}
  }

  /**
   * Coalesce resizes to one per frame.
   *
   * A drag of the window edge delivers a stream of ResizeObserver callbacks, and every
   * `resize()` reallocates the GL drawing buffer and clears it - the blink. Even one per frame
   * blinks, because the clear and the repaint are not the same frame.
   *
   * So the resize waits for the drag to settle, and the CSS below stretches the last good frame
   * to the new box in the meantime. The map is briefly scaled rather than blank, and it snaps
   * back to a crisp render once the pointer stops.
   */
  let resizeTimer = null;
  function scheduleResize() {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      mainMap?.resize();
      secondaryMap?.resize();
    }, 140);
  }

  /**
   * Resize both maps after a layout change.
   *
   * The ResizeObserver covers window resizes, but its callbacks are delivered as part of the
   * frame lifecycle - so a collapse that happens while the window is hidden or occluded leaves
   * the canvas at its old size until something else forces a frame. Calling it directly makes
   * the panel toggles deterministic.
   */
  async function relayout() {
    await tick();
    mainMap?.resize();
    secondaryMap?.resize();
  }

  const mapFor = (w) => (w === "secondary" ? secondaryMap : mainMap);
  const listFor = (w) => (w === "secondary" ? secondarySources : mainSources);
  const which = (source) => (mainSources.includes(source) ? "main" : "secondary");

  async function addDefaults() {
    const basemap = canonicalFirst(renderable.filter((a) => a.kind === "basemap"))[0];
    // routes are a first-class output of this pipeline, not an extra: show them with the map
    const routes = canonicalFirst(renderable.filter((a) => a.kind === "routes"))[0];
    if (terrainArt) await addArtifact(terrainArt, "main");
    if (basemap) await addArtifact(basemap, "main");
    // contours are a vector overlay of this pipeline too: a build that just made them is shown
    // with them, above the basemap and under the routes
    const contours = canonicalFirst(renderable.filter((a) => a.kind === "contours"))[0];
    if (contours) await addArtifact(contours, "main");
    if (routes) await addArtifact(routes, "main");
    routePackage = packages[0]?.file_name ?? "";
  }

  /** Resolve once a map's style has finished loading, so a reset can be awaited. */
  function styleReady(map) {
    return new Promise((resolve) => (map ? whenStyleReady(map, resolve) : resolve()));
  }

  /**
   * Open a different area.
   *
   * Nothing of the previous area carries over: different archives, different bounds, a different
   * routing package. Only `addDefaults` ever loaded anything and it ran once, at mount - so
   * changing the area repointed the menus and the availability flags while the map went on
   * showing the area that happened to be open at startup. Everything that reads a loaded source
   * read the wrong one with it, the style editor included, which is what made local archives
   * untestable for every area but the first.
   */
  async function switchArea(name) {
    if (!name || name === areaName) return;
    areaName = name;

    for (const w of ["main", "secondary"]) {
      const map = mapFor(w);
      for (const source of listFor(w)) removeSourceFromMap(map, source);
    }
    mainSources = [];
    secondarySources = [];
    // routing, the drawn line and anything inspected belong to the area they were made on
    routePackage = "";
    inspected = null;
    tileDump = null;
    clearDrawing();

    // A custom style is built against one archive's source, so it cannot outlive it. The base
    // style has to be back *before* the new area is added, or `setStyle` would wipe it straight
    // off again - and the lists are already empty, so nothing old is re-added on the way.
    if (styleApplied) {
      setBackdrop(backdrop);
      await styleReady(mainMap);
    }
    await addDefaults();
    if (showTileGrid) refreshTileGrid();
  }

  async function addArtifact(artifact, w = "main") {
    const map = mapFor(w);
    if (!map || loaded(listFor(w), artifact)) return;
    try {
      const res = await fetch(`${base}/tilejson/${artifact.area}/${artifact.file_name}`);
      if (!res.ok) throw new Error(`tilejson ${res.status}`);
      const source = makeSource(artifact, await res.json(), base);
      const apply = () => {
        addSourceToMap(map, source);
        const current = listFor(w);
        const wasEmpty = current.length === 0;
        if (w === "secondary") secondarySources = [source, ...current];
        else mainSources = [source, ...current];
        restack(w);
        if (wasEmpty) fit(source, map);
      };
      map.isStyleLoaded() ? apply() : whenStyleReady(map, apply);
    } catch (err) {
      error = String(err);
    }
  }

  function restack(w) {
    const map = mapFor(w);
    const list = listFor(w);
    applyOrder(map, list);
    for (const s of list) { applyVisibility(map, s, geomFilter); applyOpacity(map, s); }
    if (drawn.length) drawOverlay();
    if (trip) drawRoute();
    if (showTileGrid) refreshTileGrid();
  }

  function bump() {
    mainSources = [...mainSources];
    secondarySources = [...secondarySources];
  }

  function toggleSource(source) {
    source.visible = !source.visible;
    applyVisibility(mapFor(which(source)), source, geomFilter);
    bump();
  }
  function toggleLayer(source, layer) {
    layer.visible = !layer.visible;
    applyVisibility(mapFor(which(source)), source, geomFilter);
    bump();
  }
  function setAll(source, visible) {
    setAllLayers(source, visible);
    applyVisibility(mapFor(which(source)), source, geomFilter);
    bump();
  }
  function setOpacity(source, value) {
    source.opacity = value;
    applyOpacity(mapFor(which(source)), source);
    bump();
  }
  function setTerrainMode(source, mode) {
    const w = which(source);
    const map = mapFor(w);
    const was = source.terrainMode;
    source.terrainMode = mode;
    // the source type itself changes between modes, so it is rebuilt rather than re-styled
    removeSourceFromMap(map, source);
    addSourceToMap(map, source);
    // 3D is for looking at tile edges, and a flat camera shows none of them: tilt on the way in
    // and flatten on the way out, but never fight a pitch the user set themselves
    if (mode === "terrain3d" && map?.getPitch() === 0) map.easeTo({ pitch: 62, duration: 400 });
    else if (was === "terrain3d" && mode !== "terrain3d") map?.easeTo({ pitch: 0, duration: 400 });
    restack(w);
    bump();
  }

  /**
   * Pretend a terrain archive stops at `cap`, so two build depths can be compared without
   * rebuilding either.
   *
   * "Is z13 worth four times the tiles?" is answered by looking at the same hillside twice, and
   * the honest way to see what z12 would give is to stop the source at z12 and let MapLibre
   * overzoom above it - which is precisely what the phone does with a z12 archive. `null` puts
   * the archive's real maxzoom back.
   */
  function setMaxzoomCap(source, cap) {
    const w = which(source);
    const map = mapFor(w);
    source.maxzoomCap = cap;
    // the source is defined from its tile template rather than its TileJSON when capped, so it
    // has to be rebuilt rather than re-styled
    removeSourceFromMap(map, source);
    addSourceToMap(map, source);
    restack(w);
    bump();
  }

  function move(list, index, delta) {
    const target = index + delta;
    if (target < 0 || target >= list.length) return list;
    const next = [...list];
    [next[index], next[target]] = [next[target], next[index]];
    return next;
  }
  function moveIn(w, index, delta) {
    if (w === "secondary") secondarySources = move(secondarySources, index, delta);
    else mainSources = move(mainSources, index, delta);
    restack(w);
  }
  function remove(source) {
    const w = which(source);
    removeSourceFromMap(mapFor(w), source);
    if (w === "secondary") secondarySources = secondarySources.filter((s) => s !== source);
    else mainSources = mainSources.filter((s) => s !== source);
  }
  function fit(source, map = mapFor(which(source))) {
    const b = boundsOf(source);
    if (b && map) map.fitBounds(b, { padding: 20, animate: false });
  }

  $effect(() => {
    geomFilter;
    for (const w of ["main", "secondary"]) {
      const map = mapFor(w);
      if (map) for (const s of listFor(w)) applyVisibility(map, s, geomFilter);
    }
  });

  async function setBackdrop(value) {
    backdrop = value;
    styleApplied = false;
    for (const w of ["main", "secondary"]) {
      const map = mapFor(w);
      if (!map) continue;
      const list = listFor(w);
      releaseTerrain(map);
      map.setStyle(baseStyle(backdrop));
      whenStyleReady(map, () => {
        for (const s of [...list].reverse()) addSourceToMap(map, s);
        restack(w);
      });
    }
  }

  async function toggleCompare() {
    comparing = !comparing;
    if (!comparing) {
      compare?.remove();
      compare = null;
      mainMap?.resize();
      secondaryMap?.resize();
      return;
    }
    await tick();
    compare = new Compare(mainMap, secondaryMap, containerEl, {});
    compare.setSlider((containerEl?.clientWidth || 800) / 2);
    guardSwiperDrag();
  }

  function guardSwiperDrag() {
    const swiper = containerEl?.querySelector(".compare-swiper-vertical, .compare-swiper-horizontal");
    if (!swiper) return;
    swiper.addEventListener("mousedown", (e) => {
      e.preventDefault();
      document.body.classList.add("swiping");
      const stop = () => {
        document.body.classList.remove("swiping");
        document.removeEventListener("mouseup", stop);
      };
      document.addEventListener("mouseup", stop);
    });
  }

  // ------------------------------------------------------------------ tile grid

  function refreshTileGrid() {
    if (!mainMap?.isStyleLoaded()) return;
    const data = showTileGrid
      ? tileGrid(mainMap)
      : { type: "FeatureCollection", features: [] };
    const existing = mainMap.getSource("tilegrid");
    if (existing) { existing.setData(data); return; }
    if (!showTileGrid) return;
    try {
      mainMap.addSource("tilegrid", { type: "geojson", data });
      mainMap.addLayer({
        id: "tilegrid-line", type: "line", source: "tilegrid",
        paint: { "line-color": "#ff6b6b", "line-width": 1, "line-opacity": 0.8 },
      });
      mainMap.addLayer({
        id: "tilegrid-label", type: "symbol", source: "tilegrid",
        filter: ["==", "$type", "Point"],
        layout: { "text-field": ["get", "label"], "text-size": 11, "text-allow-overlap": true },
        paint: { "text-color": "#ff6b6b", "text-halo-color": "#12151a", "text-halo-width": 1.5 },
      });
    } catch {}
  }

  $effect(() => { showTileGrid; refreshTileGrid(); });

  // ------------------------------------------------------------------ interaction

  function onMapClick(e) {
    if (mode === "route") {
      drawn = [...drawn, [e.lngLat.lng, e.lngLat.lat]];
      drawOverlay();
      if (drawn.length >= 2) computeRoute();
      return;
    }
    if (mode === "profile") {
      drawn = [...drawn, [e.lngLat.lng, e.lngLat.lat]];
      drawOverlay();
      return;
    }
    if (mode === "tiles") {
      dumpTile(e.lngLat);
      return;
    }
    const features = mainMap.queryRenderedFeatures(e.point);
    if (!features.length) { inspected = null; return; }
    inspected = {
      at: [e.lngLat.lng.toFixed(5), e.lngLat.lat.toFixed(5)],
      features: features.slice(0, 8).map((f) => ({
        layer: f.sourceLayer ?? f.layer.id,
        type: f.geometry?.type,
        props: Object.entries(f.properties ?? {}),
      })),
    };
  }

  async function dumpTile(lngLat) {
    const source = mainSources.find((s) => s.vector);
    if (!source) { error = "no vector layer to read a tile from"; return; }
    const tile = pointToTile(lngLat.lng, lngLat.lat, Math.floor(mainMap.getZoom()));
    try {
      tileDump = await fetchTileJson(base, source.area, source.file, tile, source.tileEncoding);
      tileDump.file = source.file;
    } catch (err) {
      error = String(err);
    }
  }

  function drawOverlay() {
    if (!mainMap) return;
    if (!mainMap.isStyleLoaded()) { whenStyleReady(mainMap, drawOverlay); return; }
    const data = {
      type: "FeatureCollection",
      features: [
        ...(drawn.length > 1
          ? [{ type: "Feature", geometry: { type: "LineString", coordinates: drawn }, properties: {} }]
          : []),
        ...drawn.map((c) => ({ type: "Feature", geometry: { type: "Point", coordinates: c }, properties: {} })),
      ],
    };
    const existing = mainMap.getSource("drawn");
    if (existing) { existing.setData(data); return; }
    try {
      mainMap.addSource("drawn", { type: "geojson", data });
      mainMap.addLayer({ id: "drawn-line", type: "line", source: "drawn",
        paint: { "line-color": "#ffd166", "line-width": 2.5 } });
      mainMap.addLayer({ id: "drawn-pt", type: "circle", source: "drawn",
        filter: ["==", "$type", "Point"],
        paint: { "circle-color": "#ffd166", "circle-radius": 4, "circle-stroke-width": 1,
                 "circle-stroke-color": "#12151a" } });
    } catch {}
  }

  async function computeRoute() {
    if (drawn.length < 2 || !canRoute) return;
    routeBusy = true; error = "";
    try {
      const raw = await invoke("valhalla_route", {
        req: { area: areaName, locations: drawn, costing, package: routePackage || null },
      });
      trip = readTrip(JSON.parse(raw).trip);
      routingInfo = await invoke("routing_status");
    } catch (err) {
      error = String(err);
      trip = null;
    } finally {
      routeBusy = false;
    }
    drawRoute();
  }

  function drawRoute() {
    if (!mainMap || !trip) return;
    if (!mainMap.isStyleLoaded()) { whenStyleReady(mainMap, drawRoute); return; }
    const { line, maneuvers } = tripToGeoJson(trip);
    for (const [id, data] of [["route", line], ["route-maneuvers", maneuvers]]) {
      const existing = mainMap.getSource(id);
      if (existing) { existing.setData(data); continue; }
      try { mainMap.addSource(id, { type: "geojson", data }); } catch { continue; }
      if (id === "route") {
        mainMap.addLayer({ id: "route-casing", type: "line", source: id,
          layout: { "line-cap": "round", "line-join": "round" },
          paint: { "line-color": "#0b1a33", "line-width": 8, "line-opacity": 0.65 } });
        mainMap.addLayer({ id: "route-line", type: "line", source: id,
          layout: { "line-cap": "round", "line-join": "round" },
          paint: { "line-color": "#4aa3ff", "line-width": 3.5 } });
      } else {
        mainMap.addLayer({ id: "route-maneuver-dots", type: "circle", source: id,
          paint: { "circle-color": "#ffd166", "circle-radius": 3.5,
                   "circle-stroke-width": 1, "circle-stroke-color": "#0b1a33" } });
      }
    }
  }

  async function computeProfile(line = drawn) {
    if (line.length < 2 || !terrainArt) return;
    profileBusy = true; error = "";
    try {
      profile = await invoke("elevation_profile", {
        req: { path: terrainArt.path, line, densifyM: 50, thresholdM: 3 },
      });
    } catch (err) {
      error = String(err);
    } finally {
      profileBusy = false;
    }
  }

  function clearDrawing() {
    drawn = []; trip = null; profile = null;
    drawOverlay();
    for (const id of ["route", "route-maneuvers"]) {
      if (mainMap?.getSource(id)) mainMap.getSource(id).setData({ type: "FeatureCollection", features: [] });
    }
  }

  // ------------------------------------------------------------------ style testing

  function applyCustomStyle(style, source) {
    if (!mainMap) return;
    const url = `${base}/tilejson/${source.area}/${source.file}`;
    const next = structuredClone(style);
    next.sources = next.sources ?? {};
    // repoint every vector source at the local archive, whatever the style called them
    const names = Object.keys(next.sources).filter((k) => next.sources[k]?.type === "vector");
    const targets = names.length ? names : ["SOURCE"];
    for (const name of targets) {
      next.sources[name] = { type: "vector", url, encoding: source.tileEncoding };
    }
    for (const layer of next.layers ?? []) {
      if (layer.source && layer.source !== "SOURCE" && !next.sources[layer.source]) {
        layer.source = targets[0];
      } else if (layer.source === "SOURCE") {
        layer.source = targets[0];
      }
    }
    // a custom style carries no DEM, so 3D cannot survive it; drop the mode rather than leave
    // a button claiming to be on
    for (const source of mainSources) {
      if (source.terrainMode === "terrain3d") source.terrainMode = "hillshade";
    }
    bump();
    releaseTerrain(mainMap);
    mainMap.setStyle(next);
    styleApplied = true;
    whenStyleReady(mainMap, () => { if (showTileGrid) refreshTileGrid(); });
  }

  function clearCustomStyle() {
    styleApplied = false;
    setBackdrop(backdrop);
  }

  let copied = $state(false);
  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1200);
    } catch (err) {
      error = String(err);
    }
  }

  /**
   * Keep select-all inside the dump.
   *
   * A `pre` is not a text control, so the browser hands ctrl/cmd-A to the document and selects
   * the whole app. Focused, it selects its own contents instead - which is what someone
   * pressing it over a wall of JSON meant.
   */
  function selectAllInside(event) {
    if (event.key !== "a" || !(event.metaKey || event.ctrlKey)) return;
    event.preventDefault();
    const range = document.createRange();
    range.selectNodeContents(event.currentTarget);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }

  /**
   * Switch modes, undoing anything the old one left behind.
   *
   * A custom style replaces the whole map style, so leaving style mode without restoring it
   * leaves Inspect looking at someone else's layers - nothing to click, and the layer panel
   * describing sources the map no longer draws that way.
   */
  function leaveMode(next) {
    if (mode === "style" && next !== "style" && styleApplied) clearCustomStyle();
    mode = next;
    relayout();
  }

  // the mode list, its prose and the terrain-mode help live in modes.js so the docs tab shows
  // the same thing; only availability is decided here, because it depends on the area's data
  const ENABLED = {
    inspect: () => true,
    route: () => canRoute,
    profile: () => canProfile,
    tiles: () => true,
    style: () => true,
  };
  let modes = $derived(
    MAP_MODES.map((m) => ({ ...m, enabled: ENABLED[m.id](), })),
  );
  let currentMode = $derived(modes.find((m) => m.id === mode));

  function whyDisabled(id) {
    if (id === "route") {
      return valhallaBuilt
        ? "this area has no routing package"
        : "this build has no Valhalla linked";
    }
    return "this area has no terrain archive";
  }

</script>

<div class="shell" bind:this={shellEl}>
  <div class="toolbar">
    <div class="cluster">
      <span class="lbl">Area</span>
      <select value={areaName} onchange={(e) => switchArea(e.target.value)} title="area">
        {#each areas as a}<option value={a.name}>{a.name}</option>{/each}
      </select>
    </div>

    <div class="rule"></div>

    <div class="seg" role="group" aria-label="mode">
      {#each modes as m}
        <button class:on={mode === m.id} disabled={!m.enabled}
                title={m.enabled ? m.hint : `${m.label} needs ${m.needs} - ${whyDisabled(m.id)}`}
                onclick={() => { leaveMode(m.id); }}>{m.label}</button>
      {/each}
    </div>

    <div class="rule"></div>

    <div class="cluster">
      <span class="lbl">Show</span>
      <div class="seg" role="group" aria-label="geometry filter">
        {#each [["all", "All"], ["polygons", "Poly"], ["lines", "Lines"], ["points", "Pts"]] as [id, label]}
          <button class:on={geomFilter === id} onclick={() => (geomFilter = id)}>{label}</button>
        {/each}
      </div>
    </div>

    <div class="cluster">
      <select value={backdrop} onchange={(e) => setBackdrop(e.target.value)} title="backdrop">
        <option value="">no backdrop</option>
        {#each Object.entries(BACKDROPS) as [key, b]}<option value={key}>{b.label}</option>{/each}
      </select>
      <button class="tgl" class:on={showTileGrid} onclick={() => (showTileGrid = !showTileGrid)}
              title="tile boundaries with z/x/y">Grid</button>
    </div>

    <div class="spacer"></div>

    <span class="readout" title="centre and zoom">
      {view.lat.toFixed(4)}, {view.lng.toFixed(4)} <span class="z">z{view.zoom.toFixed(1)}</span>
    </span>
    <button class="tgl" class:on={comparing} onclick={toggleCompare}
            disabled={!secondarySources.length && !comparing}
            title={secondarySources.length || comparing
              ? "swipe between the two maps"
              : "open the right panel and add layers to the comparison map first"}>
      Compare
    </button>
  </div>

  {#if error}
    <p class="bar-error">
      <span class="dot"></span>{error}
      <button class="link" onclick={() => (error = "")}>dismiss</button>
    </p>
  {/if}

  <div class="body">
    <div class="side left" class:closed={!leftOpen}>
      {#if leftOpen}
        <SourcePanel sources={mainSources} title={comparing ? "Layers · right map" : "Layers"}
                     addable={addableMain} onAdd={(a) => addArtifact(a, "main")}
                     onToggleSource={toggleSource} onToggleLayer={toggleLayer}
                     onSetAllLayers={setAll} onOpacity={setOpacity} onTerrainMode={setTerrainMode}
                     onMaxzoomCap={setMaxzoomCap}
                     onMove={(i, d) => moveIn("main", i, d)} onRemove={remove} onFit={fit} />
      {/if}
    </div>
    <button class="grip left" onclick={() => { leftOpen = !leftOpen; relayout(); }}
            title={leftOpen ? "collapse" : "layers"}>{leftOpen ? "‹" : "›"}</button>

    <div class="maps" bind:this={containerEl} class:comparing>
      <div class="map" bind:this={secondaryEl}></div>
      <div class="map" bind:this={mainEl}></div>
    </div>

    <button class="grip right" class:hot={comparing || secondarySources.length}
            onclick={() => { rightOpen = !rightOpen; relayout(); }}
            title={rightOpen ? "collapse" : "layers for the comparison map"}>{rightOpen ? "›" : "‹"}</button>
    <div class="side right" class:closed={!rightOpen}>
      {#if rightOpen}
        <SourcePanel sources={secondarySources} title="Layers · left map"
                     emptyHint="add layers here, then hit Compare to swipe between the two maps"
                     addable={addableSecondary} onAdd={(a) => addArtifact(a, "secondary")}
                     onToggleSource={toggleSource} onToggleLayer={toggleLayer}
                     onSetAllLayers={setAll} onOpacity={setOpacity} onTerrainMode={setTerrainMode}
                     onMaxzoomCap={setMaxzoomCap}
                     onMove={(i, d) => moveIn("secondary", i, d)} onRemove={remove} onFit={fit} />
      {/if}
    </div>
  </div>

  <div class="drawer" class:tall={mode === "style"} class:shut={!drawerOpen}>
    <div class="drawer-head">
      <button class="chev" onclick={() => { drawerOpen = !drawerOpen; relayout(); }}
              title={drawerOpen ? "collapse" : "expand"} aria-expanded={drawerOpen}>
        {drawerOpen ? "▾" : "▸"}
      </button>
      <h4>{currentMode?.label ?? mode}</h4>
      <span class="what">{currentMode?.hint ?? ""}</span>
      <button class="info" class:on={showHelp} title="what this mode is for"
              aria-label="what this mode is for"
              onclick={() => (showHelp = !showHelp)}>?</button>
    </div>
    {#if drawerOpen && showHelp && currentMode}
      <p class="modehelp">
        {currentMode.summary}
        {#if currentMode.needs}<span class="needs">Needs {currentMode.needs}.</span>{/if}
      </p>
    {/if}
    {#if !drawerOpen}
      <!-- collapsed: the head is the whole drawer -->
    {:else if mode === "style"}
      <StyleEditor sources={mainSources} {base} applied={styleApplied}
                   onApply={applyCustomStyle} onClear={clearCustomStyle} />
    {:else if mode === "route"}
      <div class="row">
        <select bind:value={routePackage} title="routing package"
                onchange={() => drawn.length >= 2 && computeRoute()}>
          {#each packages as p}
            <option value={p.file_name}>{p.file_name}</option>
          {/each}
        </select>
        <select bind:value={costing} onchange={() => drawn.length >= 2 && computeRoute()}>
          {#each COSTING_MODELS as c}<option value={c}>{c}</option>{/each}
        </select>
        <span class="muted">{drawn.length} waypoints</span>
        <button onclick={computeRoute} disabled={drawn.length < 2 || routeBusy}>
          {routeBusy ? "Routing…" : "Route"}
        </button>
        <button onclick={() => computeProfile(trip?.points ?? drawn)}
                disabled={!trip || !canProfile || profileBusy}>
          {profileBusy ? "Sampling…" : "Elevation profile"}
        </button>
        <button class="ghost" onclick={clearDrawing} disabled={!drawn.length}>Clear</button>
      </div>
      {#if trip}
        <p class="summary">
          <strong>{trip.lengthKm.toFixed(1)} km</strong> · {formatDuration(trip.timeS)}
          · <span class="muted">{trip.maneuvers.length} maneuvers</span>
        </p>
        <Profile {profile} />
        <ol class="maneuvers">
          {#each trip.maneuvers as m}
            <li><span class="km">{m.lengthKm.toFixed(2)} km</span> {m.instruction}</li>
          {/each}
        </ol>
      {:else}
        <p class="muted hint">Click two or more points to route between them.</p>
      {/if}
      {#if routingInfo?.tile_dir}
        <p class="muted src">
          routing on <code>{routingInfo.package ?? "?"}</code>
          <span title={routingInfo.tile_dir}>· unpacked</span>
          {#if routingInfo.config}· config <code>{routingInfo.config}</code>{/if}
        </p>
      {:else}
        <p class="muted src">
          <code>{routePackage || "no package"}</code> — unpacked on the first route
          {#if routingInfo?.config}· config <code>{routingInfo.config}</code>{/if}
        </p>
      {/if}
    {:else if mode === "profile"}
      <div class="row">
        <span class="muted">{drawn.length} points</span>
        <button onclick={() => computeProfile()} disabled={drawn.length < 2 || profileBusy}>
          {profileBusy ? "Sampling…" : "Elevation profile"}
        </button>
        <button class="ghost" onclick={clearDrawing} disabled={!drawn.length}>Clear</button>
      </div>
      <Profile {profile} />
      {#if !profile}<p class="muted hint">Click points along a line, then sample.</p>{/if}
    {:else if mode === "tiles"}
      {#if tileDump}
        <p class="summary">
          <code>{tileDump.file}</code> {tileDump.z}/{tileDump.x}/{tileDump.y}
          {#if tileDump.empty}<span class="muted">— no tile here</span>
          {:else}<span class="muted">— {(tileDump.bytes / 1024).toFixed(1)} KB</span>{/if}
        </p>
        {#if tileDump.layers}
          {@const json = JSON.stringify(tileDump.layers, null, 1)}
          <div class="dumphead">
            <button class="ghost small" onclick={() => copy(json)}>
              {copied ? "copied" : "Copy JSON"}
            </button>
            <span class="muted tiny">⌘A / Ctrl+A selects this block</span>
          </div>
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <pre class="dump" tabindex="0" onkeydown={selectAllInside}>{json}</pre>
        {/if}
      {:else}
        <p class="muted hint">Click the map to read that tile's contents as JSON.</p>
      {/if}
    {:else if inspected}
      <p class="muted">{inspected.at[1]}, {inspected.at[0]}</p>
      {#each inspected.features as f}
        <div class="feat">
          <h4>{f.layer} <span class="muted">{f.type}</span></h4>
          <table><tbody>
            {#each f.props as [k, v]}<tr><td>{k}</td><td>{v}</td></tr>{/each}
          </tbody></table>
        </div>
      {/each}
    {:else}
      <p class="muted hint">Click a feature to inspect it.</p>
    {/if}
  </div>
</div>

<style>
  /* the whole view is a column that fills its parent, so the map follows the window and the
     panels can collapse without anything being sized in pixels */
  .shell { display: flex; flex-direction: column; flex: 1; min-height: 0; gap: 8px;
           /* outside the flex shell (browser dev, or a tab that is not full-height) fall back */
           height: 100%; }
  .toolbar { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; flex: none;
             background: var(--surface); border: 1px solid var(--line-2); border-radius: var(--r-lg);
             padding: 6px 8px; }
  .toolbar select { padding: 5px 8px; background: var(--bg); border: 1px solid var(--border);
                    border-radius: var(--r); color: var(--text); font: inherit; font-size: 12px;
                    max-width: 190px; }
  .cluster { display: flex; align-items: center; gap: 6px; }
  .lbl { font-size: 10px; text-transform: uppercase; letter-spacing: .07em; color: var(--faint); }
  .rule { width: 1px; align-self: stretch; background: var(--line-2); margin: 2px 2px; }
  .toolbar button { background: transparent; color: var(--text-2); padding: 5px 11px;
                    font-size: 12px; border-radius: var(--r); }
  .toolbar button:hover:not(:disabled) { background: var(--hover); color: var(--text); }
  .toolbar button.on { background: var(--accent); color: #fff; }
  .toolbar button.on:hover { background: var(--accent-hi); }
  .toolbar button:disabled { background: transparent; color: var(--faint); cursor: not-allowed; }
  .tgl { border: 1px solid var(--line-2); }
  /* one recessed track, buttons ride inside it - reads as a choice, not four separate actions */
  .seg { display: flex; gap: 2px; background: var(--bg); border: 1px solid var(--line-2);
         border-radius: var(--r); padding: 2px; }
  .spacer { flex: 1; }
  .readout { font-size: 11px; color: var(--muted-2); font-variant-numeric: tabular-nums;
             font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .readout .z { color: var(--faint); }
  .bar-error { margin: 0; flex: none; display: flex; align-items: center; gap: 8px;
               background: #2a1c1a; border: 1px solid #4a2d2d; border-radius: var(--r);
               color: #e8b4ae; font-size: 12px; padding: 7px 10px; }
  .bar-error .dot { width: 6px; height: 6px; border-radius: 50%; background: var(--danger);
                    flex: none; }

  .body { display: flex; gap: 0; flex: 1; min-height: 0; }
  .side { width: 264px; flex: none; overflow: auto; padding-right: 8px; }
  .side.right { padding-right: 0; padding-left: 8px; }
  .side.closed { width: 0; padding: 0; overflow: hidden; }
  .grip { flex: none; width: 13px; background: var(--surface); border: 1px solid var(--line-2);
          color: var(--muted-2); cursor: pointer; padding: 0; font-size: 11px;
          border-radius: var(--r-sm); margin: 0 2px; transition: background .12s, color .12s; }
  .grip:hover { color: var(--text); background: var(--hover); }
  .grip.hot { color: var(--ok); border-color: var(--accent); }

  .maps { position: relative; flex: 1; min-width: 0; border-radius: var(--r-lg); overflow: hidden;
          border: 1px solid var(--line-2); background: var(--bg-sunken); }
  .map { position: absolute; inset: 0; }
  /* the canvas keeps its own pixel size between resizes; stretching it to the box means a drag
     shows a scaled frame instead of a cleared one */
  .maps :global(.maplibregl-canvas) { width: 100% !important; height: 100% !important; }
  .maps:not(.comparing) .map:first-child { visibility: hidden; }
  :global(body.swiping) { user-select: none; }

  .drawer { flex: none; max-height: 240px; overflow: auto; background: var(--surface);
            border: 1px solid var(--line-2); border-radius: var(--r-lg); padding: 10px 12px; }
  .drawer.tall { max-height: 440px; }
  .drawer-head { display: flex; align-items: center; gap: 8px; margin: -2px 0 8px; }
  .drawer.shut { max-height: none; }
  .drawer.shut .drawer-head { margin-bottom: -2px; }
  .chev { background: none; border: 0; color: var(--muted-2); font-size: 10px; padding: 0 2px;
          cursor: pointer; line-height: 1; }
  .chev:hover { color: var(--text); background: none; }
  .drawer-head h4 { font-size: 11px; text-transform: uppercase; letter-spacing: .07em;
                    color: var(--muted-2); margin: 0; }
  .drawer-head .what { font-size: 11px; color: var(--faint); }
  .info { margin-left: auto; background: var(--line-2); color: var(--muted-2); border: 0;
          width: 18px; height: 18px; border-radius: 50%; font-size: 11px; line-height: 1;
          padding: 0; cursor: pointer; }
  .info:hover, .info.on { background: var(--accent); color: #fff; }
  .modehelp { font-size: 12.5px; color: var(--text-2); line-height: 1.55; margin: 0 0 10px;
              max-width: 80ch; border-left: 2px solid var(--accent); padding-left: 10px; }
  .modehelp .needs { color: var(--warn); }
  .row { display: flex; gap: 8px; align-items: center; margin-bottom: 8px; flex-wrap: wrap; }
  .row select { padding: 6px 8px; background: var(--bg); border: 1px solid var(--border);
                border-radius: 5px; color: var(--text); font: inherit; font-size: 12px; }
  .summary { margin: 4px 0; font-size: 14px; }
  .src { font-size: 11px; margin: 6px 0 0; }
  .maneuvers { margin: 8px 0 0; padding-left: 0; list-style: none; font-size: 12px; color: var(--text-2); }
  .maneuvers li { padding: 2px 0; border-bottom: 1px solid var(--line); }
  .km { display: inline-block; width: 62px; text-align: right; color: var(--muted-2);
        font-variant-numeric: tabular-nums; margin-right: 8px; }
  .feat { border-top: 1px solid var(--line-2); padding-top: 8px; margin-top: 8px; }
  h4 { margin: 0 0 4px; font-size: 13px; }
  table { width: 100%; border-collapse: collapse; font-size: 12px; }
  td { padding: 2px 6px 2px 0; vertical-align: top; }
  td:first-child { color: var(--muted-2); width: 34%; }
  pre { margin: 0; font-size: 11px; color: var(--text-2); white-space: pre-wrap; word-break: break-all; }
  .dump { user-select: text; cursor: text; max-height: 260px; overflow: auto; padding: 8px;
          background: var(--bg); border: 1px solid var(--line-2); border-radius: 6px; }
  .dump:focus { outline: 1px solid var(--accent); outline-offset: -1px; }
  .dumphead { display: flex; align-items: center; gap: 8px; margin: 6px 0; }
  .small { padding: 3px 9px; font-size: 11px; }
  .tiny { font-size: 11px; }
  .muted { color: var(--muted-2); }
  .hint { font-size: 13px; }
  .link { background: none; border: 0; color: var(--muted); font: inherit; font-size: 11px;
          text-decoration: underline; cursor: pointer; padding: 0; }
</style>
