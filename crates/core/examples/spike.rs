//! End-to-end check for milestone 1.1: resolve a JRE, run a real planetiler build, render the
//! event stream. Small area (monaco) so it finishes in seconds.
//!
//!   cargo run -p cairn-core --example spike -- <jar> <workdir>

use cairn_core::steps::planetiler::{run, PlanetilerJob, Schema};
use cairn_core::steps::{StepEvent, StepId};
use cairn_core::toolchain;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let jar = PathBuf::from(args.next().expect("usage: spike <jar> <workdir>"));
    let workdir = PathBuf::from(args.next().expect("usage: spike <jar> <workdir>"));
    let managed = workdir.join("jre");

    let java = match toolchain::find(None, &managed).await {
        Some(j) => {
            println!("java {} from {:?} at {}", j.version, j.source, j.path.display());
            j
        }
        None => {
            println!("no usable java, downloading from {}", toolchain::adoptium_url(21)?);
            let mut last = 0;
            let j = toolchain::download(&managed, |done, total| {
                let mb = done / 1_048_576;
                if mb != last {
                    last = mb;
                    match total {
                        Some(t) => println!("  {mb} MB / {} MB", t / 1_048_576),
                        None => println!("  {mb} MB"),
                    }
                }
            })
            .await?;
            println!("downloaded java {}", j.version);
            j
        }
    };

    let job = PlanetilerJob {
        step: StepId::Basemap,
        area: "monaco".into(),
        java: java.path,
        jar,
        schema: Schema::OpenMapTiles,
        heap_mb: 4096,
        output: workdir.join("monaco.mbtiles"),
        tmp_dir: workdir.join("tmp/spike"),
        extra_args: vec![
            "--download".into(),
            "--area=monaco".into(),
            "--languages=".into(),
            "--force".into(),
        ],
        working_dir: workdir.clone(),
        log_interval: "1s".into(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let handle = tokio::spawn(run(job, tx));

    let (mut phases, mut progress_events) = (0, 0);
    while let Some(event) = rx.recv().await {
        match event {
            StepEvent::Started { area, .. } => println!("START {area}"),
            StepEvent::Phase { name, .. } => {
                phases += 1;
                println!("PHASE {name}");
            }
            StepEvent::Progress { label, percent, .. } => {
                progress_events += 1;
                println!("  {label:>12} {percent:>3}%");
            }
            StepEvent::Finished { ok, elapsed, outputs, .. } => {
                println!("FINISHED ok={ok} elapsed={elapsed:?} outputs={outputs:?}");
            }
            StepEvent::Skipped { reason, .. } => println!("SKIPPED {reason}"),
            StepEvent::Command { argv, .. } => println!("$ {}", argv.join(" ")),
            StepEvent::Log { .. } | StepEvent::Cli { .. } => {}
        }
    }
    let ok = handle.await??;
    println!("\nphases={phases} progress_events={progress_events} exit_ok={ok}");
    Ok(())
}
