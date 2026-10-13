//! `cairn basemap` / `cairn routes` - the planetiler-driven steps.

use anyhow::{anyhow, Context, Result};
use cairn_core::presets::PresetStore;
use cairn_core::settings::Settings;
use cairn_core::steps::options;
use cairn_core::steps::planetiler::{run_cancellable, PlanetilerJob, Schema};
use cairn_core::steps::{state, StepEvent, StepId};
use cairn_core::toolchain;
use clap::Args as ClapArgs;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Area to build. Also names the output subdirectory.
    #[arg(long)]
    pub area: String,
    /// Named option set to start from, e.g. `measured`.
    #[arg(long)]
    pub preset: Option<String>,
    /// Option override, repeatable: -o simplify_tolerance=0.7
    #[arg(short = 'o', long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,
    /// Path to the planetiler jar. Defaults to a downloaded one, then the copy shipped beside
    /// this binary, then a checkout's `planetiler/planetiler-dist/target`.
    #[arg(long)]
    pub jar: Option<PathBuf>,
    /// Download the jar from this URL if none was found, and keep it for next time.
    #[arg(long)]
    pub jar_url: Option<String>,
    /// YAML schema to run instead of the bundled OpenMapTiles fork.
    #[arg(long)]
    pub schema: Option<PathBuf>,
    /// JVM heap in megabytes.
    #[arg(long, default_value_t = 12288)]
    pub heap_mb: u32,
    /// Clip to a .poly.
    #[arg(long)]
    pub polygon: Option<PathBuf>,
    /// Print the command that would run, and stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Output mbtiles. Defaults to <output>/<area>/<area>[_routes].mbtiles.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Planetiler's scratch directory. One per run by default, since two runs sharing one
    /// delete each other's sort chunks.
    #[arg(long)]
    pub tmp_dir: Option<PathBuf>,
    /// Java binary to run. Defaults to JAVA_HOME, then PATH.
    #[arg(long)]
    pub java: Option<PathBuf>,
    /// How often planetiler reports progress.
    #[arg(long, default_value = "1s")]
    pub log_interval: String,
    /// Use the OSM extract already on disk instead of letting planetiler download one.
    #[arg(long)]
    pub no_download: bool,
    /// Stop if the output is already there, instead of rebuilding it.
    #[arg(long)]
    pub skip_existing: bool,
    /// Stream planetiler's own output.
    #[arg(short, long)]
    pub verbose: bool,
    /// Anything else, passed to planetiler verbatim, after `--`.
    ///
    /// `-o` only knows the options this app has a schema for. Planetiler has many more, and its
    /// own documentation is the reference for them: https://github.com/onthegomap/planetiler.
    /// Everything after `--` goes through untouched.
    ///
    ///   cairn basemap --area alps -- --max-point-buffer=4 --mlt-shared-dict
    #[arg(last = true)]
    pub passthrough: Vec<String>,
}

/// Parse `key=value` overrides, checked against the step's schema.
///
/// An unknown key is refused rather than passed along: planetiler ignores flags it does not
/// know, so a typo would otherwise produce a build that silently used the default.
fn parse_overrides(
    defs: &[options::OptionDef],
    raw: &[String],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut values = BTreeMap::new();
    for entry in raw {
        let (key, value) =
            entry.split_once('=').ok_or_else(|| anyhow!("expected key=value, got `{entry}`"))?;
        let def = options::find(defs, key)
            .ok_or_else(|| anyhow!("unknown option `{key}` (see `cairn options <step>`)"))?;
        let parsed = match def.kind {
            options::OptionKind::Bool => serde_json::Value::Bool(
                value.parse().with_context(|| format!("`{key}` wants true or false"))?,
            ),
            options::OptionKind::Int { .. } => serde_json::Value::from(
                value.parse::<i64>().with_context(|| format!("`{key}` wants a whole number"))?,
            ),
            options::OptionKind::Float { .. } => serde_json::Value::from(
                value.parse::<f64>().with_context(|| format!("`{key}` wants a number"))?,
            ),
            _ => serde_json::Value::String(value.to_string()),
        };
        values.insert(key.to_string(), parsed);
    }
    Ok(values)
}

/// One line describing what was found on disk, so "already built" says what it saw.
pub fn describe(status: &state::StepStatus) -> String {
    match status {
        state::StepStatus::Built { outputs, elapsed, .. } => {
            let files: Vec<String> = outputs
                .iter()
                .map(|f| {
                    if f.dir {
                        format!("{}/", f.name)
                    } else {
                        format!("{} {}", f.name, super::mb(f.bytes))
                    }
                })
                .collect();
            match elapsed {
                Some(e) => format!("{}, built in {e}", files.join(", ")),
                None => files.join(", "),
            }
        }
        other => format!("{other:?}"),
    }
}

pub fn human_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub async fn run(settings: &Settings, args: Args, routes: bool) -> Result<()> {
    let step = if routes { StepId::Routes } else { StepId::Basemap };
    let defs = if routes { options::routes_options() } else { options::basemap_options() };

    // With no --preset, start from the README's own build command rather than planetiler's bare
    // defaults: cairn exists to reproduce this repository's tiles, and an untouched run producing
    // a differently-tuned map is a trap. `--preset stock` opts out.
    let mut values = BTreeMap::new();
    {
        let name = args.preset.as_deref().unwrap_or(cairn_core::presets::DEFAULT_PRESET);
        let mut store = PresetStore::default();
        for preset in cairn_core::presets::builtin() {
            store.upsert(preset);
        }
        match store.get(step, name) {
            Some(preset) => values.extend(preset.values.clone()),
            // an explicitly named preset must exist; the default one simply may not for this step
            None if args.preset.is_some() => {
                return Err(anyhow!("no preset `{name}` for {}", step.label()))
            }
            None => {}
        }
    }
    // explicit -o wins over the preset, so a preset can be used as a starting point
    values.extend(parse_overrides(&defs, &args.options)?);

    // `area_poly` is cairn's own switch, not a planetiler flag: turn it into a real --polygon
    // before the arguments are rendered, fetching the boundary if this is the first build.
    if let Some(path) = cairn_core::steps::download::apply_area_poly(
        &mut values,
        "polygon",
        &settings.data_dir,
        &args.area,
        |_, _| {},
    )
    .await?
    {
        println!("clipping to {}", path.display());
    }

    let jar = match args.jar.clone().or_else(|| settings.planetiler_jar_path()) {
        Some(jar) => jar,
        None => {
            let url = args
                .jar_url
                .clone()
                .or_else(|| settings.planetiler_jar_url.clone())
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!("no planetiler jar found; pass --jar, or --jar-url to fetch one")
                })?;
            let dir = settings
                .jar_dir
                .clone()
                .ok_or_else(|| anyhow!("nowhere to keep a downloaded jar"))?;
            let name = url.rsplit('/').next().unwrap_or("planetiler.jar");
            let name = if name.ends_with("-with-deps.jar") {
                name.to_string()
            } else {
                format!("{}-with-deps.jar", name.trim_end_matches(".jar"))
            };
            println!("fetching {url}");
            let mut last = u8::MAX;
            cairn_core::steps::download::fetch_url(&url, &dir.join(name), |done, total| {
                let percent = match total {
                    Some(total) if total > 0 => ((done * 100) / total).min(100) as u8,
                    _ => 0,
                };
                if percent != last {
                    last = percent;
                    print!("\r  {percent:>3}%");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            })
            .await?
        }
    };
    let java = match &args.java {
        // an explicit --java is taken at its word: the point of the flag is to run a JDK this
        // probe would not have picked
        Some(path) => cairn_core::toolchain::JavaInstall {
            path: path.clone(),
            version: toolchain::MIN_JAVA,
            source: cairn_core::toolchain::JavaSource::Configured,
        },
        None => toolchain::find(None, &settings.repo_root.join(".jre"))
            .await
            .ok_or_else(|| anyhow!("no Java {}+ found", toolchain::MIN_JAVA))?,
    };

    let suffix = if routes { "_routes" } else { "" };
    let mut extra = vec![format!("--area={}", args.area), "--force".to_string()];
    if !args.no_download {
        extra.push("--download".to_string());
    }
    if let Some(polygon) = &args.polygon {
        extra.push(format!("--polygon={}", polygon.display()));
    }
    extra.extend(options::to_args(&defs, &values));
    extra.extend(args.passthrough.clone());

    let job = PlanetilerJob {
        step,
        area: args.area.clone(),
        java: java.path,
        jar,
        schema: match args.schema {
            Some(path) => Schema::Yaml { path },
            None => Schema::OpenMapTiles,
        },
        heap_mb: args.heap_mb,
        output: args.output.clone().unwrap_or_else(|| {
            settings.area_dir(&args.area).join(format!("{}{suffix}.mbtiles", args.area))
        }),
        // its own directory: two planetiler runs sharing one delete each other's sort chunks
        tmp_dir: args.tmp_dir.clone().unwrap_or_else(|| {
            settings.run_tmp_dir(&format!(
                "{}-{}",
                args.area,
                if routes { "routes" } else { "basemap" }
            ))
        }),
        extra_args: extra,
        working_dir: settings.repo_root.clone(),
        log_interval: args.log_interval.clone(),
    };

    if args.dry_run {
        let quoted: Vec<String> =
            job.command_line().iter().map(|a| cairn_core::steps::shell_quote(a)).collect();
        println!("{}", quoted.join(" \\\n  "));
        return Ok(());
    }

    // a command line runs what it says; only --skip-existing turns that into a no-op, so this
    // behaves like the shell script it replaces rather than like the app's plan
    let area_dir = settings.area_dir(&args.area);
    if args.skip_existing && job.output.is_file() {
        println!("{} is already there", job.output.display());
        return Ok(());
    }
    let started = std::time::Instant::now();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StepEvent>(512);
    let handle = tokio::spawn(run_cancellable(job, tx, tokio::sync::mpsc::channel(1).1));

    let mut last_phase = String::new();
    // kept so a failure can show what planetiler said; printing every line would bury the
    // progress display, and printing none leaves "exited non-zero" and nothing else
    let mut log: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            StepEvent::Phase { name, .. } => {
                last_phase = name;
                println!("[{last_phase}]");
            }
            StepEvent::Progress { label, percent, .. } => {
                print!("\r  {label:>12} {percent:>3}%   ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            StepEvent::Finished { ok, elapsed, outputs, .. } => {
                println!(
                    "\n{} {}",
                    if ok { "finished" } else { "FAILED" },
                    elapsed.map(|e| format!("in {e}")).unwrap_or_default()
                );
                if !ok {
                    for line in log.iter().rev().take(25).rev() {
                        println!("  {line}");
                    }
                }
                for output in outputs {
                    println!("  {output}");
                }
            }
            StepEvent::Log { line, .. } => {
                if args.verbose {
                    println!("{line}");
                }
                log.push(line);
                if log.len() > 400 {
                    log.remove(0);
                }
            }
            StepEvent::Command { argv, .. } => {
                // always printed, not only under --verbose: which jar ran and with which flags
                // is the first thing anyone asks when the output looks wrong
                println!("$ {}", argv.join(" "));
                log.push(argv.join(" "));
            }
            StepEvent::Started { .. } | StepEvent::Skipped { .. } | StepEvent::Cli { .. } => {}
        }
    }

    match handle.await? {
        Ok(true) => {
            state::mark_done(&area_dir, step, Some(human_elapsed(started.elapsed())), &values)?;
            Ok(())
        }
        Ok(false) => Err(anyhow!("planetiler exited non-zero")),
        Err(e) => Err(e),
    }
}
