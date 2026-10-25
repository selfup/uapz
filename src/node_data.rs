//! Typed per-node content: what an agent, database, script, data, directory or write-file
//! node is configured with. This is the `data` object in the graph's JSON sidecar; a blank node has none.
//!
//! Every config validates itself with [`NodeData::validate`], which the inspector shows
//! live and the library refuses to save over. Nothing here touches the filesystem or the
//! network: configs only *describe* a run. Secrets are never part of a config, only the
//! names of the environment variables that hold them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::library::{DATA_DIR, SCRIPTS_DIR};
use crate::validate::{self, Issue, MAX_PROMPT_LEN};

/// Fill color of every node that writes outside the app's own output folders, as
/// `#rrggbb`: a database allowed to write and a write-file node. Red, so it stands out.
pub const WRITE_COLOR: &str = "#ef4444";
/// Picker label for a database node created with its read-only flag off.
pub const WRITABLE_DATABASE_LABEL: &str = "Database (write)";

/// The kinds of content a node can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Input,
    Agent,
    Database,
    Script,
    Data,
    Directory,
    /// Writes the text arriving on its wire to a file in the working directory.
    File,
}

impl NodeType {
    pub const ALL: [NodeType; 7] = [
        Self::Input,
        Self::Agent,
        Self::Database,
        Self::Script,
        Self::Data,
        Self::Directory,
        Self::File,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Input => "Run input",
            Self::Agent => "Agent",
            Self::Database => "Database",
            Self::Script => "Script",
            Self::Data => "Data",
            Self::Directory => "Directory",
            Self::File => "Write file",
        }
    }

    /// Fill color for nodes of this type, as `#rrggbb`. Write-file nodes are red like a
    /// writable database: red means the node changes something outside `agents/`.
    pub fn color(self) -> &'static str {
        match self {
            Self::Input => "#06b6d4",
            Self::Agent => "#8b5cf6",
            Self::Database => "#0ea5e9",
            Self::Script => "#f59e0b",
            Self::Data => "#22c55e",
            Self::Directory => "#14b8a6",
            Self::File => WRITE_COLOR,
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.label().eq_ignore_ascii_case(label.trim()))
    }

    /// A fresh, default configuration of this type.
    pub fn default_data(self) -> NodeData {
        match self {
            Self::Input => NodeData::Input(crate::workflow::InputConfig::default()),
            Self::Agent => NodeData::Agent(AgentConfig::default()),
            Self::Database => NodeData::Database(DatabaseConfig::default()),
            Self::Script => NodeData::Script(ScriptConfig::default()),
            Self::Data => NodeData::Data(DataConfig::default()),
            Self::Directory => NodeData::Directory(DirectoryConfig::default()),
            Self::File => NodeData::File(FileConfig::default()),
        }
    }
}

/// A node's content. Serialized as an object tagged with `"type"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NodeData {
    Input(crate::workflow::InputConfig),
    Agent(AgentConfig),
    Database(DatabaseConfig),
    Script(ScriptConfig),
    Data(DataConfig),
    Directory(DirectoryConfig),
    File(FileConfig),
}

impl NodeData {
    pub fn node_type(&self) -> NodeType {
        match self {
            Self::Input(_) => NodeType::Input,
            Self::Agent(_) => NodeType::Agent,
            Self::Database(_) => NodeType::Database,
            Self::Script(_) => NodeType::Script,
            Self::Data(_) => NodeType::Data,
            Self::Directory(_) => NodeType::Directory,
            Self::File(_) => NodeType::File,
        }
    }

    /// Fill color for the canvas and sidebar: the type's, except that a database allowed
    /// to write is red so it stands out.
    pub fn color(&self) -> &'static str {
        match self {
            Self::Database(d) if !d.read_only => WRITE_COLOR,
            other => other.node_type().color(),
        }
    }

    /// The type's label, or `Database (write)` for a database allowed to write.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Database(d) if !d.read_only => WRITABLE_DATABASE_LABEL,
            other => other.node_type().label(),
        }
    }

    /// One short line for the canvas: the most identifying setting of the config.
    pub fn summary(&self) -> String {
        match self {
            Self::Input(input) => input.summary(),
            Self::Agent(a) => a.summary(),
            Self::Database(d) => d.summary(),
            Self::Script(s) => s.summary(),
            Self::Data(d) => d.summary(),
            Self::Directory(d) => d.summary(),
            Self::File(f) => f.summary(),
        }
    }

    /// Everything wrong with the config. Empty means it is safe to save and (later) run.
    pub fn validate(&self) -> Vec<Issue> {
        match self {
            Self::Input(input) => input.validate(),
            Self::Agent(a) => a.validate(),
            Self::Database(d) => d.validate(),
            Self::Script(s) => s.validate(),
            Self::Data(d) => d.validate(),
            Self::Directory(d) => d.validate(),
            Self::File(f) => f.validate(),
        }
    }
}

/// Serialize an optional payload so that a blank node writes `{}`, matching older files.
pub fn serialize_opt<S: Serializer>(data: &Option<NodeData>, s: S) -> Result<S::Ok, S::Error> {
    match data {
        Some(data) => data.serialize(s),
        None => Value::Object(Default::default()).serialize(s),
    }
}

/// Read a payload: an object with a `type` is parsed strictly, anything else is blank.
pub fn deserialize_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NodeData>, D::Error> {
    let value = Value::deserialize(d)?;
    if value.get("type").is_none() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

// ----- agent -----

/// Where an agent's model runs. Local providers listen on this machine and need no key;
/// the rest need an API key named by [`Auth`]. Everything except Anthropic, Google and
/// Ollama speaks the OpenAI-compatible chat and embeddings API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Ollama,
    LmStudio,
    LlamaCpp,
    OpenRouter,
    Anthropic,
    OpenAi,
    Google,
    Mistral,
    /// Any other OpenAI-compatible server. The endpoint is required.
    Custom,
}

/// The wire protocol a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    OpenAi,
    Ollama,
    Anthropic,
    Google,
}

impl Provider {
    pub const ALL: [Provider; 9] = [
        Self::Ollama,
        Self::LmStudio,
        Self::LlamaCpp,
        Self::OpenRouter,
        Self::Anthropic,
        Self::OpenAi,
        Self::Google,
        Self::Mistral,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
            Self::LlamaCpp => "llama.cpp",
            Self::OpenRouter => "OpenRouter",
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Google => "Google",
            Self::Mistral => "Mistral",
            Self::Custom => "Custom (OpenAI-compatible)",
        }
    }

    /// Local providers run on this machine and need no credentials.
    pub fn is_local(self) -> bool {
        matches!(self, Self::Ollama | Self::LmStudio | Self::LlamaCpp)
    }

    pub fn api(self) -> Api {
        match self {
            Self::Ollama => Api::Ollama,
            Self::Anthropic => Api::Anthropic,
            Self::Google => Api::Google,
            _ => Api::OpenAi,
        }
    }

    /// Base URL used when the config leaves the endpoint empty.
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Self::Ollama => "http://localhost:11434",
            Self::LmStudio => "http://localhost:1234/v1",
            Self::LlamaCpp => "http://localhost:8080/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::Custom => "",
        }
    }

    /// A sensible starting model, or empty where the id depends on the account or server.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Ollama => "llama3.1",
            Self::Anthropic => "claude-opus-5-5",
            _ => "",
        }
    }

    /// The conventional environment variable for the provider's API key.
    pub fn default_env_var(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Ollama | Self::LmStudio | Self::LlamaCpp | Self::Custom => "",
        }
    }

    /// Whether the executor can drive this kind of model on this provider.
    pub fn supports(self, model_type: ModelType) -> bool {
        match model_type {
            ModelType::Generative => true,
            ModelType::Embedding => self != Self::Anthropic,
            ModelType::Image => matches!(self, Self::OpenAi | Self::Custom),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    Generative,
    Embedding,
    Image,
}

impl ModelType {
    pub const ALL: [ModelType; 3] = [Self::Generative, Self::Embedding, Self::Image];

    pub fn label(self) -> &'static str {
        match self {
            Self::Generative => "Generative",
            Self::Embedding => "Embedding",
            Self::Image => "Image",
        }
    }

    /// Output types a model of this type can produce.
    pub fn outputs(self) -> &'static [OutputType] {
        match self {
            Self::Generative => &[OutputType::Text, OutputType::Json],
            Self::Embedding => &[OutputType::Embedding],
            Self::Image => &[OutputType::Image],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Text,
    Json,
    Embedding,
    Image,
}

impl OutputType {
    pub const ALL: [OutputType; 4] = [Self::Text, Self::Json, Self::Embedding, Self::Image];

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Json => "JSON",
            Self::Embedding => "Embedding",
            Self::Image => "Image",
        }
    }

    /// File extension an output of this type is saved with.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Json | Self::Embedding => "json",
            Self::Image => "png",
        }
    }
}

/// How an agent authenticates. The secret itself is never stored, only where to find it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Auth {
    /// Local provider, or none needed.
    #[default]
    None,
    /// Read the API key from this environment variable at run time.
    Env { var: String },
}

impl Auth {
    pub fn env_var(&self) -> &str {
        match self {
            Self::None => "",
            Self::Env { var } => var,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub provider: Provider,
    /// Base URL of the provider's API. Empty means the provider's default; required for
    /// `Custom`. Plain `http://` only for localhost.
    pub endpoint: String,
    pub model_type: ModelType,
    pub model: String,
    pub auth: Auth,
    /// System prompt. Reusable prompts live in `prompts/` and are copied in. `{{name}}`
    /// stands for the output of the upstream node with that name (or id).
    pub prompt: String,
    /// The user message. Empty sends the inputs joined with blank lines. Otherwise this
    /// text is sent with its `{{name}}` placeholders filled, followed by the inputs that
    /// neither it nor the prompt names.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Context window to request, in tokens.
    pub context_size: u32,
    pub temperature: f32,
    pub output: OutputType,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Ollama,
            endpoint: String::new(),
            model_type: ModelType::Generative,
            model: Provider::Ollama.default_model().to_string(),
            auth: Auth::None,
            prompt: String::new(),
            message: String::new(),
            context_size: 8192,
            temperature: 0.7,
            output: OutputType::Text,
        }
    }
}

pub const MAX_CONTEXT_SIZE: u32 = 2_000_000;

impl AgentConfig {
    /// The base URL requests go to: the override, or the provider's default.
    pub fn endpoint_url(&self) -> &str {
        let custom = self.endpoint.trim();
        if custom.is_empty() {
            self.provider.default_endpoint()
        } else {
            custom
        }
    }

    pub fn summary(&self) -> String {
        let model = if self.model.trim().is_empty() {
            "no model"
        } else {
            self.model.trim()
        };
        format!("{} · {model}", self.provider.label())
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if let Err(e) = validate::model_id(&self.model) {
            issues.push(Issue::new("model", e));
        }
        if self.endpoint.trim().is_empty() {
            if self.provider == Provider::Custom {
                issues.push(Issue::new(
                    "endpoint",
                    "a custom provider needs its base URL, e.g. http://localhost:8000/v1",
                ));
            }
        } else if let Err(e) = validate::endpoint_url(&self.endpoint) {
            issues.push(Issue::new("endpoint", e));
        }
        if !self.provider.supports(self.model_type) {
            issues.push(Issue::new(
                "model type",
                format!(
                    "{} {} models are not supported here",
                    self.provider.label(),
                    self.model_type.label().to_lowercase()
                ),
            ));
        }
        match (
            &self.auth,
            self.provider.is_local() || self.provider == Provider::Custom,
        ) {
            (Auth::None, false) => issues.push(Issue::new(
                "auth",
                format!(
                    "{} needs an API key: name the environment variable that holds it",
                    self.provider.label()
                ),
            )),
            (Auth::Env { var }, _) => {
                if let Err(e) = validate::env_var(var) {
                    issues.push(Issue::new("auth", e));
                }
            }
            (Auth::None, true) => {}
        }
        if !self.model_type.outputs().contains(&self.output) {
            issues.push(Issue::new(
                "output",
                format!(
                    "a {} model produces {}",
                    self.model_type.label().to_lowercase(),
                    join_labels(self.model_type.outputs().iter().map(|o| o.label()))
                ),
            ));
        }
        if let Err(e) = validate::in_range("context size", self.context_size, 1, MAX_CONTEXT_SIZE) {
            issues.push(Issue::new("context size", e));
        }
        if !self.temperature.is_finite() {
            issues.push(Issue::new("temperature", "temperature must be a number"));
        } else if let Err(e) = validate::in_range("temperature", self.temperature, 0.0, 2.0) {
            issues.push(Issue::new("temperature", e));
        }
        if let Err(e) = validate::text("prompt", &self.prompt, MAX_PROMPT_LEN) {
            issues.push(Issue::new("prompt", e));
        }
        if let Err(e) = validate::text("message", &self.message, MAX_PROMPT_LEN) {
            issues.push(Issue::new("message", e));
        }
        issues
    }
}

fn join_labels<'a>(labels: impl Iterator<Item = &'a str>) -> String {
    labels.collect::<Vec<_>>().join(" or ")
}

// ----- database -----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbEngine {
    Sqlite,
    DuckDb,
    Postgres,
    Mysql,
}

impl DbEngine {
    pub const ALL: [DbEngine; 4] = [Self::Sqlite, Self::DuckDb, Self::Postgres, Self::Mysql];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "SQLite",
            Self::DuckDb => "DuckDB",
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
        }
    }

    /// File-based engines open a database file inside `data/`; the others connect to a
    /// server with a URL read from the environment.
    pub fn is_file_based(self) -> bool {
        matches!(self, Self::Sqlite | Self::DuckDb)
    }

    /// Input files the executor can load into the input table. Server engines take none:
    /// loading a file there needs a schema, so load it into the database yourself.
    pub fn input_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Sqlite => &["csv"],
            Self::DuckDb => &["csv", "tsv", "json", "ndjson", "jsonl", "parquet"],
            Self::Postgres | Self::Mysql => &[],
        }
    }

    /// Extensions a database file of this engine may have.
    pub fn file_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Sqlite => &["sqlite", "sqlite3", "db"],
            Self::DuckDb => &["duckdb", "ddb", "db"],
            Self::Postgres | Self::Mysql => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub engine: DbEngine,
    /// Database file inside `data/`, for file-based engines.
    pub path: String,
    /// Environment variable holding the connection URL, for server engines. The URL (and
    /// any password in it) stays in the environment.
    pub url_env: String,
    /// Optional data file inside `data/` loaded into `input_table` before the query runs.
    pub input: String,
    pub input_table: String,
    pub query: String,
    /// Only read statements are allowed when set. Default on.
    pub read_only: bool,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            engine: DbEngine::Sqlite,
            path: String::new(),
            url_env: String::new(),
            input: String::new(),
            input_table: "input".to_string(),
            query: String::new(),
            read_only: true,
        }
    }
}

impl DatabaseConfig {
    /// A fresh config whose read-only flag is off: the red node that may write.
    pub fn writable() -> Self {
        Self {
            read_only: false,
            ..Self::default()
        }
    }

    pub fn summary(&self) -> String {
        let target = if self.engine.is_file_based() {
            file_name(&self.path).unwrap_or("no file")
        } else if self.url_env.trim().is_empty() {
            "no connection"
        } else {
            self.url_env.trim()
        };
        let mode = if self.read_only { "" } else { " · write" };
        format!("{} · {target}{mode}", self.engine.label())
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.engine.is_file_based() {
            match validate::path_inside(DATA_DIR, &self.path) {
                Err(e) => issues.push(Issue::new("path", e)),
                Ok(_) if !validate::has_extension(&self.path, self.engine.file_extensions()) => {
                    issues.push(Issue::new(
                        "path",
                        format!(
                            "a {} file should end in .{}",
                            self.engine.label(),
                            self.engine.file_extensions().join(", .")
                        ),
                    ));
                }
                Ok(_) => {}
            }
        } else if let Err(e) = validate::env_var(&self.url_env) {
            issues.push(Issue::new("connection", e));
        }
        if !self.input.trim().is_empty() {
            let allowed = self.engine.input_extensions();
            match validate::path_inside(DATA_DIR, &self.input) {
                Err(e) => issues.push(Issue::new("input", e)),
                Ok(_) if allowed.is_empty() => issues.push(Issue::new(
                    "input",
                    format!(
                        "{} nodes can't load an input file; load it into the database yourself",
                        self.engine.label()
                    ),
                )),
                Ok(_) if !validate::has_extension(&self.input, allowed) => {
                    issues.push(Issue::new(
                        "input",
                        format!(
                            "{} input must be one of .{}",
                            self.engine.label(),
                            allowed.join(", .")
                        ),
                    ));
                }
                Ok(_) => {}
            }
        }
        // Checked whatever the input is: the field's value doesn't become wrong or right
        // because a file was chosen, and the inspector should say so before a run.
        if let Err(e) = validate::identifier(&self.input_table) {
            issues.push(Issue::new("input table", e));
        }
        // An empty query is allowed: the executor then runs the SQL arriving on the wire.
        if !self.query.trim().is_empty()
            && let Err(e) = validate::sql_for_engine(&self.query, self.read_only, self.engine)
        {
            issues.push(Issue::new("query", e));
        }
        issues
    }
}

// ----- script -----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Python,
    Shell,
    Node,
}

impl Runtime {
    pub const ALL: [Runtime; 3] = [Self::Python, Self::Shell, Self::Node];

    pub fn label(self) -> &'static str {
        match self {
            Self::Python => "Python (uv)",
            Self::Shell => "Shell",
            Self::Node => "Node",
        }
    }

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Python => &["py"],
            Self::Shell => &["sh", "bash", "zsh"],
            Self::Node => &["js", "mjs", "cjs"],
        }
    }

    /// The runtime that handles a file with this extension.
    pub fn for_extension(ext: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|r| r.extensions().contains(&ext.to_ascii_lowercase().as_str()))
    }

    /// Every extension any runtime accepts: what the Scripts sidebar lists.
    pub fn all_extensions() -> Vec<&'static str> {
        Self::ALL
            .iter()
            .flat_map(|r| r.extensions())
            .copied()
            .collect()
    }
}

pub const MAX_TIMEOUT_SECS: u32 = 86_400;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptConfig {
    pub runtime: Runtime,
    /// Script file inside `scripts/`.
    pub path: String,
    pub args: Vec<String>,
    pub timeout_secs: u32,
}

impl Default for ScriptConfig {
    fn default() -> Self {
        Self {
            runtime: Runtime::Python,
            path: String::new(),
            args: Vec::new(),
            timeout_secs: 60,
        }
    }
}

impl ScriptConfig {
    pub fn summary(&self) -> String {
        format!(
            "{} · {}",
            self.runtime.label(),
            file_name(&self.path).unwrap_or("no file")
        )
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        match validate::path_inside(SCRIPTS_DIR, &self.path) {
            Err(e) => issues.push(Issue::new("path", e)),
            Ok(_) if !validate::has_extension(&self.path, self.runtime.extensions()) => {
                issues.push(Issue::new(
                    "path",
                    format!(
                        "a {} script should end in .{}",
                        self.runtime.label(),
                        self.runtime.extensions().join(", .")
                    ),
                ));
            }
            Ok(_) => {}
        }
        if let Err(e) = validate::args(&self.args) {
            issues.push(Issue::new("args", e));
        }
        if let Err(e) = validate::in_range("timeout", self.timeout_secs, 1, MAX_TIMEOUT_SECS) {
            issues.push(Issue::new("timeout", e));
        }
        issues
    }
}

// ----- data -----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    Csv,
    Tsv,
    Json,
    Text,
    Markdown,
    Image,
    Pdf,
    Parquet,
    Binary,
}

impl DataFormat {
    pub const ALL: [DataFormat; 9] = [
        Self::Csv,
        Self::Tsv,
        Self::Json,
        Self::Text,
        Self::Markdown,
        Self::Image,
        Self::Pdf,
        Self::Parquet,
        Self::Binary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Json => "JSON",
            Self::Text => "Text",
            Self::Markdown => "Markdown",
            Self::Image => "Image",
            Self::Pdf => "PDF",
            Self::Parquet => "Parquet",
            Self::Binary => "Binary",
        }
    }

    /// Whether files of this format are text that can flow along a wire.
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Self::Csv | Self::Tsv | Self::Json | Self::Text | Self::Markdown
        )
    }

    /// Extensions this format accepts; empty means any.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Csv => &["csv"],
            Self::Tsv => &["tsv", "tab"],
            Self::Json => &["json", "ndjson", "jsonl"],
            Self::Text => &["txt", "text", "log"],
            Self::Markdown => &["md", "markdown"],
            Self::Image => &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg"],
            Self::Pdf => &["pdf"],
            Self::Parquet => &["parquet"],
            Self::Binary => &[],
        }
    }

    /// The format a file with this extension most likely is; `Binary` when unknown.
    pub fn for_extension(ext: &str) -> Self {
        let ext = ext.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|f| f.extensions().contains(&ext.as_str()))
            .unwrap_or(Self::Binary)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataConfig {
    /// File inside `data/`.
    pub path: String,
    pub format: DataFormat,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            format: DataFormat::Csv,
        }
    }
}

impl DataConfig {
    pub fn summary(&self) -> String {
        format!(
            "{} · {}",
            self.format.label(),
            file_name(&self.path).unwrap_or("no file")
        )
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        match validate::path_inside(DATA_DIR, &self.path) {
            Err(e) => issues.push(Issue::new("path", e)),
            Ok(_) => {
                let allowed = self.format.extensions();
                if !allowed.is_empty() && !validate::has_extension(&self.path, allowed) {
                    issues.push(Issue::new(
                        "format",
                        format!(
                            "{} expects .{}; pick the matching format or Binary",
                            self.format.label(),
                            allowed.join(", .")
                        ),
                    ));
                }
            }
        }
        issues
    }
}

// ----- directory -----

pub const MAX_DIRECTORY_FILES: u32 = 10_000;
/// Largest content budget a directory node may have: the executor's file cap.
pub const MAX_DIRECTORY_BYTES: u32 = 64 * 1024 * 1024;

/// A directory under the working directory, read as one text: a listing of the files
/// that pass the filters, then their contents up to a budget. `.` is the working
/// directory itself, so a graph can read the repository it lives in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectoryConfig {
    /// Relative to the working directory; `.` or empty is the working directory.
    pub path: String,
    /// Glob patterns of files to take, relative to the directory (`src/**/*.rs`,
    /// `*.md`). Empty takes every file.
    pub include: Vec<String>,
    /// Glob patterns of files to skip, on top of hidden entries and symlinks, which are
    /// always skipped.
    pub exclude: Vec<String>,
    /// Whether `.gitignore` files apply, so a repository's build output stays out.
    pub gitignore: bool,
    /// Whether file contents follow the listing.
    pub contents: bool,
    pub max_files: u32,
    /// Budget for the contents, in bytes.
    pub max_bytes: u32,
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        Self {
            path: ".".to_string(),
            include: Vec::new(),
            exclude: Vec::new(),
            gitignore: true,
            contents: true,
            max_files: 200,
            max_bytes: 256 * 1024,
        }
    }
}

impl DirectoryConfig {
    /// The directory as shown: `.` for the working directory.
    pub fn display_path(&self) -> &str {
        match self.path.trim() {
            "" => ".",
            path => path,
        }
    }

    pub fn summary(&self) -> String {
        let what = if self.contents { "contents" } else { "listing" };
        let filter = self
            .include
            .first()
            .map(|p| format!(" · {p}"))
            .unwrap_or_default();
        format!("{}{filter} · {what}", self.display_path())
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if let Err(e) = validate::directory_path(&self.path) {
            issues.push(Issue::new("path", e));
        }
        for (field, patterns) in [("include", &self.include), ("exclude", &self.exclude)] {
            for pattern in patterns {
                if let Err(e) = validate::glob(pattern) {
                    issues.push(Issue::new(field, e));
                }
            }
        }
        if let Err(e) = validate::in_range("max files", self.max_files, 1, MAX_DIRECTORY_FILES) {
            issues.push(Issue::new("max files", e));
        }
        if let Err(e) = validate::in_range("max bytes", self.max_bytes, 1, MAX_DIRECTORY_BYTES) {
            issues.push(Issue::new("max bytes", e));
        }
        issues
    }
}

// ----- write file -----

/// What a write-file node does when its file already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    /// Refuse to touch an existing file. The safe default.
    Create,
    Overwrite,
    Append,
}

impl WriteMode {
    pub const ALL: [WriteMode; 3] = [Self::Create, Self::Overwrite, Self::Append];

    pub fn label(self) -> &'static str {
        match self {
            Self::Create => "Create only",
            Self::Overwrite => "Overwrite",
            Self::Append => "Append",
        }
    }
}

/// A file under the working directory that receives the text arriving on the wire: how
/// an agent's answer lands in the repository as a test, a document or a data file.
/// Missing directories on the way are created. The path is the node's own setting, never
/// the model's, and the app's directories (`scripts/`, `agents/`, …) are off limits.
///
/// With `bundle` on, `path` is a directory and the text is split on `=== path ===`
/// headers, the format a directory node emits, so one answer can write several files
/// under it. Section paths are relative to that directory and checked like any other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// Relative to the working directory: the file, or with `bundle` the directory.
    pub path: String,
    pub mode: WriteMode,
    /// Strip a surrounding Markdown code fence and end the text with a newline, which is
    /// what a model's answer usually needs to become a source file.
    pub unfence: bool,
    /// Write one file per `=== path ===` section under `path`.
    pub bundle: bool,
    /// Pause this node for review before creating directories or writing any file.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            mode: WriteMode::Create,
            unfence: true,
            bundle: false,
            review: false,
        }
    }
}

impl FileConfig {
    /// The directory a bundle writes under, as shown: `.` for the working directory.
    pub fn display_path(&self) -> &str {
        match self.path.trim() {
            "" => ".",
            path => path,
        }
    }

    pub fn summary(&self) -> String {
        let target = if self.bundle {
            format!("{}/ · sections", self.display_path())
        } else {
            file_name(&self.path).unwrap_or("no file").to_string()
        };
        format!("{target} · {}", self.mode.label().to_lowercase())
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        let checked = if self.bundle {
            // The directory may be the working directory itself; each section's path is
            // checked against the app's directories when it is written.
            validate::directory_path(&self.path).and_then(|dir| {
                if dir.as_os_str().is_empty() {
                    Ok(dir)
                } else {
                    validate::writable_path(&self.path)
                }
            })
        } else {
            validate::writable_path(&self.path)
        };
        if let Err(e) = checked {
            issues.push(Issue::new("path", e));
        }
        issues
    }
}

/// The last component of a relative path, for summaries.
fn file_name(path: &str) -> Option<&str> {
    let name = path.trim().rsplit('/').next()?.trim();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_is_valid_and_local() {
        let agent = AgentConfig::default();
        assert!(agent.validate().is_empty(), "{:?}", agent.validate());
        assert!(agent.provider.is_local());
        assert_eq!(agent.summary(), "Ollama · llama3.1");
    }

    #[test]
    fn remote_agent_needs_an_env_var_not_a_key() {
        let mut agent = AgentConfig {
            provider: Provider::Anthropic,
            model: "claude-opus-5-5".into(),
            ..AgentConfig::default()
        };
        let issues = agent.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].field, "auth");

        agent.auth = Auth::Env {
            var: "sk-ant-not-a-name".into(),
        };
        assert_eq!(agent.validate()[0].field, "auth");

        agent.auth = Auth::Env {
            var: "ANTHROPIC_API_KEY".into(),
        };
        assert!(agent.validate().is_empty());
        let json = serde_json::to_string(&agent).unwrap();
        assert!(json.contains(r#""auth":{"kind":"env","var":"ANTHROPIC_API_KEY"}"#));
    }

    #[test]
    fn agent_output_must_match_model_type() {
        let agent = AgentConfig {
            model_type: ModelType::Embedding,
            output: OutputType::Text,
            ..AgentConfig::default()
        };
        assert_eq!(agent.validate()[0].field, "output");
        let agent = AgentConfig {
            temperature: 3.0,
            context_size: 0,
            ..AgentConfig::default()
        };
        let fields: Vec<&str> = agent.validate().iter().map(|i| i.field).collect();
        assert_eq!(fields, ["context size", "temperature"]);
    }

    #[test]
    fn database_paths_and_queries_are_checked() {
        let db = DatabaseConfig {
            path: "../secrets.db".into(),
            query: "DROP TABLE t".into(),
            ..DatabaseConfig::default()
        };
        let fields: Vec<&str> = db.validate().iter().map(|i| i.field).collect();
        assert_eq!(fields, ["path", "query"]);

        let db = DatabaseConfig {
            path: "sales.sqlite".into(),
            input: "sales.csv".into(),
            query: "SELECT * FROM input LIMIT 10".into(),
            ..DatabaseConfig::default()
        };
        assert!(db.validate().is_empty(), "{:?}", db.validate());
        assert_eq!(db.summary(), "SQLite · sales.sqlite");

        let db = DatabaseConfig {
            engine: DbEngine::Postgres,
            url_env: "DATABASE_URL".into(),
            query: "SELECT 1".into(),
            ..DatabaseConfig::default()
        };
        assert!(db.validate().is_empty());
        assert_eq!(db.summary(), "PostgreSQL · DATABASE_URL");

        let db = DatabaseConfig {
            engine: DbEngine::Postgres,
            url_env: "postgres://u:p@h/db".into(),
            ..DatabaseConfig::default()
        };
        assert_eq!(db.validate()[0].field, "connection");

        // The table name is checked on its own: it is wrong whether or not a file was
        // chosen, and the inspector should say so before the node runs.
        for input in ["", "sales.csv"] {
            let db = DatabaseConfig {
                path: "sales.sqlite".into(),
                input: input.into(),
                input_table: "input; DROP TABLE t".into(),
                query: "SELECT 1".into(),
                ..DatabaseConfig::default()
            };
            let issues = db.validate();
            assert!(
                issues.iter().any(|i| i.field == "input table"),
                "input {input:?}: {issues:?}"
            );
        }
    }

    #[test]
    fn writable_database_allows_writes() {
        let db = DatabaseConfig {
            path: "x.db".into(),
            query: "INSERT INTO t VALUES (1)".into(),
            read_only: false,
            ..DatabaseConfig::default()
        };
        assert!(db.validate().is_empty());
    }

    #[test]
    fn scripts_must_match_their_runtime() {
        let script = ScriptConfig {
            path: "etl.py".into(),
            args: vec!["--fast".into()],
            ..ScriptConfig::default()
        };
        assert!(script.validate().is_empty());
        assert_eq!(script.summary(), "Python (uv) · etl.py");

        let script = ScriptConfig {
            runtime: Runtime::Shell,
            path: "etl.py".into(),
            timeout_secs: 0,
            ..ScriptConfig::default()
        };
        let fields: Vec<&str> = script.validate().iter().map(|i| i.field).collect();
        assert_eq!(fields, ["path", "timeout"]);
        assert_eq!(Runtime::for_extension("SH"), Some(Runtime::Shell));
        assert_eq!(Runtime::for_extension("rb"), None);
    }

    #[test]
    fn data_format_follows_extension() {
        assert_eq!(DataFormat::for_extension("csv"), DataFormat::Csv);
        assert_eq!(DataFormat::for_extension("JPG"), DataFormat::Image);
        assert_eq!(DataFormat::for_extension("xyz"), DataFormat::Binary);
        let data = DataConfig {
            path: "img/logo.png".into(),
            format: DataFormat::Csv,
        };
        assert_eq!(data.validate()[0].field, "format");
        let data = DataConfig {
            path: "img/logo.png".into(),
            format: DataFormat::Image,
        };
        assert!(data.validate().is_empty());
        assert_eq!(data.summary(), "Image · logo.png");
    }

    #[test]
    fn node_data_round_trips_with_type_tag() {
        let data = NodeData::Script(ScriptConfig::default());
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.starts_with(r#"{"type":"script""#));
        assert_eq!(serde_json::from_str::<NodeData>(&json).unwrap(), data);
        assert_eq!(data.node_type(), NodeType::Script);
    }

    #[test]
    fn optional_payload_treats_empty_object_as_blank() {
        #[derive(Serialize, Deserialize)]
        struct Holder {
            #[serde(
                default,
                serialize_with = "serialize_opt",
                deserialize_with = "deserialize_opt"
            )]
            data: Option<NodeData>,
        }
        let blank: Holder = serde_json::from_str(r#"{"data":{}}"#).unwrap();
        assert!(blank.data.is_none());
        let missing: Holder = serde_json::from_str(r#"{}"#).unwrap();
        assert!(missing.data.is_none());
        assert_eq!(serde_json::to_string(&blank).unwrap(), r#"{"data":{}}"#);

        let typed: Holder =
            serde_json::from_str(r#"{"data":{"type":"data","path":"a.csv"}}"#).unwrap();
        assert_eq!(
            typed.data,
            Some(NodeData::Data(DataConfig {
                path: "a.csv".into(),
                format: DataFormat::Csv,
            }))
        );
        assert!(serde_json::from_str::<Holder>(r#"{"data":{"type":"rocket"}}"#).is_err());
    }

    #[test]
    fn node_type_labels_and_colors() {
        assert_eq!(NodeType::from_label(" database "), Some(NodeType::Database));
        assert_eq!(NodeType::from_label("Blank"), None);
        for t in NodeType::ALL {
            assert!(crate::doc::hex_to_rgb(t.color()).is_some());
            assert_eq!(t.default_data().node_type(), t);
        }
    }

    #[test]
    fn writable_databases_are_red_and_labeled() {
        let read = NodeData::Database(DatabaseConfig::default());
        assert_eq!(read.color(), NodeType::Database.color());
        assert_eq!(read.label(), "Database");
        let write = NodeData::Database(DatabaseConfig::writable());
        assert_eq!(write.color(), WRITE_COLOR);
        assert_eq!(write.label(), WRITABLE_DATABASE_LABEL);
        assert!(write.summary().ends_with(" · write"), "{}", write.summary());
        assert!(!read.summary().contains("write"));
        assert_eq!(NodeData::Agent(AgentConfig::default()).label(), "Agent");
    }

    #[test]
    fn empty_queries_are_allowed_so_the_wire_can_supply_one() {
        let db = DatabaseConfig {
            path: "x.db".into(),
            ..DatabaseConfig::default()
        };
        assert!(db.validate().is_empty(), "{:?}", db.validate());
        let bad = DatabaseConfig {
            query: "   ".into(),
            ..db.clone()
        };
        assert!(bad.validate().is_empty());
        let bad = DatabaseConfig {
            query: "DELETE FROM t".into(),
            ..db
        };
        assert_eq!(bad.validate()[0].field, "query");
    }

    #[test]
    fn directory_paths_patterns_and_limits_are_checked() {
        let dir = DirectoryConfig::default();
        assert!(dir.validate().is_empty(), "{:?}", dir.validate());
        assert_eq!(dir.summary(), ". · contents");
        let filtered = DirectoryConfig {
            path: "src".into(),
            include: vec!["**/*.rs".into()],
            contents: false,
            ..DirectoryConfig::default()
        };
        assert!(filtered.validate().is_empty());
        assert_eq!(filtered.summary(), "src · **/*.rs · listing");

        let bad = DirectoryConfig {
            path: "../other".into(),
            include: vec!["src/[".into()],
            exclude: vec!["".into()],
            max_files: 0,
            max_bytes: MAX_DIRECTORY_BYTES + 1,
            ..DirectoryConfig::default()
        };
        let fields: Vec<&str> = bad.validate().iter().map(|i| i.field).collect();
        assert_eq!(
            fields,
            ["path", "include", "exclude", "max files", "max bytes"]
        );
        let json = serde_json::to_string(&NodeData::Directory(filtered.clone())).unwrap();
        assert!(json.starts_with(r#"{"type":"directory""#), "{json}");
        assert_eq!(
            serde_json::from_str::<NodeData>(&json).unwrap(),
            NodeData::Directory(filtered)
        );
    }

    #[test]
    fn file_nodes_are_red_and_keep_out_of_app_directories() {
        let blank = FileConfig::default();
        assert_eq!(blank.mode, WriteMode::Create);
        assert!(blank.unfence);
        assert!(!blank.bundle);
        assert_eq!(blank.validate()[0].field, "path");
        assert_eq!(blank.summary(), "no file · create only");

        // A bundle may target the working directory itself, but not an app directory.
        let bundle = FileConfig {
            bundle: true,
            ..FileConfig::default()
        };
        assert!(bundle.validate().is_empty(), "{:?}", bundle.validate());
        assert_eq!(bundle.summary(), "./ · sections · create only");
        let under = FileConfig {
            path: "tests/generated".into(),
            bundle: true,
            mode: WriteMode::Overwrite,
            ..FileConfig::default()
        };
        assert!(under.validate().is_empty());
        assert_eq!(under.summary(), "tests/generated/ · sections · overwrite");
        let bad = FileConfig {
            path: "agents".into(),
            bundle: true,
            ..FileConfig::default()
        };
        assert_eq!(bad.validate()[0].field, "path");

        let file = FileConfig {
            path: "tests/generated/math.rs".into(),
            mode: WriteMode::Overwrite,
            ..FileConfig::default()
        };
        assert!(file.validate().is_empty(), "{:?}", file.validate());
        assert_eq!(file.summary(), "math.rs · overwrite");
        let data = NodeData::File(file.clone());
        assert_eq!(data.color(), WRITE_COLOR);
        assert_eq!(data.label(), "Write file");
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.starts_with(r#"{"type":"file""#), "{json}");
        assert_eq!(serde_json::from_str::<NodeData>(&json).unwrap(), data);

        for path in [
            "scripts/run.sh",
            "Agents/x/agent.json",
            "graphs/g.json",
            "nodes/k.json",
            "prompts/p.md",
            "databases/d.json",
            "../out.txt",
            ".hidden",
        ] {
            let issues = FileConfig {
                path: path.into(),
                ..FileConfig::default()
            }
            .validate();
            assert_eq!(issues.len(), 1, "{path}: {issues:?}");
            assert_eq!(issues[0].field, "path");
        }
        assert!(
            FileConfig {
                path: "data/out.csv".into(),
                ..FileConfig::default()
            }
            .validate()
            .is_empty()
        );
    }
}
