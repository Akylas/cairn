//! Tauri shell. Deliberately thin: everything with logic lives in `cairn-core`, which has no
//! Tauri dependency and therefore tests in seconds rather than behind a webview build.

use cairn_core::buildconfig::{AreaBuildConfig, BuildConfigStore};
use cairn_core::cancel::Cancel;
use cairn_core::catalog::{self, Area, TileStats};
use cairn_core::elevation::{Profile, TerrainSampler};
use cairn_core::presets::{Preset, PresetStore};
use cairn_core::settings::Settings;
use cairn_core::steps::archive::{self, ArchiveFormat};
use cairn_core::steps::options::{self, OptionDef};
use cairn_core::steps::planetiler::{run_cancellable, PlanetilerJob, Schema};
use cairn_core::steps::{plan, state as build_state, StepEvent, StepId, ALL_STEPS};
use cairn_core::terrain::archive::{PartialInfo, ResumeMode, TerrainArchive};
use cairn_core::tileserver::{self, Registry, Source};
use cairn_core::toolchain::{self, JavaInstall};
use cairn_core::valhalla::{package, routing};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

/// The runs in flight, one per area.
///
/// It used to be a single slot: one build at a time for the whole app, and every event it
/// emitted looked the same whichever area it came from. Switching the Build view to another area
/// while europe was building therefore showed europe's log under the new area's name - the run
/// had no identity of its own. Keyed by area, a run does: the events carry it, the progress
/// snapshot carries it, and Cancel names it.
#[derive(Default)]
struct Runs(Mutex<BTreeMap<String, Run>>);

/// One build in flight.
struct Run {
    cancel: Cancel,
    /// What the run is doing right now, kept here rather than only in the events.
    ///
    /// Events reach whoever was listening when they were sent. This is for everyone else: a
    /// Build tab opened halfway through, a window reloaded, the run list in the header. Without
    /// it, a run that is very much alive looks like nothing at all until its next log line.
    progress: Arc<Mutex<RunProgress>>,
}

/// A run's state, flat enough to hand straight to the UI.
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct RunProgress {
    area: String,
    /// The whole plan, in order, so the list reads as a plan rather than filling in as it goes.
    planned: Vec<StepId>,
    completed: Vec<StepId>,
    /// The step executing now, if any.
    step: Option<StepId>,
    phase: String,
    label: String,
    percent: u8,
    /// Unix seconds, for "running for 14m" without the front end having to track it.
    started_at: u64,
    /// Cancel has been pressed and the run is winding down. The button says so rather than
    /// looking like it did nothing, which is what a slow step made it look like.
    cancelling: bool,
    /// The shared resource this run is queued behind, if it is waiting rather than working.
    waiting_for: Option<String>,
}

impl Runs {
    fn snapshot(&self) -> Vec<RunProgress> {
        let Ok(runs) = self.0.lock() else { return Vec::new() };
        runs.values().filter_map(|run| run.progress.lock().ok().map(|p| p.clone())).collect()
    }
}

/// One mutex per shared resource, created on first use.
///
/// Two areas building at once is the point; two `valhalla_build_tiles` writing the same
/// `valhalla_tiles/` directory is not. [`StepId::exclusive_key`] names what a step needs to hold
/// alone, and this hands out the lock for that name.
#[derive(Default)]
struct Locks(Mutex<BTreeMap<&'static str, Arc<tokio::sync::Mutex<()>>>>);

impl Locks {
    fn get(&self, key: &'static str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.0.lock().expect("locks mutex poisoned");
        map.entry(key).or_default().clone()
    }
}

/// The per-area build form state, kept in memory and written through on every change.
struct BuildConfigs {
    path: PathBuf,
    store: Mutex<BuildConfigStore>,
}

/// A step event with the area it came from.
///
/// The core's `StepEvent` deliberately does not carry one - it is emitted deep inside steps that
/// have no reason to know - so the area is attached here, where the run that owns the channel is
/// the one forwarding it.
#[derive(Serialize, Clone)]
struct RunEvent {
    area: String,
    #[serde(flatten)]
    event: StepEvent,
}

/// Settings, kept in memory and written through on every change.
struct Config {
    path: PathBuf,
    settings: Mutex<Settings>,
}

impl Config {
    fn load(app: &AppHandle) -> Self {
        let dir = app.path().app_config_dir().unwrap_or_else(|_| PathBuf::from("."));
        let path = dir.join("settings.json");
        // launched from anywhere: climb to a checkout if there is one above us, otherwise the
        // bundled resources below take over
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let repo = Settings::locate_repo(&cwd);
        let mut settings = Settings::load_or_default(&path, repo).unwrap_or_default();
        // the bundle's own files - the planetiler jar, valhalla.json, the Valhalla binaries.
        // A packaged install has no repository to fall back on, and this path moves with the
        // app, so it is discovered every launch rather than stored.
        // a jar downloaded on demand lives in app data, outside the bundle, so it survives an
        // update and a re-install does not have to fetch it again
        settings.jar_dir = app.path().app_data_dir().ok().map(|dir| dir.join("planetiler"));
        settings.resource_dir = app.path().resource_dir().ok().map(|dir| {
            // tauri.conf maps `resources/` to `resources/`, so the files land one level inside
            // the resource directory rather than at its root
            let nested = dir.join("resources");
            if nested.is_dir() {
                nested
            } else {
                dir
            }
        });
        Self { path, settings: Mutex::new(settings) }
    }

    fn get(&self) -> Result<Settings, String> {
        Ok(self.settings.lock().map_err(|_| "settings lock poisoned")?.clone())
    }

    /// Keep the discovered resource directory across a save: it is not part of what the user
    /// edits, and `save_settings` round-trips through JSON where it is skipped.
    fn set(&self, mut next: Settings) -> Result<Settings, String> {
        let mut slot = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        next.resource_dir = slot.resource_dir.clone();
        *slot = next.clone();
        Ok(next)
    }
}

#[derive(Serialize, Clone)]
struct DownloadProgress {
    done: u64,
    total: Option<u64>,
}

fn managed_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("no app data dir: {e}"))?.join("jre");
    Ok(dir)
}

#[tauri::command]
async fn detect_java(app: AppHandle) -> Result<JavaInstall, String> {
    let root = managed_root(&app)?;
    toolchain::find(None, &root).await.ok_or_else(|| {
        format!("no Java {}+ on PATH, in JAVA_HOME, or downloaded", toolchain::MIN_JAVA)
    })
}

#[tauri::command]
async fn download_java(app: AppHandle) -> Result<JavaInstall, String> {
    let root = managed_root(&app)?;
    let emitter = app.clone();
    toolchain::download(&root, move |done, total| {
        let _ = emitter.emit("jre-download", DownloadProgress { done, total });
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(config: State<'_, Config>) -> Result<Settings, String> {
    config.get()
}

#[tauri::command]
fn save_settings(config: State<'_, Config>, settings: Settings) -> Result<Settings, String> {
    settings.save(&config.path).map_err(|e| e.to_string())?;
    config.set(settings)
}

/// Scan the configured output root. Reads metadata only, never the tiles table.
///
/// `async` on purpose, and the same goes for every other command here that touches the disk or
/// spawns a process: Tauri runs a synchronous command on the main thread, which is the thread
/// the window draws on. A synchronous `discover` over a 1.4 GB output root freezes the whole app
/// until it finishes - which is exactly what it looked like.
#[tauri::command]
async fn list_areas(config: State<'_, Config>) -> Result<Vec<Area>, String> {
    let settings = config.get()?;
    catalog::discover(&settings.output_root).map_err(|e| e.to_string())
}

/// Delete one output file, and whatever sits beside it as part of the same archive.
///
/// The guard and the sidecar handling live in `catalog` so they can be tested; this only supplies
/// the configured output root.
#[tauri::command]
async fn delete_artifact(config: State<'_, Config>, path: String) -> Result<u64, String> {
    let settings = config.get()?;
    catalog::delete_artifact(&settings.output_root, Path::new(&path)).map_err(|e| e.to_string())
}

/// Walks the whole archive, so it is deliberately a separate call the UI makes on demand
/// rather than something `list_areas` pays for. About 18s for 1.4 GB of output.
#[tauri::command]
async fn artifact_stats(path: String) -> Result<TileStats, String> {
    tokio::task::spawn_blocking(move || catalog::tile_stats(Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Comparison {
    zooms: Vec<catalog::ZoomDelta>,
    metadata: catalog::MetadataDiff,
    layers: Vec<catalog::LayerDiff>,
    size_a: u64,
    size_b: u64,
}

#[tauri::command]
async fn compare_artifacts(area: String, a: String, b: String) -> Result<Comparison, String> {
    tokio::task::spawn_blocking(move || -> Result<Comparison, String> {
        let (pa, pb) = (PathBuf::from(&a), PathBuf::from(&b));
        let (art_a, art_b) = (catalog::probe(&pa, &area), catalog::probe(&pb, &area));
        let stats_a = catalog::tile_stats(&pa).map_err(|e| e.to_string())?;
        let stats_b = catalog::tile_stats(&pb).map_err(|e| e.to_string())?;
        Ok(Comparison {
            zooms: catalog::diff_zooms(&stats_a, &stats_b),
            metadata: catalog::diff_metadata(&art_a, &art_b),
            layers: catalog::diff_layers(&art_a, &art_b),
            size_a: art_a.size_bytes,
            size_b: art_b.size_bytes,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The tile server plus the registry it reads from. Started on demand, once.
#[derive(Default)]
struct Tiles {
    registry: Arc<Registry>,
    base_url: Mutex<Option<String>>,
}

/// Start the tile server if it is not already up, then (re)register every discovered artifact.
///
/// Registration is redone on every call so a rebuild is picked up without a restart; the server
/// itself is bound once because rebinding would change the port under the running map.
#[tauri::command]
async fn start_tiles(tiles: State<'_, Tiles>, config: State<'_, Config>) -> Result<String, String> {
    let settings = config.get()?;
    let registry = tiles.registry.clone();

    let base = {
        let existing = tiles.base_url.lock().map_err(|_| "tiles lock poisoned")?.clone();
        match existing {
            Some(url) => url,
            None => {
                let handle = tileserver::start_with_root(
                    0,
                    registry.clone(),
                    Some(settings.output_root.clone()),
                )
                .await
                .map_err(|e| e.to_string())?;
                let url = handle.base_url();
                *tiles.base_url.lock().map_err(|_| "tiles lock poisoned")? = Some(url.clone());
                url
            }
        }
    };

    let areas = catalog::discover(&settings.output_root).map_err(|e| e.to_string())?;
    registry.clear();
    for area in &areas {
        for art in &area.artifacts {
            // routing packages hold graph tiles, not map tiles - nothing to render
            if art.probe_error.is_some() || art.format == catalog::TileFormat::Gph3 {
                continue;
            }
            registry.set(format!("{}/{}", area.name, art.file_name), Source::from_artifact(art));
        }
    }
    Ok(base)
}

/// camelCase because the caller is JavaScript. Without this the `densify_m` / `threshold_m`
/// fields silently fall back to their serde defaults instead of using what the UI sent - a
/// wrong-but-plausible profile rather than an error.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileRequest {
    path: String,
    line: Vec<[f64; 2]>,
    #[serde(default)]
    zoom: Option<u8>,
    #[serde(default = "default_densify")]
    densify_m: f64,
    #[serde(default = "default_threshold")]
    threshold_m: f64,
}

fn default_densify() -> f64 {
    50.0
}

/// 3 m, not 0. At the terrain's 1 m vertical quantisation a flat traverse dithers between two
/// adjacent levels, and summing raw deltas turns that into phantom climb.
fn default_threshold() -> f64 {
    3.0
}

#[tauri::command]
async fn elevation_profile(req: ProfileRequest) -> Result<Profile, String> {
    tokio::task::spawn_blocking(move || -> Result<Profile, String> {
        let mut sampler = TerrainSampler::open(Path::new(&req.path)).map_err(|e| e.to_string())?;
        let zoom = req.zoom.unwrap_or(sampler.maxzoom);
        sampler.profile(&req.line, zoom, req.densify_m, req.threshold_m).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
struct StepInfo {
    id: StepId,
    label: &'static str,
    deps: Vec<StepId>,
    implemented: bool,
    /// What the step does. Comes from the graph, not from the UI, so one description serves
    /// the Build view, the docs and anything added later.
    summary: &'static str,
    /// What it needs beyond its dependencies.
    reads: &'static str,
    /// Where it writes, resolved for the area being looked at rather than as a template.
    writes: Vec<String>,
    /// The `cairn` subcommand that runs it.
    command: &'static str,
    /// How many options its form offers.
    option_count: usize,
}

/// A routing actor, kept alive between requests along with the tile directory it was opened on.
///
/// Creating one is cheap, but its graph reader caches tiles, so holding it makes the second and
/// later routes noticeably faster than the first.
#[derive(Default)]
struct Routing(Mutex<Option<(PathBuf, routing::Router)>>);

#[derive(Serialize)]
struct RoutingStatus {
    available: bool,
    /// The tile directory in use, once one has been resolved.
    tile_dir: Option<String>,
    /// The package that directory was unpacked from, so the UI can say which one it routed on.
    package: Option<String>,
    /// The `valhalla.json` used as the config template.
    config: Option<String>,
}

#[tauri::command]
fn routing_status(config: State<'_, Config>, routing_state: State<'_, Routing>) -> RoutingStatus {
    let loaded = routing_state.0.lock().ok().and_then(|g| {
        g.as_ref().map(|(dir, _)| {
            (dir.display().to_string(), PACKAGE_IN_USE.lock().ok().and_then(|p| p.clone()))
        })
    });
    let (tile_dir, package) = match loaded {
        Some((dir, pkg)) => (Some(dir), pkg),
        None => (None, None),
    };
    RoutingStatus {
        available: routing::available(),
        tile_dir,
        package,
        config: config.get().ok().map(|s| s.valhalla_config_path().display().to_string()),
    }
}

/// The package file behind the router currently held open, for `routing_status` to report.
static PACKAGE_IN_USE: Mutex<Option<String>> = Mutex::new(None);

/// Resolve a tile directory for an area, unpacking its package when needed.
///
/// The package is what actually ships, so it is preferred over the intermediate
/// `valhalla_tiles/` build output: routing against it exercises the artefact users get. The
/// unpack is cached by the package's size and modification time, so it happens once per build
/// rather than once per request.
fn tile_dir_for(
    settings: &Settings,
    area: &str,
    package: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    // an area can hold several packages (a `.base` variant, an older build); the caller picks,
    // and `{area}.vtiles` is only the default
    let package_path =
        settings.area_dir(area).join(package.unwrap_or(&format!("{area}.vtiles")).to_string());
    if !package_path.is_file() {
        let fallback = settings.repo_root.join("valhalla_tiles");
        return if fallback.is_dir() {
            Ok(fallback)
        } else {
            Err(format!("no {} and no valhalla_tiles/ to fall back on", package_path.display()))
        };
    }

    let stamp = std::fs::metadata(&package_path)
        .ok()
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}-{}", m.len(), modified)
        })
        .unwrap_or_else(|| "unknown".into());
    let key = package_path
        .file_name()
        .map(|n| n.to_string_lossy().replace(['/', '\\'], "_"))
        .unwrap_or_else(|| area.to_string());
    let unpacked = cache_root.join(format!("{key}-{stamp}"));
    if unpacked.is_dir() {
        return Ok(unpacked);
    }

    // a stale unpack of the same area would otherwise accumulate on every rebuild
    if let Ok(entries) = std::fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{key}-")) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    package::unpack(&package_path, &unpacked).map_err(|e| e.to_string())?;
    Ok(unpacked)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteRequest {
    area: String,
    /// `[lon, lat]` pairs in visiting order.
    locations: Vec<[f64; 2]>,
    costing: String,
    /// File name of the `.vtiles` package to route on. `None` picks `{area}.vtiles`.
    #[serde(default)]
    package: Option<String>,
}

/// Route through the given waypoints, returning Valhalla's JSON response verbatim.
#[tauri::command]
async fn valhalla_route(
    app: AppHandle,
    config: State<'_, Config>,
    routing_state: State<'_, Routing>,
    req: RouteRequest,
) -> Result<String, String> {
    if !routing::available() {
        return Err("this build has no Valhalla linked".into());
    }
    let settings = config.get()?;
    let cache_root = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("valhalla");
    std::fs::create_dir_all(&cache_root).map_err(|e| e.to_string())?;

    let tile_dir = tile_dir_for(&settings, &req.area, req.package.as_deref(), &cache_root)?;
    let template = settings.valhalla_config_path();
    if !template.is_file() {
        return Err(format!("no Valhalla config at {} - set one in Settings", template.display()));
    }
    let request_json =
        routing::RouteRequest { locations: req.locations, costing: req.costing }.to_json();

    let mut guard = routing_state.0.lock().map_err(|_| "routing lock poisoned")?;
    let needs_open = guard.as_ref().map(|(dir, _)| dir != &tile_dir).unwrap_or(true);
    if needs_open {
        let router = routing::Router::open(&template, &tile_dir).map_err(|e| e.to_string())?;
        *guard = Some((tile_dir, router));
        if let Ok(mut in_use) = PACKAGE_IN_USE.lock() {
            *in_use = Some(req.package.clone().unwrap_or_else(|| format!("{}.vtiles", req.area)));
        }
    }
    let (_, router) = guard.as_mut().expect("just opened");
    router.route(&request_json).map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_steps(
    config: State<'_, Config>,
    area: Option<String>,
) -> Result<Vec<StepInfo>, String> {
    let settings = config.get()?;
    let area = area.unwrap_or_else(|| "<area>".to_string());
    let steps = ALL_STEPS
        .iter()
        .map(|s| StepInfo {
            id: *s,
            label: s.label(),
            deps: s.deps().to_vec(),
            implemented: s.is_implemented(),
            summary: s.summary(),
            reads: s.reads(),
            writes: build_state::outputs_for(&settings, &area, *s, &BTreeMap::new())
                .into_iter()
                .map(|p| p.display().to_string())
                .collect(),
            command: s.command(),
            option_count: step_options(*s).len(),
        })
        .collect::<Vec<_>>();
    Ok(steps)
}

#[tauri::command]
fn step_options(step: StepId) -> Vec<OptionDef> {
    options::for_step(step)
}

fn terrain_output(
    settings: &Settings,
    area: &str,
    pmtiles: bool,
) -> Result<(PathBuf, ArchiveFormat), String> {
    let format = if pmtiles { ArchiveFormat::PMTiles } else { ArchiveFormat::MBTiles };
    let name = archive::archive_name(area, StepId::TerrainRgb, format)
        .ok_or("terrain has an archive name")?;
    Ok((settings.area_dir(area).join(name), format))
}

/// What an interrupted terrain build has already written, so the Build view can say that the next
/// run continues it.
#[tauri::command]
async fn terrain_partial(
    config: State<'_, Config>,
    area: String,
    pmtiles: bool,
) -> Result<Option<PartialInfo>, String> {
    let (path, format) = terrain_output(&config.get()?, &area, pmtiles)?;
    tokio::task::spawn_blocking(move || TerrainArchive::partial_info(&path, format))
        .await
        .map_err(|e| e.to_string())
}

/// Throw an interrupted terrain build away.
#[tauri::command]
async fn terrain_discard_partial(
    config: State<'_, Config>,
    area: String,
    pmtiles: bool,
) -> Result<(), String> {
    let (path, format) = terrain_output(&config.get()?, &area, pmtiles)?;
    tokio::task::spawn_blocking(move || TerrainArchive::discard_partial(&path, format))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// `sources.json` as the editor sees it.
#[derive(Serialize)]
struct SourcesDoc {
    path: String,
    specs: Vec<cairn_core::terrain::source::SourceSpec>,
}

#[tauri::command]
async fn get_sources(config: State<'_, Config>) -> Result<SourcesDoc, String> {
    let path = config.get()?.sources_json;
    let read = path.clone();
    let specs =
        tokio::task::spawn_blocking(move || cairn_core::terrain::source::read_specs_raw(&read))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))?;
    Ok(SourcesDoc { path: path.display().to_string(), specs })
}

/// Save the list. Returns what is wrong with it, and writes nothing, when something is; a
/// running build has already read the file, so a save never changes one in flight.
#[tauri::command]
async fn save_sources(
    config: State<'_, Config>,
    specs: Vec<cairn_core::terrain::source::SourceSpec>,
) -> Result<Vec<cairn_core::terrain::source::Problem>, String> {
    let path = config.get()?.sources_json;
    tokio::task::spawn_blocking(move || {
        let problems = cairn_core::terrain::source::validate(&specs);
        if problems.is_empty() {
            cairn_core::terrain::source::write_specs(&path, &specs)
                .map_err(|e| format!("{e:#}"))?;
        }
        Ok(problems)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Try one source, without saving it and without writing anything to its tile cache.
#[tauri::command]
async fn probe_source(
    spec: cairn_core::terrain::source::SourceSpec,
) -> Result<cairn_core::terrain::source::Probe, String> {
    tokio::task::spawn_blocking(move || cairn_core::terrain::source::probe(&spec))
        .await
        .map_err(|e| e.to_string())
}

/// One step's command line, for the Build view to show before anything runs.
#[derive(Serialize)]
struct CliLine {
    step: StepId,
    argv: Vec<String>,
    line: String,
}

/// The `cairn` command line for each step of a plan, as the form holds them now.
#[tauri::command]
fn cli_script(
    config: State<'_, Config>,
    area: String,
    steps: Vec<StepId>,
    values: BTreeMap<StepId, BTreeMap<String, serde_json::Value>>,
    extra_args: BTreeMap<StepId, String>,
) -> Result<Vec<CliLine>, String> {
    let settings = config.get()?;
    let empty = BTreeMap::new();
    Ok(steps
        .into_iter()
        .map(|step| {
            let extra = extra_args.get(&step).map(|raw| split_args(raw)).unwrap_or_default();
            let argv = cairn_core::steps::command::argv(
                &settings,
                step,
                &area,
                values.get(&step).unwrap_or(&empty),
                &extra,
            );
            CliLine { step, line: cairn_core::steps::command::line(&argv), argv }
        })
        .collect())
}

#[tauri::command]
fn plan_steps(steps: Vec<StepId>) -> Vec<StepId> {
    plan(&steps)
}

fn presets_path(config: &Config) -> PathBuf {
    config.path.with_file_name("presets.json")
}

/// What the Build form holds for one area, as it was last left.
///
/// `None` means this area has never been configured, and the form falls back to seeding itself
/// from the default preset - which is what it always used to do, for every area, on every launch.
#[tauri::command]
fn get_build_config(
    configs: State<'_, BuildConfigs>,
    area: String,
) -> Result<Option<AreaBuildConfig>, String> {
    let store = configs.store.lock().map_err(|_| "build config lock poisoned")?;
    Ok(store.get(&area).cloned())
}

/// Remember one area's form state. Written through on every change, so a crash loses nothing.
#[tauri::command]
fn save_build_config(
    configs: State<'_, BuildConfigs>,
    area: String,
    build: AreaBuildConfig,
) -> Result<(), String> {
    let mut store = configs.store.lock().map_err(|_| "build config lock poisoned")?;
    store.set(&area, build);
    store.save(&configs.path).map_err(|e| e.to_string())
}

/// Which preset the form seeds itself from, so the UI shows the values a run will actually use.
#[tauri::command]
async fn default_preset_name() -> Result<String, String> {
    Ok(cairn_core::presets::DEFAULT_PRESET.to_string())
}

#[tauri::command]
async fn list_presets(config: State<'_, Config>) -> Result<Vec<Preset>, String> {
    let mut store =
        PresetStore::load_or_default(&presets_path(&config)).map_err(|e| e.to_string())?;
    // built-ins are merged in rather than written to disk, so they keep improving with the app
    // while a user preset of the same name still wins
    let mut merged = PresetStore::default();
    for p in cairn_core::presets::builtin() {
        merged.upsert(p);
    }
    for p in store.presets.drain(..) {
        merged.upsert(p);
    }
    Ok(merged.presets)
}

#[tauri::command]
fn save_preset(config: State<'_, Config>, preset: Preset) -> Result<(), String> {
    let path = presets_path(&config);
    let mut store = PresetStore::load_or_default(&path).map_err(|e| e.to_string())?;
    store.upsert(preset);
    store.save(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_preset(config: State<'_, Config>, step: StepId, name: String) -> Result<bool, String> {
    let path = presets_path(&config);
    let mut store = PresetStore::load_or_default(&path).map_err(|e| e.to_string())?;
    let removed = store.remove(step, &name);
    store.save(&path).map_err(|e| e.to_string())?;
    Ok(removed)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    area: String,
    steps: Vec<StepId>,
    #[serde(default)]
    values: BTreeMap<StepId, BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    schema_yaml: Option<String>,
    #[serde(default)]
    jar: Option<String>,
    /// Steps to rebuild even though they are recorded as already built.
    #[serde(default)]
    force: Vec<StepId>,
    /// Rebuild everything in the plan, ignoring what is recorded.
    #[serde(default)]
    force_all: bool,
    /// Anything the option schema does not cover, per step, passed to the tool verbatim.
    ///
    /// Planetiler has far more flags than this app has a form for, and its own documentation is
    /// the reference for them. Rather than mirroring the whole list - which would be wrong by
    /// the next release - whatever is typed here goes through untouched.
    #[serde(default)]
    extra_args: BTreeMap<StepId, String>,
}

/// Split a typed argument string the way a shell would, honouring quotes.
///
/// `--polygon='/tmp/my area.poly'` has to arrive as one argument, and splitting on whitespace
/// alone would hand planetiler two broken ones.
fn split_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                any = true;
            }
            None if ch.is_whitespace() => {
                if any || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            None => current.push(ch),
        }
    }
    if any || !current.is_empty() {
        out.push(current);
    }
    out
}

fn human_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// One `cairn` subcommand, as the binary itself describes it.
#[derive(Serialize, Clone)]
struct CliCommand {
    name: String,
    about: String,
    help: String,
}

#[derive(Serialize, Clone, Default)]
struct CliReference {
    /// Where the binary was found, if it was.
    path: Option<String>,
    /// `cairn --help`, verbatim.
    usage: String,
    commands: Vec<CliCommand>,
    /// How to get the binary when it is missing.
    hint: String,
}

/// Find the `cairn` binary: beside the app, then the workspace target dirs, then `PATH`.
fn find_cli(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("cairn"));
            // in a bundle the CLI sits under Resources, not next to the executable
            candidates.push(dir.join("../Resources/cairn"));
            // cargo puts both binaries in the same target dir during development
            candidates.push(dir.join("cairn.exe"));
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("resources/cairn"));
        candidates.push(dir.join("cairn"));
    }
    for path in candidates {
        if path.is_file() {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join("cairn")).find(|p| p.is_file())
}

fn run_help(program: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// The CLI's own help, read from the binary so the in-app reference cannot drift from it.
///
/// Nothing here is written down twice: the command list, every flag and every default come from
/// `cairn --help`, which is generated from the same argument definitions the CLI parses with.
#[tauri::command]
async fn cli_reference(app: AppHandle, cached: State<'_, CliRef>) -> Result<CliReference, String> {
    // one `--help` per subcommand means sixteen processes; reading them once per launch is
    // enough, and the alternative is paying for it every time the docs tab is opened
    if let Some(ready) = cached.0.lock().map_err(|_| "cli lock poisoned")?.clone() {
        return Ok(ready);
    }
    // a packaged build ships it; a checkout has to build it
    let hint = "it ships with a packaged build; from a checkout, `cargo build --release -p \
                cairn-cli`, or put `cairn` on PATH"
        .to_string();
    let Some(path) = find_cli(&app) else {
        // not cached: the binary may appear later, and the next visit should find it
        return Ok(CliReference { path: None, usage: String::new(), commands: Vec::new(), hint });
    };
    let usage = run_help(&path, &["--help"]).unwrap_or_default();
    let commands = command_lines(&usage)
        .into_iter()
        .map(|(name, about)| CliCommand {
            help: run_help(&path, &[&name, "--help"]).unwrap_or_default(),
            name,
            about,
        })
        .collect();

    let reference = CliReference { path: Some(path.display().to_string()), usage, commands, hint };
    if let Ok(mut slot) = cached.0.lock() {
        *slot = Some(reference.clone());
    }
    Ok(reference)
}

/// The CLI reference, read once per launch.
#[derive(Default)]
struct CliRef(Mutex<Option<CliReference>>);

/// Pull `(name, description)` out of the Commands block of clap's top-level help.
///
/// Reading the binary's own help is what keeps the in-app reference from drifting: there is no
/// second list of commands to forget to update. `help` is dropped - it documents clap, not this
/// pipeline.
fn command_lines(usage: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_commands = false;
    for line in usage.lines() {
        if line.trim_start().starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        // the block ends at the first blank line; a continued description is indented further
        // than the names and carries no name of its own
        if line.trim().is_empty() {
            break;
        }
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let Some(name) = parts.next() else { continue };
        if name.is_empty() || name == "help" {
            continue;
        }
        out.push((name.to_string(), parts.next().unwrap_or("").trim().to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quoted path is one argument, not two. Getting this wrong turns `--polygon='/tmp/my
    /// area.poly'` into a flag planetiler rejects and a file name it never sees.
    #[test]
    fn quoted_arguments_stay_whole() {
        assert_eq!(
            split_args("--max-point-buffer=4  --polygon='/tmp/my area.poly'"),
            vec!["--max-point-buffer=4", "--polygon=/tmp/my area.poly"]
        );
        assert_eq!(split_args("   "), Vec::<String>::new());
        assert_eq!(split_args("--a \"b c\" --d"), vec!["--a", "b c", "--d"]);
    }

    /// Real `cairn --help` output, trimmed. The parser has to stop at the blank line rather
    /// than reading the Options block as more commands.
    #[test]
    fn reads_the_commands_block_and_stops_there() {
        let usage = concat!(
            "Build, inspect and serve offline map data\n",
            "\n",
            "Usage: cairn [OPTIONS] <COMMAND>\n",
            "\n",
            "Commands:\n",
            "  catalog   List generated areas and their artifacts\n",
            "  download  Download the area's OSM extract from Geofabrik\n",
            "  help      Print this message\n",
            "\n",
            "Options:\n",
            "      --repo <REPO>  Repository root\n",
        );
        let commands = command_lines(usage);
        assert_eq!(
            commands,
            vec![
                ("catalog".to_string(), "List generated areas and their artifacts".to_string()),
                (
                    "download".to_string(),
                    "Download the area's OSM extract from Geofabrik".to_string()
                ),
            ]
        );
    }

    /// No Commands block at all - a binary that failed to run, or one that is not clap - must
    /// give nothing rather than garbage.
    #[test]
    fn no_commands_block_is_empty() {
        assert!(command_lines("").is_empty());
        assert!(command_lines("Usage: cairn\nOptions:\n  --help\n").is_empty());
    }
}

/// Download the planetiler jar into the app's data directory.
///
/// The alternative is carrying it in the bundle, which is 89 MB for something an install that
/// only inspects tiles never runs. The URL is a setting rather than a constant because this
/// pipeline runs a fork of planetiler: a jar from planetiler's own releases would quietly build
/// a different schema.
#[tauri::command]
async fn download_planetiler(app: AppHandle, config: State<'_, Config>) -> Result<String, String> {
    let settings = config.get()?;
    let url = settings
        .planetiler_jar_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or("no jar URL set - put one in Settings, or point at a jar directly")?;
    let dir = settings.jar_dir.clone().ok_or("no app data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let name = url.rsplit('/').next().filter(|n| n.ends_with(".jar")).unwrap_or("planetiler.jar");
    // the lookup takes the newest `-with-deps.jar`, so the download has to be named like one
    let name = if name.ends_with("-with-deps.jar") {
        name.to_string()
    } else {
        name.trim_end_matches(".jar").to_string() + "-with-deps.jar"
    };
    let target = dir.join(name);

    let emitter = app.clone();
    cairn_core::steps::download::fetch_url(&url, &target, move |done, total| {
        let _ = emitter.emit("jar-download", DownloadProgress { done, total });
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(target.display().to_string())
}

/// Show a file in the system file manager, selected rather than opened.
///
/// Selecting matters: these are multi-hundred-megabyte mbtiles, and "opening" one hands it to
/// whatever the OS thinks owns `.mbtiles`.
#[tauri::command]
async fn reveal(path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    if !path.exists() {
        return Err(format!("{} is not there", path.display()));
    }
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(&path).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        // no portable "select this file" on Linux; the containing directory is the honest
        // fallback rather than launching a viewer for a 300 MB archive
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent().ok_or("no containing directory")?.to_path_buf()
        };
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// Paths the app resolves for itself, so the UI can show what will actually be used.
#[derive(Serialize)]
struct ResolvedDefaults {
    planetiler_jar: Option<String>,
    valhalla_config: Option<String>,
    /// The Valhalla binaries, where they were actually found.
    valhalla_tools: Vec<(String, Option<String>)>,
    /// The app's own bundled files, when it is a packaged build.
    resource_dir: Option<String>,
    /// Areas found in the output root, which is where a half-finished build shows up.
    areas: Vec<String>,
}

#[tauri::command]
async fn resolved_defaults(config: State<'_, Config>) -> Result<ResolvedDefaults, String> {
    let settings = config.get()?;
    let mut areas: Vec<String> = catalog::discover(&settings.output_root)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name)
        .collect();
    for area in &settings.areas {
        if !areas.contains(&area.name) {
            areas.push(area.name.clone());
        }
    }
    areas.sort();
    let bin_dirs = settings.valhalla_bin_dirs();
    // only one external tool is left: the elevation tiles are downloaded natively
    let valhalla_tools = ["valhalla_build_tiles"]
        .iter()
        .map(|name| {
            let found =
                cairn_core::steps::external::find_tool(bin_dirs.iter().map(|p| p.as_path()), name)
                    .map(|p| p.display().to_string());
            (name.to_string(), found)
        })
        .collect();

    Ok(ResolvedDefaults {
        planetiler_jar: settings.planetiler_jar_path().map(|p| p.display().to_string()),
        valhalla_config: settings
            .valhalla_config_path()
            .is_file()
            .then(|| settings.valhalla_config_path().display().to_string()),
        valhalla_tools,
        resource_dir: settings.resource_dir.as_ref().map(|p| p.display().to_string()),
        areas,
    })
}

/// What is already built for an area, judged against the files on disk.
///
/// Takes the current option values so a step whose options were edited since it ran reports as
/// changed rather than as done - skipping it would silently ignore the edit.
#[tauri::command]
async fn build_state(
    config: State<'_, Config>,
    area: String,
    values: Option<BTreeMap<StepId, BTreeMap<String, serde_json::Value>>>,
) -> Result<BTreeMap<StepId, build_state::StepStatus>, String> {
    let settings = config.get()?;
    let values = values.unwrap_or_default();
    Ok(build_state::statuses(&settings, &area, &values))
}

/// Forget what is known about a step's options, or delete what it produced.
///
/// Only the second makes the step run again: "built" is decided by the files, so forgetting the
/// record alone leaves the output in place and the step still skippable.
#[tauri::command]
async fn clear_build_state(
    config: State<'_, Config>,
    area: String,
    step: Option<StepId>,
    delete_outputs: Option<bool>,
) -> Result<Vec<String>, String> {
    let settings = config.get()?;
    let dir = settings.area_dir(&area);
    match (step, delete_outputs.unwrap_or(false)) {
        (Some(step), true) => {
            build_state::remove_outputs(&settings, &area, step).map_err(|e| e.to_string())
        }
        (Some(step), false) => {
            build_state::clear(&dir, step).map_err(|e| e.to_string())?;
            Ok(Vec::new())
        }
        (None, true) => {
            let mut removed = Vec::new();
            for step in ALL_STEPS {
                removed.extend(
                    build_state::remove_outputs(&settings, &area, step)
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(removed)
        }
        (None, false) => {
            build_state::clear_all(&dir).map_err(|e| e.to_string())?;
            Ok(Vec::new())
        }
    }
}

/// Releases this area's runner slot however `run_steps` exits.
///
/// The slot used to be cleared only at the very end, so any early `?` on the way out - a jar that
/// will not spawn, a failed tmpdir - left it occupied for the lifetime of the app. Every later run
/// was then refused with "a build is already running" the instant it started, which from the
/// outside looks like nothing happening at all.
struct RunGuard(AppHandle, String);

impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut runs) = self.0.state::<Runs>().0.lock() {
            runs.remove(&self.1);
        }
        // the run list in the UI is driven by events, so its disappearance has to be one too
        let _ = self.0.emit("runs-changed", ());
    }
}

/// Run a selection of steps, in dependency order.
///
/// Steps run one at a time on purpose. The two planetiler steps write into a temp tree, and
/// running them concurrently is what deleted each other's sort chunks - each still gets its own
/// tmpdir, but serialising removes the whole class of problem.
#[tauri::command]
async fn run_steps(
    app: AppHandle,
    runs: State<'_, Runs>,
    locks: State<'_, Locks>,
    config: State<'_, Config>,
    req: RunRequest,
) -> Result<Vec<StepId>, String> {
    let settings = config.get()?;
    let java = detect_java(app.clone()).await?;
    let jar = req
        .jar
        .clone()
        .filter(|j| !j.is_empty())
        .map(PathBuf::from)
        .or_else(|| settings.planetiler_jar_path())
        .ok_or("no planetiler jar: build the submodule, or set one in Settings")?;
    let schema = match req.schema_yaml.as_deref() {
        Some(path) if !path.is_empty() => Schema::Yaml { path: PathBuf::from(path) },
        _ => Schema::OpenMapTiles,
    };

    let ordered = plan(&req.steps);
    let cancel = Cancel::new();
    let progress = Arc::new(Mutex::new(RunProgress {
        area: req.area.clone(),
        planned: ordered.clone(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        ..Default::default()
    }));
    {
        let mut slot = runs.0.lock().map_err(|_| "runner lock poisoned")?;
        // one run per area, not one for the whole app: another area building is no reason to
        // refuse this one, and it was the reason the refusal used to read as "nothing happened"
        if slot.contains_key(&req.area) {
            return Err(format!("{} is already building", req.area));
        }
        slot.insert(req.area.clone(), Run { cancel: cancel.clone(), progress: progress.clone() });
    }
    let _slot_guard = RunGuard(app.clone(), req.area.clone());
    let _ = app.emit("runs-changed", ());

    let (tx, mut rx) = mpsc::channel::<StepEvent>(512);
    let emitter = app.clone();
    let event_area = req.area.clone();
    let snapshot = progress.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            // the snapshot is updated from the same stream the UI sees, so a Build tab opened
            // mid-run and one that watched from the start end up saying the same thing
            if let Ok(mut state) = snapshot.lock() {
                match &event {
                    StepEvent::Started { step, .. } => {
                        state.step = Some(*step);
                        state.phase = "starting".into();
                        state.label.clear();
                        state.percent = 0;
                    }
                    StepEvent::Phase { name, .. } => state.phase = name.clone(),
                    StepEvent::Progress { label, percent, .. } => {
                        state.label = label.clone();
                        state.percent = *percent;
                    }
                    StepEvent::Finished { step, ok, .. } => {
                        if *ok && !state.completed.contains(step) {
                            state.completed.push(*step);
                        }
                        state.step = None;
                    }
                    StepEvent::Skipped { step, .. } => {
                        if !state.completed.contains(step) {
                            state.completed.push(*step);
                        }
                    }
                    StepEvent::Log { .. } | StepEvent::Command { .. } | StepEvent::Cli { .. } => {}
                }
            }
            let _ = emitter.emit("step", &RunEvent { area: event_area.clone(), event });
        }
    });

    let area_dir = settings.area_dir(&req.area);
    let planned = ordered.len();
    let mut completed = Vec::new();
    for step in ordered {
        if cancel.is_cancelled() {
            break;
        }
        // Whatever the form holds, verbatim. The form seeds itself from the default preset on
        // load, so an untouched step already carries the README's values - and merging them in
        // again here would make a deliberately cleared field impossible to clear.
        let mut values = req.values.get(&step).cloned().unwrap_or_default();
        // what repeats this step in a terminal, taken from the form as it is - before the area
        // boundary below is turned into a path, which the command line does for itself
        let extra = req.extra_args.get(&step).map(|raw| split_args(raw)).unwrap_or_default();
        let cli = cairn_core::steps::command::argv(&settings, step, &req.area, &values, &extra);
        let _ = tx
            .send(StepEvent::Cli { step, line: cairn_core::steps::command::line(&cli), argv: cli })
            .await;
        // `area_poly` is cairn's own switch: resolve it to a real clip path, fetching the
        // boundary from Geofabrik on the first build that asks for it.
        let clip_key = if step == StepId::TerrainRgb { "poly_shape" } else { "polygon" };
        if let Err(e) = cairn_core::steps::download::apply_area_poly(
            &mut values,
            clip_key,
            &settings.data_dir,
            &req.area,
            |_, _| {},
        )
        .await
        {
            let _ = tx
                .send(StepEvent::Log {
                    step,
                    line: format!("could not resolve the area boundary: {e}"),
                })
                .await;
        }
        let forced = req.force_all || req.force.contains(&step);
        if !forced {
            let status = build_state::status(&settings, &req.area, step, &values);
            if let build_state::StepStatus::Built { outputs, .. } = &status {
                let names: Vec<String> = outputs
                    .iter()
                    .map(|f| format!("{} ({:.1} MB)", f.name, f.bytes as f64 / 1_048_576.0))
                    .collect();
                let _ = tx
                    .send(StepEvent::Skipped {
                        step,
                        reason: format!("{} already on disk", names.join(", ")),
                    })
                    .await;
                completed.push(step);
                continue;
            }
        }
        let started = std::time::Instant::now();
        let record = |ok: bool| {
            if !ok {
                return;
            }
            if let Err(e) = build_state::mark_done(
                &area_dir,
                step,
                Some(human_elapsed(started.elapsed())),
                &values,
            ) {
                eprintln!("could not record {step:?} as built: {e}");
            }
        };

        // Steps naming the same shared resource queue behind each other across every area.
        // Held for the step, not the run: europe's basemap and rhone-alpes' terrain overlap
        // freely, while their two basemaps do not.
        let _exclusive = match step.exclusive_key() {
            None => None,
            Some(key) => {
                let lock = locks.get(key);
                match lock.clone().try_lock_owned() {
                    Ok(guard) => Some(guard),
                    Err(_) => {
                        // say so, rather than sitting silent: from outside, a queued step and a
                        // hung one look exactly alike
                        set_waiting(&progress, Some(key));
                        let _ = tx
                            .send(StepEvent::Log {
                                step,
                                line: format!("waiting: another area is using {key}"),
                            })
                            .await;
                        let guard = cancel.guard(lock.lock_owned()).await;
                        set_waiting(&progress, None);
                        match guard {
                            Some(guard) => Some(guard),
                            // cancelled while queued, which is the moment it is most likely to
                            // be pressed
                            None => break,
                        }
                    }
                }
            }
        };

        let outcome = match step {
            StepId::DownloadOsm => run_download(&settings, &req.area, tx.clone(), &cancel).await,
            StepId::ElevationTiles => {
                run_elevation(&settings, &req.area, tx.clone(), &cancel).await
            }
            StepId::ValhallaTiles => {
                run_valhalla_tool(
                    &settings,
                    &req.area,
                    step,
                    tx.clone(),
                    &cancel,
                    locks.get("elevation"),
                )
                .await
            }
            StepId::Bathymap => {
                run_bathymap(&settings, &req.area, &values, forced, tx.clone(), cancel.receiver())
                    .await
            }
            StepId::TerrainRgb => {
                run_terrain(
                    &settings,
                    &req.area,
                    step,
                    &values,
                    tx.clone(),
                    &cancel,
                    locks.get("elevation"),
                )
                .await
            }
            StepId::ValhallaPackage => {
                run_valhalla_package(&settings, &req.area, tx.clone(), &cancel).await
            }
            StepId::Basemap | StepId::Routes => {
                let defs = options::for_step(step);
                let mut extra =
                    vec!["--download".into(), format!("--area={}", req.area), "--force".into()];
                extra.extend(options::to_args(&defs, &values));
                extra.extend(
                    req.extra_args.get(&step).map(|raw| split_args(raw)).unwrap_or_default(),
                );

                let format = ArchiveFormat::from_values(&values);
                let name = archive::archive_name(&req.area, step, format)
                    .expect("basemap and routes always have an archive name");
                let job = PlanetilerJob {
                    step,
                    area: req.area.clone(),
                    java: java.path.clone(),
                    jar: jar.clone(),
                    schema: schema.clone(),
                    heap_mb: settings.heap_mb,
                    output: settings.area_dir(&req.area).join(name),
                    tmp_dir: settings.run_tmp_dir(&format!("{}-{:?}", req.area, step)),
                    extra_args: extra,
                    working_dir: settings.repo_root.clone(),
                    log_interval: settings.log_interval.clone(),
                };
                run_cancellable(job, tx.clone(), cancel.receiver()).await.map_err(|e| e.to_string())
            }
        };

        match outcome {
            Ok(true) => {
                record(true);
                completed.push(step);
            }
            // the step stopped without finishing: cancelled, or a tool that failed and already
            // said why on its own stream
            Ok(false) => break,
            Err(e) => {
                let _ = tx.send(StepEvent::Log { step, line: format!("ERROR: {e}") }).await;
                break;
            }
        }
    }

    let cancelled = cancel.is_cancelled();
    if cancelled {
        let _ = tx
            .send(StepEvent::Log {
                step: *completed.last().unwrap_or(&StepId::DownloadOsm),
                line: format!("cancelled after {} of {planned} steps", completed.len()),
            })
            .await;
    }
    notify_run_finished(&app, &req.area, &completed, planned, cancelled);
    Ok(completed)
}

/// Record that a run is queued behind a shared resource rather than working.
fn set_waiting(progress: &Arc<Mutex<RunProgress>>, key: Option<&str>) {
    if let Ok(mut state) = progress.lock() {
        state.waiting_for = key.map(str::to_string);
    }
}

/// Tell the desktop the build is over.
///
/// A basemap takes tens of minutes, so nobody is watching the window when it ends. The in-app
/// banner only helps someone already looking at it; this is for the far more common case of
/// having switched away an hour ago.
fn notify_run_finished(
    app: &AppHandle,
    area: &str,
    completed: &[StepId],
    planned: usize,
    cancelled: bool,
) {
    use tauri_plugin_notification::NotificationExt;

    let done = completed.len();
    let (title, body) = if cancelled {
        (format!("{area} was cancelled"), format!("{done} of {planned} steps had finished"))
    } else if done == planned {
        (
            format!("{area} is built"),
            match completed {
                [] => "nothing to do".to_string(),
                [one] => one.label().to_string(),
                _ => completed.iter().map(|s| s.label()).collect::<Vec<_>>().join(", "),
            },
        )
    } else {
        (
            format!("{area} stopped early"),
            format!("{done} of {planned} steps ran - the log has the reason"),
        )
    };
    // best-effort: an unnotifiable desktop is not a reason to fail a build that succeeded
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Download the area's OSM extract, so the three steps that read it share one copy.
///
/// `Ok(false)` means cancelled: the partial transfer stays in its `.part` file and is never
/// renamed, so the next run sees a missing extract rather than a truncated one.
async fn run_download(
    settings: &Settings,
    area: &str,
    tx: mpsc::Sender<StepEvent>,
    cancel: &Cancel,
) -> Result<bool, String> {
    use cairn_core::steps::download;

    let step = StepId::DownloadOsm;
    let _ = tx.send(StepEvent::Started { step, area: area.to_string() }).await;

    let mut last_percent = u8::MAX;
    let progress = tx.clone();
    let path = download::fetch_cancellable(&settings.data_dir, area, cancel, |done, total| {
        let percent = match total {
            Some(total) if total > 0 => ((done * 100) / total).min(100) as u8,
            _ => 0,
        };
        // one event per percent: a 400 MB extract otherwise emits thousands a second
        if percent != last_percent {
            last_percent = percent;
            let _ =
                progress.try_send(StepEvent::Progress { step, label: "download".into(), percent });
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    let Some(path) = path else {
        return Ok(cancelled(&tx, step).await);
    };
    let _ = tx.send(StepEvent::Log { step, line: format!("wrote {}", path.display()) }).await;
    let _ = tx
        .send(StepEvent::Finished {
            step,
            ok: true,
            elapsed: None,
            outputs: vec![path.display().to_string()],
        })
        .await;
    Ok(true)
}

/// Report a step that stopped because the run was cancelled, and return `false` for the runner.
///
/// A cancelled step still has to emit `Finished`: the UI leaves whatever it last heard on
/// screen, so without this a cancelled terrain render stays at "running, 62%" for good.
async fn cancelled(tx: &mpsc::Sender<StepEvent>, step: StepId) -> bool {
    let _ = tx.send(StepEvent::Log { step, line: "cancelled".into() }).await;
    let _ = tx.send(StepEvent::Finished { step, ok: false, elapsed: None, outputs: vec![] }).await;
    false
}

/// Download the `.hgt` tiles covering the area.
///
/// Native, rather than `valhalla_build_elevation`: that is a Python script, and shipping it would
/// put a Python interpreter in the app's dependencies for a naming convention and a download loop.
async fn run_elevation(
    settings: &Settings,
    area: &str,
    tx: mpsc::Sender<StepEvent>,
    cancel: &Cancel,
) -> Result<bool, String> {
    use cairn_core::steps::elevation;

    let step = StepId::ElevationTiles;
    let _ = tx.send(StepEvent::Started { step, area: area.to_string() }).await;

    let bounds = area_bounds(settings, area)
        .ok_or("no bounds for this area yet - build the basemap first, or set a polygon")?;
    let tiles = elevation::tiles_for_bounds(bounds);
    let _ = tx
        .send(StepEvent::Log {
            step,
            line: format!(
                "{} tiles cover {:.2},{:.2},{:.2},{:.2}",
                tiles.len(),
                bounds.0,
                bounds.1,
                bounds.2,
                bounds.3
            ),
        })
        .await;

    let progress = tx.clone();
    // decompressed: the graph reads these and so does the terrain step
    let (downloaded, total) = elevation::fetch_cancellable(
        &settings.elevation_tiles_dir,
        &tiles,
        false,
        cancel,
        |done, total| {
            let _ = progress.try_send(StepEvent::Progress {
                step,
                label: "tiles".into(),
                percent: ((done * 100) / total.max(1)).min(100) as u8,
            });
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    if cancel.is_cancelled() {
        return Ok(cancelled(&tx, step).await);
    }
    let _ = tx
        .send(StepEvent::Log {
            step,
            line: format!("{downloaded} downloaded, {} already there", total - downloaded),
        })
        .await;
    let _ = tx.send(StepEvent::Finished { step, ok: true, elapsed: None, outputs: vec![] }).await;
    Ok(true)
}

/// The area's extent, from its basemap. The one thing every later step needs and none of them
/// can invent.
fn area_bounds(settings: &Settings, area: &str) -> Option<(f64, f64, f64, f64)> {
    let bounds = catalog::discover(&settings.output_root)
        .ok()?
        .into_iter()
        .find(|a| a.name == area)?
        .artifacts
        .iter()
        .find(|a| a.kind == catalog::ArtifactKind::Basemap)?
        .bounds
        .clone()?;
    let parts: Vec<f64> = bounds.split(',').filter_map(|v| v.trim().parse().ok()).collect();
    (parts.len() == 4).then(|| (parts[0], parts[1], parts[2], parts[3]))
}

/// Build the global landcover and depth archive.
///
/// Its own downloads and its own cache, and it never touches the OSM extract, so it has no
/// dependencies in the graph. The area only decides the bbox - and by default not even that,
/// because the layer exists to be global.
async fn run_bathymap(
    settings: &Settings,
    area: &str,
    values: &BTreeMap<String, Value>,
    force: bool,
    tx: mpsc::Sender<StepEvent>,
    cancel: mpsc::Receiver<()>,
) -> Result<bool, String> {
    use cairn_core::steps::bathymap::{self, BathymapJob};
    use cairn_core::steps::external::{self, ToolJob};

    let step = StepId::Bathymap;

    let script = bathymap::script_path(&settings.repo_root);
    if !script.is_file() {
        return Err(format!(
            "{} is missing - the bathymap step runs the script from this repository",
            script.display()
        ));
    }
    let python = bathymap::find_python(&settings.repo_root, None).ok_or(
        "no python3 - the bathymap step needs one with geopandas and shapely, which is what          `venv/` in the repository root is for",
    )?;

    // Both are looked up the same way the Valhalla binaries are: the repository's own build
    // first, then PATH.
    let tippecanoe = external::find_tool(
        [settings.repo_root.join("tippecanoe")].iter().map(|p| p.as_path()),
        "tippecanoe",
    );
    let versatiles = external::find_tool(std::iter::empty(), "versatiles");
    if versatiles.is_none() {
        return Err("versatiles not found on PATH - the landcover extract comes through it".into());
    }
    if tippecanoe.is_none() {
        return Err("tippecanoe not found - build the submodule, or put it on PATH".into());
    }

    // Global unless asked otherwise, and the same default the CLI uses. Falling back to the
    // area's bounds when a basemap happened to exist would mean the same button produced a
    // world archive or a regional one depending on what else was built.
    let global = !matches!(values.get("extent").and_then(|v| v.as_str()), Some("area"));
    let bbox = if global {
        None
    } else {
        match area_bounds(settings, area) {
            Some(bounds) => Some(bounds),
            None => {
                let _ = tx
                    .send(StepEvent::Log {
                        step,
                        line: format!(
                            "no bounds for {area} - build its basemap first, or set extent to                              global. Building the world."
                        ),
                    })
                    .await;
                None
            }
        }
    };

    let format = ArchiveFormat::from_values(values);
    let area_dir = settings.area_dir(area);
    std::fs::create_dir_all(&area_dir).map_err(|e| e.to_string())?;

    let job = BathymapJob {
        area: area.to_string(),
        python,
        script,
        output: bathymap::output_path(&area_dir, area, format),
        // beside the other caches in the repository, and deliberately not under the area: the
        // Natural Earth downloads and the landcover extract are shared by every area that
        // builds this, and re-fetching them per area would be gigabytes for nothing
        cache: settings.repo_root.join(".cache/bathymap"),
        bbox,
        versatiles,
        tippecanoe,
        values: values.clone(),
        force,
        working_dir: settings.repo_root.clone(),
    };

    let argv = job.argv();
    let _ = tx.send(StepEvent::Command { step, argv: argv.clone() }).await;

    let tool = ToolJob {
        step,
        area: area.to_string(),
        program: PathBuf::from(&argv[0]),
        args: argv[1..].to_vec(),
        working_dir: job.working_dir.clone(),
        parse: bathymap::parse_line,
    };
    external::run(tool, tx, cancel).await.map_err(|e| e.to_string())
}

/// Run one of the Valhalla command-line tools.
///
/// `valhalla_build_elevation` fetches the `.hgt` tiles the graph bakes in; `valhalla_build_tiles`
/// builds the graph itself from the OSM extract. Both are subprocesses from the submodule build.
async fn run_valhalla_tool(
    settings: &Settings,
    area: &str,
    step: StepId,
    tx: mpsc::Sender<StepEvent>,
    cancel: &Cancel,
    // held only for the `.hgt` fetch below, for the reason given on `run_terrain`
    elevation_lock: Arc<tokio::sync::Mutex<()>>,
) -> Result<bool, String> {
    use cairn_core::steps::external::{self, ToolJob};

    let bin_dirs = settings.valhalla_bin_dirs();
    let config = settings.valhalla_config_path();
    if !config.is_file() {
        return Err(format!("no Valhalla config at {} - set one in Settings", config.display()));
    }

    let pbf = cairn_core::steps::download::extract_path(&settings.data_dir, area);
    if !pbf.is_file() {
        return Err(format!("{} is missing - run the OSM download step first", pbf.display()));
    }
    let (name, args) = (
        "valhalla_build_tiles",
        vec!["-c".to_string(), config.display().to_string(), pbf.display().to_string()],
    );

    // the graph bakes elevation in while it builds, reading the same .hgt directory the terrain
    // step uses. Missing tiles are not an error there either - the graph just comes out with no
    // grades - so they are fetched first.
    if step == StepId::ValhallaTiles {
        if let Some(bounds) = area_bounds(settings, area) {
            let Some(_held) = cancel.guard(elevation_lock.lock()).await else {
                return Ok(cancelled(&tx, step).await);
            };
            let progress = tx.clone();
            let (got, total) = cairn_core::steps::elevation::ensure_cancellable(
                &settings.elevation_tiles_dir,
                bounds,
                cancel,
                |done, total| {
                    let _ = progress.try_send(StepEvent::Progress {
                        step,
                        label: "elevation".into(),
                        percent: ((done * 100) / total.max(1)).min(100) as u8,
                    });
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            let _ = tx
                .send(StepEvent::Log {
                    step,
                    line: format!("elevation: {got} downloaded of {total} covering tiles"),
                })
                .await;
        }
    }
    if cancel.is_cancelled() {
        return Ok(cancelled(&tx, step).await);
    }

    let program =
        external::find_tool(bin_dirs.iter().map(|p| p.as_path()), name).ok_or_else(|| {
            format!(
                "{name} not found - looked in {}, and on PATH. Set the binary directory in \
                 Settings.",
                bin_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        })?;
    let job = ToolJob {
        step,
        area: area.to_string(),
        program,
        args,
        working_dir: settings.repo_root.clone(),
        parse: external::valhalla_line,
    };
    external::run(job, tx, cancel.receiver()).await.map_err(|e| e.to_string())
}

/// Build the terrain pyramid from `.hgt` sources.
///
/// Reads only SRTM `.hgt`, which needs no projection library. The GeoTIFF raster sources in
/// `sources.json` (the IGN 5 m data, EPSG:2154) are not read here yet, so this produces the
/// tilezen-resolution pyramid rather than the IGN-composited one.
async fn run_terrain(
    settings: &Settings,
    area: &str,
    step: StepId,
    values: &BTreeMap<String, serde_json::Value>,
    tx: mpsc::Sender<StepEvent>,
    cancel: &Cancel,
    // Held only while the `.hgt` tiles are being fetched: that directory is shared by every
    // area, and two runs downloading the same tile write the same `.part` file. The render
    // that follows touches nothing outside this area, so it does not hold this.
    elevation_lock: Arc<tokio::sync::Mutex<()>>,
) -> Result<bool, String> {
    use cairn_core::elevation::Encoding;
    use cairn_core::terrain::{build, render, source};

    let _ = tx.send(StepEvent::Started { step, area: area.to_string() }).await;

    let sources_json = settings.sources_json.clone();
    let hgt_dir = settings.elevation_tiles_dir.clone();
    let archive_format = ArchiveFormat::from_values(values);
    let output = settings.area_dir(area).join(
        archive::archive_name(area, step, archive_format).expect("terrain has an archive name"),
    );
    // the form's values, with the schema's own "unset means the default" rule: an absent key
    // leaves `TerrainOptions::default()` standing rather than asserting a guess at it
    let num = |key: &str| values.get(key).and_then(|v| v.as_f64());
    let text = |key: &str| values.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let defaults = render::TerrainOptions::default();
    let encoding = match text("encoding").as_deref() {
        Some(name) => Encoding::parse(name).ok_or_else(|| format!("unknown encoding `{name}`"))?,
        None => defaults.encoding,
    };
    let opts = render::TerrainOptions {
        encoding,
        minzoom: num("minzoom").map(|v| v as u8).unwrap_or(defaults.minzoom),
        maxzoom: num("maxzoom").map(|v| v as u8).unwrap_or(defaults.maxzoom),
        tile_size: num("tile_size").map(|v| v as u32).unwrap_or(defaults.tile_size),
        round_digits: num("round_digits").map(|v| v as u32).unwrap_or(defaults.round_digits),
        max_round_digits: num("max_round_digits")
            .map(|v| v as u32)
            .unwrap_or(defaults.max_round_digits),
        blur_m: num("blur").unwrap_or(defaults.blur_m),
        nodata_elevation: num("nodata_elevation").unwrap_or(defaults.nodata_elevation),
    };
    let shape = match text("poly_shape") {
        Some(path) => Some(
            cairn_core::poly::Polygon::parse(std::path::Path::new(&path))
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    // the form promises 1 beside the field, and a ring of one is what every archive in this
    // repository was built with; 0 leaves a seam wherever coverage stops
    let tile_buffer = num("tile_buffer").map(|v| v as u32).unwrap_or(1);
    let download_elevation =
        values.get("download_elevation").and_then(|v| v.as_bool()).unwrap_or(true);
    let as_png = text("format").as_deref() == Some("png");

    // bounds come from the area's basemap when there is one, so terrain matches its extent
    let bounds = catalog::discover(&settings.output_root)
        .ok()
        .and_then(|areas| {
            areas
                .into_iter()
                .find(|a| a.name == area)?
                .artifacts
                .iter()
                .find(|a| a.kind == catalog::ArtifactKind::Basemap)?
                .bounds
                .clone()
        })
        .and_then(|b| {
            let p: Vec<f64> = b.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            (p.len() == 4).then(|| (p[0], p[1], p[2], p[3]))
        })
        .ok_or("no basemap bounds to derive the terrain extent from");
    // an explicit box wins, then the shape's own bounds, then the area's basemap
    let bounds = match text("bounds") {
        Some(raw) => {
            let p: Vec<f64> = raw.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if p.len() != 4 {
                return Err("bounds want west,south,east,north".into());
            }
            (p[0], p[1], p[2], p[3])
        }
        None => match &shape {
            Some(shape) => shape.bounds(),
            None => bounds?,
        },
    };

    // A source list that names no .hgt source wants none of them: downloading 25 MB a degree for
    // a render that will never open them is pure waste. When the list cannot be read at all the
    // fallback below is a .hgt directory, so then they are still needed.
    let wants_hgt = source::read_specs(&sources_json)
        .map(|specs| specs.iter().any(|s| source::is_hgt(&s.kind)))
        .unwrap_or(true);

    // the sources may name a directory of .hgt tiles; make sure the ones this render needs are
    // actually there, or the archive comes out with holes and nothing says why
    if download_elevation && wants_hgt {
        let Some(_held) = cancel.guard(elevation_lock.lock()).await else {
            return Ok(cancelled(&tx, step).await);
        };
        let progress = tx.clone();
        let (got, total) = cairn_core::steps::elevation::ensure_cancellable(
            &settings.elevation_tiles_dir,
            bounds,
            cancel,
            |done, total| {
                let _ = progress.try_send(StepEvent::Progress {
                    step,
                    label: "elevation".into(),
                    percent: ((done * 100) / total.max(1)).min(100) as u8,
                });
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        if cancel.is_cancelled() {
            return Ok(cancelled(&tx, step).await);
        }
        let _ = tx
            .send(StepEvent::Log {
                step,
                line: format!("elevation: {got} downloaded of {total} covering tiles"),
            })
            .await;
    }

    let name = format!("{area}_terrain");
    let progress = tx.clone();
    // The render is the longest thing this app does without a subprocess, and it used to be the
    // one place Cancel could not reach: `spawn_blocking` cannot be aborted from outside, so the
    // token has to be read from inside the loop. The loop checks it between batches - a batch is
    // a few seconds - and a build stopped there keeps what it wrote, so the next run continues it.
    let stop = cancel.clone();
    let poly_file = text("poly_shape").map(std::path::PathBuf::from);
    let mode = if values.get("fresh").and_then(|v| v.as_bool()).unwrap_or(false) {
        ResumeMode::Fresh
    } else {
        ResumeMode::Auto
    };
    let outcome = tokio::task::spawn_blocking(move || -> Result<build::Outcome, String> {
        // sources.json is the pipeline's own definition of what to read, in priority order;
        // fall back to the bare elevation directory when it is absent
        let specs = source::read_specs(&sources_json).unwrap_or_else(|_| {
            vec![source::SourceSpec {
                name: "elevation_tiles".into(),
                kind: "valhalla".into(),
                path: hgt_dir.clone(),
                clamp_min: Some(-10.0),
                download: None,
                ..Default::default()
            }]
        });
        let job = build::Job {
            specs: &specs,
            opts: &opts,
            bounds,
            shape: shape.as_ref(),
            shape_file: poly_file.as_deref(),
            tile_buffer,
            format: if as_png { "png" } else { "webp" },
            archive_format,
            output: &output,
            name: &name,
            mode,
            batch: build::BATCH,
        };
        build::run(&job, &|| stop.is_cancelled(), &mut |event| {
            let _ = match event {
                build::Event::Log(line) => progress.blocking_send(StepEvent::Log { step, line }),
                build::Event::Phase(name) => {
                    progress.blocking_send(StepEvent::Phase { step, name })
                }
                build::Event::Progress { label, percent } => {
                    progress.blocking_send(StepEvent::Progress { step, label, percent })
                }
            };
        })
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    if !outcome.finished {
        let _ = tx
            .send(StepEvent::Log {
                step,
                line: format!(
                    "stopped with {} tiles written; run it again to continue from here, or tick \
                     \"Start over\" to discard them",
                    outcome.written
                ),
            })
            .await;
        return Ok(cancelled(&tx, step).await);
    }
    let _ = tx.send(StepEvent::Finished { step, ok: true, elapsed: None, outputs: vec![] }).await;
    Ok(true)
}

/// Compress a Valhalla tile directory into a `.vtiles` package.
async fn run_valhalla_package(
    settings: &Settings,
    area: &str,
    tx: mpsc::Sender<StepEvent>,
    cancel: &Cancel,
) -> Result<bool, String> {
    use cairn_core::valhalla::package::{self, Compression, PackageOptions};

    let step = StepId::ValhallaPackage;
    let _ = tx.send(StepEvent::Started { step, area: area.to_string() }).await;

    let tile_dir = settings.repo_root.join("valhalla_tiles");
    let output = settings.area_dir(area).join(format!("{area}.vtiles"));
    // reuse the tile list of the previous package when there is one; otherwise there is no
    // tilemask to work from and the step cannot pick tiles on its own yet
    let tiles = package::tiles_in(&output).map_err(|_| {
        "no existing package to take a tile list from; build one with the script first"
    })?;

    let opts = PackageOptions {
        package_id: area.to_string(),
        tile_dir,
        output,
        compression: Compression::Zopfli,
    };
    let progress = tx.clone();
    let stop = cancel.clone();
    let report = tokio::task::spawn_blocking(move || {
        package::build_cancellable(&opts, &tiles, &stop, |done, total| {
            let _ = progress.blocking_send(StepEvent::Progress {
                step,
                label: "tiles".into(),
                percent: ((done * 100) / total.max(1)).min(100) as u8,
            });
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    let Some(report) = report else {
        return Ok(cancelled(&tx, step).await);
    };
    let _ = tx
        .send(StepEvent::Log {
            step,
            line: format!(
                "{} tiles, {:.1} MiB compressed (ratio {:.3}), {} missing",
                report.tiles_written,
                report.compressed_bytes as f64 / 1_048_576.0,
                report.ratio(),
                report.tiles_missing
            ),
        })
        .await;
    let _ = tx.send(StepEvent::Finished { step, ok: true, elapsed: None, outputs: vec![] }).await;
    Ok(true)
}

/// Stop one area's run, or every run when no area is given.
///
/// The token is set here and read by the step in flight: a subprocess is killed, and the native
/// steps give up at their next checkpoint - between tiles, between batches - rather than at the
/// end of the whole render.
#[tauri::command]
fn cancel_run(app: AppHandle, runs: State<'_, Runs>, area: Option<String>) -> Result<(), String> {
    let slot = runs.0.lock().map_err(|_| "runner lock poisoned")?;
    let wanted: Vec<&Run> = match &area {
        Some(area) => slot.get(area).into_iter().collect(),
        None => slot.values().collect(),
    };
    if wanted.is_empty() {
        return Err(match area {
            Some(area) => format!("{area} is not building"),
            None => "nothing running".into(),
        });
    }
    for run in wanted {
        run.cancel.cancel();
        // the run stays in the map until its step actually stops; saying so is what keeps the
        // button from looking inert while a batch finishes
        if let Ok(mut state) = run.progress.lock() {
            state.cancelling = true;
        }
    }
    drop(slot);
    let _ = app.emit("runs-changed", ());
    Ok(())
}

/// Every build in flight, for the run list and for a Build tab that mounted mid-run.
#[tauri::command]
fn active_runs(runs: State<'_, Runs>) -> Vec<RunProgress> {
    runs.snapshot()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Runs::default())
        .manage(Locks::default())
        .manage(Tiles::default())
        .manage(Routing::default())
        .manage(CliRef::default())
        .setup(|app| {
            let config = Config::load(&app.handle().clone());
            let path = config.path.with_file_name("build-config.json");
            app.manage(BuildConfigs {
                store: Mutex::new(BuildConfigStore::load_or_default(&path)),
                path,
            });
            app.manage(config);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_java,
            download_java,
            cancel_run,
            active_runs,
            get_build_config,
            save_build_config,
            get_settings,
            save_settings,
            list_areas,
            artifact_stats,
            delete_artifact,
            compare_artifacts,
            start_tiles,
            elevation_profile,
            routing_status,
            valhalla_route,
            list_steps,
            step_options,
            plan_steps,
            cli_script,
            get_sources,
            save_sources,
            probe_source,
            terrain_partial,
            terrain_discard_partial,
            list_presets,
            default_preset_name,
            build_state,
            clear_build_state,
            resolved_defaults,
            cli_reference,
            reveal,
            download_planetiler,
            save_preset,
            delete_preset,
            run_steps
        ])
        .run(tauri::generate_context!())
        .expect("error while running Cairn");
}
