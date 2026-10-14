//! `cairn download` / `elevation` / `valhalla-tiles` - the steps that were shell before.

use anyhow::{anyhow, Result};
use cairn_core::settings::Settings;
use cairn_core::steps::external::{self, ToolJob};
use cairn_core::steps::{download, state, StepEvent, StepId};
use clap::Args as ClapArgs;
use std::collections::BTreeMap;

/// Mirrors `scripts/download-osm.py`: fetch one area's extract, wherever it comes from.
#[derive(ClapArgs)]
pub struct DownloadArgs {
    /// Geofabrik area id, e.g. `rhone-alpes`. Names the output when `--output` is not given.
    #[arg(long)]
    pub area: String,
    /// Download this URL instead of resolving the area through Geofabrik's index.
    #[arg(long)]
    pub url: Option<String>,
    /// Where to write the .pbf. Defaults to <repo>/data/sources/<area>.osm.pbf.
    #[arg(long)]
    pub output: Option<std::path::PathBuf>,
    /// Stop if the extract is already there.
    #[arg(long)]
    pub skip_existing: bool,
    /// Print the resolved URL and stop.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn download(settings: &Settings, args: DownloadArgs) -> Result<()> {
    let url = match &args.url {
        Some(url) => url.clone(),
        None => download::resolve_url(&args.area).await?,
    };
    if args.dry_run {
        println!("{url}");
        return Ok(());
    }
    let target = args
        .output
        .clone()
        .unwrap_or_else(|| download::extract_path(&settings.data_dir, &args.area));
    if args.skip_existing && target.is_file() {
        println!("{} is already there", target.display());
        return Ok(());
    }

    let mut last = u8::MAX;
    let path = download::fetch_url(&url, &target, |done, total| {
        let percent = match total {
            Some(total) if total > 0 => ((done * 100) / total).min(100) as u8,
            _ => 0,
        };
        if percent != last {
            last = percent;
            print!("\r  {percent:>3}%  {}", super::mb(done));
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    })
    .await?;
    println!("\n  {}", path.display());
    // recording is a courtesy to the app's build view; it never gates what the CLI does
    let _ = state::mark_done(
        &settings.area_dir(&args.area),
        StepId::DownloadOsm,
        None,
        &BTreeMap::new(),
    );
    Ok(())
}

#[derive(ClapArgs)]
pub struct ToolArgs {
    /// Area the run is for. Names the build record, and finds the OSM extract.
    #[arg(long)]
    pub area: String,
    /// valhalla.json to use. Defaults to <repo>/valhalla.json.
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
    /// Directory holding the Valhalla binaries. Defaults to <repo>/valhalla/build, then PATH.
    #[arg(long)]
    pub bin_dir: Option<std::path::PathBuf>,
    /// `w,s,e,n` for valhalla_build_elevation, instead of taking bounds from the config.
    #[arg(long)]
    pub bbox: Option<String>,
    /// Elevation output directory, or the .osm.pbf for valhalla_build_tiles.
    #[arg(long)]
    pub input: Option<std::path::PathBuf>,
    /// Where valhalla_build_elevation writes. Defaults to <repo>/elevation_tiles.
    #[arg(long)]
    pub output: Option<std::path::PathBuf>,
    /// Stop if the output is already there.
    #[arg(long)]
    pub skip_existing: bool,
    /// Print the command that would run, and stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Extra arguments passed to the binary verbatim, after `--`.
    #[arg(last = true)]
    pub passthrough: Vec<String>,
}

/// The `.hgt` tiles the graph bakes in, downloaded natively.
///
/// `valhalla_build_elevation` is a Python script; this does the same job without one, so neither
/// the CLI nor the packaged app needs an interpreter.
pub async fn elevation(settings: &Settings, args: ToolArgs) -> Result<()> {
    use cairn_core::steps::elevation as hgt;

    let out = args.output.clone().unwrap_or_else(|| settings.elevation_tiles_dir.clone());
    let bounds = match &args.bbox {
        Some(raw) => {
            let p: Vec<f64> = raw.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if p.len() != 4 {
                return Err(anyhow!("--bbox wants west,south,east,north"));
            }
            (p[0], p[1], p[2], p[3])
        }
        None => area_bounds(settings, &args.area)?,
    };

    let tiles = hgt::tiles_for_bounds(bounds);
    println!(
        "{} tiles cover {:.2},{:.2},{:.2},{:.2} -> {}",
        tiles.len(),
        bounds.0,
        bounds.1,
        bounds.2,
        bounds.3,
        out.display()
    );
    if args.dry_run {
        for tile in &tiles {
            println!("  {}/{}", tile.dir, tile.name);
        }
        return Ok(());
    }

    let (downloaded, total) = hgt::fetch(&out, &tiles, false, |done, total| {
        print!("\r  {done}/{total}   ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    })
    .await?;
    println!("\n{downloaded} downloaded, {} already there", total - downloaded);

    let _ = state::mark_done(
        &settings.area_dir(&args.area),
        StepId::ElevationTiles,
        None,
        &BTreeMap::new(),
    );
    Ok(())
}

/// The area's extent, from its basemap - the same source the terrain step uses, so the two cover
/// the same ground.
fn area_bounds(settings: &Settings, area: &str) -> Result<(f64, f64, f64, f64)> {
    let raw = cairn_core::catalog::discover(&settings.output_root)?
        .into_iter()
        .find(|a| a.name == area)
        .and_then(|a| {
            a.artifacts
                .iter()
                .find(|x| x.kind == cairn_core::catalog::ArtifactKind::Basemap)?
                .bounds
                .clone()
        })
        .ok_or_else(|| anyhow!("no --bbox given and no basemap for `{area}` to take one from"))?;
    let p: Vec<f64> = raw.split(',').filter_map(|v| v.trim().parse().ok()).collect();
    if p.len() != 4 {
        return Err(anyhow!("the basemap's bounds are not west,south,east,north"));
    }
    Ok((p[0], p[1], p[2], p[3]))
}

fn bin(args: &ToolArgs, settings: &Settings, name: &str) -> Result<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = &args.bin_dir {
        dirs.push(dir.clone());
    }
    dirs.extend(settings.valhalla_bin_dirs());
    external::find_tool(dirs.iter().map(|p| p.as_path()), name).ok_or_else(|| {
        anyhow!(
            "{name} not found; looked in {}, and on PATH. Pass --bin-dir.",
            dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )
    })
}

/// `valhalla_build_tiles` - the routing graph itself.
pub async fn valhalla_tiles(settings: &Settings, args: ToolArgs) -> Result<()> {
    let config = args.config.clone().unwrap_or_else(|| settings.valhalla_config_path());
    let pbf = args
        .input
        .clone()
        .unwrap_or_else(|| download::extract_path(&settings.data_dir, &args.area));
    if !pbf.is_file() {
        return Err(anyhow!(
            "{} is missing; run `cairn download --area {}` or pass --input",
            pbf.display(),
            args.area
        ));
    }
    let mut tool_args =
        vec!["-c".to_string(), config.display().to_string(), pbf.display().to_string()];
    tool_args.extend(args.passthrough.clone());

    // the graph lands wherever the config's mjolnir.tile_dir points, which is the config's
    // business; the default is what `state` also looks at
    let out = args.output.clone().unwrap_or_else(|| settings.repo_root.join("valhalla_tiles"));
    let job = ToolJob {
        step: StepId::ValhallaTiles,
        area: args.area.clone(),
        program: bin(&args, settings, "valhalla_build_tiles")?,
        args: tool_args,
        working_dir: settings.repo_root.clone(),
        parse: external::valhalla_line,
    };
    run_tool(settings, args, job, out).await
}

/// `valhalla_build_admins` - the country and state boundaries the graph builder looks nodes up in.
pub async fn valhalla_admins(settings: &Settings, args: ToolArgs) -> Result<()> {
    use cairn_core::steps::admins;

    let output = admins::output_path(settings);
    if args.skip_existing && output.is_file() {
        println!("{} is already there", output.display());
        return Ok(());
    }
    let program = bin(&args, settings, "valhalla_build_admins")?;
    let prepared = admins::prepare(
        settings,
        &args.area,
        program,
        args.config.as_deref(),
        args.input.as_deref(),
    )?;
    if args.dry_run {
        let quoted: Vec<String> =
            prepared.job.command_line().iter().map(|a| cairn_core::steps::shell_quote(a)).collect();
        println!("{}", quoted.join(" "));
        println!("  -> {}", prepared.output.display());
        return Ok(());
    }

    let started = std::time::Instant::now();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StepEvent>(512);
    let job = ToolJob {
        step: prepared.job.step,
        area: prepared.job.area.clone(),
        program: prepared.job.program.clone(),
        args: prepared.job.args.clone(),
        working_dir: prepared.job.working_dir.clone(),
        parse: prepared.job.parse,
    };
    let handle = tokio::spawn(external::run(job, tx, tokio::sync::mpsc::channel(1).1));
    while let Some(event) = rx.recv().await {
        match event {
            StepEvent::Phase { name, .. } => println!("[{name}]"),
            StepEvent::Log { line, .. } => println!("{line}"),
            _ => {}
        }
    }
    match handle.await? {
        Ok(true) => {}
        Ok(false) => {
            admins::discard(&prepared);
            return Err(anyhow!("valhalla_build_admins exited non-zero"));
        }
        Err(e) => {
            admins::discard(&prepared);
            return Err(e);
        }
    }
    let report = admins::finish(&prepared)?;
    println!("{report}\n  {}", prepared.output.display());
    let _ = state::mark_done(
        &settings.area_dir(&args.area),
        StepId::ValhallaAdmins,
        Some(super::planetiler::human_elapsed(started.elapsed())),
        &BTreeMap::new(),
    );
    Ok(())
}

async fn run_tool(
    settings: &Settings,
    args: ToolArgs,
    job: ToolJob,
    output: std::path::PathBuf,
) -> Result<()> {
    let step = job.step;
    if args.dry_run {
        let quoted: Vec<String> =
            job.command_line().iter().map(|a| cairn_core::steps::shell_quote(a)).collect();
        println!("{}", quoted.join(" "));
        return Ok(());
    }
    let occupied = std::fs::read_dir(&output).map(|mut e| e.next().is_some()).unwrap_or(false);
    if args.skip_existing && occupied {
        println!("{} already holds something", output.display());
        return Ok(());
    }

    let started = std::time::Instant::now();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StepEvent>(512);
    let handle = tokio::spawn(external::run(job, tx, tokio::sync::mpsc::channel(1).1));

    while let Some(event) = rx.recv().await {
        match event {
            StepEvent::Phase { name, .. } => println!("[{name}]"),
            // these tools are slow and quiet; their own log is the only progress there is
            StepEvent::Log { line, .. } => println!("{line}"),
            _ => {}
        }
    }

    match handle.await? {
        Ok(true) => {
            let _ = state::mark_done(
                &settings.area_dir(&args.area),
                step,
                Some(super::planetiler::human_elapsed(started.elapsed())),
                &BTreeMap::new(),
            );
            Ok(())
        }
        Ok(false) => Err(anyhow!("{} exited non-zero", step.label())),
        Err(e) => Err(e),
    }
}
