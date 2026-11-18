//! Making the local copies of remote elevation sources cover the area about to be built.
//!
//! A source in `sources.json` with an `extract_from` is a file that is grown on demand, see
//! [`crate::terrain::extract`]. This is the step-level half: which sources, over what box, one at
//! a time, and where the box comes from when the area has no basemap yet.
//!
//! Every step that reads elevation calls [`ensure`] before it opens its sources, and it costs
//! nothing when the file already covers the area. That is what makes "just build it" work from a
//! clean checkout, in the app and from the command line alike.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use crate::cancel::Cancel;
use crate::poly::Polygon;
use crate::settings::Settings;
use crate::terrain::build::Event;
use crate::terrain::extract::{self, Report, Request};
use crate::terrain::source::{self, SourceSpec};

/// The finest zoom the graph build and the elevation step ask for. The graph samples a 30 m grid
/// and reads z11 or z12 for it; the zooms above are terrain's, and each terrain build fetches the
/// ones it renders.
pub const ROUTING_MAXZOOM: u8 = 12;

/// Two extracts growing the same file would each rewrite it from the other's old copy. A machine
/// has one of these; two processes on one file is the user's to avoid.
static GROWING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What [`ensure`] did, source by source.
#[derive(Debug, Default)]
pub struct Outcome {
    pub reports: Vec<(String, Report)>,
    /// Stopped before the end. What was done is kept.
    pub stopped: bool,
}

impl Outcome {
    /// One line per source, for the log.
    pub fn lines(&self) -> Vec<String> {
        self.reports
            .iter()
            .map(|(name, r)| {
                if r.covered {
                    format!("{name}: already covers the area")
                } else if r.finished {
                    format!(
                        "{name}: kept {} tiles, fetched {} ({:.0} MB)",
                        r.kept,
                        r.fetched,
                        r.fetched_bytes as f64 / 1_048_576.0
                    )
                } else {
                    format!("{name}: stopped")
                }
            })
            .collect()
    }
}

fn growable(settings: &Settings) -> Vec<SourceSpec> {
    source::read_specs(&settings.sources_json)
        .map(|specs| {
            specs
                .into_iter()
                .filter(|s| s.extract_from.is_some() && source::is_tiled(&s.kind))
                .collect()
        })
        .unwrap_or_default()
}

/// The files that [`ensure`] keeps, for a step's idea of what it has produced.
pub fn targets(settings: &Settings) -> Vec<PathBuf> {
    growable(settings).into_iter().map(|s| s.path).collect()
}

/// The box an area covers: its basemap's, else its configured polygon's, else its boundary from
/// Geofabrik, which is the same shape the terrain step clips to.
pub async fn area_bounds(settings: &Settings, area: &str) -> Result<(f64, f64, f64, f64)> {
    let from_basemap = crate::catalog::discover(&settings.output_root).ok().and_then(|areas| {
        areas
            .into_iter()
            .find(|a| a.name == area)?
            .artifacts
            .iter()
            .find(|a| a.kind == crate::catalog::ArtifactKind::Basemap)?
            .bounds
            .clone()
    });
    if let Some(raw) = from_basemap {
        let p: Vec<f64> = raw.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if let [w, s, e, n] = p[..] {
            return Ok((w, s, e, n));
        }
    }
    let poly = match settings.area(area).and_then(|a| a.poly.clone()) {
        Some(poly) if poly.is_file() => poly,
        _ => crate::steps::download::ensure_poly(&settings.data_dir, area, |_, _| {})
            .await
            .with_context(|| {
                format!("no basemap for `{area}` yet, and no boundary to take its extent from")
            })?,
    };
    Ok(crate::poly::Polygon::parse(&poly)?.bounds())
}

/// The area's outline, when there is one on disk: a configured polygon, else the boundary that
/// the terrain step clips to. Never downloads one. With it an extract skips the corners of the
/// bounding box, which at fine zooms is most of the tiles of an irregular region.
pub fn area_shape(settings: &Settings, area: &str) -> Option<Polygon> {
    let path =
        settings.area(area).and_then(|a| a.poly.clone()).filter(|p| p.is_file()).or_else(|| {
            Some(crate::steps::download::poly_path(&settings.data_dir, area))
                .filter(|p| p.is_file())
        })?;
    Polygon::parse(&path).ok()
}

/// Grow every source that is built from remote archives so that it covers `bounds`, inside `shape`
/// when there is one, and down to `zoom_cap` at most.
///
/// `emit` is called from a worker thread. Returns when every source is done, or as soon as
/// `cancel` is: the one being written keeps its progress and the next run continues it.
pub async fn ensure<F>(
    settings: &Settings,
    bounds: (f64, f64, f64, f64),
    shape: Option<Polygon>,
    zoom_cap: Option<u8>,
    cancel: &Cancel,
    emit: F,
) -> Result<Outcome>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let specs = growable(settings);
    if specs.is_empty() {
        return Ok(Outcome::default());
    }
    let _guard = tokio::select! {
        guard = GROWING.lock() => guard,
        _ = cancel.cancelled() => return Ok(Outcome { reports: Vec::new(), stopped: true }),
    };
    let stop = cancel.clone();
    tokio::task::spawn_blocking(move || -> Result<Outcome> {
        let mut outcome = Outcome::default();
        for spec in specs {
            let mut sink = |event: Event| emit(event);
            let request = Request { bounds, shape: shape.as_ref(), maxzoom: zoom_cap };
            let report = extract::ensure(&spec, &request, &|| stop.is_cancelled(), &mut sink)
                .with_context(|| {
                    format!(
                        "building {} from {}",
                        spec.path.display(),
                        spec.extract_from.as_ref().map(|o| o.urls().join(", ")).unwrap_or_default()
                    )
                })?;
            let finished = report.finished;
            outcome.reports.push((spec.name.clone(), report));
            if !finished {
                outcome.stopped = true;
                break;
            }
        }
        Ok(outcome)
    })
    .await
    .map_err(|e| anyhow!("the extract task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sources_with_somewhere_to_grow_from_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::for_repo(dir.path().to_path_buf());
        settings.sources_json = dir.path().join("sources.json");
        std::fs::write(
            &settings.sources_json,
            r#"[
              {"name":"a","type":"pmtiles","path":"a.pmtiles","extract_from":"https://h/p.pmtiles"},
              {"name":"b","type":"pmtiles","path":"b.pmtiles"},
              {"name":"c","type":"raster","path":"c.tif"}
            ]"#,
        )
        .unwrap();
        assert_eq!(targets(&settings), vec![dir.path().join("a.pmtiles")]);
    }

    /// Nothing to grow is not an error and costs nothing, including with no sources.json at all.
    #[tokio::test]
    async fn nothing_to_grow_is_an_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::for_repo(dir.path().to_path_buf());
        settings.sources_json = dir.path().join("none.json");
        let out = ensure(&settings, (0.0, 0.0, 1.0, 1.0), None, None, &Cancel::never(), |_| {})
            .await
            .unwrap();
        assert!(out.reports.is_empty() && !out.stopped);
    }
}
