//! `cairn valhalla-package` / `unpack` / `route` - the Valhalla steps.

use anyhow::{anyhow, Result};
use cairn_core::settings::Settings;
use cairn_core::steps::{state, StepId};
use cairn_core::valhalla::package::{self, Compression, PackageOptions};
use cairn_core::valhalla::routing;
use cairn_core::valhalla::select::{self, Selection};
use clap::Args as ClapArgs;
use std::path::PathBuf;

/// Mirrors `scripts/build_valhalla_package.py`, which took its tile list from a `.poly`.
#[derive(ClapArgs)]
pub struct PackageArgs {
    /// Package id, and the default name for the output.
    #[arg(long)]
    pub area: String,
    /// Valhalla tile directory. Defaults to <repo>/valhalla_tiles.
    #[arg(long)]
    pub tiles: Option<PathBuf>,
    /// Osmosis .poly to select tiles from. Without this, --tilemask or --like, an existing
    /// package is reused, and failing that the area's own boundary is used.
    #[arg(long)]
    pub poly: Option<PathBuf>,
    /// Hierarchy levels to include. Valhalla has three; all of them by default.
    #[arg(long, value_delimiter = ',', default_value = "0,1,2")]
    pub levels: Vec<u8>,
    /// Base64 quadtree tilemask, as the Python script emitted.
    #[arg(long)]
    pub tilemask: Option<String>,
    /// Zoom to expand `--tilemask` to.
    #[arg(long, default_value_t = 11)]
    pub polymaxzoom: u8,
    /// Take the tile list from an existing package.
    #[arg(long)]
    pub like: Option<PathBuf>,
    /// zopfli is ~3% smaller than zlib and much slower; both emit ordinary gzip.
    #[arg(long, default_value = "zopfli")]
    pub compression: String,
    /// Output .vtiles. Defaults to <output>/<area>/<area>.vtiles.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// List the tiles that would be packed, and stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the build when the output is already there.
    #[arg(long)]
    pub skip_existing: bool,
}

#[derive(ClapArgs)]
pub struct UnpackArgs {
    /// The .vtiles package to expand.
    pub package: PathBuf,
    /// Directory to write the tile tree into.
    pub tile_dir: PathBuf,
}

#[derive(ClapArgs)]
pub struct RouteArgs {
    /// A .vtiles package or a Valhalla tile directory.
    #[arg(long)]
    pub tiles: PathBuf,
    /// valhalla.json to use as the config template. Defaults to the configured one.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// lon,lat waypoints in order.
    #[arg(long = "point", value_name = "LON,LAT", required = true)]
    pub points: Vec<String>,
    #[arg(long, default_value = "auto")]
    pub costing: String,
    /// Print Valhalla's response as-is.
    #[arg(long)]
    pub json: bool,
}

pub async fn package(settings: &Settings, args: PackageArgs) -> Result<()> {
    let area_dir = settings.area_dir(&args.area);
    let recorded: std::collections::BTreeMap<String, serde_json::Value> =
        [("compression".to_string(), args.compression.clone().into())].into_iter().collect();
    let tile_dir = args.tiles.clone().unwrap_or_else(|| settings.repo_root.join("valhalla_tiles"));
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| settings.area_dir(&args.area).join(format!("{}.vtiles", args.area)));
    if args.skip_existing && output.is_file() {
        println!("{} is already there", output.display());
        return Ok(());
    }

    let selection = Selection {
        poly: args.poly.clone(),
        tilemask: args.tilemask.clone(),
        polymaxzoom: args.polymaxzoom,
        levels: args.levels.clone(),
        like: args.like.clone(),
    };
    // nothing named and nothing to copy from: the area's own boundary, fetched when it is not
    // already beside the extract - the same default the app uses
    let boundary = if selection.needs_area_poly(&output) {
        let path =
            cairn_core::steps::download::ensure_poly(&settings.data_dir, &args.area, |_, _| {})
                .await
                .map_err(|e| {
                    anyhow!("no tile list given, and the area boundary is not available: {e}")
                })?;
        println!("tile list from the boundary {}", path.display());
        Some(path)
    } else {
        None
    };
    let tiles = select::tiles_for(&selection, &output, boundary.as_deref())?;
    if args.dry_run {
        println!("{} tiles from {}", tiles.len(), tile_dir.display());
        for tile in &tiles {
            println!("  {}", tile.path());
        }
        return Ok(());
    }

    let compression = match args.compression.as_str() {
        "zopfli" => Compression::Zopfli,
        "zlib" => Compression::Zlib,
        other => return Err(anyhow!("unknown compression `{other}`")),
    };

    println!("packing {} tiles from {}", tiles.len(), tile_dir.display());
    let started = std::time::Instant::now();
    let report = package::build(
        &PackageOptions {
            package_id: args.area.clone(),
            tile_dir,
            output: output.clone(),
            compression,
        },
        &tiles,
        |done, total| {
            if done % 10 == 0 || done == total {
                print!("\r  {done}/{total}   ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        },
    )?;
    println!();

    println!(
        "wrote {} tiles in {:.1}s: {} raw -> {} gzip (ratio {:.3}), {} missing",
        report.tiles_written,
        started.elapsed().as_secs_f64(),
        super::mb(report.raw_bytes),
        super::mb(report.compressed_bytes),
        report.ratio(),
        report.tiles_missing
    );
    println!("  {}", output.display());
    state::mark_done(
        &area_dir,
        StepId::ValhallaPackage,
        Some(super::planetiler::human_elapsed(started.elapsed())),
        &recorded,
    )?;
    Ok(())
}

pub fn unpack(args: UnpackArgs) -> Result<()> {
    let count = package::unpack(&args.package, &args.tile_dir)?;
    println!("unpacked {count} tiles into {}", args.tile_dir.display());
    Ok(())
}

pub fn route(settings: &Settings, args: RouteArgs) -> Result<()> {
    if !routing::available() {
        return Err(anyhow!("this build has no Valhalla linked; rebuild with VALHALLA_DIR set"));
    }
    let locations: Vec<[f64; 2]> = args
        .points
        .iter()
        .map(|raw| {
            let (lon, lat) =
                raw.split_once(',').ok_or_else(|| anyhow!("expected lon,lat: `{raw}`"))?;
            Ok([lon.trim().parse::<f64>()?, lat.trim().parse::<f64>()?])
        })
        .collect::<Result<_>>()?;

    // routing against an unpacked package exercises the artefact that ships, not the
    // intermediate tile directory
    let tile_dir = if args.tiles.is_file() {
        let out = std::env::temp_dir().join("cairn-vtiles");
        let _ = std::fs::remove_dir_all(&out);
        let count = package::unpack(&args.tiles, &out)?;
        println!("unpacked {count} tiles from {}", args.tiles.display());
        out
    } else {
        args.tiles.clone()
    };

    let template = args.config.unwrap_or_else(|| settings.valhalla_config_path());
    let mut router = routing::Router::open(&template, &tile_dir)?;
    let request = routing::RouteRequest { locations, costing: args.costing.clone() };
    let response = router.route(&request.to_json())?;

    if args.json {
        println!("{response}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&response)?;
    let summary = &parsed["trip"]["summary"];
    println!(
        "{:.1} km, {:.0} min, {} costing",
        summary["length"].as_f64().unwrap_or(0.0),
        summary["time"].as_f64().unwrap_or(0.0) / 60.0,
        args.costing
    );
    for leg in parsed["trip"]["legs"].as_array().unwrap_or(&vec![]) {
        for m in leg["maneuvers"].as_array().unwrap_or(&vec![]) {
            println!(
                "  {:>6.2} km  {}",
                m["length"].as_f64().unwrap_or(0.0),
                m["instruction"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}
