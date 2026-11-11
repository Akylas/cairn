<script>
  // Per-source controls: order, visibility, opacity, terrain mode, and layer toggles.
  import { TERRAIN_MODES, layerSummary } from "./sources.js";
  import { TERRAIN_MODE_HELP } from "./modes.js";

  let {
    sources = [], title = "", collapsed = false, emptyHint = "no layers — use + to add one",
    onToggleSource, onToggleLayer, onSetAllLayers, onOpacity, onMove, onRemove, onFit,
    onTerrainMode, onMaxzoomCap, onAdd, addable = [],
  } = $props();

  /**
   * The zooms a terrain source can be pretended to stop at.
   *
   * Four below what it holds is enough to answer the question this exists for - "is z13 worth
   * four times the tiles?" - and the archive's own maxzoom is offered as the way back.
   */
  function capChoices(source) {
    const top = source.maxzoom ?? 14;
    const bottom = Math.max(source.minzoom ?? 0, top - 4);
    const zooms = [];
    for (let z = bottom; z <= top; z++) zooms.push(z);
    return zooms;
  }

  let expanded = $state(new Set());
  let adding = $state(false);

  function toggleExpanded(id) {
    const next = new Set(expanded);
    next.has(id) ? next.delete(id) : next.add(id);
    expanded = next;
  }

  const KIND_LABEL = {
    basemap: "Basemap", routes: "Routes", terrain_rgb: "Terrain", contours: "Contours",
    hillshade: "Raster", unknown: "Other",
  };

  let grouped = $derived.by(() => {
    const by = new Map();
    for (const a of addable) {
      const key = KIND_LABEL[a.kind] ?? "Other";
      if (!by.has(key)) by.set(key, []);
      by.get(key).push(a);
    }
    return [...by.entries()];
  });
</script>

<div class="panel" class:collapsed>
  <header>
    <h4>{title}</h4>
    {#if sources.length}<span class="count">{sources.length}</span>{/if}
    <button class="add" class:open={adding} onclick={() => (adding = !adding)}
            title="add a layer" aria-label="add a layer">+</button>
  </header>

  {#if adding}
    <div class="adder">
      {#each grouped as [group, items]}
        <div class="group">{group}</div>
        {#each items as art}
          <button class="option" onclick={() => { onAdd(art); adding = false; }}>
            {art.file_name}
            {#if art.variant}<span class="tag">{art.variant}</span>{/if}
          </button>
        {/each}
      {/each}
      {#if !grouped.length}<p class="empty">everything is already added</p>{/if}
    </div>
  {/if}

  {#if !sources.length && !adding}
    <div class="blank">
      <p class="empty">{emptyHint}</p>
      <button class="ghost wide" onclick={() => (adding = true)}>Add a layer</button>
    </div>
  {/if}

  {#each sources as source, index (source.id)}
    {@const summary = layerSummary(source)}
    <div class="source" class:off={!source.visible}>
      <div class="head">
        <input type="checkbox" checked={source.visible} title="show this file"
               onchange={() => onToggleSource(source)} />
        <button class="name" onclick={() => toggleExpanded(source.id)} title={source.file}>
          {source.file}
        </button>
        <div class="actions">
          <button class="icon" onclick={() => onMove(index, -1)} disabled={index === 0} title="move up">↑</button>
          <button class="icon" onclick={() => onMove(index, 1)} disabled={index === sources.length - 1} title="move down">↓</button>
          <button class="icon" onclick={() => onFit(source)} title="zoom to bounds">⤢</button>
          <button class="icon danger" onclick={() => onRemove(source)} title="remove">×</button>
        </div>
      </div>

      <div class="meta">
        <span class="badge">{source.terrain ? source.demEncoding ?? "dem" : source.tileEncoding}</span>
        {#if source.vector}
          <button class="link" onclick={() => toggleExpanded(source.id)}>
            {summary.on}/{summary.total} layers
          </button>
        {/if}
        <input class="opacity" type="range" min="0" max="1" step="0.05" value={source.opacity}
               title="opacity" oninput={(e) => onOpacity(source, parseFloat(e.target.value))} />
      </div>

      {#if source.terrain}
        <div class="modes" role="group" aria-label="terrain rendering">
          {#each TERRAIN_MODES as mode}
            <button class:on={source.terrainMode === mode} title={TERRAIN_MODE_HELP[mode]}
                    onclick={() => onTerrainMode(source, mode)}>
              {mode === "terrain3d" ? "3D" : mode}
            </button>
          {/each}
        </div>

        <!-- Stop the source short and let MapLibre overzoom the rest, which is what a shallower
             archive would look like on the phone. The point is answering "is the top zoom worth
             building?" by looking, without building it twice. -->
        {#if onMaxzoomCap && source.tiles?.length}
          <div class="zcap" role="group" aria-label="data maxzoom">
            <span class="zlabel" title="pretend the archive stops here; above it is overzoomed">
              data z
            </span>
            {#each capChoices(source) as z}
              {@const real = z === source.maxzoom}
              <button class:on={real ? source.maxzoomCap == null : source.maxzoomCap === z}
                      title={real
                        ? `${z} — everything this archive holds`
                        : `render as if the archive stopped at z${z}`}
                      onclick={() => onMaxzoomCap(source, real ? null : z)}>
                {z}
              </button>
            {/each}
          </div>
          {#if source.maxzoomCap != null}
            <p class="capnote">
              capped at z{source.maxzoomCap} — z{source.maxzoomCap + 1}+ is overzoomed, not data
            </p>
          {/if}
        {/if}
      {/if}

      {#if expanded.has(source.id) && source.vector}
        <div class="bulk">
          <button class="link" onclick={() => onSetAllLayers(source, true)}>all</button>
          <button class="link" onclick={() => onSetAllLayers(source, false)}>none</button>
        </div>
        <div class="layers">
          {#each source.layers as layer}
            <label class="layer">
              <input type="checkbox" checked={layer.visible}
                     onchange={() => onToggleLayer(source, layer)} />
              <span class="swatch" style="background:{layer.color}"></span>
              <span class="lname">{layer.id}</span>
              {#if layer.minzoom != null}<span class="z">{layer.minzoom}–{layer.maxzoom}</span>{/if}
            </label>
          {/each}
        </div>
      {/if}
    </div>
  {/each}
</div>

<style>
  .panel { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
  .panel.collapsed { display: none; }
  header { display: flex; align-items: center; gap: 7px; position: sticky; top: 0; z-index: 2;
           background: var(--bg); padding: 2px 0 6px; }
  h4 { font-size: 11px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted-2);
       margin: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .count { font-size: 10px; color: var(--faint); background: var(--line-2); border-radius: 8px;
           padding: 0 6px; }
  .add { margin-left: auto; background: var(--line-2); color: var(--text-2); border: 0;
         border-radius: var(--r-sm); width: 22px; height: 22px; line-height: 1; padding: 0;
         cursor: pointer; font-size: 15px; transition: transform .12s, background .12s; }
  .add:hover { background: var(--border); color: var(--text); }
  .add.open { transform: rotate(45deg); background: var(--accent); color: #fff; }
  .blank { border: 1px dashed var(--line-2); border-radius: var(--r); padding: 12px;
           text-align: center; }
  .wide { width: 100%; }
  .adder { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 6px;
           max-height: 230px; overflow: auto; }
  .group { font-size: 10px; text-transform: uppercase; color: var(--faint); margin: 4px 0 2px; }
  .option { display: block; width: 100%; text-align: left; background: none; border: 0;
            color: var(--text); font: inherit; font-size: 12px; padding: 3px 4px; border-radius: 3px;
            cursor: pointer; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .option:hover { background: var(--hover); }
  .empty { color: var(--faint); font-size: 12px; margin: 0 0 9px; line-height: 1.45; }
  .source { background: var(--surface); border: 1px solid var(--line-2);
            border-radius: var(--r); padding: 8px 9px; transition: border-color .12s; }
  .source:hover { border-color: var(--border); }
  .source.off { opacity: .45; }
  .head { display: flex; align-items: center; gap: 6px; }
  .name { flex: 1; background: none; border: 0; color: var(--text); font: inherit; font-size: 12px;
          text-align: left; padding: 0; cursor: pointer; overflow: hidden;
          text-overflow: ellipsis; white-space: nowrap; }
  .actions { display: flex; gap: 1px; opacity: .55; }
  .source:hover .actions { opacity: 1; }
  .icon { background: none; border: 0; color: var(--muted); font-size: 12px; padding: 0 3px;
          cursor: pointer; line-height: 1; }
  .icon.danger:hover { color: var(--danger); }
  .icon:disabled { opacity: .3; cursor: default; }
  .meta { display: flex; align-items: center; gap: 8px; margin-top: 5px; }
  .badge { background: var(--line-2); color: var(--text-3); font-size: 10px; padding: 1px 5px; border-radius: 3px; }
  .link { background: none; border: 0; color: var(--muted); font: inherit; font-size: 11px;
          padding: 0; cursor: pointer; text-decoration: underline; }
  .opacity { flex: 1; height: 3px; min-width: 40px; }
  .modes { display: flex; gap: 1px; margin-top: 6px; }
  .modes button { flex: 1; background: var(--bg); border: 1px solid var(--border); color: var(--text-3);
                  font-size: 10px; padding: 3px 0; cursor: pointer; }
  .modes button:first-child { border-radius: 4px 0 0 4px; }
  .modes button:last-child { border-radius: 0 4px 4px 0; }
  .modes button.on { background: var(--accent); border-color: var(--accent); color: #fff; }
  /* the zoom cap: same strip as the mode buttons, with the zoom numbers as the choices */
  .zcap { display: flex; align-items: center; gap: 1px; margin-top: 4px; }
  .zlabel { font-size: 10px; color: var(--faint); margin-right: 5px; white-space: nowrap; }
  .zcap button { flex: 1; background: var(--bg); border: 1px solid var(--border);
                 color: var(--text-3); font-size: 10px; padding: 3px 0; cursor: pointer;
                 font-variant-numeric: tabular-nums; }
  .zcap button:first-of-type { border-radius: 4px 0 0 4px; }
  .zcap button:last-of-type { border-radius: 0 4px 4px 0; }
  .zcap button.on { background: var(--accent); border-color: var(--accent); color: #fff; }
  .capnote { margin: 4px 0 0; font-size: 10px; color: var(--warn); }
  .bulk { display: flex; gap: 8px; margin-top: 6px; }
  .layers { margin-top: 4px; max-height: 210px; overflow: auto; display: flex;
            flex-direction: column; gap: 1px; }
  .layer { display: flex; align-items: center; gap: 6px; font-size: 11px; color: var(--text-2); }
  .swatch { width: 9px; height: 9px; border-radius: 2px; flex: none; }
  .lname { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .z { color: var(--faint); font-size: 10px; }
  input[type="checkbox"] { margin: 0; }
</style>
