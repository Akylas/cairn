<script>
  import Areas from "./lib/Areas.svelte";
  import MapView from "./lib/MapView.svelte";
  import Build from "./lib/Build.svelte";
  import Settings from "./lib/Settings.svelte";
  import Docs from "./lib/Docs.svelte";
  import Runs from "./lib/Runs.svelte";
  import { startLogging } from "./lib/runlog.svelte.js";
  import { onMount } from "svelte";

  // every step event becomes a log line, whichever tab is open
  onMount(() => {
    let stop = () => {};
    startLogging().then((off) => (stop = off));
    return () => stop();
  });

  let tab = $state("areas");
  let areasRef = $state(null);
  let buildRef = $state(null);

  /// Open a running build. The run strip is visible from every tab, so this is how a click on
  /// it gets to the log and the options that run belongs to.
  function openRun(area) {
    tab = "build";
    buildRef?.showArea(area);
  }
  /// Which area the map should open on. A build that just finished is the one worth looking at,
  /// not whichever area happens to sort first.
  let mapArea = $state("");

  function showOnMap(area) {
    mapArea = area;
    tab = "map";
  }

  const TABS = [
    ["areas", "Output"], ["map", "Map"], ["build", "Build"],
    ["settings", "Settings"], ["docs", "Docs"],
  ];

  // digits switch tabs, the way every other tool with a tab bar does - but not while someone is
  // typing a path into a field
  function onKey(event) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const el = event.target;
    if (el instanceof HTMLElement && (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName))) return;
    const index = "12345".indexOf(event.key);
    if (index < 0) return;
    tab = TABS[index][0];
  }
</script>

<svelte:window onkeydown={onKey} />

<main class:wide={tab === "map"}>
  <header>
    <div class="headinner">
      <h1><img class="mark" src="/mark.svg" alt="" width="17" height="18" />Cairn</h1>
      <nav role="tablist">
        {#each TABS as [id, label], i}
          <button role="tab" aria-selected={tab === id} class:active={tab === id}
                  title={`${label}  (${i + 1})`} onclick={() => (tab = id)}>
            {label}<kbd>{i + 1}</kbd>
          </button>
        {/each}
      </nav>
    </div>
  </header>

  <!-- pinned under the header, not inside a tab: a build is the one thing that has to stay
       visible while you are looking at the map it will redraw -->
  <Runs onOpen={openRun} />

  <div class="page">
    <!-- Build and Output stay mounted: a finished run switches to the map, and destroying Build
         on the way out would take the log and the result banner with it. The map is the one tab
         worth tearing down, because it holds two WebGL contexts and a tile server connection. -->
    <div class="inner" hidden={tab !== "areas"}><Areas bind:this={areasRef} /></div>
    <div class="inner" hidden={tab !== "build"}>
      <Build bind:this={buildRef} onFinished={() => areasRef?.refresh()} onShowOnMap={showOnMap} />
    </div>
    {#if tab === "map"}
      <div class="inner"><MapView area={mapArea} /></div>
    {:else if tab === "settings"}
      <div class="inner"><Settings /></div>
    {:else if tab === "docs"}
      <div class="inner"><Docs /></div>
    {/if}
  </div>
</main>

<style>
  /* One palette and one scale for the whole app: every component pulls from these, so a
     change lands everywhere instead of in whichever file was edited last. */
  :global(:root) {
    --bg: #12151a;
    --bg-sunken: #0f1115;
    --surface: #161b22;
    --surface-2: #171c24;
    --card: #1a1f27;
    --hover: #1f2630;
    --line: #1e242d;
    --line-2: #262d38;
    --border: #303845;
    --disabled-bg: #22282f;

    --accent: #2d7a5c;
    --accent-hi: #379469;
    --accent-dim: #1e4436;
    --accent-fg: #cfe8db;

    --ok: #7cc9a0;
    --warn: #d99a5b;
    --danger: #e6584d;

    --text: #dde3ea;
    --text-2: #9aa5b1;
    --text-3: #8a94a2;
    --muted: #7c8896;
    --muted-2: #6b7684;
    --faint: #5d6673;

    --r-sm: 4px;
    --r: 6px;
    --r-lg: 9px;
    --shadow: 0 6px 20px rgba(0, 0, 0, .35);
    --focus: 0 0 0 2px rgba(55, 148, 105, .55);
  }
  :global(body) {
    margin: 0;
    font: 14px/1.5 ui-sans-serif, system-ui, sans-serif;
    background: var(--bg);
    color: var(--text);
  }
  :global(button) {
    padding: 7px 13px; background: var(--accent); border: 0; border-radius: var(--r);
    color: #fff; font: inherit; cursor: pointer;
    transition: background .12s ease, color .12s ease, opacity .12s ease;
  }
  :global(button:hover:not(:disabled)) { background: var(--accent-hi); }
  :global(button.ghost) { background: var(--line-2); color: var(--text-2); }
  :global(button.ghost:hover:not(:disabled)) { background: var(--border); color: var(--text); }
  :global(button:disabled) { background: var(--disabled-bg); color: var(--faint); cursor: not-allowed; }
  /* one focus treatment everywhere, and only for keyboard users */
  :global(:focus-visible) { outline: none; box-shadow: var(--focus); border-radius: var(--r-sm); }
  :global(code) { font-size: 12px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  :global(::-webkit-scrollbar) { width: 10px; height: 10px; }
  :global(::-webkit-scrollbar-thumb) { background: var(--line-2); border-radius: 6px;
                                       border: 2px solid var(--bg); }
  :global(::-webkit-scrollbar-thumb:hover) { background: var(--border); }
  :global(::-webkit-scrollbar-track) { background: transparent; }
  /* The window is the frame: the header is pinned to it and only the page below scrolls.
     Scrolling the tab bar away meant losing the way out of a long Build form. */
  main { height: 100vh; display: flex; flex-direction: column; box-sizing: border-box; }
  header { flex: none; padding: 14px 20px 12px; border-bottom: 1px solid var(--line-2);
           background: var(--bg); }
  .headinner { display: flex; align-items: center; justify-content: space-between;
               max-width: 1100px; margin: 0 auto; width: 100%; }
  .page { flex: 1; min-height: 0; overflow-y: auto; }
  .inner { max-width: 1100px; margin: 0 auto; padding: 18px 20px 60px; }
  .inner[hidden] { display: none; }
  /* the map wants the whole window; every other tab reads better in a column */
  main.wide .headinner { max-width: none; }
  main.wide .page { overflow: hidden; display: flex; flex-direction: column; }
  /* :not([hidden]) so the panes kept mounted behind the map stay collapsed - a plain
     `.inner` rule here outranks `[hidden]` and would draw all three at once */
  main.wide .inner:not([hidden]) { max-width: none; flex: 1; min-height: 0; padding: 12px 16px;
                                   box-sizing: border-box; display: flex; flex-direction: column; }
  h1 { font-size: 15px; font-weight: 600; margin: 0; letter-spacing: -.01em;
       display: flex; align-items: center; gap: 9px; }
  .mark { display: block; width: 17px; height: auto; }
  nav { display: flex; gap: 2px; background: var(--surface); border: 1px solid var(--line-2);
        border-radius: var(--r-lg); padding: 3px; }
  nav button { background: transparent; color: var(--muted); padding: 5px 12px;
               border-radius: var(--r); font-size: 13px; display: flex; align-items: center; gap: 6px; }
  nav button:hover:not(.active) { background: var(--hover); color: var(--text-2); }
  nav button.active { background: var(--card); color: var(--text); box-shadow: var(--shadow); }
  kbd { font: inherit; font-size: 10px; color: var(--faint); background: var(--bg);
        border: 1px solid var(--line-2); border-radius: 3px; padding: 0 4px; line-height: 1.4; }
  nav button.active kbd { color: var(--muted); }
</style>
