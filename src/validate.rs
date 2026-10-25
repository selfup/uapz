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

// ----- node names and templates -----

/// A node's name, as `{{name}}` in an agent's prompt refers to it: letters, digits, `_`
/// and `-`, starting with a letter or `_`, at most [`MAX_NAME_LEN`] characters. A name
/// shaped like a node id (`n3`) is refused, since a placeholder may also hold an id.
pub fn node_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("name is longer than {MAX_NAME_LEN} characters"));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('0');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("name must start with a letter or `_`".into());
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))) {
        return Err(format!(
            "name contains `{bad}`; only letters, digits, `_` and `-` are allowed"
        ));
    }
    if is_node_id(name) {
        return Err(format!("`{name}` looks like a node id; pick another name"));
    }
    Ok(())
}

/// `n` followed by digits: the shape of a node id in a graph file.
pub fn is_node_id(s: &str) -> bool {
    s.strip_prefix('n')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// What may stand between the braces of a placeholder: a node name or a node id.
fn is_placeholder_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// A template split into plain text and `{{name}}` holes. A hole is `{{`, optional
/// spaces, a name or id, optional spaces, `}}`; braces around anything else are text.
enum Piece<'a> {
    Text(&'a str),
    Hole(&'a str),
}

fn pieces(template: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let inner = after[..end].trim();
        if is_placeholder_name(inner) {
            out.push(Piece::Text(&rest[..start]));
            out.push(Piece::Hole(inner));
        } else {
            out.push(Piece::Text(&rest[..start + 2 + end + 2]));
        }
        rest = &after[end + 2..];
    }
    out.push(Piece::Text(rest));
    out
}

/// The names a template's placeholders refer to, in order of first appearance.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for piece in pieces(template) {
        if let Piece::Hole(name) = piece
            && !found.iter().any(|f| f == name)
        {
            found.push(name.to_string());
        }
    }
    found
}

/// The template with every placeholder replaced by what `lookup` returns for its name.
/// The first lookup error ends the rendering.
pub fn render(
    template: &str,
    lookup: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    for piece in pieces(template) {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Hole(name) => out.push_str(&lookup(name)?),
        }
    }
    Ok(out)
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

/// A directory node's path: `.` or empty for the working directory itself, otherwise a
/// [`relative_path`].
pub fn directory_path(raw: &str) -> Result<PathBuf, String> {
    match raw.trim() {
        "" | "." => Ok(PathBuf::new()),
        other => relative_path(other),
    }
}

/// A glob pattern for a directory node, checked by compiling it. `*` matches across `/`,
/// so `*.rs` takes Rust files at any depth and `src/**/*.rs` only those under `src/`.
pub fn glob(pattern: &str) -> Result<(), String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("pattern is empty".into());
    }
    if pattern.len() > MAX_PATH_LEN {
        return Err(format!("pattern is longer than {MAX_PATH_LEN} characters"));
    }
    if pattern.chars().any(char::is_control) {
        return Err("pattern contains a control character".into());
    }
    globset::Glob::new(pattern)
        .map(|_| ())
        .map_err(|e| format!("`{pattern}` is not a valid pattern: {}", e.kind()))
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

/// Model output with a surrounding Markdown code fence removed: "```sql\n…\n```" becomes
/// "…". Text without a fence is only trimmed.
pub fn unfenced(text: &str) -> String {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text.to_string();
    };
    // The opening fence may carry a language tag; it ends at the first newline.
    let body = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    let body = body.trim_end().strip_suffix("```").unwrap_or(body);
    body.trim().to_string()
}

/// Resolve a directory node's path inside `root` and prove, with symlinks followed, that
/// it is a directory still under the real root. `.` is the root itself.
pub fn resolve_directory_inside(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let rel = directory_path(raw)?;
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
    if !real.is_dir() {
        return Err(format!("{} is not a directory", real.display()));
    }
    Ok(real)
}

/// A path a write node may create or replace: a [`relative_path`] whose first component
/// is not one of the directories the app manages, so a node can never plant a script,
/// agent, prompt, database preset, kind or graph. Pure: the file need not exist.
pub fn writable_path(raw: &str) -> Result<PathBuf, String> {
    let rel = relative_path(raw)?;
    reserved_dir(&rel).map_or(Ok(rel), |reserved| {
        Err(format!(
            "`{reserved}/` is managed by the app; nodes can't write into it"
        ))
    })
}

/// The first component of `rel` when it names a directory the app owns, compared without
/// case so a case-insensitive filesystem can't be talked around.
fn reserved_dir(rel: &Path) -> Option<&'static str> {
    let first = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or_default();
    crate::library::RESERVED_DIRS
        .iter()
        .copied()
        .find(|d| d.eq_ignore_ascii_case(first))
}

/// Resolve a file that may not exist yet inside `root`: the deepest existing ancestor is
/// canonicalized and proven to be a directory under the real root, then the missing tail
/// is appended. An existing target is resolved like [`resolve_inside`] and must be a
/// regular file. For executors that create or replace files.
pub fn resolve_new_inside(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let path = root.join(rel);
    if path.symlink_metadata().is_ok() {
        let real = path.canonicalize().map_err(|e| {
            // A link whose target is missing: what would be written is the target, and
            // that can't be proven to stay inside the root, so it is refused. Writing to
            // the target directly is the way through.
            if path.is_symlink() {
                format!(
                    "{} is a symlink whose target does not exist; write to the target instead \
                     ({e})",
                    rel.display()
                )
            } else {
                format!("{}: {e}", path.display())
            }
        })?;
        if !real.starts_with(&root) {
            return Err(format!(
                "{} resolves to {}, outside {}",
                rel.display(),
                real.display(),
                root.display()
            ));
        }
        if !real.is_file() {
            return Err(format!("{} is not a regular file", real.display()));
        }
        return Ok(real);
    }
    // Walk up to something that exists. `rel` has no `.` or `..`, so this stops at the
    // root at the latest.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut dir = path.as_path();
    loop {
        tail.push(dir.file_name().unwrap_or_default().to_os_string());
        dir = dir
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        if dir.symlink_metadata().is_ok() {
            break;
        }
    }
    let real_dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    if !real_dir.starts_with(&root) {
        return Err(format!(
            "{} resolves to {}, outside {}",
            rel.display(),
            real_dir.display(),
            root.display()
        ));
    }
    if !real_dir.is_dir() {
        return Err(format!("{} is not a directory", real_dir.display()));
    }
    let mut out = real_dir;
    for part in tail.iter().rev() {
        out.push(part);
    }
    Ok(out)
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

/// Split an arguments line the way a shell does: whitespace separates arguments, double
/// or single quotes group words into one, and a backslash escapes the next character
/// outside single quotes. An unterminated quote is an error.
pub fn split_args(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => match c {
                '\'' => quote = None,
                _ => current.push(c),
            },
            Some(_) => match c {
                '"' => quote = None,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '\\' | '$' | '`')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => current.push('\\'),
                },
                _ => current.push(c),
            },
            None => match c {
                c if c.is_whitespace() => {
                    if in_arg {
                        args.push(std::mem::take(&mut current));
                        in_arg = false;
                    }
                }
                '"' | '\'' => {
                    quote = Some(c);
                    in_arg = true;
                }
                '\\' => {
                    in_arg = true;
                    match chars.next() {
                        Some(next) => current.push(next),
                        None => return Err("arguments end with a lone backslash".into()),
                    }
                }
                _ => {
                    in_arg = true;
                    current.push(c);
                }
            },
        }
    }
    match quote {
        Some('"') => return Err("arguments have an unclosed double quote".into()),
        Some(_) => return Err("arguments have an unclosed single quote".into()),
        None => {}
    }
    if in_arg {
        args.push(current);
    }
    Ok(args)
}

/// Arguments joined into one line that [`split_args`] reads back unchanged: those with
/// whitespace, quotes or backslashes (or nothing at all) go in double quotes.
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && !arg
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'));
            if plain {
                arg.clone()
            } else {
                format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

/// Commands the MySQL client reads from a statement's first line before SQL: they reach
/// the filesystem, another server or this machine, exactly like the backslash commands of
/// `psql` and the dot commands of `sqlite3`.
const MYSQL_CLIENT_COMMANDS: &[&str] = &[
    "system",
    "source",
    "tee",
    "notee",
    "pager",
    "nopager",
    "edit",
    "connect",
    "charset",
    "delimiter",
    "prompt",
    "rehash",
    "warnings",
    "status",
    "ego",
    "print",
    "help",
    "clear",
    "exit",
    "quit",
];

/// Check a SQL query before it reaches an engine. Always: non-empty, bounded, exactly one
/// statement, no MySQL executable comments, no words from [`BLOCKED_WORDS`], and no client
/// meta-command ([`shell_metacharacters`]). With `read_only`: must start with a read
/// statement and contain no [`WRITE_WORDS`].
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
    if let Some(bad) = q
        .chars()
        .find(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(format!(
            "query contains the control character U+{:04X}",
            bad as u32
        ));
    }
    shell_metacharacters(q)?;
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
    // Client commands begin a line, so `SELECT source FROM t` stays legal while a line of
    // its own naming one reaches the shell instead of the engine.
    for line in q.lines() {
        let first = line.split_whitespace().next().unwrap_or_default();
        if let Some(command) = MYSQL_CLIENT_COMMANDS
            .iter()
            .find(|c| first.eq_ignore_ascii_case(c))
        {
            return Err(format!(
                "`{}` is not allowed: the client runs it on this machine, not the database",
                command.to_uppercase()
            ));
        }
    }
    Ok(())
}

/// Reject what a database client would read as its own command instead of SQL: `psql`
/// takes `\` anywhere on a line (`SELECT 1 \! rm -rf ~` runs a shell), `sqlite3` takes a
/// dot command at the start of one, and both are fed the query as stdin. Strings,
/// identifiers and comments are stepped over, so `'\d'` and `-- .help` stay legal.
fn shell_metacharacters(q: &str) -> Result<(), String> {
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    // Whether everything before `i` on this line is whitespace, which is where `sqlite3`
    // reads a dot command.
    let mut line_start = true;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' => {
                line_start = false;
                i = skip_string(&chars, i + 1)?;
            }
            '"' | '`' | '[' => {
                line_start = false;
                let close = if c == '[' { ']' } else { c };
                let (_, end) = take_quoted_ident(&chars, i + 1, close)?;
                i = end;
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
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
                line_start = false;
            }
            '\n' | '\r' => {
                line_start = true;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '\\' => {
                return Err(
                    "query contains `\\`, which the database client runs as its own command; \
                     use the query, not a client command"
                        .into(),
                );
            }
            '.' if line_start => {
                return Err(
                    "query has a line starting with `.`, which the database client runs as its \
                     own command"
                        .into(),
                );
            }
            _ => {
                line_start = false;
                i += 1;
            }
        }
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
    fn node_names_are_identifiers_that_are_not_ids() {
        assert!(node_name("schema").is_ok());
        assert!(node_name("_x-1").is_ok());
        assert!(node_name("  schema ").is_ok());
        assert!(node_name("n3x").is_ok());
        assert!(node_name("").is_err());
        assert!(node_name("2fast").is_err());
        assert!(node_name("a b").is_err());
        assert!(node_name("a.b").is_err());
        assert!(node_name("n3").unwrap_err().contains("node id"));
        assert!(node_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(is_node_id("n0"));
        assert!(is_node_id("n42"));
        assert!(!is_node_id("n"));
        assert!(!is_node_id("x1"));
        assert!(!is_node_id("n-1"));
    }

    #[test]
    fn templates_find_and_fill_placeholders() {
        assert_eq!(
            placeholders(
                "Use {{schema}} and {{ question }}; {{schema}} again. {{not a name}}, {{}} and {{ stay."
            ),
            ["schema", "question"]
        );
        assert_eq!(placeholders("{{n3}} {{{x}}}"), ["n3"]);
        assert!(placeholders("no holes").is_empty());

        let filled = render(
            "A: {{a}}, B: {{ b }}, {{not a name}}, {{a}}",
            &mut |name| match name {
                "a" => Ok("1".into()),
                "b" => Ok("2".into()),
                other => Err(format!("no {other}")),
            },
        )
        .unwrap();
        assert_eq!(filled, "A: 1, B: 2, {{not a name}}, 1");
        assert_eq!(
            render("x {{missing}}", &mut |n| Err(format!("no {n}"))).unwrap_err(),
            "no missing"
        );
        assert_eq!(
            render("plain {{ text", &mut |_| Ok(String::new())).unwrap(),
            "plain {{ text"
        );
        assert_eq!(render("", &mut |_| Ok(String::new())).unwrap(), "");
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
    fn client_commands_never_reach_the_shell() {
        // Every one of these is fed to the client as stdin, and each client runs its own
        // commands from there: psql takes `\` anywhere on a line, sqlite3 a dot command at
        // the start of one, mysql a word on the first line. All are refused either way.
        for bad in [
            // psql: mid-line backslash, before and after the statement is complete.
            "SELECT 1 \\! touch /tmp/pwned",
            "SELECT 1\n\\! touch /tmp/pwned",
            "SELECT 1 \n\\! id",
            "SELECT * FROM t \\gset",
            "SELECT 1 \\x",
            "SELECT 1 \\o /tmp/out",
            "SELECT 1 \\q",
            "WITH x AS (SELECT 1) SELECT * FROM x \n\\! id",
            // sqlite3: dot commands, including after a finished statement.
            "SELECT 1\n.shell touch /tmp/pwned",
            "SELECT 1;\n.shell touch /tmp/pwned;\n",
            "SELECT 1\n.read /etc/passwd",
            "SELECT 1\n.output /tmp/out",
            "SELECT 1\n.import /etc/passwd x",
            "SELECT 1\n.once /tmp/x",
            "SELECT 1\n.log /tmp/x",
            // mysql client commands.
            "SELECT 1;\nsystem touch /tmp/pwned",
            "source /etc/passwd",
            "tee /tmp/out",
            // A NUL and stray control characters are never part of a statement.
            "SELECT 1\x07",
            "SELECT\x1b[31m 1",
        ] {
            assert!(sql(bad, true).is_err(), "{bad:?} should be rejected");
            assert!(
                sql(bad, false).is_err(),
                "{bad:?} must be refused even unsigned"
            );
        }
    }

    #[test]
    fn client_command_lookalikes_inside_sql_stay_legal() {
        for ok in [
            // A backslash or a dot inside a literal, identifier or comment is inert.
            r"SELECT '\d' AS slash",
            "SELECT 'a.b' FROM t",
            "SELECT * FROM t -- .help and \\! are not commands in a comment",
            "SELECT * FROM t /* \\! .shell */",
            "SELECT \"weird.name\" FROM t",
            // A column or table that happens to be called like a command, mid-statement.
            "SELECT source FROM t",
            "SELECT tee FROM t",
            "SELECT system FROM t",
            // A dot that starts neither a line nor a command.
            "SELECT t.x FROM t",
            "SELECT .5 + 1.5",
        ] {
            assert!(sql(ok, true).is_ok(), "{ok}: {:?}", sql(ok, true));
        }
    }

    #[test]
    fn shell_metacharacters_step_over_quotes_and_comments() {
        assert!(shell_metacharacters("SELECT 1").is_ok());
        assert!(shell_metacharacters("SELECT 'it''s' , \"a\\b\"").is_ok());
        assert!(shell_metacharacters("SELECT 1 \\").is_err());
        assert!(shell_metacharacters(".x").is_err());
        assert!(shell_metacharacters("  .x").is_err());
        assert!(shell_metacharacters("SELECT 1\r\\.x").is_err());
        assert!(shell_metacharacters("SELECT 1.5").is_ok());
        assert!(shell_metacharacters("SELECT 'unterminated").is_err());
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

    #[test]
    fn fences_are_stripped() {
        assert_eq!(unfenced("```sql\nSELECT 1;\n```"), "SELECT 1;");
        assert_eq!(unfenced("```\nSELECT 1\n```\n"), "SELECT 1");
        assert_eq!(unfenced("  SELECT 1  "), "SELECT 1");
        assert_eq!(unfenced("```SELECT 1```"), "SELECT 1");
        assert_eq!(unfenced(""), "");
    }

    #[test]
    fn writable_paths_skip_app_directories_and_resolve_new_files() {
        assert_eq!(
            writable_path("tests/gen/a.rs").unwrap(),
            PathBuf::from("tests/gen/a.rs")
        );
        assert!(writable_path("data/out.csv").is_ok());
        for bad in [
            "scripts/x.sh",
            "SCRIPTS/x.sh",
            "agents/a/agent.json",
            "graphs/g.md",
        ] {
            let err = writable_path(bad).unwrap_err();
            assert!(err.contains("managed by the app"), "{bad}: {err}");
        }
        assert!(writable_path("../x").is_err());
        assert!(writable_path("").is_err());

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("uapz-new-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("outside.txt"), "").unwrap();
        let real_root = root.canonicalize().unwrap();

        // A new file in an existing directory, and one under directories yet to be made.
        let fresh = resolve_new_inside(&root, Path::new("src/new.rs")).unwrap();
        assert_eq!(fresh, real_root.join("src/new.rs"));
        let deep = resolve_new_inside(&root, Path::new("tests/gen/deep.rs")).unwrap();
        assert_eq!(deep, real_root.join("tests/gen/deep.rs"));
        assert!(!deep.parent().unwrap().exists(), "nothing is created here");

        // An existing regular file resolves; a directory does not.
        assert_eq!(
            resolve_new_inside(&root, Path::new("src/lib.rs")).unwrap(),
            real_root.join("src/lib.rs")
        );
        assert!(
            resolve_new_inside(&root, Path::new("src"))
                .unwrap_err()
                .contains("not a regular file")
        );

        // Symlinks are followed and must stay inside the root.
        #[cfg(unix)]
        {
            let elsewhere = std::env::temp_dir().join(format!("uapz-new-out-{nanos}"));
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, root.join("escape")).unwrap();
            let err = resolve_new_inside(&root, Path::new("escape/file.txt")).unwrap_err();
            assert!(err.contains("outside"), "{err}");
            std::os::unix::fs::symlink(root.join("outside.txt"), root.join("src/alias.txt"))
                .unwrap();
            // Points inside the root, so writing through it is allowed.
            assert_eq!(
                resolve_new_inside(&root, Path::new("src/alias.txt")).unwrap(),
                real_root.join("outside.txt")
            );
            std::fs::remove_dir_all(&elsewhere).unwrap();
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_paths_and_globs() {
        assert_eq!(directory_path(".").unwrap(), PathBuf::new());
        assert_eq!(directory_path("  ").unwrap(), PathBuf::new());
        assert_eq!(directory_path("src/bin").unwrap(), PathBuf::from("src/bin"));
        assert!(directory_path("..").is_err());
        assert!(directory_path("/etc").is_err());
        assert!(directory_path("./src").is_err());
        assert!(glob("src/**/*.rs").is_ok());
        assert!(glob("*.{rs,md}").is_ok());
        assert!(glob("").is_err());
        assert!(glob("src/[").unwrap_err().contains("not a valid pattern"));
        assert!(glob("a\nb").is_err());
    }

    #[test]
    fn argument_lines_honor_quotes_and_round_trip() {
        let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(split_args("a  b\tc").unwrap(), v(&["a", "b", "c"]));
        assert_eq!(
            split_args(r#""read the changes" --fast 'it''s' x\ y"#).unwrap(),
            v(&["read the changes", "--fast", "its", "x y"])
        );
        assert_eq!(
            split_args(r#""say \"hi\" \$HOME \n""#).unwrap(),
            v(&[r#"say "hi" $HOME \n"#])
        );
        assert_eq!(split_args(r#""" ''"#).unwrap(), v(&["", ""]));
        assert_eq!(split_args("   ").unwrap(), Vec::<String>::new());
        assert!(split_args(r#""open"#).unwrap_err().contains("double quote"));
        assert!(split_args("'open").unwrap_err().contains("single quote"));
        assert!(split_args("trailing\\").unwrap_err().contains("backslash"));

        for args in [
            v(&[
                "plain",
                "two words",
                "",
                r#"with "quotes""#,
                "back\\slash",
                "it's",
            ]),
            v(&["-p", "read the uncommitted changes and do a security pass"]),
        ] {
            let line = join_args(&args);
            assert_eq!(split_args(&line).unwrap(), args, "{line}");
        }
        assert_eq!(join_args(&v(&["a", "b c"])), r#"a "b c""#);
    }
}
