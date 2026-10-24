//! The on-disk library of reusable node content, one directory per node type next to
//! `graphs/` and `nodes/`:
//!
//! - `agents/<name>/agent.json` is a saved agent; its runs land in
//!   `agents/<name>/outputs/<timestamp>.<ext>`.
//! - `prompts/<name>.md` is a reusable system prompt.
//! - `databases/<name>.json` is a saved database config.
//! - `scripts/` and `data/` hold the files script and data nodes point at; the app lists
//!   them but never writes there.
//!
//! Every name passes through [`validate::file_stem`] again here, so nothing a caller does
//! can produce a path outside these directories.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::node_data::{AgentConfig, DatabaseConfig, OutputType, Runtime};
use crate::validate;

pub const AGENTS_DIR: &str = "agents";
pub const PROMPTS_DIR: &str = "prompts";
pub const DATABASES_DIR: &str = "databases";
pub const SCRIPTS_DIR: &str = "scripts";
pub const DATA_DIR: &str = "data";

const AGENT_FILE: &str = "agent.json";
const OUTPUTS_DIR: &str = "outputs";
/// Listing files stops after this many entries so a huge `data/` can't stall the UI.
const MAX_LISTED: usize = 2000;
const MAX_DEPTH: usize = 4;

/// A saved, named configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset<T> {
    pub name: String,
    pub config: T,
}

// ----- agents -----

pub fn agent_dir(name: &str) -> PathBuf {
    Path::new(AGENTS_DIR).join(validate::file_stem(name))
}

pub fn agent_path(name: &str) -> PathBuf {
    agent_dir(name).join(AGENT_FILE)
}

pub fn agent_outputs_dir(name: &str) -> PathBuf {
    agent_dir(name).join(OUTPUTS_DIR)
}

/// Write `agents/<name>/agent.json`, creating the directory. The config must validate.
pub fn save_agent(name: &str, config: &AgentConfig) -> Result<PathBuf, String> {
    reject_issues(config.validate())?;
    let path = agent_path(name);
    write_json(&path, config)?;
    Ok(path)
}

/// Every saved agent, sorted by name. Directories without a parsable config are skipped.
pub fn list_agents() -> Vec<Preset<AgentConfig>> {
    let Ok(entries) = fs::read_dir(AGENTS_DIR) else {
        return Vec::new();
    };
    let mut agents: Vec<Preset<AgentConfig>> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let config = read_json(&e.path().join(AGENT_FILE)).ok()?;
            Some(Preset { name, config })
        })
        .collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

/// Remove the agent's config. Saved outputs are kept; the directory goes only if empty.
pub fn delete_agent(name: &str) -> Result<(), String> {
    let path = agent_path(name);
    fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
    let _ = fs::remove_dir(agent_dir(name));
    Ok(())
}

/// Save one run's output as `<agents_dir>/<name>/outputs/<timestamp>.<ext>`. The executor
/// passes its agents directory (`agents/` under the working directory in the app).
pub fn write_agent_output(
    agents_dir: &Path,
    name: &str,
    output: OutputType,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let dir = agents_dir.join(validate::file_stem(name)).join(OUTPUTS_DIR);
    write_output_in(&dir, output, bytes, timestamp())
}

fn write_output_in(
    dir: &Path,
    output: OutputType,
    bytes: &[u8],
    stamp: String,
) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut path = dir.join(format!("{stamp}.{}", output.extension()));
    // Two outputs in the same millisecond get a suffix instead of clobbering each other.
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stamp}-{n}.{}", output.extension()));
        n += 1;
    }
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// How many outputs an agent has saved.
pub fn agent_output_count(name: &str) -> usize {
    fs::read_dir(agent_outputs_dir(name))
        .map(|entries| entries.flatten().filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}

// ----- prompts -----

pub fn prompt_path(name: &str) -> PathBuf {
    Path::new(PROMPTS_DIR).join(format!("{}.md", validate::file_stem(name)))
}

pub fn save_prompt(name: &str, text: &str) -> Result<PathBuf, String> {
    validate::text("prompt", text, validate::MAX_PROMPT_LEN)?;
    fs::create_dir_all(PROMPTS_DIR).map_err(|e| format!("create {PROMPTS_DIR}/: {e}"))?;
    let path = prompt_path(name);
    fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn load_prompt(name: &str) -> Result<String, String> {
    let path = prompt_path(name);
    fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

pub fn delete_prompt(name: &str) -> Result<(), String> {
    let path = prompt_path(name);
    fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))
}

/// Names of every saved prompt, sorted.
pub fn list_prompts() -> Vec<String> {
    list_stems(PROMPTS_DIR, "md")
}

// ----- databases -----

pub fn database_path(name: &str) -> PathBuf {
    Path::new(DATABASES_DIR).join(format!("{}.json", validate::file_stem(name)))
}

pub fn save_database(name: &str, config: &DatabaseConfig) -> Result<PathBuf, String> {
    reject_issues(config.validate())?;
    let path = database_path(name);
    write_json(&path, config)?;
    Ok(path)
}

pub fn list_databases() -> Vec<Preset<DatabaseConfig>> {
    let mut dbs: Vec<Preset<DatabaseConfig>> = list_stems(DATABASES_DIR, "json")
        .into_iter()
        .filter_map(|name| {
            let config = read_json(&database_path(&name)).ok()?;
            Some(Preset { name, config })
        })
        .collect();
    dbs.sort_by(|a, b| a.name.cmp(&b.name));
    dbs
}

pub fn delete_database(name: &str) -> Result<(), String> {
    let path = database_path(name);
    fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))
}

// ----- scripts and data: files the user brings -----

/// Script files under `scripts/` (any runtime's extension), as `/`-separated paths
/// relative to that directory, sorted.
pub fn list_scripts() -> Vec<String> {
    let allowed = Runtime::all_extensions();
    list_files(Path::new(SCRIPTS_DIR), Some(&allowed))
}

/// Every regular file under `data/`, as `/`-separated paths relative to it, sorted.
pub fn list_data() -> Vec<String> {
    list_files(Path::new(DATA_DIR), None)
}

/// Walk `root` a few levels deep, skipping hidden entries and symlinks, and return the
/// relative paths of regular files, optionally only those with an allowed extension.
fn list_files(root: &Path, allowed: Option<&[&str]>) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, 0, allowed, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, allowed: Option<&[&str]>, out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() >= MAX_LISTED {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_LISTED {
            return;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // symlink_metadata doesn't follow links, so a link anywhere is skipped outright.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, depth + 1, allowed, out);
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        let rel = rel.join("/");
        if validate::relative_path(&rel).is_err() {
            continue;
        }
        if let Some(allowed) = allowed
            && !validate::has_extension(&rel, allowed)
        {
            continue;
        }
        out.push(rel);
    }
}

// ----- shared helpers -----

fn reject_issues(issues: Vec<validate::Issue>) -> Result<(), String> {
    match issues.first() {
        None => Ok(()),
        Some(first) if issues.len() == 1 => Err(format!("{}: {}", first.field, first.message)),
        Some(first) => Err(format!(
            "{}: {} (and {} more)",
            first.field,
            first.message,
            issues.len() - 1
        )),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}/: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// File stems in `dir` with the given extension, sorted and deduplicated.
fn list_stems(dir: &str, ext: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some(ext) {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// ----- timestamps -----

/// The current UTC time as `YYYYMMDDTHHMMSSmmmZ`: sortable, filesystem-safe, unambiguous.
pub fn timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format_timestamp(millis)
}

fn format_timestamp(unix_millis: u128) -> String {
    let secs = (unix_millis / 1000) as i64;
    let millis = (unix_millis % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day). Howard Hinnant's
/// algorithm, valid for the whole range `i64` days can address.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("uapz-library-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn library_paths_are_sanitized() {
        assert_eq!(agent_path("../x"), PathBuf::from("agents/---x/agent.json"));
        assert_eq!(
            agent_outputs_dir("summarizer"),
            PathBuf::from("agents/summarizer/outputs")
        );
        assert_eq!(prompt_path("a/b"), PathBuf::from("prompts/a-b.md"));
        assert_eq!(
            database_path(".hidden"),
            PathBuf::from("databases/-hidden.json")
        );
    }

    #[test]
    fn timestamps_are_utc_and_sortable() {
        assert_eq!(format_timestamp(0), "19700101T000000000Z");
        // 2026-10-24 15:30:45.123 UTC (`date -u -r 1792855845`)
        assert_eq!(format_timestamp(1_792_855_845_123), "20261024T153045123Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert!(format_timestamp(1) < format_timestamp(2_000));
    }

    #[test]
    fn outputs_never_overwrite_each_other() {
        let dir = scratch("outputs");
        let a = write_output_in(&dir, OutputType::Text, b"one", "T".into()).unwrap();
        let b = write_output_in(&dir, OutputType::Text, b"two", "T".into()).unwrap();
        let c = write_output_in(&dir, OutputType::Json, b"{}", "T".into()).unwrap();
        assert_eq!(a.file_name().unwrap(), "T.txt");
        assert_eq!(b.file_name().unwrap(), "T-1.txt");
        assert_eq!(c.file_name().unwrap(), "T.json");
        assert_eq!(fs::read_to_string(&a).unwrap(), "one");
        assert_eq!(fs::read_to_string(&b).unwrap(), "two");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_skips_hidden_symlinked_and_disallowed_files() {
        let dir = scratch("list");
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::write(dir.join("a.py"), "").unwrap();
        fs::write(dir.join("b.sh"), "").unwrap();
        fs::write(dir.join("c.rb"), "").unwrap();
        fs::write(dir.join(".hidden.py"), "").unwrap();
        fs::write(dir.join("sub/deep/d.js"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("a.py"), dir.join("link.py")).unwrap();

        let scripts = list_files(&dir, Some(&Runtime::all_extensions()));
        assert_eq!(scripts, ["a.py", "b.sh", "sub/deep/d.js"]);
        let everything = list_files(&dir, None);
        assert_eq!(everything, ["a.py", "b.sh", "c.rb", "sub/deep/d.js"]);
        assert!(list_files(&dir.join("nope"), None).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_configs_are_refused_with_a_reason() {
        let bad = AgentConfig {
            model: String::new(),
            temperature: 9.0,
            ..AgentConfig::default()
        };
        let err = reject_issues(bad.validate()).unwrap_err();
        assert!(err.starts_with("model: "), "{err}");
        assert!(err.ends_with("(and 1 more)"), "{err}");
        assert!(reject_issues(Vec::new()).is_ok());
    }

    #[test]
    fn json_round_trips_through_a_file() {
        let dir = scratch("json");
        let path = dir.join("nested/db.json");
        let config = DatabaseConfig {
            path: "x.db".into(),
            query: "SELECT 1".into(),
            ..DatabaseConfig::default()
        };
        write_json(&path, &config).unwrap();
        let back: DatabaseConfig = read_json(&path).unwrap();
        assert_eq!(back, config);
        assert!(read_json::<DatabaseConfig>(&dir.join("missing.json")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
