//! What is already built, judged from the output files themselves.
//!
//! A build is long enough that stopping halfway is normal, and re-running the whole plan to get
//! the one step that is missing is not acceptable. So a step counts as built when the files it
//! produces are there and non-empty - not because something wrote down that it ran. Output
//! built by the shell scripts, copied in from another machine, or produced before any of this
//! existed all count the same way, and deleting a file is all it takes to make the step run
//! again.
//!
//! A record (`<area>/.cairn-state.json`) is kept alongside, but only as *extra* information:
//! how long the step took, and which options it ran with, so an option edited since then can be
//! reported as a reason to rebuild. Losing the record loses the timing and the option check,
//! never the knowledge that the output exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::StepId;
use crate::settings::Settings;

const FILE_NAME: &str = ".cairn-state.json";
const VERSION: u32 = 1;

/// One output, as found on disk. Some steps produce a directory rather than a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub name: String,
    pub bytes: u64,
    /// Seconds since the epoch. Formatting is the UI's business.
    pub modified: u64,
    #[serde(default)]
    pub dir: bool,
}

/// Where each step writes.
///
/// Not every step writes into the area directory: the OSM extract lands in planetiler's source
/// downloads, the elevation tiles and the raw Valhalla graph in their own trees. Resolving all
/// of them here is what lets one existence check cover every step.
pub fn outputs_for(
    settings: &Settings,
    area: &str,
    step: StepId,
    options: &BTreeMap<String, Value>,
) -> Vec<PathBuf> {
    let in_area = |name: String| vec![settings.area_dir(area).join(name)];
    // The archive steps write .mbtiles or .pmtiles depending on the option, so the existence
    // check has to look for the one this run would actually produce. Judging freshness by the
    // other name is how a step reports itself built and then runs anyway.
    let format = super::archive::ArchiveFormat::from_values(options);
    let archive = |step: StepId| {
        in_area(
            super::archive::archive_name(area, step, format).expect("archive steps have a name"),
        )
    };
    match step {
        StepId::Basemap => archive(StepId::Basemap),
        StepId::Routes => archive(StepId::Routes),
        StepId::Bathymap => archive(StepId::Bathymap),
        StepId::TerrainRgb => archive(StepId::TerrainRgb),
        StepId::ValhallaPackage => in_area(format!("{area}.vtiles")),
        // planetiler names its downloads with underscores
        StepId::DownloadOsm => {
            vec![settings.data_dir.join(format!("{}.osm.pbf", area.replace('-', "_")))]
        }
        StepId::ElevationTiles => vec![settings.elevation_tiles_dir.clone()],
        StepId::ValhallaTiles => vec![settings.repo_root.join("valhalla_tiles")],
    }
}

/// One completed run of one step. Supplementary to the files, never a substitute for them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub finished_at: u64,
    #[serde(default)]
    pub elapsed: Option<String>,
    /// Fingerprint of the options the step ran with.
    pub options_hash: String,
    /// The options themselves, so the UI can say *what* changed rather than only that it did.
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
}

/// Everything recorded for one area.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub steps: BTreeMap<StepId, StepRecord>,
}

/// Whether a step needs to run, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StepStatus {
    /// Nothing on disk. Zero-byte files count as nothing: an interrupted run leaves those
    /// behind, and treating one as output would skip the step that should replace it.
    Missing { missing: Vec<String> },
    /// Every output present. Safe to skip.
    Built {
        outputs: Vec<OutputFile>,
        /// From the record, when there is one.
        elapsed: Option<String>,
        /// When the step last ran, if recorded; otherwise the newest output's timestamp.
        finished_at: u64,
        /// Whether a record backs this up, or the files are all that is known.
        tracked: bool,
    },
    /// Output is there, but the options have been edited since it was produced.
    OptionsChanged { changed: Vec<String>, outputs: Vec<OutputFile> },
}

impl StepStatus {
    /// Whether a re-run can be skipped.
    pub fn is_fresh(&self) -> bool {
        matches!(self, StepStatus::Built { .. })
    }
}

pub fn state_path(area_dir: &Path) -> PathBuf {
    area_dir.join(FILE_NAME)
}

/// Read the record for an area. A missing or unreadable record is an empty one, not an error -
/// the files are what matter.
pub fn load(area_dir: &Path) -> BuildState {
    let raw = match std::fs::read_to_string(state_path(area_dir)) {
        Ok(raw) => raw,
        Err(_) => return BuildState::default(),
    };
    match serde_json::from_str::<BuildState>(&raw) {
        Ok(state) if state.version == VERSION => state,
        // a record written by a different layout is not worth guessing at
        _ => BuildState::default(),
    }
}

pub fn save(area_dir: &Path, state: &BuildState) -> anyhow::Result<()> {
    std::fs::create_dir_all(area_dir)?;
    let mut state = state.clone();
    state.version = VERSION;
    let json = serde_json::to_string_pretty(&state)?;
    // write-then-rename: a crash mid-write must not leave a half-parsed record behind
    let tmp = state_path(area_dir).with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, state_path(area_dir))?;
    Ok(())
}

/// Note that a step finished, with the options it used. The files are what make it count as
/// built; this only adds the timing and the option fingerprint.
pub fn mark_done(
    area_dir: &Path,
    step: StepId,
    elapsed: Option<String>,
    options: &BTreeMap<String, Value>,
) -> anyhow::Result<()> {
    let mut state = load(area_dir);
    state.steps.insert(
        step,
        StepRecord {
            finished_at: now(),
            elapsed,
            options_hash: hash_options(options),
            options: options.clone(),
        },
    );
    save(area_dir, &state)
}

/// Drop the record for one step, so its options no longer count as known.
///
/// This does not delete anything the step produced - the output is still there, and the step
/// still reads as built. Removing the file is what makes it run again.
pub fn clear(area_dir: &Path, step: StepId) -> anyhow::Result<()> {
    let mut state = load(area_dir);
    state.steps.remove(&step);
    save(area_dir, &state)
}

pub fn clear_all(area_dir: &Path) -> anyhow::Result<()> {
    save(area_dir, &BuildState::default())
}

/// Delete what a step produced, which is the honest way to make it run again.
///
/// Returns what was actually removed. Directories are left alone: the elevation tiles and the
/// Valhalla graph are shared between areas and expensive to rebuild, so deleting them has to be
/// a deliberate act outside this app.
pub fn remove_outputs(
    settings: &Settings,
    area: &str,
    step: StepId,
) -> anyhow::Result<Vec<String>> {
    let mut removed = Vec::new();
    // Both archive formats, because the caller is saying "make this run again" and does not
    // necessarily know which one the previous run wrote.
    let mut candidates = outputs_for(settings, area, step, &BTreeMap::new());
    let pmtiles: BTreeMap<String, Value> =
        [(super::archive::PMTILES_KEY.to_string(), Value::Bool(true))].into_iter().collect();
    for path in outputs_for(settings, area, step, &pmtiles) {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        if path.is_file() {
            std::fs::remove_file(&path)?;
            removed.push(name_of(&path));
        }
    }
    let _ = clear(&settings.area_dir(area), step);
    Ok(removed)
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn probe(path: &Path) -> Option<OutputFile> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if meta.is_dir() {
        // a directory counts when it holds something; an empty one is the same as absent
        let empty = std::fs::read_dir(path).map(|mut e| e.next().is_none()).unwrap_or(true);
        if empty {
            return None;
        }
        return Some(OutputFile { name: name_of(path), bytes: 0, modified, dir: true });
    }
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    Some(OutputFile { name: name_of(path), bytes: meta.len(), modified, dir: false })
}

/// Judge one step from its output files, then from the record if there is one.
pub fn status(
    settings: &Settings,
    area: &str,
    step: StepId,
    options: &BTreeMap<String, Value>,
) -> StepStatus {
    let area_dir = settings.area_dir(area);
    let expected = outputs_for(settings, area, step, options);
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for path in &expected {
        match probe(path) {
            Some(file) => found.push(file),
            None => missing.push(name_of(path)),
        }
    }
    if !missing.is_empty() {
        return StepStatus::Missing { missing };
    }

    let newest = found.iter().map(|f| f.modified).max().unwrap_or(0);
    let Some(record) = load(&area_dir).steps.remove(&step) else {
        return StepStatus::Built {
            outputs: found,
            elapsed: None,
            finished_at: newest,
            tracked: false,
        };
    };

    if record.options_hash != hash_options(options) {
        let mut changed: Vec<String> = Vec::new();
        for key in record.options.keys().chain(options.keys()) {
            if record.options.get(key) != options.get(key) && !changed.contains(key) {
                changed.push(key.clone());
            }
        }
        return StepStatus::OptionsChanged { changed, outputs: found };
    }

    StepStatus::Built {
        outputs: found,
        elapsed: record.elapsed,
        finished_at: record.finished_at,
        tracked: true,
    }
}

/// Status for every step, for the UI to render in one call.
pub fn statuses(
    settings: &Settings,
    area: &str,
    options: &BTreeMap<StepId, BTreeMap<String, Value>>,
) -> BTreeMap<StepId, StepStatus> {
    let empty = BTreeMap::new();
    super::ALL_STEPS
        .iter()
        .map(|step| (*step, status(settings, area, *step, options.get(step).unwrap_or(&empty))))
        .collect()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Options that change how a run goes and nothing about what it produces.
const TRANSIENT_OPTIONS: &[&str] = &["fresh"];

/// Fingerprint a set of option values.
///
/// `BTreeMap` gives a stable key order and `serde_json` a stable rendering of each value, so the
/// same options always hash the same way without pulling in a hashing crate for what is really
/// a change detector.
fn hash_options(options: &BTreeMap<String, Value>) -> String {
    // choices about how a run goes rather than what it makes: ticking "start over" must not make
    // a finished archive look stale
    let kept: BTreeMap<&String, &Value> =
        options.iter().filter(|(k, _)| !TRANSIENT_OPTIONS.contains(&k.as_str())).collect();
    let canonical = serde_json::to_string(&kept).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    /// Settings whose output root is the temp dir itself, so `alps/` is the area directory.
    fn settings_at(root: &Path) -> Settings {
        let mut settings = Settings::for_repo(root.to_path_buf());
        settings.output_root = root.to_path_buf();
        settings
    }

    /// Create an area directory and return it, so a test can drop output into it.
    fn area_dir(root: &Path) -> PathBuf {
        let dir = root.join("alps");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_output_means_the_step_has_to_run() {
        let dir = tempfile::tempdir().unwrap();
        match status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new()) {
            StepStatus::Missing { missing } => assert_eq!(missing, vec!["alps.mbtiles"]),
            other => panic!("expected missing output, got {other:?}"),
        }
    }

    /// The whole point of checking files rather than a record: output from the shell scripts
    /// counts, with nothing written down anywhere.
    #[test]
    fn output_alone_counts_as_built() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(area_dir(dir.path()).join("alps.mbtiles"), b"x").unwrap();
        let status = status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new());
        assert!(status.is_fresh());
        match status {
            StepStatus::Built { tracked, outputs, .. } => {
                assert!(!tracked);
                assert_eq!(outputs[0].bytes, 1);
            }
            other => panic!("expected built, got {other:?}"),
        }
    }

    /// An interrupted run leaves a zero-byte file. Counting it as output would skip the step
    /// that should replace it.
    #[test]
    fn an_empty_file_is_not_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(area_dir(dir.path()).join("alps.mbtiles"), b"").unwrap();
        assert!(
            !status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new()).is_fresh()
        );
    }

    #[test]
    fn deleting_the_output_makes_it_run_again() {
        let dir = tempfile::tempdir().unwrap();
        let out = area_dir(dir.path()).join("alps.mbtiles");
        std::fs::write(&out, b"x").unwrap();
        mark_done(&area_dir(dir.path()), StepId::Basemap, Some("2m".into()), &BTreeMap::new())
            .unwrap();
        assert!(
            status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new()).is_fresh()
        );
        std::fs::remove_file(&out).unwrap();
        assert!(
            !status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new()).is_fresh()
        );
    }

    #[test]
    fn changing_an_option_makes_it_stale_and_says_which() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(area_dir(dir.path()).join("alps.mbtiles"), b"x").unwrap();
        mark_done(&area_dir(dir.path()), StepId::Basemap, None, &opts(&[("maxzoom", json!(14))]))
            .unwrap();
        match status(
            &settings_at(dir.path()),
            "alps",
            StepId::Basemap,
            &opts(&[("maxzoom", json!(13))]),
        ) {
            StepStatus::OptionsChanged { changed, .. } => assert_eq!(changed, vec!["maxzoom"]),
            other => panic!("expected changed options, got {other:?}"),
        }
    }

    /// Key order is an artefact of how the UI happened to build the map, not a change.
    #[test]
    fn option_order_does_not_count_as_a_change() {
        let a = opts(&[("z", json!(1)), ("a", json!(2))]);
        let b = opts(&[("a", json!(2)), ("z", json!(1))]);
        assert_eq!(hash_options(&a), hash_options(&b));
    }

    /// Clearing forgets the options, not the output - the file is still there, so the step is
    /// still built.
    #[test]
    fn clearing_forgets_the_options_not_the_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(area_dir(dir.path()).join("alps.mbtiles"), b"x").unwrap();
        mark_done(&area_dir(dir.path()), StepId::Basemap, None, &opts(&[("maxzoom", json!(14))]))
            .unwrap();
        clear(&area_dir(dir.path()), StepId::Basemap).unwrap();
        match status(
            &settings_at(dir.path()),
            "alps",
            StepId::Basemap,
            &opts(&[("maxzoom", json!(13))]),
        ) {
            StepStatus::Built { tracked, .. } => assert!(!tracked),
            other => panic!("expected built and untracked, got {other:?}"),
        }
    }

    #[test]
    fn removing_outputs_is_what_makes_a_step_run_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(area_dir(dir.path()).join("alps.mbtiles"), b"x").unwrap();
        let removed = remove_outputs(&settings_at(dir.path()), "alps", StepId::Basemap).unwrap();
        assert_eq!(removed, vec!["alps.mbtiles"]);
        assert!(
            !status(&settings_at(dir.path()), "alps", StepId::Basemap, &BTreeMap::new()).is_fresh()
        );
    }

    /// Not every step writes into the area directory. The OSM extract lands in planetiler's
    /// download folder, and it has to be judged there rather than reported as unknowable.
    #[test]
    fn steps_writing_outside_the_area_are_still_checked() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_at(dir.path());
        assert!(!status(&settings, "alps", StepId::DownloadOsm, &BTreeMap::new()).is_fresh());

        std::fs::create_dir_all(&settings.data_dir).unwrap();
        std::fs::write(settings.data_dir.join("alps.osm.pbf"), b"x").unwrap();
        assert!(status(&settings, "alps", StepId::DownloadOsm, &BTreeMap::new()).is_fresh());
    }

    /// The elevation tiles are a directory, not a file: an empty one is the same as absent.
    #[test]
    fn a_directory_output_counts_when_it_holds_something() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_at(dir.path());
        std::fs::create_dir_all(&settings.elevation_tiles_dir).unwrap();
        assert!(!status(&settings, "alps", StepId::ElevationTiles, &BTreeMap::new()).is_fresh());

        std::fs::write(settings.elevation_tiles_dir.join("N45E006.hgt"), b"x").unwrap();
        match status(&settings, "alps", StepId::ElevationTiles, &BTreeMap::new()) {
            StepStatus::Built { outputs, .. } => assert!(outputs[0].dir),
            other => panic!("expected built, got {other:?}"),
        }
    }

    /// The archive steps have two possible filenames, and freshness has to be judged on the one
    /// this run would write - otherwise a PMTiles run finds the old .mbtiles, calls itself
    /// built, and never produces the file that was asked for.
    #[test]
    fn the_format_decides_which_file_counts_as_the_output() {
        let settings = settings_at(Path::new("/repo"));
        let pmtiles: BTreeMap<String, Value> =
            [(super::super::archive::PMTILES_KEY.to_string(), Value::Bool(true))]
                .into_iter()
                .collect();

        let mbtiles_out = outputs_for(&settings, "alps", StepId::Basemap, &BTreeMap::new());
        let pmtiles_out = outputs_for(&settings, "alps", StepId::Basemap, &pmtiles);
        assert!(mbtiles_out[0].to_string_lossy().ends_with("alps.mbtiles"));
        assert!(pmtiles_out[0].to_string_lossy().ends_with("alps.pmtiles"));

        // terrain has its own PMTiles writer, so it moves too
        let terrain = outputs_for(&settings, "alps", StepId::TerrainRgb, &pmtiles);
        assert!(terrain[0].to_string_lossy().ends_with("alps_terrain.pmtiles"), "{terrain:?}");

        // the package has no writer for it at all, and must keep its own name
        let package = outputs_for(&settings, "alps", StepId::ValhallaPackage, &pmtiles);
        assert!(package[0].to_string_lossy().ends_with("alps.vtiles"), "{package:?}");
    }

    #[test]
    fn the_bathymap_writes_into_the_area_directory() {
        let settings = settings_at(Path::new("/repo"));
        let out = outputs_for(&settings, "world", StepId::Bathymap, &BTreeMap::new());
        // `Path::ends_with` compares components, so the separator does not matter
        assert!(out[0].ends_with("world/world_bathymap.mbtiles"), "{out:?}");
    }

    #[test]
    fn a_transient_option_does_not_change_the_fingerprint() {
        let plain = opts(&[("maxzoom", json!(12))]);
        let fresh = opts(&[("maxzoom", json!(12)), ("fresh", json!(true))]);
        assert_eq!(hash_options(&plain), hash_options(&fresh));
        assert_ne!(hash_options(&plain), hash_options(&opts(&[("maxzoom", json!(13))])));
    }
}
