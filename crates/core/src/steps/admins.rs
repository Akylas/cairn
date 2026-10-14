//! Valhalla's admin boundary database.
//!
//! `valhalla_build_tiles` looks every node up in a database of country and state boundaries, to
//! know which side of the road traffic drives on, which access defaults a country applies to ways
//! nobody tagged, and what to call things in an instruction. Without one it builds anyway and
//! treats everywhere alike - and says so once per tile.
//!
//! The database is not about an area. It describes boundaries, it is built from an extract that
//! holds them, and one serves every graph built afterwards - so it lives beside the graph rather
//! than in an area's directory, at the path `valhalla.json` names, which is where the graph
//! builder will look for it.
//!
//! What it needs is whole countries. A boundary is a relation made of ways, and an extract of a
//! region does not hold the ways of the country around it: `valhalla_build_admins` run over a
//! regional extract finds the relation, cannot close it, and writes a database with no boundaries
//! in it. That is checked here and reported, instead of leaving an empty file to make the step
//! look done.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;

use super::external::{self, ToolJob};
use super::StepId;
use crate::settings::Settings;

/// Where `valhalla.json` says the admin database is, relative paths resolved the way the graph
/// builder will resolve them - against the directory it is run from, the repository root.
pub fn output_path(settings: &Settings) -> PathBuf {
    let named = std::fs::read_to_string(settings.valhalla_config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json["mjolnir"]["admin"].as_str().map(PathBuf::from));
    match named {
        Some(path) if path.is_absolute() => path,
        Some(path) => settings.repo_root.join(path),
        None => settings.repo_root.join("valhalla_admin/admins.sqlite"),
    }
}

/// A run set up and not yet started.
pub struct Prepared {
    pub job: ToolJob,
    /// Where the finished database goes.
    pub output: PathBuf,
    /// Where it is written meanwhile, so that a graph build that starts in the middle never opens
    /// half of one.
    part: PathBuf,
}

/// What a finished database holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub areas: u64,
    pub countries: u64,
    pub bytes: u64,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} boundaries, {} of them countries, {:.0} MB",
            self.areas,
            self.countries,
            self.bytes as f64 / 1_048_576.0
        )
    }
}

fn with_part(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    name.into()
}

/// Set up the tool to run over `area`'s extract, writing where `valhalla.json` says.
///
/// The tool takes its output path from the config, so it is given a copy of the config that names
/// the scratch file instead; the real config is not touched.
pub fn prepare(
    settings: &Settings,
    area: &str,
    program: PathBuf,
    config: Option<&Path>,
    extract: Option<&Path>,
) -> Result<Prepared> {
    let config = config.map(Path::to_path_buf).unwrap_or_else(|| settings.valhalla_config_path());
    let text = std::fs::read_to_string(&config).with_context(|| {
        format!("no Valhalla config at {} - set one in Settings", config.display())
    })?;
    let mut json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;

    let pbf = extract
        .map(Path::to_path_buf)
        .unwrap_or_else(|| super::download::extract_path(&settings.data_dir, area));
    if !pbf.is_file() {
        return Err(anyhow!("{} is missing - run the OSM download step first", pbf.display()));
    }

    let output = output_path(settings);
    let part = with_part(&output);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // a scratch file from an interrupted run would be added to, not replaced
    let _ = std::fs::remove_file(&part);

    json["mjolnir"]["admin"] = serde_json::Value::String(part.display().to_string());
    let tmp = settings.run_tmp_dir(area);
    std::fs::create_dir_all(&tmp)?;
    let scratch_config = tmp.join("valhalla-admins.json");
    std::fs::write(&scratch_config, serde_json::to_vec_pretty(&json)?)?;

    Ok(Prepared {
        job: ToolJob {
            step: StepId::ValhallaAdmins,
            area: area.to_string(),
            program,
            args: vec![
                "-c".to_string(),
                scratch_config.display().to_string(),
                pbf.display().to_string(),
            ],
            working_dir: settings.repo_root.clone(),
            parse: external::valhalla_line,
        },
        output,
        part,
    })
}

/// Check what the tool wrote and put it in place.
///
/// A database with no boundaries in it is an error, and is removed: it is what a regional
/// extract produces, and it would otherwise sit there looking like the step had worked.
pub fn finish(prepared: &Prepared) -> Result<Report> {
    let report = inspect(&prepared.part)?;
    if report.areas == 0 {
        let _ = std::fs::remove_file(&prepared.part);
        return Err(anyhow!(
            "that extract holds no complete country boundaries, so the database came out empty. \
             Boundaries need the whole of a country: use the extract of a continent or the planet \
             (the same one the graph is built from, usually)."
        ));
    }
    std::fs::rename(&prepared.part, &prepared.output).with_context(|| {
        format!("moving the database into place at {}", prepared.output.display())
    })?;
    Ok(report)
}

/// Throw away what an unfinished run left.
pub fn discard(prepared: &Prepared) {
    let _ = std::fs::remove_file(&prepared.part);
}

/// Count what is in a database.
pub fn inspect(path: &Path) -> Result<Report> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("the tool wrote no database at {}", path.display()))?;
    let count = |sql: &str| -> Result<u64> {
        Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0)).context("reading the database")? as u64)
    };
    Ok(Report {
        areas: count("SELECT COUNT(*) FROM admins")?,
        // admin_level 2 is a country in OSM
        countries: count("SELECT COUNT(*) FROM admins WHERE admin_level = 2")?,
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(dir: &Path) -> Settings {
        Settings::for_repo(dir.to_path_buf())
    }

    fn database(path: &Path, rows: &[(i64, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE admins (admin_level INTEGER, name TEXT);").unwrap();
        for (level, name) in rows {
            conn.execute("INSERT INTO admins VALUES (?, ?)", rusqlite::params![level, name])
                .unwrap();
        }
    }

    #[test]
    fn the_output_is_where_the_config_says() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = settings(dir.path());
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/valhalla.json"),
            r#"{"mjolnir": {"admin": "somewhere/admins.sqlite"}}"#,
        )
        .unwrap();
        assert_eq!(output_path(&s), dir.path().join("somewhere/admins.sqlite"));

        std::fs::write(
            dir.path().join("config/valhalla.json"),
            r#"{"mjolnir": {"admin": "/abs/admins.sqlite"}}"#,
        )
        .unwrap();
        assert_eq!(output_path(&s), PathBuf::from("/abs/admins.sqlite"));

        // no config at all: somewhere that is not inside the graph's own directory
        s.valhalla_config = Some(dir.path().join("none.json"));
        assert!(!output_path(&s).starts_with(dir.path().join("valhalla_tiles")));
    }

    /// A regional extract produces a database with no boundaries, which must not look like success.
    #[test]
    fn an_empty_database_is_an_error_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("admins.sqlite.part");
        database(&part, &[]);
        let prepared = Prepared {
            job: ToolJob {
                step: StepId::ValhallaAdmins,
                area: "x".into(),
                program: PathBuf::new(),
                args: vec![],
                working_dir: dir.path().to_path_buf(),
                parse: external::valhalla_line,
            },
            output: dir.path().join("admins.sqlite"),
            part: part.clone(),
        };
        let err = finish(&prepared).unwrap_err().to_string();
        assert!(err.contains("continent"), "{err}");
        assert!(!part.exists());
        assert!(!prepared.output.exists());
    }

    #[test]
    fn a_database_with_boundaries_is_moved_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("admins.sqlite.part");
        database(&part, &[(2, "France"), (2, "Italia"), (4, "Savoie")]);
        let prepared = Prepared {
            job: ToolJob {
                step: StepId::ValhallaAdmins,
                area: "x".into(),
                program: PathBuf::new(),
                args: vec![],
                working_dir: dir.path().to_path_buf(),
                parse: external::valhalla_line,
            },
            output: dir.path().join("admins.sqlite"),
            part: part.clone(),
        };
        let report = finish(&prepared).unwrap();
        assert_eq!((report.areas, report.countries), (3, 2));
        assert!(prepared.output.is_file());
        assert!(!part.exists());
    }

    #[test]
    fn preparing_gives_the_tool_a_config_that_names_the_scratch_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/valhalla.json"),
            r#"{"mjolnir": {"admin": "valhalla_admin/admins.sqlite", "tile_dir": "t"}}"#,
        )
        .unwrap();
        let pbf = dir.path().join("europe.osm.pbf");
        std::fs::write(&pbf, b"x").unwrap();

        let prepared = prepare(&s, "europe", PathBuf::from("/bin/true"), None, Some(&pbf)).unwrap();
        assert_eq!(prepared.output, dir.path().join("valhalla_admin/admins.sqlite"));
        let used = std::fs::read_to_string(&prepared.job.args[1]).unwrap();
        let json: serde_json::Value = serde_json::from_str(&used).unwrap();
        assert!(json["mjolnir"]["admin"].as_str().unwrap().ends_with("admins.sqlite.part"));
        assert_eq!(json["mjolnir"]["tile_dir"], "t", "everything else is left as it was");
        // and the real config is untouched
        let real = std::fs::read_to_string(dir.path().join("config/valhalla.json")).unwrap();
        assert!(real.contains("valhalla_admin/admins.sqlite\""));
    }

    #[test]
    fn a_missing_extract_is_said_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/valhalla.json"), "{}").unwrap();
        let err = prepare(&s, "nowhere", PathBuf::new(), None, None).err().unwrap().to_string();
        assert!(err.contains("download"), "{err}");
    }
}
