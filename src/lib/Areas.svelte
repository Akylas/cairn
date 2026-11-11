<script>
  import { invoke } from "./api.js";
  import { mb, pct, KIND_LABEL, formatLabel } from "./format.js";

  let areas = $state([]);
  let active = $state(0);
  let error = $state("");
  let stats = $state({});
  let loading = $state({});

  let compareA = $state("");
  let compareB = $state("");
  let comparison = $state(null);
  let comparing = $state(false);

  export async function refresh() {
    error = "";
    try {
      areas = await invoke("list_areas");
      if (active >= areas.length) active = 0;
    } catch (err) {
      error = String(err);
    }
  }

  refresh();

  let area = $derived(areas[active]);

  async function loadStats(artifact) {
    if (stats[artifact.path] || loading[artifact.path]) return;
    loading = { ...loading, [artifact.path]: true };
    try {
      stats = { ...stats, [artifact.path]: await invoke("artifact_stats", { path: artifact.path }) };
    } catch (err) {
      stats = { ...stats, [artifact.path]: { error: String(err) } };
    } finally {
      loading = { ...loading, [artifact.path]: false };
    }
  }

  /// Output is a flat list of files whose names carry the meaning, which reads as noise once an
  /// area has a dozen of them. Group by kind so the eye lands on "Basemap" rather than parsing
  /// `rhone-alpes_terrain.mbtiles.old` to work out what it is.
  const KIND_ORDER = ["basemap", "routes", "terrain_rgb", "contours", "hillshade", "valhalla_package", "unknown"];

  /// One 16px glyph per kind, drawn with currentColor so it takes the accent from the section
  /// heading. Stroked rather than filled: at this size a filled glyph reads as a blob.
  const KIND_ICON = {
    // stacked layers
    basemap: "M2 5.2 8 2.2l6 3-6 3-6-3Zm0 3.4 6 3 6-3M2 11.6l6 3 6-3",
    // a winding way with its two ends marked
    routes: "M4.2 13.2c0-2.2 2-2.4 3.4-3s2.6-1 2.6-2.6-1.4-2.4-2.8-2.4M4.2 13.2h.01M7.4 5.2h.01",
    // peaks
    terrain_rgb: "M1.6 12.8 6 5.2l2.6 4.4M6.6 12.8h7.8L10.6 6.4l-2 3.2",
    hillshade: "M1.6 12.8 6 5.2l2.6 4.4M6.6 12.8h7.8L10.6 6.4l-2 3.2",
    // three nested rings, as on a map
    contours: "M2 11.4c1.6-.2 2-2.8 4-3.2s3.4 1.6 5.2.8 2.2-3.4 2.8-4M2 14c2.6-.2 3.4-3.4 6.2-3.8s4 1.8 5.8.8M3.4 8.6C4.8 8 5.6 5.6 7.6 5.2s2.6 1.2 3.6.4",
    // a navigation arrow
    valhalla_package: "M14 2.4 2 7.2l4.8 2 2 4.8L14 2.4Z",
    unknown: "M9.2 1.8H4a1.4 1.4 0 0 0-1.4 1.4v9.6A1.4 1.4 0 0 0 4 14.2h8a1.4 1.4 0 0 0 1.4-1.4V6l-4.2-4.2Zm0 0V6h4.2",
  };

  let groups = $derived.by(() => {
    if (!area) return [];
    const by = new Map();
    for (const art of area.artifacts) {
      if (!by.has(art.kind)) by.set(art.kind, []);
      by.get(art.kind).push(art);
    }
    return [...by.entries()]
      .sort((a, b) => {
        const ia = KIND_ORDER.indexOf(a[0]), ib = KIND_ORDER.indexOf(b[0]);
        return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
      })
      .map(([kind, items]) => ({
        kind,
        items: [...items].sort((x, y) => Number(!!x.variant) - Number(!!y.variant)
          || x.file_name.localeCompare(y.file_name)),
        bytes: items.reduce((sum, x) => sum + x.size_bytes, 0),
      }));
  });

  let deleting = $state({});

  async function remove(artifact) {
    const what = artifact.variant ? `${artifact.file_name} (${artifact.variant})` : artifact.file_name;
    if (!confirm(`Delete ${what}?\n\nThis cannot be undone.`)) return;
    deleting = { ...deleting, [artifact.path]: true };
    try {
      await invoke("delete_artifact", { path: artifact.path });
      // drop any stats held for it, then rescan so every total is recomputed from disk rather
      // than adjusted by hand and left to drift from what is actually there
      const { [artifact.path]: _gone, ...rest } = stats;
      stats = rest;
      await refresh();
    } catch (err) {
      error = String(err);
    } finally {
      deleting = { ...deleting, [artifact.path]: false };
    }
  }

  async function runCompare() {
    if (!compareA || !compareB || compareA === compareB) return;
    comparing = true;
    comparison = null;
    try {
      comparison = await invoke("compare_artifacts", { area: area.name, a: compareA, b: compareB });
    } catch (err) {
      comparison = { error: String(err) };
    } finally {
      comparing = false;
    }
  }
</script>

<div class="head">
  <div class="tabs">
    {#each areas as a, i}
      <button class:active={i === active} onclick={() => (active = i)}>
        {a.name}<span class="size">{mb(a.artifacts.reduce((s, x) => s + x.size_bytes, 0))}</span>
      </button>
    {/each}
  </div>
  <button class="ghost" onclick={refresh}>Rescan</button>
</div>

{#if error}
  <p class="warn">{error}</p>
{:else if !areas.length}
  <p class="muted">No areas found under the configured output root.</p>
{/if}

{#if area}
  {#each groups as group}
    <section class="kind">
      <h3>
        <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
          <path d={KIND_ICON[group.kind] ?? KIND_ICON.unknown} />
        </svg>
        {KIND_LABEL[group.kind] ?? group.kind}
        <span class="count">{group.items.length} file{group.items.length === 1 ? "" : "s"}</span>
        <span class="bytes">{mb(group.bytes)}</span>
      </h3>
      <table>
        <thead>
          <tr>
            <th>File</th><th>Format</th><th>Zooms</th>
            <th class="num">Size</th><th class="actions"></th>
          </tr>
        </thead>
        <tbody>
          {#each group.items as art}
            <tr class:variant={art.variant}>
              <td>
                <code>{art.file_name}</code>
                {#if art.variant}<span class="tag">{art.variant}</span>{/if}
                {#if art.probe_error}<span class="tag err">unreadable</span>{/if}
              </td>
              <td>
                {formatLabel(art.format)}
                {#if art.encoding}<span class="tag">{art.encoding}</span>{/if}
              </td>
              <td>{art.minzoom ?? "—"}–{art.maxzoom ?? "—"}</td>
              <td class="num">{mb(art.size_bytes)}</td>
              <td class="actions">
                <button class="ghost" onclick={() => loadStats(art)} disabled={loading[art.path]}>
                  {loading[art.path] ? "…" : stats[art.path] ? "↻" : "stats"}
                </button>
                <button class="ghost danger" onclick={() => remove(art)}
                        disabled={deleting[art.path]} title="Delete this file">
                  {deleting[art.path] ? "…" : "delete"}
                </button>
              </td>
            </tr>
            {#if stats[art.path]}
              {@const st = stats[art.path]}
              <tr class="detail">
                <td colspan="5">
                  {#if st.error}
                    <span class="warn">{st.error}</span>
                  {:else}
                    <p>
                      {st.addressed_tiles.toLocaleString()} tiles addressed &middot; {mb(st.addressed_bytes)}
                      {#if st.unique_tiles != null}
                        &nbsp;|&nbsp; {st.unique_tiles.toLocaleString()} unique &middot; {mb(st.unique_bytes)}
                        <span class="tag">dedup {((1 - st.unique_tiles / st.addressed_tiles) * 100).toFixed(1)}%</span>
                      {/if}
                    </p>
                    <div class="zoombars">
                      {#each st.per_zoom.slice().reverse() as z}
                        {@const share = z.bytes / st.addressed_bytes}
                        <div class="zoomrow">
                          <span class="z">z{z.zoom}</span>
                          <span class="bar"><i style="width:{(share * 100).toFixed(1)}%"></i></span>
                          <span class="v">{mb(z.bytes)}</span>
                          <span class="n">{z.tiles.toLocaleString()}</span>
                        </div>
                      {/each}
                    </div>
                  {/if}
                </td>
              </tr>
            {/if}
            {#if art.layers?.length}
              <tr class="detail muted"><td colspan="5">
                {art.layers.length} layers: {art.layers.map((l) => l.id).join(", ")}
              </td></tr>
            {/if}
          {/each}
        </tbody>
      </table>
    </section>
  {/each}

  <section class="compare">
    <h3>Compare two builds</h3>
    <div class="row">
      <select bind:value={compareA}>
        <option value="">A…</option>
        {#each area.artifacts as art}<option value={art.path}>{art.file_name}</option>{/each}
      </select>
      <select bind:value={compareB}>
        <option value="">B…</option>
        {#each area.artifacts as art}<option value={art.path}>{art.file_name}</option>{/each}
      </select>
      <button onclick={runCompare} disabled={comparing || !compareA || !compareB || compareA === compareB}>
        {comparing ? "Comparing…" : "Compare"}
      </button>
    </div>

    {#if comparison?.error}
      <p class="warn">{comparison.error}</p>
    {:else if comparison}
      <p class="muted">
        file size {mb(comparison.size_a)} → {mb(comparison.size_b)}
        <strong class={comparison.size_b <= comparison.size_a ? "ok" : "warn"}>
          {pct(((comparison.size_b - comparison.size_a) / comparison.size_a) * 100)}
        </strong>
      </p>
      <table class="cmp">
        <thead><tr><th>Zoom</th><th class="num">A</th><th class="num">B</th><th class="num">Δ</th><th class="num">tiles A→B</th></tr></thead>
        <tbody>
          {#each comparison.zooms.slice().reverse() as z}
            {@const change = z.bytes_a > 0 ? ((z.bytes_b - z.bytes_a) / z.bytes_a) * 100 : null}
            <tr>
              <td>z{z.zoom}</td>
              <td class="num">{mb(z.bytes_a)}</td>
              <td class="num">{mb(z.bytes_b)}</td>
              <td class="num" class:ok={change < 0} class:warn={change > 0}>{pct(change)}</td>
              <td class="num muted">{z.tiles_a.toLocaleString()} → {z.tiles_b.toLocaleString()}</td>
            </tr>
          {/each}
        </tbody>
      </table>

      {#if comparison.layers.length}
        <h4>Layer fields</h4>
        {#each comparison.layers as l}
          <p class="muted">
            <code>{l.layer}</code>
            {#if l.only_in_a_fields.length}<span class="warn">−{l.only_in_a_fields.join(" −")}</span>{/if}
            {#if l.only_in_b_fields.length}<span class="ok">+{l.only_in_b_fields.join(" +")}</span>{/if}
          </p>
        {/each}
      {/if}

      {#if Object.keys(comparison.metadata.changed).length}
        <h4>Metadata</h4>
        {#each Object.entries(comparison.metadata.changed) as [key, [va, vb]]}
          <p class="muted"><code>{key}</code> {va} → {vb}</p>
        {/each}
      {/if}
    {/if}
  </section>
{/if}

<style>
  .head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .tabs { display: flex; gap: 4px; flex: 1; flex-wrap: wrap; }
  .tabs button { background: var(--card); border: 1px solid var(--line-2); color: var(--text-2);
                 padding: 6px 12px; }
  .tabs button:hover:not(.active) { background: var(--hover); color: var(--text); }
  .tabs button.active { background: var(--accent); border-color: var(--accent); color: #fff; }
  .size { color: var(--muted-2); margin-left: 8px; font-size: 12px; }
  .tabs button.active .size { color: var(--accent-fg); }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th { text-align: left; color: var(--muted-2); font-weight: 500; font-size: 11px;
       text-transform: uppercase; letter-spacing: .05em; padding: 7px 8px;
       border-bottom: 1px solid var(--line-2); position: sticky; top: 0; background: var(--bg);
       z-index: 1; }
  td { padding: 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
  tbody tr:not(.detail):hover td { background: var(--surface-2); }
  .num { text-align: right; font-variant-numeric: tabular-nums; }
  tr.variant code { color: var(--text-3); }
  tr.detail td { background: var(--surface); padding: 10px 12px; }
  .tag { display: inline-block; margin-left: 6px; padding: 1px 6px; border-radius: 3px;
         background: var(--line-2); color: var(--text-2); font-size: 11px; }
  .tag.err { background: #4a2d2d; color: var(--warn); }
  .zoombars { display: flex; flex-direction: column; gap: 2px; }
  .zoomrow { display: grid; grid-template-columns: 34px 1fr 74px 64px; gap: 8px; align-items: center;
             font-size: 12px; font-variant-numeric: tabular-nums; }
  .z { color: var(--muted); }
  .bar { background: var(--line); height: 8px; border-radius: 2px; overflow: hidden; }
  .bar i { display: block; height: 100%; background: var(--accent-hi); }
  .v { text-align: right; }
  .n { text-align: right; color: var(--muted-2); }
  /* a card per kind: the heading alone was not enough separation once an area has a dozen files */
  .kind { margin-bottom: 18px; border: 1px solid var(--line-2); border-radius: var(--r);
    background: var(--card); overflow: hidden; }
  .kind h3 { display: flex; align-items: center; gap: 9px; margin: 0;
    padding: 9px 12px; background: var(--hover); border-bottom: 1px solid var(--line-2);
    font-size: 12px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase;
    color: var(--text); }
  /* the accent bar is what the eye catches when scrolling past several sections */
  .kind h3::before { content: ""; width: 3px; align-self: stretch; margin: -9px 3px -9px -12px;
    background: var(--accent); }
  .icon { width: 16px; height: 16px; flex: none; color: var(--accent-hi);
    fill: none; stroke: currentColor; stroke-width: 1.4;
    stroke-linecap: round; stroke-linejoin: round; }
  .kind .count { font-weight: 400; letter-spacing: 0; text-transform: none; color: var(--muted-2); }
  .kind .bytes { margin-left: auto; font-weight: 500; letter-spacing: 0; text-transform: none;
    font-variant-numeric: tabular-nums; color: var(--text-2); }
  .kind table { font-size: 13px; }
  .kind th { padding-top: 8px; }
  .kind td, .kind th { padding-left: 12px; padding-right: 12px; }
  .kind tbody tr:last-child td { border-bottom: none; }
  .actions { text-align: right; white-space: nowrap; }
  .actions button + button { margin-left: 4px; }
  .danger:hover:not(:disabled) { border-color: var(--warn); color: var(--warn); }
  .compare { margin-top: 22px; border-top: 1px solid var(--line-2); padding-top: 16px; }
  h3 { font-size: 13px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); margin: 0 0 12px; }
  h4 { font-size: 12px; color: var(--muted); margin: 16px 0 6px; }
  .row { display: flex; gap: 8px; margin-bottom: 12px; }
  select { flex: 1; padding: 7px 9px; background: var(--bg); border: 1px solid var(--border);
           border-radius: 5px; color: var(--text); font: inherit; }
  .cmp { margin-top: 8px; }
  .muted { color: var(--muted-2); }
  .ok { color: var(--ok); }
  .warn { color: var(--warn); }
  p { margin: 4px 0; }
</style>
