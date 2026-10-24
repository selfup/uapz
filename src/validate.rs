//! Sanitizing and validating everything a node can point at or hand to a backend: file
//! names, paths that must stay inside one of the app's directories, environment variable
//! names, SQL identifiers and queries, model ids, argument lists and numeric ranges.
//!
//! The checks are deliberately conservative: a construct the checker doesn't understand is
//! rejected rather than waved through, and a false positive only costs the user a clearer
//! name. Everything here is pure (no filesystem access) except [`resolve_inside`], which an
//! executor should call right before it opens a file.

use std::path::{Path, PathBuf};

/// One problem with a node's configuration, attributed to the field it concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub field: &'static str,
    pub message: String,
}

impl Issue {
    pub fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

pub const MAX_NAME_LEN: usize = 64;
pub const MAX_PATH_LEN: usize = 512;
pub const MAX_ENV_VAR_LEN: usize = 128;
pub const MAX_IDENTIFIER_LEN: usize = 63;
pub const MAX_MODEL_ID_LEN: usize = 128;
pub const MAX_QUERY_LEN: usize = 64 * 1024;
pub const MAX_PROMPT_LEN: usize = 256 * 1024;
pub const MAX_ARGS: usize = 64;
pub const MAX_ARG_LEN: usize = 4096;

// ----- names -----

/// Turn user input into a file stem that stays inside a single directory: letters, digits,
/// `-`, `_` and single spaces only, trimmed and capped at [`MAX_NAME_LEN`] characters.
/// Dots are replaced too, so a stem can never be `..`, a hidden file or carry an extension.
pub fn file_stem(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .take(MAX_NAME_LEN)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "untitled".to_string()
    } else {
        cleaned
    }
}

/// A POSIX environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn env_var(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("environment variable name is empty".into());
    }
    if name.len() > MAX_ENV_VAR_LEN {
        return Err(format!(
            "environment variable name is longer than {MAX_ENV_VAR_LEN} characters"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('0');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("environment variable name must start with a letter or `_`".into());
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
        return Err(format!(
            "environment variable name contains `{bad}`; only letters, digits and `_` are allowed"
        ));
    }
    Ok(())
}

/// An unquoted SQL identifier such as a table name: `[A-Za-z_][A-Za-z0-9_]*`, at most
/// [`MAX_IDENTIFIER_LEN`] bytes (PostgreSQL's limit, the strictest of the supported engines).
pub fn identifier(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("identifier is empty".into());
    }
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(format!(
            "identifier is longer than {MAX_IDENTIFIER_LEN} characters"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('0');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("identifier must start with a letter or `_`".into());
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
        return Err(format!(
            "identifier contains `{bad}`; only letters, digits and `_` are allowed"
        ));
    }
    Ok(())
}

/// A model id as providers write them: `llama3.1`, `claude-opus-5-5`, `org/model:tag`.
pub fn model_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("model is empty".into());
    }
    if id.len() > MAX_MODEL_ID_LEN {
        return Err(format!(
            "model id is longer than {MAX_MODEL_ID_LEN} characters"
        ));
    }
    if let Some(bad) = id
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@')))
    {
        return Err(format!(
            "model id contains `{bad}`; only letters, digits and `. _ - : / @` are allowed"
        ));
    }
    if id.starts_with(['/', '.', '-']) {
        return Err("model id must start with a letter or digit".into());
    }
    // Ids end up in URL paths, so no segment tricks.
    if id.contains("..") || id.contains("//") || id.ends_with('/') {
        return Err("model id must not contain `..`, `//` or end in `/`".into());
    }
    Ok(())
}

// ----- paths -----

/// Check that `raw` is a plain relative path with no way out of the directory it will be
/// joined to: no absolute paths, no `.` or `..` components, no hidden components, no
/// backslashes, no control characters. Returns the normalized relative path.
pub fn relative_path(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("path is empty".into());
    }
    if raw.len() > MAX_PATH_LEN {
        return Err(format!("path is longer than {MAX_PATH_LEN} characters"));
    }
    if raw.chars().any(char::is_control) {
        return Err("path contains a control character".into());
    }
    if raw.contains('\\') {
        return Err("path contains `\\`; use `/` to separate directories".into());
    }
    if raw.starts_with(['/', '~']) || Path::new(raw).is_absolute() || has_drive_prefix(raw) {
        return Err("path must be relative to the node's directory, not absolute".into());
    }
    // Split by hand: `Path::components` drops interior `.` segments and `//`, and we
    // want to reject those rather than quietly normalize them.
    let mut out = PathBuf::new();
    for part in raw.split('/') {
        match part {
            "" => return Err("path has an empty segment (`//` or a trailing `/`)".into()),
            "." => return Err("path contains `.`".into()),
            ".." => return Err("path contains `..`".into()),
            _ if part.starts_with('.') => {
                return Err(format!("`{part}` is a hidden file or directory"));
            }
            _ => out.push(part),
        }
    }
    Ok(out)
}

fn has_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// [`relative_path`] joined onto `root`. Pure: the file need not exist.
pub fn path_inside(root: &str, raw: &str) -> Result<PathBuf, String> {
    relative_path(raw).map(|rel| Path::new(root).join(rel))
}

/// Resolve `raw` inside `root` and prove, with symlinks followed, that the real file is
/// still under the real root. For executors: call this right before opening the file.
pub fn resolve_inside(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let rel = relative_path(raw)?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let path = root.join(&rel);
    let real = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !real.starts_with(&root) {
        return Err(format!(
            "{} resolves to {}, outside {}",
            rel.display(),
            real.display(),
            root.display()
        ));
    }
    Ok(real)
}

/// The lowercase extension of a path, if it has one.
pub fn extension(raw: &str) -> Option<String> {
    Path::new(raw.trim())
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Whether the path's extension is one of `allowed` (compared lowercase, without the dot).
pub fn has_extension(raw: &str, allowed: &[&str]) -> bool {
    extension(raw).is_some_and(|ext| allowed.contains(&ext.as_str()))
}

// ----- scalars and lists -----

/// `value` within `min..=max`, with a message naming the field.
pub fn in_range<T: PartialOrd + std::fmt::Display>(
    field: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("{field} must be between {min} and {max}"))
    } else {
        Ok(())
    }
}

/// Command-line arguments: a bounded number of bounded, printable strings.
pub fn args(args: &[String]) -> Result<(), String> {
    if args.len() > MAX_ARGS {
        return Err(format!("more than {MAX_ARGS} arguments"));
    }
    for (i, arg) in args.iter().enumerate() {
        if arg.len() > MAX_ARG_LEN {
            return Err(format!(
                "argument {} is longer than {MAX_ARG_LEN} characters",
                i + 1
            ));
        }
        if arg.chars().any(char::is_control) {
            return Err(format!("argument {} contains a control character", i + 1));
        }
    }
    Ok(())
}

/// Free text such as a prompt: bounded and without NUL bytes.
pub fn text(field: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.len() > max_len {
        return Err(format!("{field} is longer than {max_len} bytes"));
    }
    if value.contains('\0') {
        return Err(format!("{field} contains a NUL byte"));
    }
    Ok(())
}

// ----- URLs -----

/// An HTTP(S) base URL for a model server. Credentials in the URL are rejected (they go
/// in the environment), as are queries and fragments; plain `http://` is allowed only for
/// loopback hosts, so a key never travels unencrypted to a remote server.
pub fn endpoint_url(raw: &str) -> Result<url::Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("endpoint is empty".into());
    }
    if raw.len() > MAX_PATH_LEN {
        return Err(format!("endpoint is longer than {MAX_PATH_LEN} characters"));
    }
    let url = url::Url::parse(raw).map_err(|e| format!("endpoint is not a valid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "endpoint scheme must be http or https, not `{other}`"
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "endpoint must not carry credentials; name the environment variable instead".into(),
        );
    }
    let Some(host) = url.host_str() else {
        return Err("endpoint has no host".into());
    };
    if url.query().is_some() || url.fragment().is_some() {
        return Err("endpoint must not have a query or fragment".into());
    }
    if url.scheme() == "http" && !is_loopback(host) {
        return Err("plain http is only allowed for localhost; remote endpoints need https".into());
    }
    Ok(url)
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.ends_with(".localhost")
}

// ----- SQL -----

/// Statements a read-only query may start with.
const READ_ONLY_STARTS: &[&str] = &[
    "select", "with", "explain", "show", "describe", "desc", "values",
];

/// Words that write, change schema, or change session state. Rejected anywhere in a
/// read-only query, including inside CTEs (`WITH x AS (...) DELETE ...` is valid SQL).
const WRITE_WORDS: &[&str] = &[
    "insert",
    "update",
    "delete",
    "drop",
    "alter",
    "create",
    "truncate",
    "replace",
    "merge",
    "upsert",
    "into",
    "vacuum",
    "reindex",
    "pragma",
    "set",
    "reset",
    "lock",
    "unlock",
    "begin",
    "start",
    "commit",
    "rollback",
    "savepoint",
    "release",
    "prepare",
    "deallocate",
    "declare",
    "call",
    "exec",
    "execute",
    "do",
    "use",
    "refresh",
    "cluster",
    "analyze",
    "analyse",
    "comment",
    "listen",
    "notify",
    "unlisten",
    "discard",
    "checkpoint",
    "import",
    "export",
];

/// Words that reach outside the database: other files, the network, extensions, other
/// sessions, or the server itself. Rejected in every query, read-only or not. Both plain
/// keywords and function names are listed; qualified names are checked part by part.
const BLOCKED_WORDS: &[&str] = &[
    // statements
    "attach",
    "detach",
    "load",
    "install",
    "copy",
    "grant",
    "revoke",
    "shutdown",
    "kill",
    "outfile",
    "dumpfile",
    "infile",
    // postgresql
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_stat_file",
    "pg_ls_logdir",
    "pg_ls_waldir",
    "pg_execute_server_program",
    "lo_import",
    "lo_export",
    "lo_from_bytea",
    "dblink",
    "dblink_connect",
    "dblink_exec",
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    // sqlite
    "load_extension",
    "readfile",
    "writefile",
    "edit",
    "fsdir",
    "zipfile",
    // duckdb: files are loaded through the node's validated input, never from the query
    "read_csv",
    "read_csv_auto",
    "read_parquet",
    "read_json",
    "read_json_auto",
    "read_json_objects",
    "read_ndjson",
    "read_ndjson_auto",
    "read_ndjson_objects",
    "read_text",
    "read_blob",
    "read_xlsx",
    "glob",
    "parquet_scan",
    "sniff_csv",
    "httpfs",
    // mysql
    "load_file",
    "sys_eval",
    "sys_exec",
    "benchmark",
    "sleep",
    // sql server
    "xp_cmdshell",
    "openrowset",
    "opendatasource",
    "bulk",
];

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// A bare or quoted identifier, keyword or number, lowercased.
    Word(String),
    Semicolon,
    /// A string literal or operator: inert for our purposes.
    Other,
}

/// Check a SQL query before it reaches an engine. Always: non-empty, bounded, exactly one
/// statement, no MySQL executable comments, no words from [`BLOCKED_WORDS`]. With
/// `read_only`: must start with a read statement and contain no [`WRITE_WORDS`].
///
/// Quoted identifiers are scanned like bare ones because `"pg_read_file"(...)` is a valid
/// call, and `/* */` comments end at the first `*/` because that is what the engines that
/// don't nest comments do; both choices only ever reject more.
pub fn sql(query: &str, read_only: bool) -> Result<(), String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("query is empty".into());
    }
    if q.len() > MAX_QUERY_LEN {
        return Err(format!("query is longer than {MAX_QUERY_LEN} bytes"));
    }
    if q.contains('\0') {
        return Err("query contains a NUL byte".into());
    }
    let toks = tokenize(q)?;
    if let Some(i) = toks.iter().position(|t| *t == Tok::Semicolon)
        && i + 1 != toks.len()
    {
        return Err("only one statement is allowed".into());
    }
    let words: Vec<&str> = toks
        .iter()
        .filter_map(|t| match t {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .collect();
    let Some(first) = words.first() else {
        return Err("query has no statement".into());
    };
    if read_only {
        if !READ_ONLY_STARTS.contains(first) {
            return Err(format!(
                "a read-only query must start with SELECT, WITH, EXPLAIN, SHOW, DESCRIBE or VALUES, not `{}`",
                first.to_uppercase()
            ));
        }
        if let Some(w) = words.iter().find(|w| WRITE_WORDS.contains(w)) {
            return Err(format!(
                "`{}` is not allowed in a read-only query",
                w.to_uppercase()
            ));
        }
    }
    if let Some(w) = words.iter().find(|w| BLOCKED_WORDS.contains(w)) {
        return Err(format!(
            "`{}` is not allowed: it reaches outside the database",
            w.to_uppercase()
        ));
    }
    Ok(())
}

fn tokenize(q: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = q.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            c if c.is_whitespace() => i += 1,
            '\'' => {
                i = skip_string(&chars, i + 1)?;
                out.push(Tok::Other);
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let (word, end) = take_quoted_ident(&chars, i + 1, close)?;
                out.push(Tok::Word(word));
                i = end;
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                if chars.get(i + 2) == Some(&'!') {
                    return Err("executable comments (`/*!`) are not allowed".into());
                }
                i += 2;
                loop {
                    match (chars.get(i), chars.get(i + 1)) {
                        (None, _) => return Err("unterminated comment".into()),
                        (Some('*'), Some('/')) => {
                            i += 2;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            ';' => {
                out.push(Tok::Semicolon);
                i += 1;
            }
            c if is_word_char(c) => {
                let start = i;
                while i < chars.len() && is_word_char(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                out.push(Tok::Word(word.to_lowercase()));
            }
            '.' => i += 1,
            _ => {
                out.push(Tok::Other);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Skip a single-quoted literal starting after its opening quote. `''` and `\'` continue
/// the literal. Returns the index after the closing quote.
fn skip_string(chars: &[char], mut i: usize) -> Result<usize, String> {
    loop {
        match chars.get(i) {
            None => return Err("unterminated string literal".into()),
            Some('\\') if i + 1 < chars.len() => i += 2,
            Some('\'') if chars.get(i + 1) == Some(&'\'') => i += 2,
            Some('\'') => return Ok(i + 1),
            Some(_) => i += 1,
        }
    }
}

/// Read a quoted identifier up to `close` (a doubled `close` is an escape). Returns the
/// lowercased name and the index after the closing quote.
fn take_quoted_ident(chars: &[char], mut i: usize, close: char) -> Result<(String, usize), String> {
    let mut word = String::new();
    loop {
        match chars.get(i) {
            None => return Err("unterminated quoted identifier".into()),
            Some(&c) if c == close && chars.get(i + 1) == Some(&close) => {
                word.push(c);
                i += 2;
            }
            Some(&c) if c == close => return Ok((word.to_lowercase(), i + 1)),
            Some(&c) => {
                word.push(c);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stems_stay_inside_one_directory() {
        assert_eq!(file_stem("../etc/passwd"), "---etc-passwd");
        assert_eq!(file_stem("  My Graph 2 "), "My Graph 2");
        assert_eq!(file_stem(""), "untitled");
        assert_eq!(file_stem(".hidden"), "-hidden");
        assert_eq!(file_stem("a.json"), "a-json");
        assert_eq!(file_stem(&"x".repeat(200)).len(), MAX_NAME_LEN);
    }

    #[test]
    fn env_var_names() {
        assert!(env_var("ANTHROPIC_API_KEY").is_ok());
        assert!(env_var("_x1").is_ok());
        assert!(env_var("").is_err());
        assert!(env_var("1ABC").is_err());
        assert!(env_var("MY-KEY").is_err());
        assert!(env_var("KEY=value").is_err());
        assert!(env_var("$(whoami)").is_err());
    }

    #[test]
    fn identifiers() {
        assert!(identifier("input").is_ok());
        assert!(identifier("_t2").is_ok());
        assert!(identifier("").is_err());
        assert!(identifier("2fast").is_err());
        assert!(identifier("t; drop table x").is_err());
        assert!(identifier("a.b").is_err());
        assert!(identifier(&"a".repeat(64)).is_err());
    }

    #[test]
    fn model_ids() {
        assert!(model_id("llama3.1").is_ok());
        assert!(model_id("claude-opus-5-5").is_ok());
        assert!(model_id("org/model:tag").is_ok());
        assert!(model_id("").is_err());
        assert!(model_id("gpt 4").is_err());
        assert!(model_id("/etc").is_err());
        assert!(model_id("x;y").is_err());
        assert!(model_id("a/../b").is_err());
        assert!(model_id("a//b").is_err());
        assert!(model_id("a/").is_err());
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert_eq!(relative_path("a/b.csv").unwrap(), PathBuf::from("a/b.csv"));
        assert_eq!(
            relative_path(" sales.csv ").unwrap(),
            PathBuf::from("sales.csv")
        );
        for bad in [
            "",
            "/etc/passwd",
            "~/x",
            "../x",
            "a/../../x",
            "./a",
            "a/./b",
            ".env",
            "a/.git/config",
            "a\\b",
            "C:x",
            "a\0b",
            "a\nb",
        ] {
            assert!(relative_path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(relative_path(&"a/".repeat(300)).is_err());
    }

    #[test]
    fn path_inside_joins_the_root() {
        assert_eq!(
            path_inside("data", "x/y.csv").unwrap(),
            PathBuf::from("data/x/y.csv")
        );
        assert!(path_inside("data", "../graphs/a.json").is_err());
    }

    #[test]
    fn resolve_inside_follows_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "uapz-validate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/ok.csv"), "a\n").unwrap();
        std::fs::write(root.join("secret.txt"), "s\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("secret.txt"), root.join("data/link.txt")).unwrap();

        let data = root.join("data");
        assert!(resolve_inside(&data, "ok.csv").is_ok());
        assert!(resolve_inside(&data, "missing.csv").is_err());
        #[cfg(unix)]
        assert!(
            resolve_inside(&data, "link.txt").is_err(),
            "symlink escapes"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("A.CSV").as_deref(), Some("csv"));
        assert_eq!(extension("noext"), None);
        assert!(has_extension("x.py", &["py"]));
        assert!(!has_extension("x.sh", &["py"]));
    }

    #[test]
    fn ranges_and_args() {
        assert!(in_range("t", 0.5, 0.0, 2.0).is_ok());
        assert!(in_range("t", 2.5, 0.0, 2.0).is_err());
        assert!(args(&["--flag".into(), "value".into()]).is_ok());
        assert!(args(&["a\nb".into()]).is_err());
        assert!(args(&vec!["x".to_string(); MAX_ARGS + 1]).is_err());
        assert!(text("prompt", "hello", 10).is_ok());
        assert!(text("prompt", "hello world!", 10).is_err());
        assert!(text("prompt", "a\0b", 10).is_err());
    }

    #[test]
    fn endpoint_urls() {
        assert!(endpoint_url("http://localhost:11434").is_ok());
        assert!(endpoint_url("http://127.0.0.1:8080/v1").is_ok());
        assert!(endpoint_url("https://openrouter.ai/api/v1").is_ok());
        for bad in [
            "",
            "localhost:11434",
            "ftp://x",
            "http://example.com/v1",
            "https://user:pw@example.com/v1",
            "https://example.com/v1?key=abc",
            "https://example.com/v1#frag",
            "file:///etc/passwd",
        ] {
            assert!(endpoint_url(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn read_only_sql_accepts_reads() {
        for ok in [
            "SELECT * FROM t",
            "select a, b from t where x = 'DROP TABLE t;' order by a desc;",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "EXPLAIN SELECT 1",
            "SELECT * FROM t -- trailing comment",
            "SELECT /* block */ 1",
            "SELECT \"weird name\" FROM t",
            "SELECT 'it''s' FROM t",
            "SELECT $1",
        ] {
            assert!(sql(ok, true).is_ok(), "{ok}: {:?}", sql(ok, true));
        }
    }

    #[test]
    fn read_only_sql_rejects_writes_and_escapes() {
        for bad in [
            "",
            "DROP TABLE t",
            "DELETE FROM t",
            "SELECT 1; DROP TABLE t",
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
            "SELECT * INTO x FROM t",
            "SELECT * FROM read_csv('/etc/passwd')",
            "SELECT pg_read_file('/etc/passwd')",
            "SELECT pg_catalog.pg_read_file('x')",
            "SELECT \"pg_read_file\"('x')",
            "SELECT * FROM t /*! UNION SELECT 1 */",
            "SELECT 'unterminated",
            "PRAGMA journal_mode = wal",
            "SELECT load_extension('x')",
            "COPY t TO '/tmp/x'",
            "ATTACH 'other.db' AS o",
            "SELECT sleep(10)",
            "insert into t values (1)",
        ] {
            assert!(sql(bad, true).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn writable_sql_still_blocks_escapes() {
        assert!(sql("INSERT INTO t VALUES (1)", false).is_ok());
        assert!(sql("UPDATE t SET a = 1 WHERE b = 2", false).is_ok());
        assert!(sql("INSERT INTO t VALUES (1); DELETE FROM t", false).is_err());
        assert!(sql("COPY t FROM '/etc/passwd'", false).is_err());
        assert!(sql("SELECT * FROM read_parquet('x')", false).is_err());
        assert!(sql("LOAD httpfs", false).is_err());
        assert!(sql("GRANT ALL ON t TO u", false).is_err());
    }

    #[test]
    fn tokenizer_handles_quotes_and_comments() {
        let toks = tokenize("select \"A\"\"b\" from `t` -- x\n/* y */ ;").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::Word("select".into()),
                Tok::Word("a\"b".into()),
                Tok::Word("from".into()),
                Tok::Word("t".into()),
                Tok::Semicolon,
            ]
        );
        assert!(tokenize("/* open").is_err());
        assert!(tokenize("\"open").is_err());
    }
}
