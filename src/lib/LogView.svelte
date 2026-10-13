<script>
  // A build's log, readable while it is still being written.
  //
  // Each line is its own element, keyed, so a new line adds one row instead of rebuilding the
  // whole text: what is selected stays selected, and scrolling up to read something stays put.
  // It follows the end of the log only while it is at the end - scroll up and it stops, and a
  // button says there is more below.
  import { tick } from "svelte";
  import { linesOf, logText } from "./runlog.svelte.js";

  let { area, height = "320px", labels = {} } = $props();

  /** Rows drawn at once. The log keeps far more; this is what the page can paint cheaply. */
  const LIMIT = 1500;

  let query = $state("");
  let quiet = $state(false);
  let stepFilter = $state("");
  let wrap = $state(false);
  let follow = $state(true);
  let copied = $state(false);
  let box;

  let all = $derived(linesOf(area));

  /** Steps that have written anything, for the filter. */
  let steps = $derived([...new Set(all.map((l) => l.step).filter(Boolean))]);

  let filtering = $derived(!!query.trim() || quiet || !!stepFilter);

  let matching = $derived.by(() => {
    if (!filtering) return all;
    const q = query.trim().toLowerCase();
    const out = [];
    // from the end, and stopping once a screenful is found: nobody filters a log to read the
    // beginning of it
    for (let i = all.length - 1; i >= 0 && out.length < LIMIT + 1; i--) {
      const l = all[i];
      if (stepFilter && l.step !== stepFilter) continue;
      if (quiet && l.level === "info") continue;
      if (q && !l.text.toLowerCase().includes(q)) continue;
      out.push(l);
    }
    return out.reverse();
  });

  let shown = $derived(matching.length > LIMIT ? matching.slice(-LIMIT) : matching);
  let hidden = $derived(matching.length - shown.length);

  const time = (at) => new Date(at).toLocaleTimeString([], { hour12: false });

  function onScroll() {
    if (!box) return;
    follow = box.scrollHeight - box.scrollTop - box.clientHeight < 32;
  }

  function toEnd() {
    follow = true;
    if (box) box.scrollTop = box.scrollHeight;
  }

  // follow the end, but only when that is where the reader is
  $effect(() => {
    shown.length;
    shown[shown.length - 1]?.count;
    if (follow) tick().then(() => box && (box.scrollTop = box.scrollHeight));
  });

  async function copy() {
    try {
      await navigator.clipboard.writeText(logText(area, matching));
      copied = true;
      setTimeout(() => (copied = false), 1200);
    } catch {}
  }
</script>

<div class="logview">
  <div class="bar">
    <input class="find" bind:value={query} placeholder="filter" spellcheck="false" />
    {#if steps.length > 1}
      <select bind:value={stepFilter} title="only one step">
        <option value="">all steps</option>
        {#each steps as s}<option value={s}>{labels[s] ?? s}</option>{/each}
      </select>
    {/if}
    <label title="hide ordinary lines, keep warnings and errors">
      <input type="checkbox" bind:checked={quiet} /> warnings only
    </label>
    <label><input type="checkbox" bind:checked={wrap} /> wrap</label>
    <span class="count">{all.length.toLocaleString()} lines{filtering ? `, ${matching.length.toLocaleString()} match` : ""}</span>
    <button class="ghost tiny" onclick={copy} disabled={!matching.length}>{copied ? "copied" : "copy"}</button>
  </div>

  <div class="rows" class:wrap bind:this={box} onscroll={onScroll} style="height:{height}">
    {#if hidden > 0}
      <div class="more">… {hidden.toLocaleString()} earlier lines. Narrow the filter to see them, or copy the log.</div>
    {/if}
    {#each shown as line (line.id)}
      <div class="row {line.level}">
        <span class="t">{time(line.at)}</span>
        <span class="text">{line.text}</span>
        {#if line.count > 1}<span class="n">×{line.count.toLocaleString()}</span>{/if}
      </div>
    {:else}
      <div class="empty">{filtering ? "nothing matches" : "nothing yet"}</div>
    {/each}
  </div>

  {#if !follow}
    <button class="jump" onclick={toEnd}>↓ latest</button>
  {/if}
</div>

<style>
  .logview { position: relative; }
  .bar { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; margin-bottom: 6px;
         font-size: 12px; color: var(--muted); }
  .bar label { display: flex; gap: 4px; align-items: center; cursor: pointer; }
  .find { flex: 1; min-width: 120px; max-width: 260px; padding: 4px 8px; background: var(--bg);
          border: 1px solid var(--border); border-radius: var(--r-sm); color: var(--text); font: inherit; }
  select { padding: 3px 6px; background: var(--bg); border: 1px solid var(--border);
           border-radius: var(--r-sm); color: var(--text); font: inherit; }
  .count { margin-left: auto; color: var(--faint); font-variant-numeric: tabular-nums; }
  .tiny { padding: 1px 8px; font-size: 11px; }
  .rows { overflow: auto; background: var(--bg-sunken); border: 1px solid var(--line-2);
          border-radius: var(--r); font: 11.5px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace;
          padding: 4px 0; }
  .row { display: flex; gap: 10px; padding: 0 10px; white-space: pre; color: var(--text-2); }
  .wrap .row { white-space: pre-wrap; word-break: break-word; }
  .row:hover { background: var(--hover); }
  .t { flex: none; color: var(--faint); user-select: none; font-variant-numeric: tabular-nums; }
  .text { flex: 1; min-width: 0; }
  .n { flex: none; color: var(--warn); font-variant-numeric: tabular-nums; }
  .warn .text { color: var(--warn); }
  .error .text { color: var(--danger); }
  .mark .text, .done .text { color: var(--ok); }
  .phase .text { color: var(--text); font-weight: 600; }
  .command .text { color: var(--muted); }
  .more, .empty { padding: 4px 10px; color: var(--faint); font-style: italic; }
  .jump { position: absolute; right: 18px; bottom: 12px; padding: 3px 10px; font-size: 12px;
          box-shadow: var(--shadow); }
</style>
