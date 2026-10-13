// One entry point for backend calls, so the UI runs in two places.
//
// Inside the Tauri webview it forwards to the real commands. In a plain browser (`npm run dev`)
// it falls back to the standalone tile server, which exposes the same catalog, tile and profile
// endpoints. That fallback is what makes the map testable without building the whole app - and
// how the blank-map bug below was actually found.
const inTauri = typeof window !== "undefined" && !!window.__TAURI_INTERNALS__;

/** Dev-only: where `cargo run -p cairn-core --example serve` is listening. */
const DEV_BASE =
  new URLSearchParams(location.search).get("tiles") ||
  localStorage.getItem("tilesBase") ||
  "http://127.0.0.1:8787";

async function devInvoke(cmd, args) {
  switch (cmd) {
    case "start_tiles":
      return DEV_BASE;
    case "list_areas": {
      const res = await fetch(`${DEV_BASE}/catalog`);
      if (!res.ok) throw new Error(`catalog ${res.status}`);
      return res.json();
    }
    case "list_steps": {
      const url = new URL(`${DEV_BASE}/steps`);
      if (args?.area) url.searchParams.set("area", args.area);
      return (await fetch(url)).json();
    }
    case "step_options":
      return (await fetch(`${DEV_BASE}/step-options/${args.step}`)).json();
    case "default_preset_name":
      return "measured";
    case "delete_artifact":
      // the tile server serves the catalog read-only; deleting is the app's job
      throw new Error("deleting is only available inside the app");
    case "download_planetiler":
      throw new Error("downloading is only available inside the app");
    case "reveal":
      throw new Error("the file manager is only reachable from the app");
    case "cli_reference":
      // the browser cannot run a binary; the app reads the real --help
      return { path: null, usage: "", commands: [], hint: "available inside the app" };
    case "resolved_defaults": {
      const areas = await (await fetch(`${DEV_BASE}/catalog`)).json();
      return { planetiler_jar: null, valhalla_config: null, areas: areas.map((a) => a.name) };
    }
    case "build_state":
      return (await fetch(`${DEV_BASE}/build-state/${args.area}`)).json();
    // builds only happen inside the app, so in the browser there is never one in flight
    case "active_runs":
      // a test can put runs here: window.__runs = [{ area, step, phase, percent, ... }]
      return window.__runs ?? [];
    // command lines are built by the app from its real settings; the browser has none to use
    case "cli_script":
      return [];
    // the real file is the app's to write; the browser keeps its own copy so the editor can be
    // developed and looked at without building the app
    case "get_sources": {
      let specs;
      try { specs = JSON.parse(localStorage.getItem("devSources")); } catch { specs = null; }
      return {
        path: "(browser) sources.json",
        specs: specs ?? [{
          name: "mapterhorn", type: "xyz", url: "https://tiles.mapterhorn.com/{z}/{x}/{y}.webp",
          encoding: "terrarium", maxzoom: 14, clamp_min: -10,
        }],
      };
    }
    case "save_sources": {
      const problems = [];
      args.specs.forEach((s, index) => {
        if (!s.name) problems.push({ index, field: "name", message: `source ${index + 1} needs a name` });
        if (s.type === "xyz" && !/\{z\}.*\{x\}.*\{y\}/.test(s.url ?? ""))
          problems.push({ index, field: "url", message: `${s.name}: the URL needs {z}, {x} and {y} in it` });
      });
      if (!problems.length) localStorage.setItem("devSources", JSON.stringify(args.specs));
      return problems;
    }
    case "probe_source":
      return { ok: true, summary: "works (browser stub): 2113 m at 46N 8E", details: ["xyz: z0-z14, 512-pixel webp tiles", "1 request(s), 212 ms"] };
    case "terrain_partial":
      return null;
    case "terrain_discard_partial":
      return null;
    case "cancel_run":
      throw new Error("nothing running");
    // the per-area form state lives in the app's config directory; the browser keeps its own
    // copy so the dev UI is still usable, and the two never have to agree
    case "get_build_config": {
      try { return JSON.parse(localStorage.getItem(`buildConfig:${args.area}`)) ?? null; }
      catch { return null; }
    }
    case "save_build_config": {
      try { localStorage.setItem(`buildConfig:${args.area}`, JSON.stringify(args.build)); }
      catch {}
      return null;
    }
    case "list_presets":
      return (await fetch(`${DEV_BASE}/presets`)).json();
    case "plan_steps": {
      const res = await fetch(`${DEV_BASE}/plan`, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify(args.steps),
      });
      return res.json();
    }
    case "routing_status":
      return (await fetch(`${DEV_BASE}/routing-status`)).json();
    case "valhalla_route": {
      const res = await fetch(`${DEV_BASE}/route`, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({
          locations: args.req.locations,
          costing: args.req.costing,
          package: args.req.package,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      return res.text();
    }
    case "get_settings":
      // dev mode has no settings store; enough shape for the form to render
      return { areas: [{ name: "rhone-alpes" }], planetiler_jar: "", heap_mb: 12288 };
    case "elevation_profile": {
      const r = args.req;
      const res = await fetch(`${DEV_BASE}/profile`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          path: r.path, line: r.line, zoom: r.zoom,
          densify_m: r.densifyM, threshold_m: r.thresholdM,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      return res.json();
    }
    default:
      throw new Error(`${cmd} is not available in browser dev mode`);
  }
}

export async function invoke(cmd, args) {
  if (!inTauri) return devInvoke(cmd, args);
  const { invoke: real } = await import("@tauri-apps/api/core");
  return real(cmd, args);
}

// In the browser there is no backend to send events, so handlers are kept where a test can reach
// them: `window.__emit("step", {...})` plays one, which is how a log full of lines can be looked
// at without running a build.
const devHandlers = new Map();
if (!inTauri && typeof window !== "undefined") {
  window.__emit = (event, payload) =>
    (devHandlers.get(event) ?? []).forEach((h) => h({ event, payload }));
}

export async function listen(event, handler) {
  if (!inTauri) {
    devHandlers.set(event, [...(devHandlers.get(event) ?? []), handler]);
    return () => devHandlers.set(event, (devHandlers.get(event) ?? []).filter((h) => h !== handler));
  }
  const { listen: real } = await import("@tauri-apps/api/event");
  return real(event, handler);
}

export const isTauri = inTauri;
