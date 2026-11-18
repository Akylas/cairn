//! Persisted configuration.
//!
//! Everything the pipeline touches is a setting, because every path has moved at least once:
//! output lives in `output/`, sources in `data/sources`, elevation in `elevation_tiles/`, and
//! the planetiler jar version drifts with every submodule bump. Hard-coding any of them would
//! bake in a lie.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where the fork's jar is published. A tag that moves, so this is always the newest one.
pub const DEFAULT_JAR_URL: &str =
    "https://github.com/Akylas/cairn/releases/download/planetiler-latest/planetiler-with-deps.jar";
use std::path::{Path, PathBuf};

/// One buildable area. `name` is both the output subdirectory and planetiler's `--area`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AreaConfig {
    pub name: String,
    /// `--polygon`, clipping the build to a shape rather than the extract's bbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poly: Option<PathBuf>,
}

/// Where the Valhalla graph build gets the elevation it bakes road grades in from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingElevation {
    /// Rendered on demand from the terrain sources in `sources.json`, so routing and terrain
    /// share one copy of the ground and nothing is written to `elevation_tiles/`.
    #[default]
    Terrain,
    /// 1-arcsecond `.hgt` files in `elevation_tiles/`, downloaded from the Skadi mirror.
    Hgt,
}

impl RoutingElevation {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "terrain" => Some(Self::Terrain),
            "hgt" => Some(Self::Hgt),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terrain => "terrain",
            Self::Hgt => "hgt",
        }
    }
}

/// Every field carries `#[serde(default)]` so a settings file written by an older build still
/// loads after new fields land - the alternative is an app that refuses to start after upgrade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Root of the data-generator checkout; the default for every other path hangs off it.
    pub repo_root: PathBuf,
    /// Where areas and their artifacts live. One subdirectory per area.
    pub output_root: PathBuf,
    /// Planetiler's source downloads (`data/sources`).
    pub data_dir: PathBuf,
    /// Parent for per-run temp dirs. Never shared between two concurrent runs.
    pub tmp_dir: PathBuf,
    pub elevation_tiles_dir: PathBuf,
    pub sources_json: PathBuf,
    /// Where the graph's elevation comes from. See [`RoutingElevation`].
    pub routing_elevation: RoutingElevation,
    /// Override for JRE discovery; `None` means probe `$JAVA_HOME` then `$PATH`.
    pub java_home: Option<PathBuf>,
    pub planetiler_jar: Option<PathBuf>,
    /// Where to fetch the planetiler jar when there is none, so a released app need not carry
    /// 89 MB it may never use.
    ///
    /// This pipeline runs a *fork* of planetiler - the route and landcover layers are not
    /// upstream - so it cannot point at planetiler's own releases: a jar from there would build
    /// a different schema without saying so. It points instead at the release the
    /// `planetiler-jar` workflow publishes, under a tag that moves, so one URL is always the
    /// newest build of the fork.
    #[serde(default)]
    pub planetiler_jar_url: Option<String>,
    pub valhalla_bin_dir: Option<PathBuf>,
    /// The `valhalla.json` used as the template for routing. The embedded router validates the
    /// whole document, so a hand-written stub is not enough - this points at the real one.
    /// `None` means `repo_root/valhalla.json`.
    #[serde(default)]
    pub valhalla_config: Option<PathBuf>,
    pub heap_mb: u32,
    /// Passed as `--loginterval`. Planetiler's own default is `10s`, which yields about six
    /// progress lines for a one-minute build - far too coarse to drive a progress bar.
    pub log_interval: String,
    pub areas: Vec<AreaConfig>,
    /// Where the front end's own bundled files live: the planetiler jar, valhalla.json, the
    /// Valhalla binaries.
    ///
    /// Discovered at run time, never stored - a packaged app's resource directory moves with
    /// the app, and a stale path written into settings.json would outlive an update. A packaged
    /// build has no repository to fall back on, which is why every tool lookup consults this
    /// first.
    #[serde(skip)]
    pub resource_dir: Option<PathBuf>,
    /// Where a downloaded jar is kept: the app's data directory, which survives an app update
    /// and is not inside the bundle. Discovered at run time like `resource_dir`.
    #[serde(skip)]
    pub jar_dir: Option<PathBuf>,
}

/// The newest `-with-deps.jar` in a directory. Several versions can sit side by side in a
/// submodule build; the highest name is the most recent.
/// Markers a checkout has and nothing else does.
fn is_checkout(dir: &Path) -> bool {
    dir.join("config/valhalla.json").is_file() || dir.join("planetiler").is_dir()
}

fn newest_jar(dir: &Path) -> Option<PathBuf> {
    let mut jars: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("-with-deps.jar"))
        .collect();
    jars.sort();
    jars.pop()
}

impl Default for Settings {
    fn default() -> Self {
        Self::for_repo(PathBuf::from("."))
    }
}

impl Settings {
    /// Find the repository from a starting directory, climbing a few levels.
    ///
    /// The app's working directory is wherever it was launched from - for `tauri dev` that is
    /// `src-tauri/`, one level below the checkout, so a jar sitting in
    /// `planetiler/planetiler-dist/target` was invisible. A packaged install finds nothing here
    /// and falls back to its bundled resources, which is the point.
    pub fn locate_repo(start: &Path) -> PathBuf {
        let mut dir = start.to_path_buf();
        for _ in 0..4 {
            if is_checkout(&dir) {
                return dir;
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => break,
            }
        }
        start.to_path_buf()
    }

    /// Defaults matching this repository's actual layout.
    pub fn for_repo(repo_root: PathBuf) -> Self {
        Self {
            output_root: repo_root.join("output"),
            data_dir: repo_root.join("data/sources"),
            tmp_dir: repo_root.join("data/tmp"),
            elevation_tiles_dir: repo_root.join("elevation_tiles"),
            sources_json: repo_root.join("config/sources.json"),
            routing_elevation: RoutingElevation::Terrain,
            java_home: None,
            planetiler_jar: None,
            planetiler_jar_url: Some(DEFAULT_JAR_URL.to_string()),
            valhalla_bin_dir: Some(repo_root.join("valhalla/build")),
            valhalla_config: Some(repo_root.join("config/valhalla.json")),
            heap_mb: 12288,
            log_interval: "1s".into(),
            areas: Vec::new(),
            resource_dir: None,
            jar_dir: None,
            repo_root,
        }
    }

    /// Load from disk, falling back to defaults when the file does not exist yet.
    ///
    /// `repo_root` is stored, so renaming or moving the working copy leaves it naming a
    /// directory that no longer holds a checkout - and then the submodule's planetiler build is
    /// invisible and the process runs with a working directory that does not exist. A stored
    /// root that has lost its markers is replaced by the one located at load time.
    pub fn load_or_default(path: &Path, repo_root: PathBuf) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut settings: Self = serde_json::from_str(&text)
                    .with_context(|| format!("parsing settings at {}", path.display()))?;
                if !is_checkout(&settings.repo_root) && is_checkout(&repo_root) {
                    settings.repo_root = repo_root;
                }
                settings.adopt_moved_config();
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::for_repo(repo_root)),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// `sources.json` and `valhalla.json` moved from the repository root to `config/`, and the
    /// settings file stores their full paths - so an install from before the move kept naming
    /// files that no longer exist, and terrain quietly lost its sources. A stored path that is
    /// not a file, while the checkout has the default one, means exactly that: follow it.
    fn adopt_moved_config(&mut self) {
        let defaults = Self::for_repo(self.repo_root.clone());
        if !self.sources_json.is_file() && defaults.sources_json.is_file() {
            self.sources_json = defaults.sources_json;
        }
        let stale = self.valhalla_config.as_ref().is_some_and(|path| !path.is_file());
        if stale && defaults.valhalla_config.as_ref().is_some_and(|path| path.is_file()) {
            self.valhalla_config = defaults.valhalla_config;
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Per-run temp directory. Two planetiler processes sharing one delete each other's sort
    /// chunks, so this is keyed by area and never handed out twice for concurrent runs.
    pub fn run_tmp_dir(&self, area: &str) -> PathBuf {
        self.tmp_dir.join(area)
    }

    pub fn area_dir(&self, area: &str) -> PathBuf {
        self.output_root.join(area)
    }

    /// The planetiler jar to run.
    ///
    /// A configured path wins outright. Otherwise the candidates are ranked by modification
    /// time and the newest one runs.
    ///
    /// It used to be a fixed order with the submodule's own build placed last, behind the copy
    /// in the bundle's resources. That was written believing "a developer checkout has no
    /// bundle" - but `cargo tauri dev` stages resources into `target/debug/resources`, so a
    /// checkout has both, and the staged copy is only refreshed when the app is rebuilt. The
    /// result was a fork change that was compiled, and then silently not run: the app kept
    /// using a weeks-old jar, and the only trace was a `planetiler:githash` in the output
    /// nobody reads. Newest-wins makes "I just rebuilt the jar" mean what it says.
    pub fn planetiler_jar_path(&self) -> Option<PathBuf> {
        if let Some(jar) = &self.planetiler_jar {
            if jar.is_file() {
                return Some(jar.clone());
            }
        }
        [
            self.jar_dir.as_ref().and_then(|dir| newest_jar(dir)),
            self.resource_dir.as_ref().and_then(|dir| newest_jar(dir)),
            newest_jar(&self.repo_root.join("planetiler/planetiler-dist/target")),
        ]
        .into_iter()
        .flatten()
        .max_by_key(|jar| {
            std::fs::metadata(jar)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
    }

    /// Where the Valhalla config template lives.
    ///
    /// The embedded router validates the whole document, so this cannot be a stub: it is either
    /// the one configured, the one shipped with the app, or the repository's.
    pub fn valhalla_config_path(&self) -> PathBuf {
        if let Some(path) = &self.valhalla_config {
            return path.clone();
        }
        if let Some(bundled) = self.resource_dir.as_ref().map(|dir| dir.join("valhalla.json")) {
            if bundled.is_file() {
                return bundled;
            }
        }
        self.repo_root.join("config/valhalla.json")
    }

    /// Where to look for `valhalla_build_tiles` and friends, in order. `PATH` is searched after
    /// all of these by the tool lookup itself.
    ///
    /// A checkout's own build comes before the copy bundled with the app. `cargo tauri dev` stages
    /// the bundle into `target/debug/resources` without the libraries its load commands point at,
    /// so that copy does not start - and it is as old as the last staging, where the build in the
    /// submodule is the one that was just compiled. A packaged install has no checkout, so it
    /// falls through to the bundle.
    pub fn valhalla_bin_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(dir) = &self.valhalla_bin_dir {
            dirs.push(dir.clone());
        }
        dirs.push(self.repo_root.join("valhalla/build"));
        if let Some(dir) = &self.resource_dir {
            dirs.push(dir.join("valhalla"));
            dirs.push(dir.clone());
        }
        dirs
    }

    pub fn area(&self, name: &str) -> Option<&AreaConfig> {
        self.areas.iter().find(|a| a.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_repo_layout() {
        let s = Settings::for_repo(PathBuf::from("/repo"));
        assert_eq!(s.output_root, PathBuf::from("/repo/output"));
        assert_eq!(s.data_dir, PathBuf::from("/repo/data/sources"));
        assert_eq!(s.area_dir("rhone-alpes"), PathBuf::from("/repo/output/rhone-alpes"));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load_or_default(&dir.path().join("nope.json"), "/repo".into()).unwrap();
        assert_eq!(s, Settings::for_repo("/repo".into()));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings::for_repo("/repo".into());
        s.areas.push(AreaConfig { name: "rhone-alpes".into(), poly: Some("/repo/ra.poly".into()) });
        s.heap_mb = 32768;
        s.save(&path).unwrap();
        assert_eq!(Settings::load_or_default(&path, "/other".into()).unwrap(), s);
    }

    /// A settings file from an older build must still load once new fields are added, or every
    /// upgrade bricks the app.
    #[test]
    fn tolerates_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"heap_mb": 4096}"#).unwrap();
        let s = Settings::load_or_default(&path, "/repo".into()).unwrap();
        assert_eq!(s.heap_mb, 4096);
        assert_eq!(s.log_interval, "1s", "unspecified fields fall back to defaults");
    }

    /// The bug this ranking exists for: a fork change is compiled into the submodule's jar, the
    /// app keeps running the copy staged in `target/*/resources`, and nothing says so. The output
    /// carries the old schema and the only clue is a `planetiler:githash` nobody reads.
    #[test]
    fn a_freshly_built_jar_beats_the_one_staged_in_resources() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("resources");
        let built = dir.path().join("repo/planetiler/planetiler-dist/target");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(&built).unwrap();

        let stale = resources.join("planetiler-dist-0.10.3-with-deps.jar");
        let fresh = built.join("planetiler-dist-0.10.3-SNAPSHOT-with-deps.jar");
        std::fs::write(&stale, b"old").unwrap();
        // an explicit old mtime, so the test does not depend on how fast two writes land
        filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        std::fs::write(&fresh, b"new").unwrap();
        filetime::set_file_mtime(&fresh, filetime::FileTime::from_unix_time(2_000_000, 0)).unwrap();

        let mut settings = Settings::for_repo(dir.path().join("repo"));
        settings.resource_dir = Some(resources.clone());
        assert_eq!(settings.planetiler_jar_path(), Some(fresh));

        // and the other way round: a bundled jar newer than a stale checkout build wins
        filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(3_000_000, 0)).unwrap();
        assert_eq!(settings.planetiler_jar_path(), Some(stale));
    }

    /// A configured path is a decision, not a candidate: it is not ranked against anything.
    #[test]
    fn a_configured_jar_wins_however_old_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("resources");
        std::fs::create_dir_all(&resources).unwrap();
        let chosen = dir.path().join("chosen-with-deps.jar");
        std::fs::write(&chosen, b"x").unwrap();
        filetime::set_file_mtime(&chosen, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let newer = resources.join("planetiler-with-deps.jar");
        std::fs::write(&newer, b"y").unwrap();

        let mut settings = Settings::for_repo(dir.path().join("nowhere"));
        settings.resource_dir = Some(resources);
        settings.planetiler_jar = Some(chosen.clone());
        assert_eq!(settings.planetiler_jar_path(), Some(chosen));
    }

    /// The configured directory may be one that has since moved - a path stored by an older
    /// checkout - and the bundled copy of a dev run cannot start. The checkout's own build is what
    /// is left, and it must be tried before the bundle.
    #[test]
    fn a_checkouts_valhalla_build_is_tried_before_the_bundled_copy() {
        let mut settings = Settings::for_repo(PathBuf::from("/repo"));
        settings.valhalla_bin_dir = Some(PathBuf::from("/gone/valhalla/build"));
        settings.resource_dir = Some(PathBuf::from("/app/resources"));
        assert_eq!(
            settings.valhalla_bin_dirs(),
            vec![
                PathBuf::from("/gone/valhalla/build"),
                PathBuf::from("/repo/valhalla/build"),
                PathBuf::from("/app/resources/valhalla"),
                PathBuf::from("/app/resources"),
            ]
        );
    }

    /// The app is launched from wherever, and `tauri dev` launches it one level inside the
    /// checkout. Climbing is what makes the submodule jar visible from there.
    #[test]
    fn locate_repo_climbs_to_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        let inner = repo.join("src-tauri");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::write(repo.join("config/valhalla.json"), b"{}").unwrap();
        assert_eq!(Settings::locate_repo(&inner), repo);
    }

    /// A settings file written before `sources.json` moved to `config/` names a file that is
    /// gone; loading must follow it rather than leave terrain with no sources.
    #[test]
    fn a_stored_path_to_a_moved_config_file_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::write(repo.join("config/sources.json"), b"[]").unwrap();
        std::fs::write(repo.join("config/valhalla.json"), b"{}").unwrap();

        let mut old = Settings::for_repo(repo.clone());
        old.sources_json = repo.join("sources.json");
        old.valhalla_config = Some(repo.join("valhalla.json"));
        let path = dir.path().join("settings.json");
        old.save(&path).unwrap();

        let loaded = Settings::load_or_default(&path, repo.clone()).unwrap();
        assert_eq!(loaded.sources_json, repo.join("config/sources.json"));
        assert_eq!(loaded.valhalla_config, Some(repo.join("config/valhalla.json")));
    }

    /// A path the user chose, that exists, is theirs and stays.
    #[test]
    fn an_existing_custom_sources_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::write(repo.join("config/sources.json"), b"[]").unwrap();
        let mine = dir.path().join("mine.json");
        std::fs::write(&mine, b"[]").unwrap();

        let mut custom = Settings::for_repo(repo.clone());
        custom.sources_json = mine.clone();
        let path = dir.path().join("settings.json");
        custom.save(&path).unwrap();

        assert_eq!(Settings::load_or_default(&path, repo).unwrap().sources_json, mine);
    }

    /// A packaged install has no checkout above it; inventing one would point every path at a
    /// directory the user never chose.
    #[test]
    fn locate_repo_gives_up_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(Settings::locate_repo(&deep), deep);
    }

    /// The jar shipped with the app is found without a checkout - which is the only way a
    /// packaged install can find one at all.
    #[test]
    fn the_bundled_jar_is_found_without_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("planetiler-dist-0.10.3-with-deps.jar"), b"x").unwrap();

        let mut settings = Settings::for_repo(dir.path().join("nowhere"));
        assert_eq!(settings.planetiler_jar_path(), None);
        settings.resource_dir = Some(resources.clone());
        assert_eq!(
            settings.planetiler_jar_path(),
            Some(resources.join("planetiler-dist-0.10.3-with-deps.jar"))
        );
    }
}
