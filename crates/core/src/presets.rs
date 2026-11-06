//! Named option sets, per step.
//!
//! This is the `bench/` workflow made first-class. Comparing builds meant hand-editing a shell
//! script per variant and remembering which flags produced which file; a preset is that variant,
//! named, saved, and re-runnable.

use crate::steps::StepId;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub step: StepId,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PresetStore {
    pub presets: Vec<Preset>,
}

impl PresetStore {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing presets at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn for_step(&self, step: StepId) -> Vec<&Preset> {
        self.presets.iter().filter(|p| p.step == step).collect()
    }

    pub fn get(&self, step: StepId, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.step == step && p.name == name)
    }

    /// Insert or replace. Names are unique per step, not globally - "final" can mean one thing
    /// for the basemap and another for routes.
    pub fn upsert(&mut self, preset: Preset) {
        match self.presets.iter_mut().find(|p| p.step == preset.step && p.name == preset.name) {
            Some(existing) => *existing = preset,
            None => self.presets.push(preset),
        }
    }

    pub fn remove(&mut self, step: StepId, name: &str) -> bool {
        let before = self.presets.len();
        self.presets.retain(|p| !(p.step == step && p.name == name));
        self.presets.len() != before
    }
}
/// The preset applied when a run does not name one.
///
/// cairn exists to reproduce this repository's tiles, so an untouched run should produce them.
/// `stock` is the opt-out that gives planetiler's own defaults.
pub const DEFAULT_PRESET: &str = "measured";

/// The flag sets this repository actually measured, shipped so a fresh install starts from the
/// tuned configuration rather than from nothing.
pub fn builtin() -> Vec<Preset> {
    let v = |pairs: &[(&str, Value)]| -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, val)| (k.to_string(), val.clone())).collect()
    };
    vec![
        Preset {
            name: "measured".into(),
            step: StepId::Basemap,
            description:
                "The basemap command from the repo README - the flag set that produces the \
                 shipped tiles. -8.8% of tile bytes against a build with none of it, with vertex \
                 removal doing the work rather than feature deletion, plus road surface detail."
                    .into(),
            values: v(&[
                // mirrors the basemap command in the repo README - the set that produced the
                // shipped tiles. Operational flags (area, polygon, mbtiles, force, download) are
                // cairn's to supply.
                ("languages", serde_json::json!("")),
                ("compact_db", serde_json::json!(true)),
                ("transportation_name_limit_merge", serde_json::json!(true)),
                ("area_poly", serde_json::json!(true)),
                ("exclude_layers", serde_json::json!("route")),
                ("nodemap_type", serde_json::json!("sparsearray")),
                ("max_point_buffer", serde_json::json!(4)),
                ("transportation_z13_paths", serde_json::json!(true)),
                ("mlt_shared_dict", serde_json::json!(true)),
                ("parallel_tmp_io", serde_json::json!(true)),
                ("simplify_tolerance", serde_json::json!(0.70)),
                ("simplify_tolerance_at_max_zoom", serde_json::json!(0.25)),
                ("min_feature_size_at_max_zoom", serde_json::json!(0.25)),
                ("landcover_tolerance_z11_13", serde_json::json!(1.05)),
                ("landcover_drop_redundant_subclass", serde_json::json!(true)),
                ("landcover_merge_maxzoom", serde_json::json!(true)),
                ("water_pool_tolerance", serde_json::json!(1)),
                ("drop_redundant_name_int", serde_json::json!(true)),
                ("transportation_surface_detail", serde_json::json!(true)),
                ("transportation_cycleway", serde_json::json!(true)),
                ("poi_custom_ranks", serde_json::json!(true)),
            ]),
        },
        Preset {
            name: "measured".into(),
            step: StepId::Routes,
            description:
                "The routes command from the repo README. Keeps name, extent and symbol - \
                 nothing is dropped - with simplification matched to the transportation layer so \
                 routes stay aligned with the tracks they follow."
                    .into(),
            values: v(&[
                // mirrors the routes command in the repo README.
                ("languages", serde_json::json!("")),
                ("compact_db", serde_json::json!(true)),
                ("transportation_name_limit_merge", serde_json::json!(true)),
                ("area_poly", serde_json::json!(true)),
                ("only_layers", serde_json::json!("route")),
                ("nodemap_type", serde_json::json!("sparsearray")),
                ("max_point_buffer", serde_json::json!(4)),
                ("mlt_shared_dict", serde_json::json!(true)),
                ("parallel_tmp_io", serde_json::json!(true)),
                ("simplify_tolerance_at_max_zoom", serde_json::json!(0.25)),
                ("min_feature_size_at_max_zoom", serde_json::json!(0.25)),
                ("route_road_tolerance", serde_json::json!(true)),
                ("route_extent_digits", serde_json::json!(2)),
                ("route_symbol_id", serde_json::json!(true)),
            ]),
        },
        Preset {
            name: "stock".into(),
            step: StepId::Basemap,
            description: "Planetiler defaults, as a comparison baseline.".into(),
            values: v(&[("exclude_layers", serde_json::json!("route"))]),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steps::options;

    #[test]
    fn upsert_replaces_within_a_step() {
        let mut store = PresetStore::default();
        store.upsert(Preset {
            name: "a".into(),
            step: StepId::Basemap,
            description: String::new(),
            values: BTreeMap::new(),
        });
        store.upsert(Preset {
            name: "a".into(),
            step: StepId::Basemap,
            description: "second".into(),
            values: BTreeMap::new(),
        });
        assert_eq!(store.presets.len(), 1);
        assert_eq!(store.presets[0].description, "second");
    }

    /// Names are scoped per step, so "measured" can mean different things for basemap and routes.
    #[test]
    fn same_name_coexists_across_steps() {
        let mut store = PresetStore::default();
        for step in [StepId::Basemap, StepId::Routes] {
            store.upsert(Preset {
                name: "measured".into(),
                step,
                description: String::new(),
                values: BTreeMap::new(),
            });
        }
        assert_eq!(store.presets.len(), 2);
        assert!(store.get(StepId::Routes, "measured").is_some());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        let mut store = PresetStore::default();
        for p in builtin() {
            store.upsert(p);
        }
        store.save(&path).unwrap();
        assert_eq!(PresetStore::load_or_default(&path).unwrap(), store);
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            PresetStore::load_or_default(&dir.path().join("none.json")).unwrap(),
            PresetStore::default()
        );
    }

    #[test]
    fn remove_reports_whether_it_matched() {
        let mut store = PresetStore::default();
        store.upsert(Preset {
            name: "a".into(),
            step: StepId::Basemap,
            description: String::new(),
            values: BTreeMap::new(),
        });
        assert!(store.remove(StepId::Basemap, "a"));
        assert!(!store.remove(StepId::Basemap, "a"));
    }

    /// A built-in preset that names a flag the schema does not define would silently drop that
    /// flag from the command line, which is exactly how a "tuned" build quietly becomes a stock
    /// one.
    #[test]
    fn builtin_presets_only_use_known_options() {
        for preset in builtin() {
            let defs = match preset.step {
                StepId::Basemap => options::basemap_options(),
                StepId::Routes => options::routes_options(),
                other => panic!("no schema wired for {other:?}"),
            };
            for key in preset.values.keys() {
                assert!(
                    options::find(&defs, key).is_some(),
                    "{:?}/{} references unknown option {key}",
                    preset.step,
                    preset.name
                );
            }
        }
    }

    /// The measured basemap set must render the flags it was measured with.
    ///
    /// max-point-buffer is the one to watch: the place layer declares a 256px buffer, nine times a
    /// tile's own area, so leaving it uncapped costs tens of MB.
    #[test]
    fn measured_basemap_renders_its_flags() {
        let preset = builtin()
            .into_iter()
            .find(|p| p.step == StepId::Basemap && p.name == "measured")
            .unwrap();
        let args = options::to_args(&options::basemap_options(), &preset.values);
        for expected in [
            "--max-point-buffer=4",
            "--landcover_tolerance_z11_13=1.05",
            "--landcover_merge_maxzoom=true",
            "--water_pool_tolerance=1",
            "--drop_redundant_name_int=true",
            "--transportation_surface_detail=true",
            "--transportation_cycleway=true",
            "--transportation-name-limit-merge=true",
            "--simplify-tolerance=0.7",
            "--languages=",
        ] {
            assert!(args.contains(&expected.to_string()), "missing {expected} from {args:?}");
        }
    }
}
