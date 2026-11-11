//! `cairn contours` - contour lines as vector tiles, from the same sources as `cairn terrain`.
//!
//! Replaces `build_contours.sh` and the two scripts that followed it. Its flags keep the old
//! script's names - `--minzoom`, `--maxzoom`, `--poly-shape`, `--polyzoom`, `--bounds`, `--name`,
//! `--output` - and take the elevation from `sources.json` where the script wanted a GeoTIFF
//! built first.

use anyhow::{anyhow, Context, Result};
use cairn_core::contours::{build, tileset, ContourOptions};
use cairn_core::settings::Settings;
use cairn_core::steps::{archive::ArchiveFormat, state, StepId};
use cairn_core::terrain::archive::ResumeMode;
use cairn_core::terrain::source;
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::terrain::{basemap_bounds, load_specs, parse_bounds};

#[derive(ClapArgs)]
pub struct Args {
    /// Area, used to name the output.
    #[arg(long)]
    pub area: String,
    /// sources.json listing the elevation sources, lowest priority first.
    #[arg(long)]
    pub sources: Option<PathBuf>,
    /// Where the .hgt elevation tiles are, when sources.json does not name a directory.
    #[arg(long)]
    pub elevation_dir: Option<PathBuf>,
    /// Lowest zoom written.
    #[arg(long, default_value_t = 11)]
    pub minzoom: u8,
    /// Highest zoom written.
    #[arg(long, default_value_t = 14)]
    pub maxzoom: u8,
    /// Metres between two lines.
    #[arg(long, default_value_t = 10)]
    pub interval: u32,
    /// Name of the vector layer.
    #[arg(long, default_value = "contour")]
    pub layer: String,
    /// The `name` in the archive's metadata. Defaults to `<area>_contours`.
    #[arg(long)]
    pub name: Option<String>,
    /// Douglas-Peucker tolerance, in tile units of 4096 to a tile. 0 keeps every vertex.
    #[arg(long, default_value_t = 3.0)]
    pub simplify: f64,
    /// The finest the elevation is sampled, in metres between samples.
    #[arg(long, default_value_t = 10.0)]
    pub resolution: f64,
    /// The most samples along one side of a tile.
    #[arg(long, default_value_t = 256)]
    pub max_grid: u32,
    /// Metres over which a higher-priority source fades in at its coverage boundary.
    #[arg(long, default_value_t = 1000.0)]
    pub blur: f64,
    /// west,south,east,north. Defaults to the shape's bounds, else the area's basemap bounds.
    #[arg(long)]
    pub bounds: Option<String>,
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Write PMTiles instead of MBTiles.
    #[arg(long)]
    pub pmtiles: bool,
    /// Osmosis .poly limiting which tiles are written; its tile mask goes in the metadata.
    #[arg(long)]
    pub poly_shape: Option<PathBuf>,
    /// Clip to the area's own boundary, downloading it from Geofabrik when it is not already
    /// beside the extract. Ignored when --poly-shape names a shape.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub area_poly: bool,
    /// Zoom of the tile mask stored in the metadata.
    #[arg(long, default_value_t = 11)]
    pub polyzoom: u8,
    /// Render only the tiles the area's basemap also has.
    #[arg(long)]
    pub match_basemap: bool,
    /// Worker threads. Defaults to the number of cores.
    #[arg(long, short = 'j')]
    pub workers: Option<usize>,
    /// Do not fetch missing .hgt tiles; trace only what is already on disk.
    #[arg(long)]
    pub no_elevation_download: bool,
    /// Stop if the output is already there, instead of replacing it.
    #[arg(long)]
    pub skip_existing: bool,
    /// Print what would be built, and stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Discard a partial build left by an interrupted run and start from the beginning. Without
    /// it, a run that finds one made with the same settings continues it, and one made with
    /// different settings is refused.
    #[arg(long)]
    pub fresh: bool,
}

impl Args {
    /// For the test that holds the flags' defaults to the core's.
    #[cfg(test)]
    pub fn options_for_test(&self) -> ContourOptions {
        self.options()
    }

    fn options(&self) -> ContourOptions {
        ContourOptions {
            minzoom: self.minzoom,
            maxzoom: self.maxzoom,
            interval: self.interval,
            layer: self.layer.clone(),
            simplify: self.simplify,
            resolution_m: self.resolution,
            max_grid: self.max_grid,
            blur_m: self.blur,
            polyzoom: self.polyzoom,
        }
    }
}

/// The option values this run is defined by, for the build record.
///
/// Only the ones that change the output: `--output` and `--skip-existing` decide where it goes and
/// whether to run, not what gets written.
fn recorded_options(args: &Args) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut values = std::collections::BTreeMap::new();
    values.insert("minzoom".into(), args.minzoom.into());
    values.insert("maxzoom".into(), args.maxzoom.into());
    values.insert("interval".into(), args.interval.into());
    values.insert("layer".into(), args.layer.clone().into());
    values.insert("simplify".into(), args.simplify.into());
    values.insert("resolution".into(), args.resolution.into());
    values.insert("max_grid".into(), args.max_grid.into());
    values.insert("blur".into(), args.blur.into());
    values.insert("polyzoom".into(), args.polyzoom.into());
    if args.pmtiles {
        values.insert(cairn_core::steps::archive::PMTILES_KEY.into(), true.into());
    }
    if args.match_basemap {
        values.insert("match_basemap".into(), true.into());
    }
    if let Some(poly) = &args.poly_shape {
        values.insert("poly_shape".into(), poly.display().to_string().into());
    }
    if let Some(bounds) = &args.bounds {
        values.insert("bounds".into(), bounds.clone().into());
    }
    if let Some(name) = &args.name {
        values.insert("name".into(), name.clone().into());
    }
    values
}

pub async fn run(settings: &Settings, mut args: Args) -> Result<()> {
    let area_dir = settings.area_dir(&args.area);
    if args.poly_shape.is_none() && args.area_poly {
        let path =
            cairn_core::steps::download::ensure_poly(&settings.data_dir, &args.area, |_, _| {})
                .await
                .with_context(|| format!("resolving the boundary for `{}`", args.area))?;
        println!("clipping to {}", path.display());
        args.poly_shape = Some(path);
    }
    let recorded = recorded_options(&args);
    let opts = args.options();
    opts.check()?;

    let (specs, sources_path, hgt_dir) =
        load_specs(settings, args.sources.clone(), args.elevation_dir.clone())?;
    let (composite, skipped) = source::CompositeSource::open(&specs)?;
    if args.dry_run {
        println!("sources, highest priority first: {:?}", composite.names());
        for note in &skipped {
            println!("  skipped {note}");
        }
    }
    if composite.names().is_empty() {
        return Err(anyhow!("no usable elevation sources in {}", sources_path.display()));
    }
    drop(composite);

    let shape =
        args.poly_shape.as_ref().map(|p| cairn_core::poly::Polygon::parse(p)).transpose()?;
    let bounds = match (&args.bounds, &shape) {
        (Some(raw), _) => parse_bounds(raw)?,
        (None, Some(shape)) => shape.bounds(),
        (None, None) => basemap_bounds(settings, &args.area)?,
    };

    let archive_format = if args.pmtiles { ArchiveFormat::PMTiles } else { ArchiveFormat::MBTiles };
    let output = args.output.clone().unwrap_or_else(|| {
        area_dir.join(
            cairn_core::steps::archive::archive_name(&args.area, StepId::Contours, archive_format)
                .expect("contours have an archive name"),
        )
    });
    if args.skip_existing && output.is_file() {
        println!("{} is already there", output.display());
        return Ok(());
    }
    if let Some(workers) = args.workers {
        // best effort: a second build in the same process would keep the first pool
        let _ = rayon::ThreadPoolBuilder::new().num_threads(workers).build_global();
    }

    let basemap = if args.match_basemap {
        match tileset::basemap_path(&area_dir, &args.area) {
            Some(path) => Some(path),
            None => {
                println!("no basemap for `{}` yet, so every tile is kept", args.area);
                None
            }
        }
    } else {
        None
    };

    let name = args.name.clone().unwrap_or_else(|| format!("{}_contours", args.area));
    if args.dry_run {
        let job = job(
            &specs,
            &opts,
            bounds,
            shape.as_ref(),
            &args,
            basemap.as_deref(),
            &output,
            &name,
            archive_format,
        );
        let filter = match &basemap {
            Some(path) => Some(tileset::TileFilter::open(path, opts.minzoom, opts.maxzoom)?),
            None => None,
        };
        let centre = cairn_core::contours::centre_latitude(bounds);
        let mut tiles = 0u64;
        for zoom in opts.minzoom..=opts.maxzoom {
            let count = build::wanted_tiles(&job, filter.as_ref(), zoom).len() as u64;
            let grid = cairn_core::contours::grid_for(&opts, zoom, centre);
            println!("z{zoom}: up to {count} tiles, a {0}x{0} grid each", grid.side());
            tiles += count;
        }
        println!(
            "z{}-{} over {:.4},{:.4},{:.4},{:.4}: up to {tiles} tiles -> {}",
            opts.minzoom,
            opts.maxzoom,
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3,
            output.display()
        );
        return Ok(());
    }

    // a missing .hgt is silent - there are no lines there and the archive comes out with a hole -
    // so the tiles this build needs are fetched first, unless no source reads them at all
    let wants_hgt = specs.iter().any(|s| source::is_hgt(&s.kind));
    if !args.no_elevation_download && wants_hgt {
        let (got, total) = cairn_core::steps::elevation::ensure(&hgt_dir, bounds, |done, total| {
            print!("\r  elevation {done}/{total}   ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        })
        .await?;
        if total > 0 {
            println!("\r  elevation: {got} downloaded of {total} covering tiles");
        }
    }

    println!(
        "bounds {:.4},{:.4},{:.4},{:.4}  z{}-{}  every {} m  -> {}",
        bounds.0,
        bounds.1,
        bounds.2,
        bounds.3,
        opts.minzoom,
        opts.maxzoom,
        opts.interval,
        output.display()
    );

    let started = std::time::Instant::now();
    let job = job(
        &specs,
        &opts,
        bounds,
        shape.as_ref(),
        &args,
        basemap.as_deref(),
        &output,
        &name,
        archive_format,
    );

    // a progress line is redrawn in place, so a log line has to clear it first
    let mut drawing = false;
    let outcome = build::run(&job, &|| false, &mut |event| {
        use cairn_core::terrain::build::Event;
        use std::io::Write;
        match event {
            Event::Log(line) => {
                if drawing {
                    print!("\r\x1b[K");
                    drawing = false;
                }
                println!("  {line}");
            }
            Event::Phase(_) => {}
            Event::Progress { label, percent } => {
                print!("\r  {label} {percent}%   ");
                let _ = std::io::stdout().flush();
                drawing = true;
            }
        }
    })?;
    println!();
    if !outcome.finished {
        println!("stopped with {} tiles written; run it again to continue", outcome.written);
        return Ok(());
    }

    state::mark_done(
        &area_dir,
        StepId::Contours,
        Some(super::planetiler::human_elapsed(started.elapsed())),
        &recorded,
    )?;

    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {} tiles, {} in {:.1}s",
        outcome.written,
        super::mb(size),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn job<'a>(
    specs: &'a [source::SourceSpec],
    opts: &'a ContourOptions,
    bounds: (f64, f64, f64, f64),
    shape: Option<&'a cairn_core::poly::Polygon>,
    args: &'a Args,
    basemap: Option<&'a std::path::Path>,
    output: &'a std::path::Path,
    name: &'a str,
    archive_format: ArchiveFormat,
) -> build::Job<'a> {
    build::Job {
        specs,
        opts,
        bounds,
        shape,
        shape_file: args.poly_shape.as_deref(),
        only_tiles_of: basemap,
        archive_format,
        output,
        name,
        mode: if args.fresh { ResumeMode::Fresh } else { ResumeMode::Auto },
        batch: cairn_core::terrain::build::BATCH,
    }
}
