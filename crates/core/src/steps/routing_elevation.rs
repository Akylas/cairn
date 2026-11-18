//! Elevation for the Valhalla graph build, taken from the terrain sources.
//!
//! `valhalla_build_tiles` reads `.hgt` files and nothing else. Instead of keeping a second copy of
//! the ground on disk for it, a build runs [`HgtServer`] for as long as it lasts and points a
//! copy of `valhalla.json` at it: Valhalla fetches each degree square it needs, from mapterhorn or
//! whatever else `sources.json` lists, and keeps it in memory.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::settings::{RoutingElevation, Settings};
use crate::terrain::hgt_server::{HgtOptions, HgtServer, HgtStats};
use crate::terrain::source;

/// Concurrent fetches Valhalla may have in flight. Its default is one, which would make every
/// build thread wait in line for the one that is rendering.
const FETCHERS: u64 = 16;

/// Whether anything will read `.hgt` files from `elevation_tiles/`.
///
/// Yes when the graph is set to, and also when a terrain source is a directory of them. An
/// unreadable `sources.json` counts as yes: the terrain step then falls back to that directory.
pub fn hgt_needed(settings: &Settings) -> bool {
    settings.routing_elevation == RoutingElevation::Hgt
        || source::read_specs(&settings.sources_json)
            .map(|specs| specs.iter().any(|spec| source::is_hgt(&spec.kind)))
            .unwrap_or(true)
}

/// A running elevation server and the Valhalla config that points at it.
pub struct Supply {
    /// The config to hand to `valhalla_build_tiles` in place of the template.
    pub config: PathBuf,
    server: HgtServer,
    scratch: PathBuf,
}

impl Supply {
    /// Stop serving and tidy up. Returns what was served, for the log.
    pub async fn finish(self) -> HgtStats {
        let stats = self.server.stop().await;
        // empty unless Valhalla is an older build that stores what it fetches
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = std::fs::remove_file(&self.config);
        stats
    }
}

/// One line saying what a [`Supply`] did.
pub fn describe(stats: HgtStats) -> String {
    format!(
        "elevation: {} degree squares rendered from the terrain sources, {} without data",
        stats.rendered, stats.empty
    )
}

/// Start serving, and write the config that uses it. `None` when the graph reads `.hgt` files.
pub async fn prepare(
    settings: &Settings,
    area: &str,
    template: &Path,
    mode: RoutingElevation,
) -> Result<Option<Supply>> {
    if mode == RoutingElevation::Hgt {
        return Ok(None);
    }
    let specs = source::read_specs(&settings.sources_json)
        .with_context(|| format!("reading {}", settings.sources_json.display()))?;
    let server = HgtServer::start(specs, HgtOptions::default()).await.map_err(|e| {
        anyhow!("{e:#}. Fix the source in Settings, or set the routing elevation to `hgt`.")
    })?;

    let work = settings.run_tmp_dir(area);
    let scratch = work.join("elevation");
    std::fs::create_dir_all(&scratch)?;
    let config = work.join("valhalla.terrain.json");
    let text = std::fs::read_to_string(template)
        .with_context(|| format!("reading {}", template.display()))?;
    let derived = with_server(&text, &server.url_template(), &scratch)?;
    std::fs::write(&config, derived)?;
    Ok(Some(Supply { config, server, scratch }))
}

/// `template` with its elevation pointed at a server.
///
/// `elevation` has to name a directory that exists, or Valhalla skips elevation altogether; it
/// stays empty when the Valhalla in use keeps what it fetches in memory.
fn with_server(template: &str, url: &str, scratch: &Path) -> Result<String> {
    let mut config: Value =
        serde_json::from_str(template).context("parsing the Valhalla config")?;
    let root =
        config.as_object_mut().ok_or_else(|| anyhow!("the Valhalla config is not an object"))?;
    let section = |root: &mut serde_json::Map<String, Value>, key: &str| -> Result<()> {
        let entry = root.entry(key).or_insert_with(|| json!({}));
        entry
            .as_object()
            .map(|_| ())
            .ok_or_else(|| anyhow!("`{key}` in the Valhalla config is not an object"))
    };
    section(root, "additional_data")?;
    section(root, "mjolnir")?;

    let data = root["additional_data"].as_object_mut().unwrap();
    data.insert("elevation".into(), json!(scratch.display().to_string()));
    data.insert("elevation_url".into(), json!(url));
    data.insert("elevation_url_store".into(), json!(false));

    let mjolnir = root["mjolnir"].as_object_mut().unwrap();
    let have = mjolnir.get("max_concurrent_reader_users").and_then(Value::as_u64).unwrap_or(1);
    mjolnir.insert("max_concurrent_reader_users".into(), json!(have.max(FETCHERS)));
    Ok(serde_json::to_string_pretty(&config)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_pointed_at_the_server_and_nothing_else_changes() {
        let template = r#"{"mjolnir": {"tile_dir": "valhalla_tiles", "max_concurrent_reader_users": 2},
                           "additional_data": {"elevation": "elevation_tiles"}, "loki": {"x": 1}}"#;
        let out =
            with_server(template, "http://127.0.0.1:9/{tilePath}", Path::new("/tmp/e")).unwrap();
        let json: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["additional_data"]["elevation"], "/tmp/e");
        assert_eq!(json["additional_data"]["elevation_url"], "http://127.0.0.1:9/{tilePath}");
        assert_eq!(json["additional_data"]["elevation_url_store"], false);
        assert_eq!(json["mjolnir"]["tile_dir"], "valhalla_tiles");
        assert_eq!(json["mjolnir"]["max_concurrent_reader_users"], 16);
        assert_eq!(json["loki"]["x"], 1);
    }

    #[test]
    fn a_config_with_neither_section_gets_both() {
        let out = with_server("{}", "http://h/{tilePath}", Path::new("/e")).unwrap();
        let json: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["additional_data"]["elevation_url_store"], false);
        assert_eq!(json["mjolnir"]["max_concurrent_reader_users"], 16);
        assert!(with_server("[]", "u", Path::new("/e")).is_err());
        assert!(with_server(r#"{"mjolnir": 3}"#, "u", Path::new("/e")).is_err());
    }

    /// A user who keeps `.hgt` tiles as a terrain source, or has no `sources.json` at all, still
    /// needs the directory; one with only mapterhorn does not.
    #[test]
    fn hgt_is_needed_only_when_something_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::for_repo(dir.path().to_path_buf());
        settings.sources_json = dir.path().join("sources.json");
        assert!(hgt_needed(&settings), "no sources.json: terrain falls back to the directory");

        std::fs::write(
            &settings.sources_json,
            r#"[{"name":"m","type":"pmtiles","path":"m.pmtiles"}]"#,
        )
        .unwrap();
        assert!(!hgt_needed(&settings));
        settings.routing_elevation = RoutingElevation::Hgt;
        assert!(hgt_needed(&settings));
        settings.routing_elevation = RoutingElevation::Terrain;

        std::fs::write(&settings.sources_json, r#"[{"name":"h","type":"valhalla","path":"e"}]"#)
            .unwrap();
        assert!(hgt_needed(&settings));
    }
}
