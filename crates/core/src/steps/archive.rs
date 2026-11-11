//! Which container a step writes its tiles into.
//!
//! For the steps driven by an external tool this is only a filename: planetiler picks the
//! container from the extension through `TileArchiveConfig`, tippecanoe through
//! `pmtiles_has_suffix`. Naming the output is the whole of the work.
//!
//! Terrain and contours are the exceptions, because they write their own archive rather than
//! handing tiles to a tool - so they need a second writer, which is `terrain::archive`. It is in this list because
//! it has one, not because a name is enough.
//!
//! A step that has neither keeps its own extension whatever is asked. Producing an mbtiles
//! under a `.pmtiles` name would be worse than refusing, and is exactly the bug that shipped
//! once already when a temporary filename hid the extension from tippecanoe.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::StepId;

/// The shared option key. One name across every step that has the choice, so a preset that sets
/// it once means the same thing everywhere.
pub const PMTILES_KEY: &str = "pmtiles";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    MBTiles,
    PMTiles,
}

impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::MBTiles => "mbtiles",
            ArchiveFormat::PMTiles => "pmtiles",
        }
    }

    /// Read the choice out of a step's collected option values.
    ///
    /// Absent means mbtiles, because that is what every archive in this repository already is
    /// and an unset option must never change an existing build's output path.
    pub fn from_values(values: &BTreeMap<String, Value>) -> Self {
        match values.get(PMTILES_KEY) {
            Some(Value::Bool(true)) => ArchiveFormat::PMTiles,
            _ => ArchiveFormat::MBTiles,
        }
    }
}

/// Whether the step's underlying tool can write PMTiles.
pub fn supports_pmtiles(step: StepId) -> bool {
    matches!(
        step,
        StepId::Basemap | StepId::Routes | StepId::Bathymap | StepId::TerrainRgb | StepId::Contours
    )
}

/// The file name a tile step writes, for one area and one format.
pub fn archive_name(area: &str, step: StepId, format: ArchiveFormat) -> Option<String> {
    let suffix = match step {
        StepId::Basemap => "",
        StepId::Routes => "_routes",
        StepId::TerrainRgb => "_terrain",
        StepId::Contours => "_contours",
        StepId::Bathymap => "_bathymap",
        _ => return None,
    };
    // A step whose writer cannot produce PMTiles keeps its own extension whatever is asked,
    // rather than being named for a container it is not going to write.
    let format = if supports_pmtiles(step) { format } else { ArchiveFormat::MBTiles };
    Some(format!("{area}{suffix}.{}", format.extension()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn unset_means_mbtiles() {
        assert_eq!(ArchiveFormat::from_values(&values(&[])), ArchiveFormat::MBTiles);
        assert_eq!(
            ArchiveFormat::from_values(&values(&[(PMTILES_KEY, json!(false))])),
            ArchiveFormat::MBTiles
        );
    }

    #[test]
    fn set_means_pmtiles() {
        assert_eq!(
            ArchiveFormat::from_values(&values(&[(PMTILES_KEY, json!(true))])),
            ArchiveFormat::PMTiles
        );
    }

    #[test]
    fn names_follow_the_format() {
        assert_eq!(
            archive_name("rhone-alpes", StepId::Basemap, ArchiveFormat::PMTiles).unwrap(),
            "rhone-alpes.pmtiles"
        );
        assert_eq!(
            archive_name("rhone-alpes", StepId::Routes, ArchiveFormat::MBTiles).unwrap(),
            "rhone-alpes_routes.mbtiles"
        );
        assert_eq!(
            archive_name("world", StepId::Bathymap, ArchiveFormat::PMTiles).unwrap(),
            "world_bathymap.pmtiles"
        );
    }

    #[test]
    fn terrain_can_be_asked_for_pmtiles() {
        assert!(supports_pmtiles(StepId::TerrainRgb));
        assert_eq!(
            archive_name("rhone-alpes", StepId::TerrainRgb, ArchiveFormat::PMTiles).unwrap(),
            "rhone-alpes_terrain.pmtiles"
        );
    }

    /// A step with no PMTiles writer must keep its own extension even when the option is set,
    /// rather than being named for a container nothing is going to write.
    #[test]
    fn a_step_without_a_writer_keeps_its_extension() {
        assert!(!supports_pmtiles(StepId::ValhallaPackage));
        assert!(archive_name("a", StepId::ValhallaPackage, ArchiveFormat::PMTiles).is_none());
    }

    #[test]
    fn steps_without_an_archive_have_no_name() {
        assert!(archive_name("a", StepId::DownloadOsm, ArchiveFormat::MBTiles).is_none());
        assert!(archive_name("a", StepId::ValhallaPackage, ArchiveFormat::MBTiles).is_none());
    }
}
