<script>
  import { invoke } from "./api.js";
  import Sources from "./Sources.svelte";

  let settings = $state(null);
  let saved = $state(false);
  let error = $state("");
  let newArea = $state("");

  load();

  async function load() {
    try {
      settings = await invoke("get_settings");
    } catch (err) {
      error = String(err);
    }
  }

  async function save() {
    error = "";
    try {
      settings = await invoke("save_settings", { settings });
      saved = true;
      setTimeout(() => (saved = false), 1500);
    } catch (err) {
      error = String(err);
    }
  }

  function addArea() {
    const name = newArea.trim();
    if (!name || settings.areas.some((a) => a.name === name)) return;
    settings.areas = [...settings.areas, { name }];
    newArea = "";
  }

  const PATHS = [
    ["repo_root", "Repo root", "only a source of defaults; a packaged install needs none of it"],
    ["output_root", "Output root", "one subdirectory per area"],
    ["data_dir", "Source data", "planetiler downloads"],
    ["tmp_dir", "Temp root", "per-run subdirectories are created under this"],
    ["elevation_tiles_dir", "Elevation tiles", ""],
    ["sources_json", "sources.json", "terrain raster sources"],
  ];
  const OPTIONAL = [
    ["planetiler_jar", "Planetiler jar", "blank = downloaded, else shipped with the app, else a checkout's build"],
    ["planetiler_jar_url", "Planetiler jar URL", "where to fetch it when there is none. Must be a build of this pipeline's planetiler fork - an upstream jar builds a different schema"],
    ["java_home", "Java home", "blank = probe JAVA_HOME then PATH"],
    ["valhalla_bin_dir", "Valhalla binaries", "blank = shipped with the app, else a checkout, else PATH"],
    ["valhalla_config", "valhalla.json", "routing config template; blank = the one shipped with the app"],
  ];
</script>

{#if error}<p class="warn">{error}</p>{/if}

{#if settings}
  <h3>Paths</h3>
  {#each PATHS as [key, label, hint]}
    <label>
      {label} {#if hint}<span class="hint">{hint}</span>{/if}
      <input bind:value={settings[key]} />
    </label>
  {/each}

  <h3>Terrain sources</h3>
  <Sources />

  <h3>Tools</h3>
  {#each OPTIONAL as [key, label, hint]}
    <label>
      {label} {#if hint}<span class="hint">{hint}</span>{/if}
      <input value={settings[key] ?? ""}
             oninput={(e) => (settings[key] = e.target.value || null)} />
    </label>
  {/each}

  <h3>Defaults</h3>
  <div class="pair">
    <label>Heap (MB)<input type="number" bind:value={settings.heap_mb} step="1024" min="1024" /></label>
    <label>
      Log interval <span class="hint">planetiler's own default of 10s is too coarse for a bar</span>
      <input bind:value={settings.log_interval} />
    </label>
  </div>

  <h3>Areas</h3>
  {#each settings.areas as area, i}
    <div class="pair">
      <label>Name<input bind:value={area.name} /></label>
      <label>Polygon<input value={area.poly ?? ""}
                          oninput={(e) => (settings.areas[i].poly = e.target.value || null)} /></label>
      <button class="ghost" onclick={() => (settings.areas = settings.areas.filter((_, j) => j !== i))}>×</button>
    </div>
  {/each}
  <div class="row">
    <input bind:value={newArea} placeholder="new area name" onkeydown={(e) => e.key === "Enter" && addArea()} />
    <button class="ghost" onclick={addArea}>Add</button>
  </div>

  <div class="row save">
    <button onclick={save}>Save</button>
    {#if saved}<span class="ok">saved</span>{/if}
  </div>
{/if}

<style>
  h3 { font-size: 12px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted);
       margin: 20px 0 10px; }
  h3:first-child { margin-top: 0; }
  label { display: block; margin-bottom: 10px; color: var(--text-2); font-size: 13px; }
  .hint { color: var(--faint); font-size: 11px; }
  input { display: block; width: 100%; margin-top: 4px; padding: 7px 9px; background: var(--bg);
          border: 1px solid var(--border); border-radius: 5px; color: var(--text); font: inherit;
          box-sizing: border-box; }
  .pair { display: flex; gap: 8px; align-items: flex-end; }
  .pair label { flex: 1; }
  .row { display: flex; gap: 8px; align-items: center; }
  .save { margin-top: 18px; }
  .ok { color: var(--ok); font-size: 13px; }
  .warn { color: var(--warn); }
</style>
