//! `cairn` - the pipeline from a terminal.
//!
//! Every subcommand runs the same code the desktop app runs, so a build started here and one
//! started from the GUI produce the same bytes. That is the point of the split: `cairn-core`
//! holds the pipeline, this and the Tauri shell are two ways of asking it to run.
//!
//! Replaces the shell scripts it mirrors - `buildAll.sh`, `build_terrain_rgb.py`,
//! `build_valhalla_package.py` - and keeps their shape, so the flags read the same way.

mod steps;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cairn", about = "Build, inspect and serve offline map data", version)]
pub struct Cli {
    /// Repository root. Every other path defaults from it.
    #[arg(long, global = true, default_value = ".")]
    pub repo: PathBuf,

    // Named `output_root`, not `output`: the field name is the clap id, and a global shares its
    // id with any subcommand arg of the same name. With a global `--output`, `terrain --output
    // file.mbtiles` set the output *root* and every later write looked for a directory inside an
    // mbtiles file.
    /// Output root. Defaults to <repo>/output. Per-step `--output` names one file.
    #[arg(long, global = true)]
    pub output_root: Option<PathBuf>,

    /// Where OSM extracts live. Defaults to <repo>/data/sources.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List generated areas and their artifacts.
    Catalog(steps::catalog::Args),
    /// Download the area's OSM extract from Geofabrik.
    Download(steps::tools::DownloadArgs),
    /// Download the elevation tiles the Valhalla graph bakes in.
    Elevation(steps::tools::ToolArgs),
    /// Build the basemap vector tiles.
    Basemap(steps::planetiler::Args),
    /// Build the routes vector tiles.
    Routes(steps::planetiler::Args),
    /// Build the global landcover and depth archive for the low zooms.
    Bathymap(steps::bathymap::Args),
    /// Build terrain-RGB tiles from the sources in sources.json.
    Terrain(steps::terrain::Args),
    /// Build the country and state boundaries the Valhalla graph builder looks nodes up in.
    ValhallaAdmins(steps::tools::ToolArgs),
    /// Build the Valhalla routing graph from the OSM extract.
    ValhallaTiles(steps::tools::ToolArgs),
    /// Pack a Valhalla tile directory into a .vtiles routing package.
    #[command(alias = "package")]
    ValhallaPackage(steps::valhalla::PackageArgs),
    /// Expand a .vtiles package back into a tile directory.
    Unpack(steps::valhalla::UnpackArgs),
    /// Route between points using a package or tile directory.
    Route(steps::valhalla::RouteArgs),
    /// Sample an elevation profile along a line.
    Profile(steps::profile::Args),
    /// Serve the output over HTTP for a browser or the desktop app.
    Serve(steps::serve::Args),
    /// Show the options a build step accepts.
    Options(steps::options::Args),
    /// Show or clear what is already built for an area.
    State(steps::state::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let settings = steps::settings_for(&cli)?;

    match cli.command {
        Command::Catalog(args) => steps::catalog::run(&settings, args),
        Command::Download(args) => steps::tools::download(&settings, args).await,
        Command::Elevation(args) => steps::tools::elevation(&settings, args).await,
        Command::ValhallaAdmins(args) => steps::tools::valhalla_admins(&settings, args).await,
        Command::ValhallaTiles(args) => steps::tools::valhalla_tiles(&settings, args).await,
        Command::Basemap(args) => steps::planetiler::run(&settings, args, false).await,
        Command::Routes(args) => steps::planetiler::run(&settings, args, true).await,
        Command::Bathymap(args) => steps::bathymap::run(&settings, args).await,
        Command::Terrain(args) => steps::terrain::run(&settings, args).await,
        Command::ValhallaPackage(args) => steps::valhalla::package(&settings, args).await,
        Command::Unpack(args) => steps::valhalla::unpack(args),
        Command::Route(args) => steps::valhalla::route(&settings, args),
        Command::Profile(args) => steps::profile::run(args),
        Command::Serve(args) => steps::serve::run(&settings, args).await,
        Command::Options(args) => steps::options::run(args),
        Command::State(args) => steps::state::run(&settings, args),
    }
}

#[cfg(test)]
mod command_line_tests {
    //! The command the app shows for a step has to be one this parser accepts. It used to be
    //! assembled in the web view from the option schema and drifted: it printed `--area_poly`,
    //! a bare `--`, and an inverted `--no-elevation-download`.

    use super::*;
    use cairn_core::settings::Settings;
    use cairn_core::steps::options::{self, OptionKind};
    use cairn_core::steps::{command, StepId, ALL_STEPS};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    /// Every option of a step set to something its kind accepts.
    fn everything(step: StepId, on: bool) -> BTreeMap<String, Value> {
        options::for_step(step)
            .into_iter()
            .map(|def| {
                let value = match def.kind {
                    OptionKind::Bool => json!(on),
                    OptionKind::Int { min, .. } => json!(min.unwrap_or(1).max(1)),
                    OptionKind::Float { min, .. } => json!(min.unwrap_or(1.0).max(1.0)),
                    OptionKind::Text => match def.key.as_str() {
                        // the few text options whose text has a shape
                        "levels" => json!("0,1"),
                        "bounds" | "bbox" => json!("1,2,3,4"),
                        _ => json!("a-value"),
                    },
                    OptionKind::Choice { choices } => json!(choices[0]),
                };
                (def.key, value)
            })
            .collect()
    }

    #[test]
    fn what_the_app_prints_for_every_step_parses() {
        let mut settings = Settings::for_repo(std::path::PathBuf::from("/work/cairn"));
        settings.output_root = "/data/out".into();
        settings.sources_json = "/elsewhere/my sources.json".into();
        settings.elevation_tiles_dir = "/elsewhere/elevation".into();
        for step in ALL_STEPS {
            for on in [true, false] {
                for extra in [vec![], vec!["--max-point-buffer=4".to_string()]] {
                    // only the planetiler steps take anything after `--`
                    if !extra.is_empty() && !matches!(step, StepId::Basemap | StepId::Routes) {
                        continue;
                    }
                    let argv =
                        command::argv(&settings, step, "europe", &everything(step, on), &extra);
                    let parsed = Cli::try_parse_from(&argv);
                    assert!(
                        parsed.is_ok(),
                        "{step:?} printed a line the parser refuses:\n  {}\n{}",
                        command::line(&argv),
                        parsed.err().map(|e| e.to_string()).unwrap_or_default()
                    );
                }
            }
        }
    }

    /// Parsing is not the same as meaning it: the terrain line has to carry the choices that
    /// decide what gets built.
    #[test]
    fn the_terrain_line_round_trips_its_values() {
        let settings = Settings::for_repo(std::path::PathBuf::from("/work/cairn"));
        let values: BTreeMap<String, Value> = [
            ("maxzoom", json!(13)),
            ("tile_size", json!(256)),
            ("encoding", json!("terrarium")),
            ("pmtiles", json!(true)),
            ("fresh", json!(true)),
            ("download_elevation", json!(false)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let argv = command::argv(&settings, StepId::TerrainRgb, "europe", &values, &[]);
        let Cli { command: Command::Terrain(args), .. } = Cli::try_parse_from(&argv).unwrap()
        else {
            panic!("not a terrain command: {argv:?}");
        };
        assert_eq!(args.maxzoom, 13);
        assert_eq!(args.tile_size, 256);
        assert_eq!(args.encoding, "terrarium");
        assert!(args.pmtiles && args.fresh && args.no_elevation_download);
        // the form's default is off, and it is written out because the command line's is on
        assert!(!args.area_poly);
    }
}
