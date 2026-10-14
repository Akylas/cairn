//! Running the Valhalla command-line tools as steps.
//!
//! `valhalla_build_elevation` and `valhalla_build_tiles` are the two parts of the pipeline that
//! stay subprocesses. Both are C++ binaries shipped with the app or built from the submodule,
//! both take an hour or more on a large area, and neither has an embeddable form worth the
//! linking. What this module adds
//! is what the app needs and a shell does not give: their output as events, and a cancel that
//! actually kills the process.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{StepEvent, StepId};

/// How a tool's output line becomes an event.
///
/// Each tool announces its progress in its own shape, and none of them is a contract: the
/// parser is per-job so that a new tool does not mean teaching one function about every format
/// there has ever been.
pub type LineParser = fn(StepId, &str) -> StepEvent;

/// One external binary, its arguments, and where to run it.
pub struct ToolJob {
    pub step: StepId,
    pub area: String,
    /// The binary, already resolved to a path.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    /// Reads this tool's own progress format. `valhalla_line` is the historical default.
    pub parse: LineParser,
}

impl ToolJob {
    /// What would run, for a dry run or an error message.
    pub fn command_line(&self) -> Vec<String> {
        let mut line = vec![self.program.display().to_string()];
        line.extend(self.args.clone());
        line
    }
}

/// Find a Valhalla binary in the given directories, then on `PATH`.
///
/// The caller passes them in order of authority - configured, bundled with the app, built in
/// the submodule - because a packaged install has no submodule and a checkout has no bundle.
pub fn find_tool<'a>(dirs: impl IntoIterator<Item = &'a Path>, name: &str) -> Option<PathBuf> {
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Run the tool, streaming its output as events, killing it if `cancel` fires.
///
/// Returns whether it exited cleanly. Both binaries log to stderr, so the two streams are read
/// together - reading only stdout is how a run looks silent for forty minutes.
pub async fn run(
    job: ToolJob,
    tx: mpsc::Sender<StepEvent>,
    mut cancel: mpsc::Receiver<()>,
) -> Result<bool> {
    let step = job.step;
    if !job.program.is_file() {
        return Err(anyhow!(
            "{} not found - it ships with a packaged build; otherwise build the Valhalla \
             submodule or set the binary directory in Settings",
            job.program.display()
        ));
    }
    let _ = tx.send(StepEvent::Started { step, area: job.area.clone() }).await;
    let _ = tx
        .send(StepEvent::Log {
            step,
            line: job
                .command_line()
                .iter()
                .map(|a| super::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        })
        .await;

    // what the build would otherwise only complain about once per tile, hours in
    if step == StepId::ValhallaTiles {
        let config = job.args.windows(2).find(|w| w[0] == "-c").map(|w| PathBuf::from(&w[1]));
        for warning in config.iter().flat_map(|c| config_warnings(c, &job.working_dir)) {
            let _ = tx.send(StepEvent::Log { step, line: warning }).await;
        }
    }

    let mut child = Command::new(&job.program)
        .args(&job.args)
        .current_dir(&job.working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let parse = job.parse;
    let mut out = BufReader::new(child.stdout.take().expect("stdout piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("stderr piped")).lines();
    let mut cancelled = false;

    loop {
        tokio::select! {
            line = out.next_line() => match line? {
                Some(line) => emit(&tx, step, parse, line).await,
                // stdout closing does not mean the process is done; stderr may still be open
                None => if err_done(&mut err, &tx, step, parse).await? { break },
            },
            line = err.next_line() => match line? {
                Some(line) => emit(&tx, step, parse, line).await,
                None => if out_done(&mut out, &tx, step, parse).await? { break },
            },
            signal = cancel.recv() => {
                if signal.is_none() {
                    // nobody can cancel any more; keep reading until the process ends
                    cancel = mpsc::channel(1).1;
                    continue;
                }
                cancelled = true;
                let _ = child.start_kill();
                break;
            }
        }
    }

    let status = child.wait().await?;
    let ok = status.success() && !cancelled;
    let _ = tx.send(StepEvent::Finished { step, ok, elapsed: None, outputs: vec![] }).await;
    Ok(ok)
}

/// Drain the other stream once one has closed, then report that both are done.
async fn err_done(
    err: &mut tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    tx: &mpsc::Sender<StepEvent>,
    step: StepId,
    parse: LineParser,
) -> Result<bool> {
    while let Some(line) = err.next_line().await? {
        emit(tx, step, parse, line).await;
    }
    Ok(true)
}

async fn out_done(
    out: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    tx: &mpsc::Sender<StepEvent>,
    step: StepId,
    parse: LineParser,
) -> Result<bool> {
    while let Some(line) = out.next_line().await? {
        emit(tx, step, parse, line).await;
    }
    Ok(true)
}

/// Turn a log line into an event, promoting the stage markers to phases.
async fn emit(tx: &mpsc::Sender<StepEvent>, step: StepId, parse: LineParser, line: String) {
    // The tools colour what they print unless told not to, and the escape codes end up in the
    // log as text - and, worse, in the middle of the markers the parsers look for.
    let line = crate::progress::strip_ansi(&line);
    match parse(step, &line) {
        // A phase is extra information *about* the line, not a replacement for it - the log
        // still has to show the banner the tool printed.
        event @ StepEvent::Phase { .. } => {
            // A tool with no percentage of its own at least has a place in its sequence, and a
            // bar that sits at 0% for an hour reads as a hung process.
            let reading = match (&event, stage_percent(step, &line)) {
                (StepEvent::Phase { name, .. }, Some(percent)) => {
                    Some(StepEvent::Progress { step, label: name.clone(), percent })
                }
                _ => None,
            };
            let _ = tx.send(event).await;
            if let Some(reading) = reading {
                let _ = tx.send(reading).await;
            }
            let _ = tx.send(StepEvent::Log { step, line }).await;
        }
        // A progress reading replaces its line. The line it came from is either a restatement
        // of the reading, or - for a tool that redraws with carriage returns and no newline -
        // every frame it has ever drawn, which for one global build was a single 17 kB line.
        event @ StepEvent::Progress { .. } => {
            let _ = tx.send(event).await;
        }
        event => {
            let _ = tx.send(event).await;
        }
    }
}

/// The Valhalla tools' own format, and the default for anything that has no parser of its own.
///
/// `valhalla_build_tiles` announces each stage as `[INFO] Parsing files for ways: ...` or
/// `[INFO] Building 2 tiles with 18 threads...`; anything else is ordinary chatter.
pub fn valhalla_line(step: StepId, line: &str) -> StepEvent {
    let clean = crate::progress::strip_ansi(line);
    match phase_of(step, &clean) {
        Some(name) => StepEvent::Phase { step, name },
        None => StepEvent::Log { step, line: clean },
    }
}

/// The stages of `valhalla_build_tiles`, in the order it runs them: what to look for in an info
/// line, what to call the stage, and roughly how far through the build it is.
///
/// The percentages are positions in the sequence, not shares of the time: parsing a continent's
/// ways and the enhancer take hours and the stages between them seconds. They exist so that a
/// build that is working does not look like one that is not - and they are only ever read from
/// banners the tool prints, so a change in its wording costs a stage name, never a build.
const STAGES: [(&str, &str, u8); 14] = [
    ("Parsing files for ways", "Parsing ways", 3),
    ("Parsing files for relations", "Parsing relations", 20),
    ("Parsing files for nodes", "Parsing nodes", 24),
    ("Creating graph edges from ways", "Creating graph edges", 38),
    ("Sorting graph", "Sorting graph", 42),
    ("Reclassifying_V2 link", "Classifying links", 46),
    ("tiles with", "Building tiles", 50),
    ("Enhancing local graph", "Enhancing the graph", 58),
    ("GraphFilter", "Filtering", 80),
    ("HierarchyBuilder", "Building the hierarchy", 83),
    ("Creating shortcuts on level", "Creating shortcuts", 87),
    ("Adding complex turn restrictions", "Adding turn restrictions", 91),
    ("Validating, finishing and binning", "Validating tiles", 94),
    ("Binning inter-tile", "Binning edges", 97),
];

/// The same for `valhalla_build_admins`, which prints different banners - some of them (`Parsing
/// ways...`) the same words as the graph builder's, which is why the table follows the step.
const ADMIN_STAGES: [(&str, &str, u8); 6] = [
    ("Parsing relations", "Finding boundaries", 3),
    ("admin polygons comprised", "Found boundaries", 20),
    ("Parsing ways...", "Parsing ways", 25),
    ("Parsing nodes...", "Parsing nodes", 40),
    ("Start populating admin tables", "Building the database", 85),
    ("Writing database to disk", "Writing the database", 97),
];

fn stages_of(step: StepId) -> &'static [(&'static str, &'static str, u8)] {
    match step {
        StepId::ValhallaAdmins => &ADMIN_STAGES,
        _ => &STAGES,
    }
}

fn info_of(line: &str) -> Option<&str> {
    Some(line.split_once("[INFO] ")?.1)
}

/// Where in the build this line says it is, for a tool that does not report a percentage.
pub fn stage_percent(step: StepId, line: &str) -> Option<u8> {
    let rest = info_of(line)?;
    stages_of(step).iter().find(|(needle, ..)| rest.contains(needle)).map(|(.., percent)| *percent)
}

/// Recognise the stage banners Valhalla prints, so the UI can show where a long build is.
fn phase_of(step: StepId, line: &str) -> Option<String> {
    let rest = info_of(line)?;
    if let Some((_, name, _)) = stages_of(step).iter().find(|(needle, ..)| rest.contains(needle)) {
        return Some((*name).to_string());
    }
    const BANNERS: [&str; 8] = [
        "Parsing files",
        "Building",
        "Sorting",
        "Enhancing",
        "Filtering",
        "Validating",
        "Elevation",
        "Cleaning",
    ];
    let banner = BANNERS.iter().find(|b| rest.starts_with(**b))?;
    Some(rest.split(':').next().unwrap_or(banner).trim().to_string())
}

/// Things wrong with a Valhalla config that the build will only mention much later, once per
/// tile, in a log nobody is reading.
///
/// `valhalla.json` names the admin and time zone databases; without them the graph is built
/// anyway, minus the country and time zone data that country-specific routing rules and opening
/// hours depend on - and the build says "Admin db not found" for every tile of a continent.
pub fn config_warnings(config: &Path, working_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config) else { return Vec::new() };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, what, tool) in [
        ("admin", "admin boundary", "valhalla_build_admins"),
        ("timezone", "time zone", "valhalla_build_timezones"),
    ] {
        let Some(path) = json["mjolnir"][key].as_str() else { continue };
        let resolved = working_dir.join(path);
        if !resolved.exists() {
            out.push(format!(
                "warning: {} names a {what} database that does not exist ({}). The graph will be \
                 built without that data, and the build prints a warning for every tile. \
                 `{tool}` makes it.",
                config.display(),
                resolved.display()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_banners_become_phases() {
        assert_eq!(
            phase_of(
                StepId::ValhallaTiles,
                "2026/08/20 10:00:00.000000 [INFO] Parsing files: data/alps.osm.pbf"
            ),
            Some("Parsing files".to_string())
        );
        assert_eq!(
            phase_of(StepId::ValhallaTiles, "[INFO] Building tiles from level 0"),
            Some("Building tiles from level 0".to_string())
        );
    }

    /// Ordinary log lines are not phases. Treating every INFO line as one would reset the phase
    /// display several times a second.
    #[test]
    fn chatter_is_not_a_phase() {
        assert_eq!(phase_of(StepId::ValhallaTiles, "[INFO] 1000 nodes processed"), None);
        assert_eq!(phase_of(StepId::ValhallaTiles, "[WARN] Building something"), None);
        assert_eq!(phase_of(StepId::ValhallaTiles, "no marker at all"), None);
    }

    /// The tools colour the level tag, which puts an escape between `[INFO]` and the space the
    /// banner search looked for - so with colour on, no stage was ever recognised.
    #[test]
    fn coloured_output_is_still_recognised() {
        let raw = "2026-10-13 16:26:11.745131 \x1b[32;1m[INFO]\x1b[0m Parsing files for ways: europe.osm.pbf";
        match valhalla_line(StepId::ValhallaTiles, raw) {
            StepEvent::Phase { name, .. } => assert_eq!(name, "Parsing ways"),
            other => panic!("expected a phase, got {other:?}"),
        }
        match valhalla_line(StepId::ValhallaTiles, "x \x1b[33;1m[WARN]\x1b[0m Admin db not found.")
        {
            StepEvent::Log { line, .. } => assert!(!line.contains('\x1b'), "{line:?}"),
            other => panic!("expected a log line, got {other:?}"),
        }
    }

    #[test]
    fn the_stages_only_move_forward() {
        for table in [&STAGES[..], &ADMIN_STAGES[..]] {
            let percents: Vec<u8> = table.iter().map(|(.., p)| *p).collect();
            assert!(percents.windows(2).all(|w| w[0] < w[1]), "{percents:?}");
        }
        let tiles = StepId::ValhallaTiles;
        assert_eq!(stage_percent(tiles, "t [INFO] Building 2 tiles with 18 threads..."), Some(50));
        assert_eq!(stage_percent(tiles, "t [INFO] Finished with 6473 graph edges"), None);
        assert_eq!(stage_percent(tiles, "t [WARN] Building tiles with problems"), None);
    }

    /// `Parsing ways...` is a banner of both tools, and means a different place in each.
    #[test]
    fn the_admin_tool_has_its_own_stages() {
        let line = "t [INFO] Parsing ways...";
        assert_eq!(stage_percent(StepId::ValhallaAdmins, line), Some(25));
        assert_eq!(stage_percent(StepId::ValhallaTiles, line), None);
        assert_eq!(
            stage_percent(StepId::ValhallaAdmins, "t [INFO] Writing database to disk."),
            Some(97)
        );
    }

    #[test]
    fn a_missing_admin_database_is_said_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("valhalla.json");
        std::fs::write(
            &config,
            r#"{"mjolnir": {"admin": "tiles/admins.sqlite", "timezone": "tiles/tz.sqlite"}}"#,
        )
        .unwrap();
        assert_eq!(config_warnings(&config, dir.path()).len(), 2);
        std::fs::create_dir_all(dir.path().join("tiles")).unwrap();
        std::fs::write(dir.path().join("tiles/admins.sqlite"), b"").unwrap();
        let left = config_warnings(&config, dir.path());
        assert_eq!(left.len(), 1);
        assert!(left[0].contains("valhalla_build_timezones"), "{left:?}");
        assert!(config_warnings(&dir.path().join("nope.json"), dir.path()).is_empty());
    }
}
