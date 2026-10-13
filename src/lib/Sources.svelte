<script>
  // Editor for sources.json: the list of elevation sources the terrain step reads, lowest
  // priority first. The file used to be edited by hand; this reads and writes the same file, in
  // the same layout, so a hand edit and an edit here are interchangeable.
  import { invoke } from "./api.js";

  let doc = $state(null);
  let specs = $state([]);
  let saved = $state("");
  let dirty = $state(false);
  let error = $state("");
  let problems = $state([]);
  let probes = $state({});
  let copied = $state("");

  load();

  async function load() {
    error = "";
    try {
      doc = await invoke("get_sources");
      specs = doc.specs.map(fromFile);
      dirty = false;
      problems = [];
    } catch (err) {
      error = String(err);
    }
  }

  // The form holds numbers as text so a half-typed one is not lost; they go back to the file as
  // numbers, and a blank field goes back as an absent key.
  const NUMBERS = ["minzoom", "maxzoom", "tile_size", "clamp_min", "concurrency"];

  function fromFile(spec) {
    const out = { ...spec, type: spec.type };
    for (const key of NUMBERS) out[key] = spec[key] == null ? "" : String(spec[key]);
    for (const key of ["url", "path", "encoding", "cache_dir"]) out[key] = spec[key] ?? "";
    out.download = spec.download ?? null;
    return out;
  }

  function toFile(spec) {
    const out = { ...spec };
    for (const key of NUMBERS) {
      const text = String(spec[key] ?? "").trim();
      if (text === "") delete out[key];
      else out[key] = Number(text);
    }
    for (const key of ["url", "path", "encoding", "cache_dir"]) {
      if (!String(spec[key] ?? "").trim()) delete out[key];
      else out[key] = String(spec[key]).trim();
    }
    if (out.download == null) delete out.download;
    return out;
  }

  const KINDS = [
    ["xyz", "Tile endpoint ({z}/{x}/{y})"],
    ["pmtiles", "PMTiles archive"],
    ["valhalla", "Folder of .hgt tiles"],
    ["raster", "GeoTIFF raster"],
  ];

  const TEMPLATES = [
    {
      label: "mapterhorn · tile endpoint",
      about: "One request per tile. Simple; slow for a large area, and it asks the service for every tile, sea included.",
      spec: {
        name: "mapterhorn", type: "xyz", url: "https://tiles.mapterhorn.com/{z}/{x}/{y}.webp",
        encoding: "terrarium", maxzoom: 14, clamp_min: -10,
      },
    },
    {
      label: "mapterhorn · planet.pmtiles (remote)",
      about: "Read by byte range. Which tiles exist is known from the archive's directory, so the sea costs no requests. z0–z12.",
      spec: {
        name: "mapterhorn", type: "pmtiles", url: "https://download.mapterhorn.com/planet.pmtiles",
        encoding: "terrarium", maxzoom: 12, clamp_min: -10,
      },
    },
    {
      label: "mapterhorn · local extract",
      about: "The best for a big area: download your region once (see below), then builds make no requests at all.",
      spec: { name: "mapterhorn", type: "pmtiles", path: "", encoding: "terrarium", maxzoom: 12, clamp_min: -10 },
    },
    {
      label: "SRTM .hgt folder",
      about: "1 arc-second tiles, the fallback every install has.",
      spec: { name: "tilezen", type: "valhalla", path: "elevation_tiles", clamp_min: -10 },
    },
    {
      label: "GeoTIFF raster",
      about: "A local raster in any projection (the IGN 5 m data, for example).",
      spec: { name: "raster", type: "raster", path: "" },
    },
  ];

  function add(template) {
    const base = fromFile({ ...template.spec });
    let name = base.name;
    for (let n = 2; specs.some((s) => s.name === name); n++) name = `${base.name}-${n}`;
    specs = [...specs, { ...base, name }];
    touched();
  }

  function touched() {
    dirty = true;
    saved = "";
  }

  function move(index, by) {
    const to = index + by;
    if (to < 0 || to >= specs.length) return;
    const next = [...specs];
    [next[index], next[to]] = [next[to], next[index]];
    specs = next;
    touched();
  }

  function remove(index) {
    specs = specs.filter((_, i) => i !== index);
    probes = {};
    touched();
  }

  async function save() {
    error = "";
    try {
      problems = await invoke("save_sources", { specs: specs.map(toFile) });
      if (!problems.length) {
        dirty = false;
        saved = "saved";
        setTimeout(() => (saved = ""), 2000);
      }
    } catch (err) {
      error = String(err);
    }
  }

  async function test(index) {
    probes = { ...probes, [index]: { busy: true } };
    try {
      probes = { ...probes, [index]: await invoke("probe_source", { spec: toFile(specs[index]) }) };
    } catch (err) {
      probes = { ...probes, [index]: { ok: false, summary: String(err), details: [] } };
    }
  }

  const problemFor = (index, field) => problems.find((p) => p.index === index && p.field === field);

  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      copied = text;
      setTimeout(() => (copied = ""), 1200);
    } catch {}
  }

  const EXTRACT =
    "pmtiles extract --bbox=<west>,<south>,<east>,<north> https://download.mapterhorn.com/planet.pmtiles mapterhorn.pmtiles";
</script>

{#if error}<p class="warn">{error}</p>{/if}

{#if doc}
  <p class="lede">
    <span class="mono">{doc.path}</span><br />
    Listed lowest priority first: where several sources have data, <strong>the last one wins</strong>,
    and it fades in over the one before it at the edge of its coverage. A build reads this file when
    it starts, so a change applies from the next run and never disturbs one in progress.
  </p>

  {#if !specs.length}
    <p class="muted">No sources yet. Without any, the terrain step falls back to the .hgt folder.</p>
  {/if}

  {#each specs as spec, i (i)}
    <div class="card">
      <div class="head">
        <label class="grow">
          Name
          <input bind:value={spec.name} oninput={touched} class:bad={problemFor(i, "name")} />
        </label>
        <label>
          Type
          <select bind:value={spec.type} onchange={touched}>
            {#each KINDS as [value, label]}<option {value}>{label}</option>{/each}
          </select>
        </label>
        <div class="tools">
          <button class="ghost tiny" title="lower priority" onclick={() => move(i, -1)} disabled={i === 0}>↑</button>
          <button class="ghost tiny" title="higher priority" onclick={() => move(i, 1)} disabled={i === specs.length - 1}>↓</button>
          <button class="ghost tiny" title="remove" onclick={() => remove(i)}>×</button>
        </div>
      </div>
      {#if problemFor(i, "name")}<p class="warn small">{problemFor(i, "name").message}</p>{/if}

      {#if spec.type === "xyz"}
        <label>
          URL template <span class="hint">needs {"{z}"}, {"{x}"} and {"{y}"}</span>
          <input bind:value={spec.url} oninput={touched} spellcheck="false" class:bad={problemFor(i, "url")} />
        </label>
      {:else if spec.type === "pmtiles"}
        <div class="pair">
          <label>
            URL <span class="hint">read by byte range</span>
            <input bind:value={spec.url} oninput={touched} spellcheck="false" class:bad={problemFor(i, "url")}
                   placeholder="https://download.mapterhorn.com/planet.pmtiles" />
          </label>
          <label>
            or a file on this disk
            <input bind:value={spec.path} oninput={touched} spellcheck="false"
                   placeholder="/path/to/mapterhorn.pmtiles" />
          </label>
        </div>
        {#if !spec.url && !spec.path}
          <details class="how">
            <summary>How to get a local extract of your region</summary>
            <p class="muted small">
              Needs the <code>pmtiles</code> command (go-pmtiles). Add <code>--dry-run</code> first to see
              the download size. The planet archive covers z0–z12; z13 and up live in separate regional
              files, listed on mapterhorn's data-access page.
            </p>
            <div class="asline">
              <code>{EXTRACT}</code>
              <button class="ghost tiny" onclick={() => copy(EXTRACT)}>{copied === EXTRACT ? "copied" : "copy"}</button>
            </div>
          </details>
        {/if}
      {:else}
        <label>
          {spec.type === "raster" ? "File or glob" : "Folder"}
          <input bind:value={spec.path} oninput={touched} spellcheck="false" class:bad={problemFor(i, "path")} />
        </label>
      {/if}
      {#each ["url", "path", "type"] as field}
        {#if problemFor(i, field)}<p class="warn small">{problemFor(i, field).message}</p>{/if}
      {/each}

      <details class="more">
        <summary>More</summary>
        <div class="grid">
          <label>Encoding
            <select bind:value={spec.encoding} onchange={touched}>
              <option value="">terrarium (default)</option>
              <option value="terrarium">terrarium</option>
              <option value="mapbox">mapbox</option>
            </select>
          </label>
          <label>Min zoom <input bind:value={spec.minzoom} oninput={touched} inputmode="numeric" /></label>
          <label>Max zoom <input bind:value={spec.maxzoom} oninput={touched} inputmode="numeric" /></label>
          <label>Tile size <span class="hint">px, 512 if blank</span>
            <input bind:value={spec.tile_size} oninput={touched} inputmode="numeric" /></label>
          <label>Lowest elevation kept <span class="hint">clamp, m</span>
            <input bind:value={spec.clamp_min} oninput={touched} inputmode="numeric" /></label>
          {#if spec.type === "xyz" || (spec.type === "pmtiles" && spec.url)}
            <label>Requests at once <span class="hint">24 if blank</span>
              <input bind:value={spec.concurrency} oninput={touched} inputmode="numeric"
                     class:bad={problemFor(i, "concurrency")} /></label>
          {/if}
        </div>
        {#each ["encoding", "minzoom", "tile_size", "concurrency"] as field}
          {#if problemFor(i, field)}<p class="warn small">{problemFor(i, field).message}</p>{/if}
        {/each}
        {#if spec.type === "xyz" || (spec.type === "pmtiles" && spec.url)}
          <p class="muted small">
            Fetched tiles are kept in <code>.cache/terrain-tiles/{spec.name || "…"}</code> beside this
            file, so a rebuild asks the service for nothing it already has.
          </p>
        {/if}
      </details>

      <div class="foot">
        <button class="ghost" onclick={() => test(i)} disabled={probes[i]?.busy}>
          {probes[i]?.busy ? "Trying…" : "Test"}
        </button>
        {#if probes[i] && !probes[i].busy}
          <span class:ok={probes[i].ok} class:warn={!probes[i].ok}>{probes[i].summary}</span>
          <span class="muted small">{(probes[i].details ?? []).join(" · ")}</span>
        {/if}
      </div>
    </div>
  {/each}

  <div class="adder">
    <span class="muted small">Add</span>
    {#each TEMPLATES as t}
      <button class="ghost" title={t.about} onclick={() => add(t)}>{t.label}</button>
    {/each}
  </div>

  <div class="row save">
    <button onclick={save} disabled={!dirty}>Save</button>
    <button class="ghost" onclick={load} disabled={!dirty}>Revert</button>
    {#if saved}<span class="ok">{saved}</span>{/if}
    {#if problems.length}<span class="warn">{problems[0].message}</span>{/if}
  </div>
{/if}

<style>
  .lede { color: var(--text-2); font-size: 13px; margin: 0 0 14px; line-height: 1.5; }
  .mono { font-family: ui-monospace, monospace; font-size: 12px; color: var(--muted); }
  .card { border: 1px solid var(--border); border-radius: 8px; padding: 12px; margin-bottom: 12px;
          background: var(--bg-2, transparent); }
  .head { display: flex; gap: 8px; align-items: flex-end; }
  .grow { flex: 1; }
  .tools { display: flex; gap: 4px; padding-bottom: 2px; }
  .pair { display: flex; gap: 8px; }
  .pair label { flex: 1; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 8px 10px; }
  label { display: block; margin-top: 8px; color: var(--text-2); font-size: 12px; }
  .hint { color: var(--faint); font-size: 11px; }
  input, select { display: block; width: 100%; margin-top: 3px; padding: 6px 8px; background: var(--bg);
                  border: 1px solid var(--border); border-radius: 5px; color: var(--text); font: inherit;
                  box-sizing: border-box; }
  input.bad { border-color: var(--warn); }
  details { margin-top: 8px; }
  summary { cursor: pointer; font-size: 12px; color: var(--muted); }
  .foot { display: flex; gap: 10px; align-items: center; margin-top: 10px; flex-wrap: wrap; }
  .adder { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; margin: 6px 0 14px; }
  .asline { display: flex; gap: 8px; align-items: center; }
  .asline code { flex: 1; overflow-x: auto; white-space: nowrap; font-size: 12px; }
  .row { display: flex; gap: 8px; align-items: center; }
  .save { margin-top: 8px; }
  .ok { color: var(--ok); font-size: 13px; }
  .warn { color: var(--warn); }
  .small { font-size: 12px; margin: 4px 0 0; }
  .muted { color: var(--muted); }
  .tiny { padding: 2px 7px; }
</style>
