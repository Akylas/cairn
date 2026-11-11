//! The `cairn` command line that does what a run in the app does.
//!
//! The app and the command line run the same code, so a build can be started in one and repeated
//! in the other - provided someone can see what to type. That used to be worked out in the web
//! view from the option schema, in a way that had quietly come apart from the real parser: it
//! printed `--area_poly` where the flag is `--area-poly <bool>`, a bare `--` for the PMTiles
//! switch (which ends option parsing, so everything after it was lost), and `--no-elevation-download`
//! for a form value that meant the opposite.
//!
//! So it is built here, from the same values the runner is handed and the same settings it runs
//! with, and the CLI crate has a test that feeds what this produces to the real parser.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::options::{self, OptionDef, OptionKind};
use super::{shell_quote, StepId};
use crate::settings::Settings;

/// How a step takes its options on the command line.
enum Style {
    /// It has none: the area is all it needs.
    Area,
    /// Planetiler's: `-o key=value`, one per option.
    Overrides,
    /// An ordinary flag per option.
    Flags,
}

fn style(step: StepId) -> Style {
    match step {
        StepId::Basemap | StepId::Routes => Style::Overrides,
        StepId::TerrainRgb | StepId::Contours | StepId::ValhallaPackage | StepId::Bathymap => {
            Style::Flags
        }
        StepId::DownloadOsm
        | StepId::ElevationTiles
        | StepId::ValhallaAdmins
        | StepId::ValhallaTiles => Style::Area,
    }
}

/// The argument vector, `cairn` first, for one step as the app would run it.
///
/// `extra` is whatever the form could not express, passed through after `--` exactly as the
/// planetiler steps take it.
pub fn argv(
    settings: &Settings,
    step: StepId,
    area: &str,
    values: &BTreeMap<String, Value>,
    extra: &[String],
) -> Vec<String> {
    let mut out = vec!["cairn".to_string()];
    global(settings, &mut out);
    out.push(step.command().to_string());
    out.push("--area".into());
    out.push(area.to_string());

    let defs = options::for_step(step);
    match style(step) {
        Style::Area => {}
        Style::Overrides => {
            planetiler_paths(settings, &mut out);
            for def in &defs {
                let Some(value) = values.get(&def.key) else { continue };
                if let Some(text) = render(value) {
                    out.push("-o".into());
                    out.push(format!("{}={text}", def.key));
                }
            }
        }
        Style::Flags => {
            if reads_sources(step) {
                terrain_paths(settings, &mut out);
            }
            for def in &defs {
                flag(step, def, values.get(&def.key), &mut out);
            }
            if reads_sources(step) {
                // The form leaves this off and the command line leaves it on, so the line says
                // which one the run used rather than leaving it to the default.
                let on = values.get("area_poly").and_then(Value::as_bool).unwrap_or(false);
                out.push("--area-poly".into());
                out.push(on.to_string());
            }
        }
    }
    if !extra.is_empty() {
        out.push("--".into());
        out.extend(extra.iter().cloned());
    }
    out
}

/// The line to paste into a terminal.
pub fn line(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Every step in the plan as one script, in the order they run.
pub fn script(lines: &[Vec<String>]) -> String {
    lines.iter().map(|argv| line(argv)).collect::<Vec<_>>().join("\n")
}

/// The paths the command line would otherwise work out for itself from `--repo`.
///
/// The CLI deliberately does not read the app's saved settings - a command line should do what it
/// says - so anything the app was configured to do differently has to be on the line.
fn global(settings: &Settings, out: &mut Vec<String>) {
    out.push("--repo".into());
    out.push(settings.repo_root.display().to_string());
    let defaults = Settings::for_repo(settings.repo_root.clone());
    if settings.output_root != defaults.output_root {
        out.push("--output-root".into());
        out.push(settings.output_root.display().to_string());
    }
    if settings.data_dir != defaults.data_dir {
        out.push("--data-dir".into());
        out.push(settings.data_dir.display().to_string());
    }
}

fn planetiler_paths(settings: &Settings, out: &mut Vec<String>) {
    let defaults = Settings::for_repo(settings.repo_root.clone());
    if settings.heap_mb != defaults.heap_mb {
        out.push("--heap-mb".into());
        out.push(settings.heap_mb.to_string());
    }
    if let Some(jar) = &settings.planetiler_jar {
        out.push("--jar".into());
        out.push(jar.display().to_string());
    }
}

/// The steps that sample the elevation sources, and so take the same source and clip flags.
fn reads_sources(step: StepId) -> bool {
    matches!(step, StepId::TerrainRgb | StepId::Contours)
}

fn terrain_paths(settings: &Settings, out: &mut Vec<String>) {
    let defaults = Settings::for_repo(settings.repo_root.clone());
    let differs = |a: &Path, b: &Path| a != b;
    if differs(&settings.sources_json, &defaults.sources_json) {
        out.push("--sources".into());
        out.push(settings.sources_json.display().to_string());
    }
    if differs(&settings.elevation_tiles_dir, &defaults.elevation_tiles_dir) {
        out.push("--elevation-dir".into());
        out.push(settings.elevation_tiles_dir.display().to_string());
    }
}

fn render(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// One option as an ordinary flag.
///
/// The few that are not simply `--flag value` are named here, next to each other, because they
/// are exactly the ones a generic rule gets wrong.
fn flag(step: StepId, def: &OptionDef, value: Option<&Value>, out: &mut Vec<String>) {
    // chosen by the output path in the app, and a switch of its own on the command line - which
    // is why its option has no flag name to go by
    if def.key == crate::steps::archive::PMTILES_KEY {
        if value.and_then(Value::as_bool) == Some(true) {
            out.push("--pmtiles".into());
        }
        return;
    }
    if reads_sources(step) {
        match def.key.as_str() {
            // written out separately, always, after the others
            "area_poly" => return,
            // the form's checkbox says "download" and defaults on; the flag says the opposite
            "download_elevation" => {
                if value.and_then(Value::as_bool) == Some(false) {
                    out.push("--no-elevation-download".into());
                }
                return;
            }
            _ => {}
        }
    }
    let Some(value) = value else { return };
    if def.flag.is_empty() {
        return;
    }
    match (&def.kind, value) {
        // a switch is its presence; false means "do not pass it"
        (OptionKind::Bool, Value::Bool(true)) => out.push(format!("--{}", def.flag)),
        (OptionKind::Bool, _) => {}
        (_, Value::String(s)) if s.is_empty() => {}
        (_, other) => {
            if let Some(text) = render(other) {
                out.push(format!("--{}", def.flag));
                out.push(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn settings() -> Settings {
        let mut s = Settings::for_repo(PathBuf::from("/work/cairn"));
        s.output_root = PathBuf::from("/data/out");
        s
    }

    fn values(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    /// Your own europe terrain form, which printed three flags the parser rejects or misread.
    #[test]
    fn the_terrain_form_becomes_a_command_that_means_the_same() {
        let v = values(&[
            ("area_poly", json!(true)),
            ("encoding", json!("terrarium")),
            ("format", json!("webp")),
            ("max_round_digits", json!(15)),
            ("maxzoom", json!(13)),
            ("pmtiles", json!(true)),
            ("round_digits", json!(8)),
            ("tile_size", json!(256)),
            ("download_elevation", json!(true)),
        ]);
        let line = line(&argv(&settings(), StepId::TerrainRgb, "europe", &v, &[]));
        assert!(line
            .starts_with("cairn --repo /work/cairn --output-root /data/out terrain --area europe"));
        assert!(line.contains("--pmtiles"), "{line}");
        assert!(line.contains("--area-poly true"), "{line}");
        assert!(line.contains("--tile-size 256"), "{line}");
        assert!(!line.contains("--area_poly"), "{line}");
        assert!(!line.contains(" -- "), "a bare -- would swallow what follows: {line}");
        assert!(!line.contains("no-elevation-download"), "downloading is the default: {line}");
    }

    #[test]
    fn declining_to_download_is_the_negative_flag() {
        let v = values(&[("download_elevation", json!(false))]);
        let line = line(&argv(&settings(), StepId::TerrainRgb, "x", &v, &[]));
        assert!(line.contains("--no-elevation-download"), "{line}");
    }

    /// The form has the area boundary off by default and the command line has it on, so an unset
    /// form has to say so explicitly or the two build different archives.
    #[test]
    fn an_unset_area_boundary_is_written_out_as_off() {
        let line = line(&argv(&settings(), StepId::TerrainRgb, "x", &BTreeMap::new(), &[]));
        assert!(line.contains("--area-poly false"), "{line}");
    }

    #[test]
    fn planetiler_options_are_overrides() {
        let v = values(&[("simplify_tolerance", json!(0.7)), ("languages", json!(""))]);
        let argv =
            argv(&settings(), StepId::Basemap, "europe", &v, &["--max-point-buffer=4".into()]);
        let line = line(&argv);
        assert!(line.contains("-o simplify_tolerance=0.7"), "{line}");
        assert!(line.contains("-o languages= "), "an empty value means something: {line}");
        assert!(line.ends_with("-- --max-point-buffer=4"), "{line}");
    }

    #[test]
    fn paths_the_cli_cannot_guess_are_on_the_line() {
        let mut s = settings();
        s.sources_json = PathBuf::from("/elsewhere/my sources.json");
        let line = line(&argv(&s, StepId::TerrainRgb, "x", &BTreeMap::new(), &[]));
        assert!(line.contains("--sources '/elsewhere/my sources.json'"), "{line}");
        // and the ones it can guess are left off
        let line = super::line(&argv(&settings(), StepId::TerrainRgb, "x", &BTreeMap::new(), &[]));
        assert!(!line.contains("--sources"), "{line}");
    }

    #[test]
    fn a_step_with_no_options_is_just_its_area() {
        let a = argv(&settings(), StepId::DownloadOsm, "europe", &BTreeMap::new(), &[]);
        assert_eq!(&a[a.len() - 3..], ["download", "--area", "europe"]);
    }
}
