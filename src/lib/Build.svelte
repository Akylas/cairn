<script>
  import { invoke, listen, isTauri } from "./api.js";
  import { onMount } from "svelte";
  import Section from "./Section.svelte";
  import { buildConfig } from "./buildconfig.svelte.js";
  import { pushLog, clearLog, linesOf, logText } from "./runlog.svelte.js";
  import LogView from "./LogView.svelte";

  let { onFinished, onShowOnMap } = $props();

  let java = $state(null);
  let javaError = $state("");
  let downloading = $state(null);

  let settings = $state(null);
  let areas = $state([]);
  let jarDefault = $state("");
  let jarUrl = $state("");
  let fetching = $state(null);

  async function fetchJar() {
    fetching = 0;
    const off = await listen("jar-download", (e) => {
      const { done, total } = e.payload;
      fetching = total ? Math.round((done * 100) / total) : 0;
    });
    try {
      jar = await invoke("download_planetiler");
      jarDefault = jar;
    } catch (err) {
      pushLog(area, `ERROR: ${err}`, { level: "error" });
    } finally {
      off();
      fetching = null;
    }
  }
  let steps = $state([]);
  let selected = $state(new Set());
  let planned = $state([]);
  let area = $state("");
  let schemaMode = $state("bundled");
  let schemaYaml = $state("");
  let jar = $state("");

  let optionDefs = $state({});
  let values = $state({});
  let presets = $state([]);
  let defaultPreset = $state("measured");
  let presetName = $state({});
  /// Free-text arguments per step, for the flags this app has no form for.
  let extraArgs = $state({});

  /**
   * Run state per area, keyed by area name.
   *
   * It used to be one set of fields for the whole view. With one build at a time that was merely
   * confusing - switching area mid-run left europe's log scrolling under rhone-alpes' name - and
   * now that areas build concurrently it would be wrong outright: two runs would overwrite each
   * other's progress, and Cancel would have no way to say which one it meant.
   */
  let runState = $state({});
  const BLANK = {
    running: false, phase: "", label: "", percent: 0, runningStep: null,
    commands: {}, cli: {}, results: [], status: {}, runError: "", done: null,
    /// Cancel has been pressed; the run is stopping at its next checkpoint.
    cancelling: false,
    /// The shared resource this run is queued behind, when it is waiting rather than working.
    waitingFor: null,
  };
  function stateFor(name) {
    if (!runState[name]) {
      runState[name] = { ...BLANK, commands: {}, cli: {}, results: [], status: {} };
    }
    return runState[name];
  }
  /** The selected area's run. Everything the progress panel draws reads from here. */
  let cur = $derived(runState[area] ?? BLANK);
  let running = $derived(cur.running);
  /// The log lives in `runlog.svelte.js`, shared with the strip that is visible from every tab.
  let lineCount = $derived(linesOf(area).length);
  let logOpen = $state(true);
  /** What is on disk, per step: the app never decides "already built" from a record alone. */
  let built = $state({});
  let force = $state(new Set());
  let forceAll = $state(false);
  /** Builds running in other areas, so switching away from one does not mean losing it. */
  let elsewhere = $state([]);

  onMount(async () => {
    await detect();
    try {
      settings = await invoke("get_settings");
      const defaults = await invoke("resolved_defaults");
      jarDefault = defaults.planetiler_jar ?? "";
      jarUrl = settings.planetiler_jar_url ?? "";
      // the jar the app already found beats making someone paste the same path
      jar = settings.planetiler_jar ?? "";
      // areas come from the output root, not just the config: a half-finished build is in the
      // output root and nowhere else, which is exactly when this view is needed
      areas = defaults.areas ?? [];
      steps = await invoke("list_steps", { area: areas[0] ?? "" });
      presets = await invoke("list_presets");
      defaultPreset = await invoke("default_preset_name");
      for (const s of steps) {
        optionDefs[s.id] = await invoke("step_options", { step: s.id });
      }
      await selectArea(areas[0] ?? settings.areas?.[0]?.name ?? "");
    } catch (err) {
      javaError = String(err);
    }
    const off = await listen("step", (e) => onEvent(e.payload));
    // a run already in flight when this view mounts - the window was reloaded, or the build was
    // started and the tab left - would otherwise look like no run at all
    await adoptRuns();
    const offRuns = await listen("runs-changed", adoptRuns);
    return () => { off(); offRuns(); };
  });

  /**
   * Pick up what the backend says is running, for runs this view did not start.
   *
   * Only the coarse state: the log lines it missed are gone, and inventing them would be worse
   * than a panel that starts from the current step.
   */
  async function adoptRuns() {
    let active = [];
    try { active = await invoke("active_runs"); }
    catch { active = []; }
    elsewhere = active.filter((r) => r.area !== area);
    for (const run of active) {
      const state = stateFor(run.area);
      state.running = true;
      state.cancelling = run.cancelling;
      state.waitingFor = run.waitingFor ?? null;
      state.runningStep = run.step ?? state.runningStep;
      if (run.step) {
        state.phase = run.phase || state.phase;
        state.label = run.label || state.label;
        state.percent = run.percent ?? state.percent;
      }
      for (const id of run.planned ?? []) {
        if (!state.status[id]) state.status[id] = { state: "queued" };
      }
      for (const id of run.completed ?? []) {
        // the snapshot counts a skipped step as completed, so what this view watched happen
        // wins: "skipped · already on disk" is the more useful of the two
        const known = state.status[id]?.state;
        if (!known || known === "queued") {
          state.status[id] = { ...(state.status[id] ?? {}), state: "done", percent: 100 };
        }
      }
      if (run.step) {
        state.status[run.step] = {
          ...(state.status[run.step] ?? {}),
          state: "running", phase: run.phase, percent: run.percent,
        };
      }
    }
    // a run this view believed in that the backend no longer has is over
    for (const [name, state] of Object.entries(runState)) {
      if (state.running && !active.some((r) => r.area === name)) {
        state.running = false;
        state.cancelling = false;
        state.waitingFor = null;
      }
    }
  }

  async function detect() {
    javaError = "";
    try { java = await invoke("detect_java"); }
    catch (err) { java = null; javaError = String(err); }
  }

  async function downloadJre() {
    downloading = { done: 0, total: null };
    try { java = await invoke("download_java"); }
    catch (err) { javaError = String(err); }
    finally { downloading = null; }
  }

  async function refreshBuilt() {
    if (!area) return;
    try { built = await invoke("build_state", { area, values }); }
    catch { built = {}; }
  }

  /// True while a stored configuration is being poured into the form, so restoring one does not
  /// immediately save itself back.
  let loading = $state(false);

  /**
   * Switch to another area, saving the one being left and restoring the one being opened.
   *
   * An area's options are part of what the area *is* - europe is not built at rhone-alpes'
   * maxzoom - so they are stored per area rather than as one global form. Without this every
   * launch, and every area switch, silently reset the form to the default preset while the
   * fields still looked deliberate.
   */
  async function selectArea(next) {
    flushSave();
    area = next;
    loading = true;
    try {
      let stored = null;
      try { stored = await invoke("get_build_config", { area: next }); }
      catch { stored = null; }
      applyConfig(stored);
      // what was just restored is what is stored, so the form does not save itself back the
      // instant it is filled in
      lastSaved = JSON.stringify(formState());
    } finally {
      loading = false;
    }
    // `writes` in each step's description is resolved for the area, so the list is re-read
    try { steps = await invoke("list_steps", { area: next }); } catch {}
    await replan();
    await refreshBuilt();
    elsewhere = elsewhere.filter((r) => r.area !== next);
  }

  /** Seed every step's options from the default preset - the form's state with nothing stored. */
  function seededValues() {
    const seeded = {};
    for (const s of steps) {
      // cairn is here to rebuild this repository's tiles, so the fields should show the values
      // that will actually be used - a blank form that silently builds something else is a trap
      const seed = presets.find((p) => p.step === s.id && p.name === defaultPreset);
      seeded[s.id] = seed ? { ...seed.values } : {};
    }
    return seeded;
  }

  function applyConfig(stored) {
    const seeded = seededValues();
    // `force` and `force all` are deliberately never restored: they delete and rebuild hours of
    // output, and a switch that silently re-arms them is not a setting, it is a trap
    force = new Set();
    forceAll = false;
    if (!stored) {
      values = seeded;
      selected = new Set(["basemap"]);
      extraArgs = {};
      schemaMode = "bundled";
      schemaYaml = "";
      return;
    }
    // a step added to the graph since this area was last configured still gets the preset's
    // values rather than an empty form
    values = { ...seeded, ...(stored.values ?? {}) };
    selected = new Set(stored.steps ?? []);
    extraArgs = { ...(stored.extraArgs ?? {}) };
    schemaMode = stored.schemaMode ?? "bundled";
    schemaYaml = stored.schemaYaml ?? "";
    if (stored.jar) jar = stored.jar;
  }

  /// Written through on a short delay: the form changes on every keystroke, and each save
  /// rewrites the whole file.
  let saveTimer = null;
  let lastSaved = "";

  /** Exactly what gets stored, so "has it changed?" is one comparison and not a guess. */
  function formState() {
    return {
      steps: [...selected], values, extraArgs,
      // never stored, so a restored area cannot silently re-arm a rebuild
      force: [], forceAll: false,
      schemaMode, schemaYaml, jar: jar || null,
    };
  }

  function scheduleSave() {
    if (loading || !area) return;
    // the stringify is also what makes the effect calling this track a value three levels down
    if (JSON.stringify(formState()) === lastSaved) return;
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => flushSave(), 400);
  }

  /** Write now rather than in 400ms - used when leaving an area, which is when it would be lost. */
  function flushSave() {
    clearTimeout(saveTimer);
    saveTimer = null;
    if (loading || !area) return;
    const build = formState();
    const serialised = JSON.stringify(build);
    if (serialised === lastSaved) return;
    lastSaved = serialised;
    invoke("save_build_config", { area, build }).catch(() => {});
  }

  /** Delete what a step produced. That, not a record, is what makes it run again. */
  async function clearOutputs(step) {
    confirming = "";
    clearTimeout(disarmTimer);
    const outputs = (built[step]?.outputs ?? []).map((f) => f.name);
    const what = outputs.length ? outputs.join(", ") : labelFor(step);
    if (!confirm(`Delete ${what}?`)) return;
    try {
      await invoke("clear_build_state", { area, step, deleteOutputs: true });
      await refreshBuilt();
      onFinished?.();
    } catch (err) {
      javaError = String(err);
    }
  }

  function toggleForce(step) {
    const next = new Set(force);
    if (next.has(step)) {
      next.delete(step);
    } else {
      next.add(step);
      // forcing a step that is not selected does nothing at all: only selected steps are sent to
      // the runner, so the button looked like it armed a rebuild while quietly changing nothing
      if (!selected.has(step)) {
        selected = new Set(selected).add(step);
        replan();
      }
    }
    force = next;
  }

  /// Eight steps in one flat list is how someone ends up running the basemap without the
  /// extract it reads. Grouping by what a step produces puts the prerequisites above the
  /// things that consume them, and gives the eye something to land on other than seven
  /// identical rows.
  const STAGES = [
    {
      id: "source",
      label: "Source data",
      note: "downloaded once, read by everything below",
      // an arrow into a tray
      icon: "M8 1.8v7.6m0 0 2.8-2.8M8 9.4 5.2 6.6M2.4 10.8v2.4h11.2v-2.4",
      steps: ["download_osm", "elevation_tiles"],
    },
    {
      id: "tiles",
      label: "Tiles",
      note: "what the map renders",
      // stacked layers
      icon: "M2 5.2 8 2.2l6 3-6 3-6-3Zm0 3.4 6 3 6-3M2 11.6l6 3 6-3",
      steps: ["basemap", "routes", "bathymap", "terrain_rgb"],
    },
    {
      id: "routing",
      label: "Routing",
      note: "the graph the phone routes on",
      // a navigation arrow
      icon: "M14 2.4 2 7.2l4.8 2 2 4.8L14 2.4Z",
      steps: ["valhalla_admins", "valhalla_tiles", "valhalla_package"],
    },
  ];

  let stepGroups = $derived.by(() => {
    const placed = new Set(STAGES.flatMap((g) => g.steps));
    const groups = STAGES.map((g) => ({
      ...g,
      items: g.steps.map((id) => steps.find((s) => s.id === id)).filter(Boolean),
    }));
    // a step the backend grows before this list catches up still has to be reachable
    const rest = steps.filter((s) => !placed.has(s.id));
    if (rest.length) {
      groups.push({ id: "other", label: "Other", note: "", icon: STAGES[1].icon, items: rest });
    }
    return groups
      .filter((g) => g.items.length)
      .map((g) => ({
        ...g,
        chosen: g.items.filter((s) => selected.has(s.id) || planned.includes(s.id)).length,
        active: g.items.some((s) => cur.status[s.id]?.state === "running"),
        broken: g.items.some((s) => cur.status[s.id]?.state === "failed"),
      }));
  });

  /// Steps whose output the map can draw. A run of only `download_osm` has nothing new to
  /// show, so it should not yank anyone over to the map tab.
  const MAPPABLE = ["basemap", "routes", "terrain_rgb"];

  const fmtSize = (b) =>
    b > 1073741824 ? `${(b / 1073741824).toFixed(1)} GB`
    : b > 1048576 ? `${(b / 1048576).toFixed(1)} MB`
    : `${(b / 1024).toFixed(0)} KB`;
  const fmtWhen = (secs) => (secs ? new Date(secs * 1000).toLocaleString() : "");

  async function replan() {
    try { planned = await invoke("plan_steps", { steps: [...selected] }); }
    catch { planned = [...selected]; }
  }

  async function toggle(id) {
    const next = new Set(selected);
    next.has(id) ? next.delete(id) : next.add(id);
    selected = next;
    await replan();
  }

  function applyPreset(preset) {
    values = { ...values, [preset.step]: { ...preset.values } };
  }

  async function savePreset(step) {
    const name = (presetName[step] ?? "").trim();
    if (!name || !step) return;
    await invoke("save_preset", {
      preset: { name, step, description: "", values: values[step] ?? {} },
    });
    presets = await invoke("list_presets");
    presetName = { ...presetName, [step]: "" };
  }

  function setValue(step, key, raw, kind) {
    const next = { ...(values[step] ?? {}) };
    if (raw === "" && kind !== "text") delete next[key];
    else if (kind === "bool") next[key] = raw;
    else if (kind === "int") next[key] = parseInt(raw, 10);
    else if (kind === "float") next[key] = parseFloat(raw);
    else next[key] = raw;
    values = { ...values, [step]: next };
  }

  function clearValue(step, key) {
    const next = { ...(values[step] ?? {}) };
    delete next[key];
    values = { ...values, [step]: next };
  }

  /// Deleting is one tap away from destroying an hour of build, so the first tap only arms it.
  /// A second tap on the armed button deletes; anything else disarms.
  let confirming = $state("");
  let disarmTimer = null;
  function arm(id) {
    confirming = id;
    clearTimeout(disarmTimer);
    disarmTimer = setTimeout(() => (confirming = ""), 4000);
  }

  async function reveal(path) {
    try { await invoke("reveal", { path }); }
    catch (err) { pushLog(area, `ERROR: ${err}`, { level: "error" }); }
  }

  /// Steps whose description is showing. The prose comes from the backend, so the graph and
  /// the explanation of it cannot disagree.
  let explained = $state(new Set());
  function explain(id) {
    const next = new Set(explained);
    next.has(id) ? next.delete(id) : next.add(id);
    explained = next;
  }

  let copiedLine = $state("");
  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      copiedLine = text;
      setTimeout(() => (copiedLine = ""), 1200);
    } catch {}
  }

  let logCopied = $state(false);
  async function copyLog() {
    try {
      // the commands go with it: a pasted log without them cannot be diagnosed
      const head = Object.values(cur.commands).map((argv) => `$ ${argv.join(" ")}`);
      await navigator.clipboard.writeText([...head, "", logText(area)].join("\n"));
      logCopied = true;
      setTimeout(() => (logCopied = false), 1200);
    } catch {}
  }

  let summaryLine = $derived.by(() => {
    if (!cur.results.length) return "";
    const bad = cur.results.filter((r) => !r.ok).length;
    return bad ? `${bad} of ${cur.results.length} failed` : `${cur.results.length} finished`;
  });

  function mark(state, step, patch) {
    state.status = { ...state.status, [step]: { ...(state.status[step] ?? {}), ...patch } };
  }

  /// Events carry the area they came from, so one arriving while another area is on screen
  /// updates that area's panel and leaves this one alone.
  function onEvent(ev) {
    const state = stateFor(ev.area);
    switch (ev.event) {
      case "started":
        state.running = true; state.phase = "starting"; state.runningStep = ev.step;
        state.waitingFor = null;
        mark(state, ev.step, { state: "running", percent: 0, phase: "starting" });
        break;
      case "phase":
        state.phase = ev.name;
        mark(state, ev.step, { phase: ev.name });
        break;
      case "progress":
        state.label = ev.label; state.percent = ev.percent;
        mark(state, ev.step, { percent: ev.percent, label: ev.label });
        break;
      case "cli":
        // what repeats this step in a terminal; kept apart from `command`, which is the
        // subprocess it spawns
        state.cli = { ...state.cli, [ev.step]: ev.line };
        break;
      case "command":
        // kept out of `lines` so it cannot scroll away: this is the record of which jar ran
        // and which flags reached it, which is the first thing to check when the output looks
        // like an older schema
        state.commands = { ...state.commands, [ev.step]: ev.argv };
        break;
      case "log":
        // the backend says so before it starts waiting; it is the difference between a step
        // that is queued and one that has hung
        if (ev.line.startsWith("waiting: ")) state.waitingFor = ev.line.slice(9);
        break;
      case "finished":
        state.results = [...state.results, ev];
        state.runningStep = null;
        mark(state, ev.step, { state: ev.ok ? "done" : "failed", elapsed: ev.elapsed, percent: 100 });
        if (ev.area === area) refreshBuilt();
        break;
      case "skipped":
        mark(state, ev.step, { state: "skipped", reason: ev.reason });
        break;
    }
  }

  async function run() {
    // captured: `area` is a moving target once a run can outlive the selection that started it
    const at = area;
    const state = stateFor(at);
    Object.assign(state, {
      ...BLANK, running: true,
      commands: {}, cli: {}, results: [],
      // queued up front, so the list reads as a plan rather than filling in as it goes
      status: Object.fromEntries(planned.map((id) => [id, { state: "queued" }])),
    });
    const attempted = [...planned];
    clearLog(at);
    try {
      await invoke("run_steps", {
        req: {
          area: at, steps: [...selected], values, extraArgs,
          schemaYaml: schemaMode === "yaml" ? schemaYaml : null,
          jar: jar || null,
          force: [...force],
          forceAll,
        },
      });
    } catch (err) {
      // shown beside the button as well as logged: a run refused up front produces no
      // events at all, so the log alone leaves the UI looking simply inert
      state.runError = String(err);
      pushLog(at, `ERROR: ${err}`, { level: "error" });
    } finally {
      state.running = false;
      state.cancelling = false;
      state.waitingFor = null;
      if (at === area) await refreshBuilt();
      onFinished?.();
      await adoptRuns();
      state.done = outcome(state, attempted);
      // the map is where a finished build is actually inspected, and it is two clicks away at
      // the moment the log stops moving. Only for runs that produced something drawable, and
      // only for the area actually on screen - yanking the view to a build that finished in the
      // background is not help.
      if (state.done.ok && at === area && attempted.some((id) => MAPPABLE.includes(id))) {
        onShowOnMap?.(at);
      }
    }
  }

  async function cancel(which = area) {
    const state = stateFor(which);
    state.cancelling = true;
    try { await invoke("cancel_run", { area: which }); }
    catch (err) { pushLog(which, `ERROR: ${err}`, { level: "error" }); state.cancelling = false; }
  }

  /// Read the run's result off the per-step state rather than the `finished` events alone:
  /// a step that was skipped because its output was already there never emits one.
  function outcome(state, attempted) {
    const of = (want) => attempted.filter((id) => state.status[id]?.state === want);
    const failed = of("failed");
    const untouched = attempted.filter(
      (id) => !["done", "skipped", "failed"].includes(state.status[id]?.state),
    );
    return {
      ok: !state.runError && !failed.length && !untouched.length,
      built: of("done"),
      skipped: of("skipped"),
      failed,
      stopped: untouched,
      note: state.runError,
    };
  }

  /// Let the run list open the area a background build belongs to.
  export function showArea(name) {
    if (name && name !== area) selectArea(name);
  }

  function groupsFor(step) {
    const by = new Map();
    for (const d of optionDefs[step] ?? []) {
      if (!by.has(d.group)) by.set(d.group, []);
      by.get(d.group).push(d);
    }
    return [...by.entries()];
  }
  /// Steps that take arbitrary tool arguments, and where their documentation lives. Mirroring
  /// planetiler's whole flag list here would be wrong by its next release; passing them through
  /// and pointing at the real reference stays right.
  const PASSTHROUGH = {
    basemap: {
      tool: "planetiler",
      docs: "https://github.com/onthegomap/planetiler/blob/main/PLANET.md",
      placeholder: "--max-point-buffer=4 --mlt-shared-dict",
    },
    routes: {
      tool: "planetiler",
      docs: "https://github.com/onthegomap/planetiler/blob/main/PLANET.md",
      placeholder: "--max-point-buffer=4",
    },
  };

  const labelFor = (id) => steps.find((s) => s.id === id)?.label ?? id;
  const setCountFor = (step) => Object.keys(values[step] ?? {}).length;

  // options for everything that will actually run, dependencies included - selecting two steps
  // used to leave only the last-clicked one configurable
  let optionSteps = $derived(planned.length ? planned : [...selected]);

  // The command line for each step, asked of the backend: it is built from the same values and
  // settings the runner is handed, and checked against the real parser. A copy assembled here
  // from the option schema had drifted into printing lines the parser refuses.
  let cliLines = $state([]);
  let cliTimer;
  $effect(() => {
    const request = {
      area, steps: [...optionSteps], values: $state.snapshot(values),
      extraArgs: $state.snapshot(extraArgs),
    };
    clearTimeout(cliTimer);
    cliTimer = setTimeout(async () => {
      if (!request.area || !request.steps.length) { cliLines = []; return; }
      try { cliLines = await invoke("cli_script", request); }
      catch { cliLines = []; }
    }, 150);
  });
  const lineFor = (step) => cliLines.find((l) => l.step === step)?.line ?? "";

  // the CLI view shows this same run as a command line; it reads what the form holds rather
  // than being told separately, so the two cannot describe different builds
  $effect(() => {
    buildConfig.area = area;
    buildConfig.steps = optionSteps;
    buildConfig.lines = cliLines;
  });

  // An interrupted terrain build keeps what it wrote, and the next run continues it. Said here,
  // because otherwise the only sign of it is a run that is oddly fast to start.
  let partial = $state(null);
  async function refreshPartial() {
    if (!area) { partial = null; return; }
    try {
      partial = await invoke("terrain_partial", {
        area, pmtiles: values.terrain_rgb?.pmtiles === true,
      });
    } catch { partial = null; }
  }
  $effect(() => {
    area; values.terrain_rgb?.pmtiles; running;
    refreshPartial();
  });
  async function discardPartial() {
    try {
      await invoke("terrain_discard_partial", {
        area, pmtiles: values.terrain_rgb?.pmtiles === true,
      });
    } catch (err) { javaError = String(err); }
    await refreshPartial();
  }

  // Remember the form, per area.
  $effect(() => {
    scheduleSave();
  });

  let ready = $derived(java && (jar || jarDefault) && area && selected.size && !running);
</script>

{#if !isTauri}
  <p class="warn">Browser dev mode — builds run only inside the app.</p>
{/if}

<Section title="1 · Runtime" open={!java}
         badge={java ? "ready" : "needs Java"}
         subtitle={java ? `Java ${java.version} · ${area || "no area"}` : ""}>
  {#if java}
    <p class="ok">Java {java.version} · <code>{java.source}</code></p>
  {:else if downloading}
    <p>Downloading JRE… {(downloading.done / 1048576).toFixed(1)} MB</p>
  {:else}
    <p class="warn">No Java 21+ found. {javaError}</p>
    <button onclick={downloadJre}>Download JRE 21</button>
  {/if}
  <div class="pair">
    <label>Area
      {#if areas.length}
        <select value={area} onchange={(e) => selectArea(e.target.value)}>
          {#each areas as a}<option value={a}>{a}</option>{/each}
          {#if !areas.includes(area)}<option value={area}>{area}</option>{/if}
        </select>
      {:else}
        <input value={area} placeholder="rhone-alpes"
               onchange={(e) => selectArea(e.target.value)} />
      {/if}
    </label>
    <label>Planetiler jar
      <input bind:value={jar} placeholder={jarDefault || "none found"} />
      {#if !jarDefault && !jar}
        <span class="hint">
          {#if jarUrl}
            <button class="ghost tiny" onclick={fetchJar} disabled={!!fetching}>
              {fetching ? `downloading ${fetching}%` : "Download it"}
            </button>
            from <code>{jarUrl}</code>
          {:else}
            Nothing to run builds with. Point Settings at a jar, or set a URL there to fetch one -
            it has to be a build of this pipeline's planetiler fork.
          {/if}
        </span>
      {/if}
    </label>
  </div>
  <div class="pair">
    <label>Schema
      <select bind:value={schemaMode}>
        <option value="bundled">Bundled OpenMapTiles fork</option>
        <option value="yaml">YAML schema (custommap)</option>
      </select>
    </label>
    {#if schemaMode === "yaml"}
      <label>Schema file<input bind:value={schemaYaml} placeholder="…/shortbread.yml" /></label>
    {/if}
  </div>
</Section>

<Section title="2 · Steps" subtitle={planned.length ? `${planned.length} to run` : "nothing selected"}>
  {#each stepGroups as g}
    <section class="stage" class:active={g.active} class:broken={g.broken}>
      <h3>
        <svg class="icon" viewBox="0 0 16 16" aria-hidden="true"><path d={g.icon} /></svg>
        {g.label}
        {#if g.note}<span class="note">{g.note}</span>{/if}
        <span class="chosen">{g.chosen ? `${g.chosen} of ${g.items.length}` : "none"}</span>
      </h3>
  <ul class="steplist">
    {#each g.items as s}
      {@const st = cur.status[s.id] ?? {}}
      {@const auto = !selected.has(s.id) && planned.includes(s.id)}
      {@const disk = built[s.id]}
      <li class="steprow" class:on={selected.has(s.id)} class:auto>
        <button class="pick" onclick={() => toggle(s.id)} disabled={running}
                title="include this step">
          <span class="box" class:checked={selected.has(s.id)} class:auto>
            {#if selected.has(s.id)}✓{:else if auto}+{/if}
          </span>
          <span class="sname">{s.label}</span>
        </button>

        {#if auto}<span class="tag soft">dependency</span>{/if}

        {#if st.state === "running"}
          <span class="stat run">{st.phase ?? "running"} {st.percent ?? 0}%</span>
        {:else if st.state === "done"}
          <span class="stat ok">just built{#if st.elapsed} · {st.elapsed}{/if}</span>
        {:else if st.state === "failed"}
          <span class="stat bad">failed</span>
        {:else if st.state === "skipped"}
          <span class="stat">skipped · {st.reason}</span>
        {:else if st.state === "queued"}
          <span class="stat">queued</span>
        {:else if disk?.state === "built"}
          <span class="stat ok" title={`${disk.outputs.map((f) => f.name).join(", ")}\n${fmtWhen(disk.finished_at)}`}>
            built · {disk.outputs.map((f) => (f.dir ? "directory" : fmtSize(f.bytes))).join(" + ")}
          </span>
        {:else if disk?.state === "options_changed"}
          <span class="stat warnish" title={`changed: ${disk.changed.join(", ")}`}>
            options changed
          </span>
        {:else if disk?.state === "missing"}
          <span class="stat">not built</span>
        {/if}

        {#if disk?.state === "built" || disk?.state === "options_changed"}
          {@const files = (disk.outputs ?? []).filter((f) => !f.dir)}
          <button class="mini" class:on={force.has(s.id)} disabled={running}
                  title="run this step even though its output is there"
                  onclick={() => toggleForce(s.id)}>force</button>
          {#if (s.writes ?? []).length}
            <button class="mini" title={`show ${s.writes[0]} in the file manager`}
                    onclick={() => reveal(s.writes[0])}>show</button>
          {/if}
          {#if files.length}
            {#if confirming === s.id}
              <button class="mini danger armed" disabled={running}
                      title={`delete ${files.map((f) => f.name).join(", ")}`}
                      onclick={() => clearOutputs(s.id)}>delete {fmtSize(files.reduce((n, f) => n + f.bytes, 0))}?</button>
              <button class="mini" onclick={() => (confirming = "")}>cancel</button>
            {:else}
              <button class="mini danger" disabled={running}
                      title="delete the output, so the step runs again"
                      onclick={() => arm(s.id)}>delete</button>
            {/if}
          {/if}
        {/if}

        <button class="mini info" class:on={explained.has(s.id)}
                title="what this step does" aria-label="what this step does"
                onclick={() => explain(s.id)}>?</button>

        {#if st.state === "running"}
          <div class="bar"><div class="fill" style="width:{st.percent ?? 0}%"></div></div>
        {/if}
      </li>

      {#if explained.has(s.id)}
        <li class="about">
          <p>{s.summary}</p>
          <dl>
            <dt>Needs</dt><dd>{s.reads}</dd>
            {#if s.deps?.length}
              <dt>After</dt><dd>{s.deps.map(labelFor).join(", ")}</dd>
            {/if}
            {#if s.writes?.length}
              <dt>Writes</dt>
              <dd>{#each s.writes as w}<code>{w}</code>{/each}</dd>
            {/if}
            <dt>Terminal</dt>
            <dd><code>cairn {s.command} --area {area || "<area>"}</code></dd>
          </dl>
        </li>
      {/if}
    {/each}
  </ul>
    </section>
  {/each}

  <div class="runbar">
    <label class="forceall" title="rebuild the steps you ticked, ignoring what is on disk. Steps added only because they are needed are still skipped when already built">
      <input type="checkbox" bind:checked={forceAll} disabled={running} /> force all
    </label>
    <button onclick={run} disabled={!ready}>
      {running ? "Running…" : `Run ${planned.length || ""}`}
    </button>
    <button class="ghost" onclick={() => cancel()} disabled={!running || cur.cancelling}>
      {cur.cancelling ? "Cancelling…" : "Cancel"}
    </button>
    {#if planned.length}
      <span class="plan">{planned.map(labelFor).join(" → ")}</span>
    {/if}
  </div>

  {#if cliLines.length}
    {@const script = cliLines.map((l) => l.line).join("\n")}
    <details class="group terminal">
      <summary>
        Repeat this from a terminal
        <button class="ghost tiny" onclick={(e) => { e.preventDefault(); copy(script); }}>
          {copiedLine === script ? "copied" : "copy"}
        </button>
      </summary>
      <pre class="argv">{script}</pre>
      <p class="muted small">
        The command line runs exactly the steps shown and ignores what is already built; it
        carries the paths this app is set to use. Stopping it with Ctrl-C keeps a terrain build's
        progress, and running the same line again continues it.
      </p>
    </details>
  {/if}

  {#if cur.waitingFor}
    <p class="warn runerr">
      Queued: another area is using <code>{cur.waitingFor}</code>. It starts when that one lets go.
    </p>
  {/if}

  <!-- Areas build at the same time now, so the one on screen is not the only one that matters.
       Without this, switching away from a running build is indistinguishable from it stopping. -->
  {#if elsewhere.length}
    <div class="othersbar">
      {#each elsewhere as r}
        <button class="other" onclick={() => selectArea(r.area)}
                title={`open ${r.area}`}>
          <span class="dot live"></span>
          <strong>{r.area}</strong>
          <span>{r.step ? labelFor(r.step) : "starting"}{r.waitingFor ? " · queued" : ""}</span>
          <span class="pctinline">{r.percent ?? 0}%</span>
        </button>
      {/each}
    </div>
  {/if}

  {#if cur.runError}
    <p class="warn runerr">{cur.runError}</p>
  {/if}

  {#if cur.done && !running}
    {@const done = cur.done}
    <div class="done" class:bad={!done.ok}>
      <span class="dot"></span>
      <div class="what">
        <strong>{done.ok ? `${area} is built` : "The run stopped early"}</strong>
        <span class="detail">
          {#if done.built.length}{done.built.map(labelFor).join(", ")} built{/if}
          {#if done.skipped.length}
            {done.built.length ? " · " : ""}{done.skipped.length} already on disk
          {/if}
          {#if done.failed.length}
            {done.built.length || done.skipped.length ? " · " : ""}{done.failed.map(labelFor).join(", ")} failed
          {/if}
          {#if done.stopped.length}
            {" · "}{done.stopped.map(labelFor).join(", ")} never ran
          {/if}
          {#if done.note}{done.note}{/if}
        </span>
      </div>
      {#if done.built.length || done.skipped.length}
        <button class="ghost" onclick={() => onShowOnMap?.(area)}>Show on map</button>
      {/if}
      <button class="ghost tiny" onclick={() => (stateFor(area).done = null)} title="dismiss">×</button>
    </div>
  {/if}

  <!-- progress belongs with the button that starts it: as its own section it sat below every
       per-step options panel, off the bottom of the window, so a run that failed instantly
       looked like a run that did nothing -->
  {#if running || lineCount || cur.results.length}
    <div class="progress">
      <div class="phead">
        <span class="pstep">
          {running ? `${area} · ${labelFor(cur.runningStep) || ""} · ${cur.phase}` : summaryLine}
        </span>
        <span class="pct">{cur.percent}%</span>
      </div>
      <div class="bar big"><div class="fill" style="width:{cur.percent}%"></div></div>
      <p class="muted small">{cur.label || cur.phase}</p>

      {#each Object.entries(cur.cli) as [step, line]}
        <details class="group cmd">
          <summary>
            {labelFor(step)} · same from a terminal
            <button class="ghost tiny" onclick={(e) => { e.preventDefault(); copy(line); }}>
              {copiedLine === line ? "copied" : "copy"}
            </button>
          </summary>
          <pre class="argv">{line}</pre>
        </details>
      {/each}

      {#each Object.entries(cur.commands) as [step, argv]}
        <!-- open: seeing which flags reached planetiler is the point of keeping it -->
        <details class="group cmd" open>
          <summary>
            {labelFor(step)} command
            <button class="ghost tiny" onclick={(e) => { e.preventDefault(); copy(argv.join(" ")); }}>
              {copiedLine === argv.join(" ") ? "copied" : "copy"}
            </button>
          </summary>
          <pre class="argv">{argv.join("\n  ")}</pre>
        </details>
      {/each}

      <div class="logbox">
        <div class="loghead">
          <button class="ghost tiny" onclick={() => (logOpen = !logOpen)}>
            {logOpen ? "▾" : "▸"} Log <span class="lines">{lineCount.toLocaleString()} lines</span>
          </button>
          <button class="ghost tiny" onclick={copyLog}>{logCopied ? "copied" : "copy with commands"}</button>
        </div>
        <!-- a button and an `{#if}`, not a <details open={...}>: the open state is the reader's,
             and nothing a progress update does should be able to take it back -->
        {#if logOpen}
          <LogView {area} height="360px" labels={Object.fromEntries(steps.map((s) => [s.id, s.label]))} />
        {/if}
      </div>
    </div>
  {/if}
</Section>

{#each optionSteps as step, i}
  <Section title={`3.${i + 1} · ${labelFor(step)}`} open={false}
           subtitle={setCountFor(step) ? `${setCountFor(step)} set` : "defaults"}>
    {#if step === "terrain_rgb" && partial}
      <p class="warn partial">
        An interrupted build is here: up to z{partial.zoom}, {partial.tiles.toLocaleString()} tiles,
        {fmtSize(partial.bytes)}. Running again continues it. It was started with
        particular settings, and a run with different ones is refused rather than mixed in;
        tick <em>Start over</em> below to discard it, or
        <button class="ghost tiny" onclick={discardPartial} disabled={running}>discard it now</button>.
      </p>
    {/if}
    {#if lineFor(step)}
      {@const line = lineFor(step)}
      <div class="asline">
        <code>{line}</code>
        <button class="ghost tiny" onclick={() => copy(line)}>
          {copiedLine === line ? "copied" : "copy"}
        </button>
      </div>
    {/if}

    {#if PASSTHROUGH[step]}
      <label class="extra">
        Extra {PASSTHROUGH[step].tool} arguments
        <input value={extraArgs[step] ?? ""} spellcheck="false"
               placeholder={PASSTHROUGH[step].placeholder}
               oninput={(e) => (extraArgs = { ...extraArgs, [step]: e.target.value })} />
        <span class="hint">
          Passed through verbatim, for the flags above do not cover.
          <a href={PASSTHROUGH[step].docs} target="_blank" rel="noreferrer">{PASSTHROUGH[step].tool} documentation</a>
        </span>
      </label>
    {/if}

    <div class="presets">
      {#each presets.filter((p) => p.step === step) as p}
        <button class="ghost" title={p.description} onclick={() => applyPreset(p)}>{p.name}</button>
      {/each}
      <input value={presetName[step] ?? ""} placeholder="save current as…"
             oninput={(e) => (presetName = { ...presetName, [step]: e.target.value })} />
      <button class="ghost" onclick={() => savePreset(step)}
              disabled={!(presetName[step] ?? "").trim()}>Save</button>
    </div>

    {#each groupsFor(step) as [group, defs]}
      {@const groupSet = defs.filter((d) => values[step]?.[d.key] !== undefined).length}
      <details class="group" open={groupSet > 0}>
        <summary>{group}{#if groupSet}<span class="count">{groupSet}</span>{/if}</summary>
        {#each defs as d}
          {@const val = values[step]?.[d.key]}
          {@const set = val !== undefined}
          <div class="opt" class:set>
            <div class="opthead">
              <label for={`${step}-${d.key}`}>{d.label}</label>
              {#if set}<button class="clear" onclick={() => clearValue(step, d.key)}>reset</button>{/if}
            </div>
            {#if d.kind.type === "bool"}
              <input id={`${step}-${d.key}`} type="checkbox" checked={val === true}
                     onchange={(e) => setValue(step, d.key, e.target.checked, "bool")} />
            {:else if d.kind.type === "choice"}
              <select id={`${step}-${d.key}`} value={val ?? ""}
                      onchange={(e) => setValue(step, d.key, e.target.value, "choice")}>
                <option value="">— unset —</option>
                {#each d.kind.choices as c}<option value={c}>{c}</option>{/each}
              </select>
            {:else}
              <input id={`${step}-${d.key}`}
                     type={d.kind.type === "text" ? "text" : "number"}
                     step={d.kind.type === "float" ? "0.05" : "1"}
                     value={val ?? ""}
                     oninput={(e) => setValue(step, d.key, e.target.value, d.kind.type)} />
            {/if}
            <p class="help">{d.help}</p>
            <p class="hint">unset → {d.hint}</p>
          </div>
        {/each}
      </details>
    {/each}
  </Section>
{/each}

<style>
  .progress { margin-top: 12px; padding-top: 12px; border-top: 1px solid var(--line-2); }
  .phead { display: flex; align-items: baseline; gap: 10px; margin-bottom: 6px; font-size: 12px; }
  .pstep { color: var(--text-2); }
  .phead .pct { margin-left: auto; font-variant-numeric: tabular-nums; color: var(--text); }
  .runerr { margin: 10px 0 0; }
  .group { border-top: 1px solid var(--line); }
  .group summary { cursor: pointer; padding: 8px 0; font-size: 11px; text-transform: uppercase;
                   letter-spacing: .05em; color: var(--muted); list-style: none; display: flex;
                   align-items: center; gap: 8px; }
  .group summary::-webkit-details-marker { display: none; }
  .group summary::before { content: "›"; color: var(--faint); display: inline-block; }
  .group[open] summary::before { transform: rotate(90deg); }
  .count { background: var(--accent); color: var(--accent-fg); font-size: 10px; padding: 0 6px;
           border-radius: 8px; }
  label { display: block; color: var(--text-2); font-size: 13px; }
  input, select { display: block; width: 100%; margin-top: 4px; padding: 6px 9px; background: var(--bg);
          border: 1px solid var(--border); border-radius: 5px; color: var(--text); font: inherit;
          font-size: 13px; box-sizing: border-box; }
  input[type="checkbox"] { width: auto; }
  .pair { display: flex; gap: 8px; margin-top: 10px; }
  .pair label { flex: 1; }
  /* One card per stage, matching the Output tab: the accent bar and the icon are what the eye
     catches when scrolling past, which a row of seven identical checkboxes never gave it. */
  .stage { margin-bottom: 10px; border: 1px solid var(--line-2); border-radius: var(--r);
           background: var(--surface); overflow: hidden; }
  .stage h3 { display: flex; align-items: center; gap: 9px; margin: 0; padding: 8px 12px;
              background: var(--hover); border-bottom: 1px solid var(--line-2);
              font-size: 12px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase;
              color: var(--text); }
  .stage h3::before { content: ""; width: 3px; align-self: stretch; margin: -8px 3px -8px -12px;
                      background: var(--accent); }
  .stage.active h3::before { background: var(--ok); }
  .stage.broken h3::before { background: var(--danger); }
  .stage .icon { width: 16px; height: 16px; flex: none; color: var(--accent-hi); fill: none;
                 stroke: currentColor; stroke-width: 1.4; stroke-linecap: round;
                 stroke-linejoin: round; }
  .stage .note { font-weight: 400; letter-spacing: 0; text-transform: none; color: var(--muted-2);
                 font-size: 12px; }
  .stage .chosen { margin-left: auto; font-weight: 500; letter-spacing: 0; text-transform: none;
                   font-size: 12px; color: var(--text-2); font-variant-numeric: tabular-nums; }
  .steplist { list-style: none; margin: 0; padding: 6px; display: flex;
              flex-direction: column; gap: 2px; }
  /* the completion banner: a run that ends while you are looking at the window should say so */
  .done { display: flex; align-items: center; gap: 10px; margin-top: 12px; padding: 9px 12px;
          border: 1px solid color-mix(in srgb, var(--ok) 40%, transparent); border-radius: var(--r);
          background: color-mix(in srgb, var(--ok) 9%, transparent); }
  .done.bad { border-color: color-mix(in srgb, var(--danger) 45%, transparent);
              background: color-mix(in srgb, var(--danger) 9%, transparent); }
  .done .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--ok); flex: none; }
  .done.bad .dot { background: var(--danger); }
  .done .what { display: flex; flex-direction: column; gap: 1px; min-width: 0; flex: 1; }
  .done strong { font-size: 13px; font-weight: 600; }
  .done .detail { font-size: 11.5px; color: var(--text-2); }
  .done .tiny { margin-left: 0; }
  /* one argument per line: a 30-flag planetiler invocation on one line is unreadable, and
     reading it is the whole point of keeping it */
  .cmd .argv { max-height: 320px; color: var(--text-3); font-size: 11px; white-space: pre;
               word-break: normal; }
  .logbox { margin-top: 10px; }
  .loghead { display: flex; gap: 8px; align-items: center; margin-bottom: 6px; }
  .lines { color: var(--faint); font-size: 10px; font-variant-numeric: tabular-nums; }
  /* builds in other areas: one row each, and clicking one goes there */
  .othersbar { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 10px; }
  .other { display: flex; align-items: center; gap: 7px; background: var(--surface-2);
           border: 1px solid var(--line-2); border-radius: var(--r); padding: 5px 9px;
           color: var(--text-2); font-size: 11.5px; }
  .other:hover:not(:disabled) { background: var(--hover); color: var(--text); }
  .other strong { font-weight: 600; color: var(--text); }
  .other .dot.live { width: 7px; height: 7px; border-radius: 50%; background: var(--ok);
                     animation: pulse 1.6s ease-in-out infinite; }
  .pctinline { color: var(--muted-2); font-variant-numeric: tabular-nums; }
  @keyframes pulse { 50% { opacity: .35; } }
  .steprow { display: flex; align-items: center; gap: 8px; padding: 6px 8px;
             border-radius: var(--r); position: relative; }
  .steprow:hover { background: var(--surface-2); }
  .about { padding: 2px 10px 10px 34px; }
  .about p { color: var(--text-2); font-size: 12.5px; line-height: 1.55; margin: 0 0 8px;
             max-width: 78ch; }
  .about dl { display: grid; grid-template-columns: 74px 1fr; gap: 3px 10px; margin: 0;
              font-size: 12px; }
  .about dt { color: var(--faint); text-transform: uppercase; font-size: 10px;
              letter-spacing: .06em; padding-top: 2px; }
  .about dd { margin: 0; color: var(--text-2); }
  .about code { display: block; color: var(--text-3); font-size: 11px; }
  .info { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .steprow.on { background: var(--surface-2); }
  .pick { display: flex; align-items: center; gap: 9px; background: none; border: 0; padding: 0;
          color: var(--text-2); font: inherit; font-size: 13px; cursor: pointer; flex: 1;
          text-align: left; }
  .steprow.on .pick { color: var(--text); }
  .pick:disabled { cursor: not-allowed; }
  .box { width: 16px; height: 16px; flex: none; border-radius: var(--r-sm);
         border: 1px solid var(--border); display: grid; place-items: center; font-size: 11px;
         color: transparent; }
  .box.checked { background: var(--accent); border-color: var(--accent); color: #fff; }
  .box.auto { border-style: dashed; color: var(--faint); }
  .stat { font-size: 11px; color: var(--muted-2); font-variant-numeric: tabular-nums; }
  .stat.run { color: var(--ok); }
  .stat.ok { color: var(--ok); }
  .stat.bad { color: var(--danger); }
  .mini.danger { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 45%, transparent); }
  .mini.danger:hover:not(:disabled) { background: color-mix(in srgb, var(--danger) 18%, transparent);
                                      color: var(--danger); }
  /* armed: the destructive state looks destructive, and says what it will destroy */
  .mini.danger.armed { background: var(--danger); border-color: var(--danger); color: #fff; }
  .mini.danger.armed:hover:not(:disabled) { background: var(--danger); color: #fff; }
  .stat.warnish { color: var(--warn); }
  .mini { background: var(--line-2); color: var(--muted-2); font-size: 10px; padding: 2px 7px;
          border-radius: var(--r-sm); border: 1px solid transparent; }
  .mini:hover:not(:disabled) { background: var(--border); color: var(--text); }
  .mini.on { background: var(--accent); color: #fff; }
  .forceall { display: flex; align-items: center; gap: 5px; font-size: 11px; color: var(--muted-2);
              white-space: nowrap; }
  .forceall input { width: auto; margin: 0; }
  .bar { position: absolute; left: 0; right: 0; bottom: 0; height: 2px; background: var(--line-2);
         border-radius: 2px; overflow: hidden; }
  .fill { height: 100%; background: var(--accent-hi); transition: width .2s ease; }
  .runbar { display: flex; gap: 8px; align-items: center; }
  .plan { font-size: 11px; color: var(--faint); overflow: hidden; text-overflow: ellipsis;
          white-space: nowrap; }
  .tag.soft { background: var(--line-2); color: var(--muted-2); }
  .extra { display: block; margin-bottom: 10px; }
  .extra input { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
  .extra .hint { display: block; font-style: normal; margin-top: 4px; }
  .extra a { color: var(--ok); }
  .asline { display: flex; align-items: center; gap: 8px; background: var(--bg);
            border: 1px solid var(--line-2); border-radius: var(--r); padding: 7px 9px;
            margin-bottom: 10px; }
  .asline code { flex: 1; color: var(--text-3); font-size: 11px; overflow-x: auto;
                 white-space: nowrap; }
  .presets { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; margin-bottom: 8px; }
  .presets input { width: 160px; margin: 0; }
  .opt { padding: 8px 10px; border-left: 2px solid var(--line-2); margin-bottom: 8px; }
  .opt.set { border-left-color: var(--accent); background: var(--surface-2); }
  .opthead { display: flex; justify-content: space-between; align-items: baseline; }
  .clear { background: none; color: var(--muted-2); font-size: 11px; padding: 0; }
  .help { color: var(--muted); font-size: 12px; margin: 6px 0 0; }
  .hint { color: var(--faint); font-size: 11px; margin: 2px 0 0; font-style: italic; }
  .tag { background: #3a3020; color: var(--warn); font-size: 10px; padding: 1px 5px;
         border-radius: 3px; margin-left: 6px; }
  .prog { display: flex; align-items: center; gap: 10px; }
  .prog .bar { position: static; flex: 1; height: 6px; }
  .pct { font-size: 12px; color: var(--muted-2); font-variant-numeric: tabular-nums; width: 38px;
         text-align: right; }
  .small { font-size: 12px; }
  .tiny { padding: 1px 7px; font-size: 10px; margin-left: auto; }
  .muted { color: var(--muted-2); margin: 6px 0 0; }
  .ok { color: var(--ok); }
  .warn { color: var(--warn); }
  pre { margin: 8px 0 0; max-height: 220px; overflow: auto; font-size: 12px; color: var(--text-2);
        white-space: pre-wrap; word-break: break-all; }
</style>
