//! Talking to model providers.
//!
//! [`build_request`] turns an [`AgentConfig`] plus the text going in into one HTTP request,
//! [`parse_reply`] turns the provider's answer into output bytes, and [`generate`] runs the
//! two around an [`Http`] client. The client is a trait so tests can use a fake; the real
//! one is [`UreqHttp`].
//!
//! [`generate_streaming`] is the same call for generative models with the reply streamed:
//! the request asks for a stream, the body is read as it arrives, and every piece of text
//! goes to a callback that may stop the request. Servers answer with server-sent events
//! (`data: {…}` lines) or, for Ollama, one JSON object per line; both are handled by the
//! same line parser. A server that ignores the request and answers in one piece still
//! works: the whole body goes through [`parse_reply`].
//!
//! Protocols:
//! - OpenAI-compatible servers (OpenAI, OpenRouter, Mistral, LM Studio, llama.cpp, custom)
//!   share `/chat/completions`, `/embeddings` and `/images/generations`. JSON output is
//!   requested with `response_format`; LM Studio only accepts its `json_schema` form.
//! - JSON output also adds an instruction to the system prompt on every provider: OpenAI's
//!   `json_object` mode refuses requests that never mention JSON, and Ollama pads replies
//!   with whitespace without one.
//! - Ollama is called on its native `/api/chat` and `/api/embed`, which carry the
//!   configured context size as `num_ctx`.
//! - Anthropic is called on API version 2026-10-21. That version rejects sampling
//!   parameters, so the configured temperature is not sent there.
//! - Google uses `generateContent` and `embedContent` with the key in a header.
//!
//! The API key only ever appears in a request header; it is never logged or stored.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::node_data::{AgentConfig, Api, ModelType, OutputType, Provider};

pub const ANTHROPIC_VERSION: &str = "2026-10-21";
const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that accept server-side refusal fallbacks; enabled for them by default.
const ANTHROPIC_FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-mythos-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];
pub const MAX_OUTPUT_TOKENS: u32 = 16_000;
/// Replies larger than this are cut off; images are the only big ones.
pub const MAX_REPLY_BYTES: u64 = 64 * 1024 * 1024;
const JSON_INSTRUCTION: &str = "Respond with a single JSON value and nothing else.";
/// The error [`generate_streaming`] returns when its callback stopped the request.
pub const STOPPED: &str = "the request was stopped";
/// How much of a streamed body is read at a time.
const STREAM_CHUNK: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// One JSON POST. Implementations return the status and body for every HTTP status; only
/// transport failures are errors.
pub trait Http: Send + Sync {
    fn post_json(&self, request: &Request) -> Result<Reply, String>;

    /// One JSON GET for listings such as the available models. The default refuses;
    /// fakes that only answer generation requests need no listing support.
    fn get_json(&self, request: &Request) -> Result<Reply, String> {
        Err(format!("{} cannot be listed with this client", request.url))
    }

    /// The same POST with the body handed over in pieces as it arrives, each with the
    /// status, for streamed replies. `sink` returns false to stop reading; the status is
    /// returned either way. The default delivers the whole body at once.
    fn post_json_streaming(
        &self,
        request: &Request,
        sink: &mut dyn FnMut(u16, &[u8]) -> bool,
    ) -> Result<u16, String> {
        let reply = self.post_json(request)?;
        sink(reply.status, reply.body.as_bytes());
        Ok(reply.status)
    }
}

pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // Only the configured endpoint is trusted with the prompt and API key.
            // Redirect policy for Authorization does not cover provider-specific keys.
            .max_redirects(0)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Http for UreqHttp {
    fn get_json(&self, request: &Request) -> Result<Reply, String> {
        let mut req = self.agent.get(&request.url);
        for (key, value) in &request.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        let mut response = req
            .call()
            .map_err(|e| format!("request to {} failed: {e}", request.url))?;
        let status = response.status().as_u16();
        reject_redirect(status)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(|e| format!("reading the reply from {} failed: {e}", request.url))?;
        Ok(Reply { status, body })
    }

    fn post_json(&self, request: &Request) -> Result<Reply, String> {
        let mut req = self.agent.post(&request.url);
        for (key, value) in &request.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        let mut response = req
            .send_json(&request.body)
            .map_err(|e| format!("request to {} failed: {e}", request.url))?;
        let status = response.status().as_u16();
        reject_redirect(status)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(|e| format!("reading the reply from {} failed: {e}", request.url))?;
        Ok(Reply { status, body })
    }

    fn post_json_streaming(
        &self,
        request: &Request,
        sink: &mut dyn FnMut(u16, &[u8]) -> bool,
    ) -> Result<u16, String> {
        let mut req = self.agent.post(&request.url);
        for (key, value) in &request.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        let mut response = req
            .send_json(&request.body)
            .map_err(|e| format!("request to {} failed: {e}", request.url))?;
        let status = response.status().as_u16();
        reject_redirect(status)?;
        let mut reader = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .reader();
        let mut buf = vec![0u8; STREAM_CHUNK];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !sink(status, &buf[..n]) {
                        break;
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "reading the reply from {} failed: {e}",
                        request.url
                    ));
                }
            }
        }
        Ok(status)
    }
}

fn reject_redirect(status: u16) -> Result<(), String> {
    if (300..400).contains(&status) {
        Err(format!(
            "provider returned HTTP {status}; redirects are refused to protect API keys and input. Configure the final endpoint directly"
        ))
    } else {
        Ok(())
    }
}

/// Send `input` through the agent and return the output bytes in the agent's output type.
pub fn generate(
    http: &dyn Http,
    config: &AgentConfig,
    key: Option<&str>,
    input: &str,
) -> Result<Vec<u8>, String> {
    let request = build_request(config, key, input)?;
    let reply = http.post_json(&request)?;
    parse_reply(config, &reply)
}

/// What [`generate_streaming`] produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Streamed {
    /// The output in the agent's output type, as [`generate`] would return it.
    pub bytes: Vec<u8>,
    /// The model id the server says answered, when the reply carries one. Servers may
    /// substitute a loaded model for an unknown name; the executor logs the difference.
    pub model: Option<String>,
}

/// [`generate`] with the reply streamed for generative models: `on_text` gets each piece
/// of text as it arrives and returns false to stop, which ends in [`STOPPED`]. The bytes
/// are the same ones [`generate`] would return, JSON validated and pretty-printed at the
/// end. Embeddings and images are not streams and go through [`generate`].
pub fn generate_streaming(
    http: &dyn Http,
    config: &AgentConfig,
    key: Option<&str>,
    input: &str,
    on_text: &mut dyn FnMut(&str) -> bool,
) -> Result<Streamed, String> {
    if config.model_type != ModelType::Generative {
        return generate(http, config, key, input).map(|bytes| Streamed { bytes, model: None });
    }
    let mut request = build_request(config, key, input)?;
    let api = config.provider.api();
    ask_for_stream(&mut request, api);
    let mut stream = Stream::new(config, on_text);
    let mut body: Vec<u8> = Vec::new();
    let mut body_overflowed = false;
    let status = http.post_json_streaming(&request, &mut |status, chunk| {
        // The body is kept only for a non-streamed reply to be read back, so it stops
        // growing at the cap; the stream itself keeps parsing the pieces it receives.
        let room = (MAX_REPLY_BYTES as usize).saturating_sub(body.len());
        if chunk.len() > room {
            body_overflowed = true;
            return false;
        } else {
            body.extend_from_slice(chunk);
        }
        // Error bodies are plain JSON, kept whole for the message below.
        status >= 400 || stream.feed(chunk)
    })?;
    if body_overflowed {
        return Err(format!(
            "the reply from {} is larger than {} bytes",
            request.url, MAX_REPLY_BYTES
        ));
    }
    if status < 400 && !stream.stopped && stream.error.is_none() {
        stream.finish();
    }
    if stream.stopped {
        return Err(STOPPED.into());
    }
    if let Some(error) = stream.error {
        return Err(error);
    }
    if status >= 400 || stream.text.is_empty() {
        // Not a stream after all (or nothing in it): read it as a single reply.
        let body = String::from_utf8_lossy(&body).into_owned();
        let model = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| reply_model(api, &value));
        let bytes = parse_reply(config, &Reply { status, body })?;
        return Ok(Streamed { bytes, model });
    }
    Ok(Streamed {
        bytes: finish_text(config, &stream.text)?,
        model: stream.model,
    })
}

/// The model id a reply or stream event names, in each API's shape.
fn reply_model(api: Api, value: &Value) -> Option<String> {
    let model = match api {
        Api::OpenAi | Api::Ollama => value["model"].as_str(),
        Api::Anthropic => value["model"]
            .as_str()
            .or_else(|| value["message"]["model"].as_str()),
        Api::Google => value["modelVersion"].as_str(),
    };
    model.map(str::to_string).filter(|m| !m.is_empty())
}

/// Turn a built request into its streaming form.
fn ask_for_stream(request: &mut Request, api: Api) {
    match api {
        Api::OpenAi | Api::Ollama | Api::Anthropic => request.body["stream"] = json!(true),
        Api::Google => {
            if let Some(base) = request.url.strip_suffix(":generateContent") {
                request.url = format!("{base}:streamGenerateContent?alt=sse");
            }
        }
    }
}

/// Reads a streamed body line by line, pulling the text out of each event.
struct Stream<'a> {
    api: Api,
    provider: &'static str,
    /// Bytes after the last complete line.
    pending: Vec<u8>,
    /// SSE data fields for the current event, joined with newlines.
    event_data: String,
    seen_sse: bool,
    seen_record: bool,
    /// A provider ignored the stream request and sent one (possibly pretty) JSON body.
    whole_reply: bool,
    text: String,
    /// The model named by the first event that names one.
    model: Option<String>,
    error: Option<String>,
    stopped: bool,
    on_text: &'a mut dyn FnMut(&str) -> bool,
}

impl<'a> Stream<'a> {
    fn new(config: &AgentConfig, on_text: &'a mut dyn FnMut(&str) -> bool) -> Self {
        Self {
            api: config.provider.api(),
            provider: config.provider.label(),
            pending: Vec::new(),
            event_data: String::new(),
            seen_sse: false,
            seen_record: false,
            whole_reply: false,
            text: String::new(),
            model: None,
            error: None,
            stopped: false,
            on_text,
        }
    }

    /// Take more bytes; false means stop reading (the callback said so, or an error event
    /// arrived).
    fn feed(&mut self, chunk: &[u8]) -> bool {
        // Scan each incoming byte once. Repeatedly searching an ever-growing pending
        // line makes a response without newlines quadratic in its length.
        for part in chunk.split_inclusive(|b| *b == b'\n') {
            self.pending.extend_from_slice(part);
            if part.ends_with(b"\n") && !self.pending_line() {
                return false;
            }
        }
        true
    }

    fn pending_line(&mut self) -> bool {
        let bytes = std::mem::take(&mut self.pending);
        let Ok(line) = std::str::from_utf8(&bytes) else {
            self.error = Some(format!("{} stream contains invalid UTF-8", self.provider));
            return false;
        };
        self.line(line.trim_end_matches(['\r', '\n']))
    }

    /// EOF may terminate the last NDJSON record or SSE data line. Process it before
    /// deciding whether this is a successful response; it may carry a late error.
    fn finish(&mut self) {
        if !self.pending.is_empty() && !self.pending_line() {
            return;
        }
        self.dispatch_sse();
    }

    /// SSE dispatches on a blank line and joins multiple data fields. Native Ollama
    /// replies use one JSON object per line; whole replies are parsed by the fallback.
    fn line(&mut self, line: &str) -> bool {
        if let Some(data) = line.strip_prefix("data:") {
            self.seen_sse = true;
            self.event_data
                .push_str(data.strip_prefix(' ').unwrap_or(data));
            self.event_data.push('\n');
            return true;
        }
        if line.is_empty() {
            return self.dispatch_sse();
        }
        if self.seen_sse || self.api != Api::Ollama || self.whole_reply {
            return true;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.value(&value),
            Err(_) if !self.seen_record => {
                self.whole_reply = true;
                true
            }
            Err(_) => self.malformed(),
        }
    }

    fn dispatch_sse(&mut self) -> bool {
        if self.event_data.is_empty() {
            return true;
        }
        let data = std::mem::take(&mut self.event_data);
        let payload = data.trim();
        if payload == "[DONE]" {
            return true;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return self.malformed();
        };
        self.value(&value)
    }

    fn malformed(&mut self) -> bool {
        self.error = Some(format!("{} returned malformed stream JSON", self.provider));
        false
    }

    fn value(&mut self, value: &Value) -> bool {
        self.seen_record = true;
        if self.model.is_none() {
            self.model = reply_model(self.api, value);
        }
        match self.event(value) {
            Err(e) => {
                self.error = Some(e);
                false
            }
            Ok(None) => true,
            Ok(Some(text)) => {
                self.text.push_str(&text);
                if (self.on_text)(&text) {
                    true
                } else {
                    self.stopped = true;
                    false
                }
            }
        }
    }

    /// The text carried by one event, if any, in each API's shape.
    fn event(&self, value: &Value) -> Result<Option<String>, String> {
        if let Some(message) = error_message(value) {
            return Err(format!("{}: {message}", self.provider));
        }
        let text = match self.api {
            Api::OpenAi => value["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string),
            Api::Ollama => value["message"]["content"].as_str().map(str::to_string),
            Api::Anthropic => match value["type"].as_str() {
                Some("content_block_delta") => value["delta"]["text"].as_str().map(str::to_string),
                Some("message_delta")
                    if value["delta"]["stop_reason"].as_str() == Some("refusal") =>
                {
                    return Err(refusal(&value["delta"]["stop_details"]));
                }
                _ => None,
            },
            Api::Google => Some(joined_text(
                value["candidates"][0]["content"]["parts"].as_array(),
                |part| part["text"].as_str(),
            )),
        };
        Ok(text.filter(|t| !t.is_empty()))
    }
}

fn refusal(details: &Value) -> String {
    format!(
        "Anthropic declined the request ({}): {}",
        details["category"].as_str().unwrap_or("no category"),
        details["explanation"].as_str().unwrap_or("no explanation")
    )
}

/// The system prompt (if any) and the user text. With no input the prompt is the user text;
/// with no prompt the input is sent alone.
fn prompt_parts(config: &AgentConfig, input: &str) -> Result<(Option<String>, String), String> {
    let prompt = config.prompt.trim();
    let input = input.trim();
    match (prompt.is_empty(), input.is_empty()) {
        (true, true) => Err("nothing to send: the agent has no prompt and no input".into()),
        (false, true) => Ok((None, prompt.to_string())),
        (true, false) => Ok((None, input.to_string())),
        (false, false) => Ok((Some(prompt.to_string()), input.to_string())),
    }
}

/// The single text embeddings and image prompts work on: the input, else the prompt.
fn single_text(config: &AgentConfig, input: &str) -> Result<String, String> {
    let (system, user) = prompt_parts(config, input)?;
    Ok(match system {
        Some(system) => format!("{system}\n\n{user}"),
        None => user,
    })
}

/// The configured temperature as a tidy f64 (`0.2`, not `0.20000000298`).
fn temperature(config: &AgentConfig) -> f64 {
    (f64::from(config.temperature) * 1000.0).round() / 1000.0
}

fn with_json_instruction(system: Option<String>, wants_json: bool) -> Option<String> {
    if !wants_json {
        return system;
    }
    Some(match system {
        Some(system) => format!("{system}\n\n{JSON_INSTRUCTION}"),
        None => JSON_INSTRUCTION.to_string(),
    })
}

/// How an OpenAI-compatible server is asked for JSON. LM Studio rejects `json_object` and
/// only takes `json_schema`; an empty schema accepts any JSON value.
fn json_response_format(provider: Provider) -> Value {
    match provider {
        Provider::LmStudio => json!({
            "type": "json_schema",
            "json_schema": {"name": "output", "schema": {}},
        }),
        _ => json!({"type": "json_object"}),
    }
}

fn chat_messages(system: Option<&str>, user: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": user}));
    Value::Array(messages)
}

fn unsupported(config: &AgentConfig) -> String {
    format!(
        "{} {} models are not supported",
        config.provider.label(),
        config.model_type.label().to_lowercase()
    )
}

pub fn build_request(
    config: &AgentConfig,
    key: Option<&str>,
    input: &str,
) -> Result<Request, String> {
    // Helpers such as write_sql call this without running a graph node's validation.
    // Check every URL input at the common boundary before adding secrets or URL paths.
    let endpoint = crate::validate::endpoint_url(config.endpoint_url())?;
    let base = endpoint.as_str().trim_end_matches('/').to_string();
    crate::validate::model_id(&config.model)?;
    let model = config.model.trim();
    let wants_json = config.output == OutputType::Json;
    let mut headers: Vec<(String, String)> = Vec::new();

    let (url, body) = match config.provider.api() {
        Api::OpenAi => {
            if let Some(key) = key {
                headers.push(("authorization".into(), format!("Bearer {key}")));
            }
            match config.model_type {
                ModelType::Generative => {
                    let (system, user) = prompt_parts(config, input)?;
                    let system = with_json_instruction(system, wants_json);
                    let mut body = json!({
                        "model": model,
                        "messages": chat_messages(system.as_deref(), &user),
                        "temperature": temperature(config),
                    });
                    if wants_json {
                        body["response_format"] = json_response_format(config.provider);
                    }
                    (format!("{base}/chat/completions"), body)
                }
                ModelType::Embedding => (
                    format!("{base}/embeddings"),
                    json!({"model": model, "input": single_text(config, input)?}),
                ),
                ModelType::Image => {
                    let mut body = json!({
                        "model": model,
                        "prompt": single_text(config, input)?,
                        "n": 1,
                    });
                    // DALL·E returns URLs unless asked; newer image models always return
                    // base64 and reject the parameter.
                    if model.starts_with("dall-e") {
                        body["response_format"] = json!("b64_json");
                    }
                    (format!("{base}/images/generations"), body)
                }
            }
        }
        Api::Ollama => match config.model_type {
            ModelType::Generative => {
                let (system, user) = prompt_parts(config, input)?;
                let system = with_json_instruction(system, wants_json);
                let mut body = json!({
                    "model": model,
                    "messages": chat_messages(system.as_deref(), &user),
                    "stream": false,
                    "options": {
                        "temperature": temperature(config),
                        "num_ctx": config.context_size,
                    },
                });
                if wants_json {
                    body["format"] = json!("json");
                }
                (format!("{base}/api/chat"), body)
            }
            ModelType::Embedding => (
                format!("{base}/api/embed"),
                json!({"model": model, "input": single_text(config, input)?}),
            ),
            ModelType::Image => return Err(unsupported(config)),
        },
        Api::Anthropic => {
            let key = key.ok_or("Anthropic needs an API key")?;
            headers.push(("x-api-key".into(), key.to_string()));
            headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
            if config.model_type != ModelType::Generative {
                return Err(unsupported(config));
            }
            let (system, user) = prompt_parts(config, input)?;
            let system = with_json_instruction(system, wants_json);
            let mut body = json!({
                "model": model,
                "max_tokens": MAX_OUTPUT_TOKENS,
                "messages": [{"role": "user", "content": user}],
            });
            if let Some(system) = system {
                body["system"] = json!(system);
            }
            if ANTHROPIC_FALLBACK_MODELS.contains(&model) {
                headers.push(("anthropic-beta".into(), ANTHROPIC_FALLBACK_BETA.into()));
                body["fallbacks"] = json!("default");
            }
            (format!("{base}/messages"), body)
        }
        Api::Google => {
            let key = key.ok_or("Google needs an API key")?;
            headers.push(("x-goog-api-key".into(), key.to_string()));
            let model = model.strip_prefix("models/").unwrap_or(model);
            match config.model_type {
                ModelType::Generative => {
                    let (system, user) = prompt_parts(config, input)?;
                    let system = with_json_instruction(system, wants_json);
                    let mut body = json!({
                        "contents": [{"role": "user", "parts": [{"text": user}]}],
                        "generationConfig": {"temperature": temperature(config)},
                    });
                    if let Some(system) = system {
                        body["system_instruction"] = json!({"parts": [{"text": system}]});
                    }
                    if wants_json {
                        body["generationConfig"]["responseMimeType"] = json!("application/json");
                    }
                    (format!("{base}/models/{model}:generateContent"), body)
                }
                ModelType::Embedding => (
                    format!("{base}/models/{model}:embedContent"),
                    json!({"content": {"parts": [{"text": single_text(config, input)?}]}}),
                ),
                ModelType::Image => return Err(unsupported(config)),
            }
        }
    };
    Ok(Request { url, headers, body })
}

pub fn parse_reply(config: &AgentConfig, reply: &Reply) -> Result<Vec<u8>, String> {
    let provider = config.provider.label();
    let value: Value = match serde_json::from_str(&reply.body) {
        Ok(value) => value,
        Err(_) if reply.status >= 400 => {
            return Err(format!(
                "{provider} returned HTTP {}: {}",
                reply.status,
                excerpt(&reply.body)
            ));
        }
        Err(e) => {
            return Err(format!(
                "{provider} reply is not JSON ({e}): {}",
                excerpt(&reply.body)
            ));
        }
    };
    if reply.status >= 400 {
        let message = error_message(&value).unwrap_or_else(|| excerpt(&reply.body));
        return Err(format!(
            "{provider} returned HTTP {}: {message}",
            reply.status
        ));
    }

    match (config.provider.api(), config.model_type) {
        (Api::OpenAi, ModelType::Generative) => {
            let text = value["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| format!("{provider} reply has no choices[0].message.content"))?;
            finish_text(config, text)
        }
        (Api::OpenAi, ModelType::Embedding) => floats(&value["data"][0]["embedding"], provider),
        (Api::OpenAi, ModelType::Image) => {
            let data = &value["data"][0];
            let Some(b64) = data["b64_json"].as_str() else {
                return Err(if data["url"].is_string() {
                    format!("{provider} returned an image URL instead of image data")
                } else {
                    format!("{provider} reply has no data[0].b64_json")
                });
            };
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("{provider} image is not valid base64: {e}"))
        }
        (Api::Ollama, ModelType::Generative) => {
            let text = value["message"]["content"]
                .as_str()
                .ok_or_else(|| format!("{provider} reply has no message.content"))?;
            finish_text(config, text)
        }
        (Api::Ollama, ModelType::Embedding) => floats(&value["embeddings"][0], provider),
        (Api::Anthropic, ModelType::Generative) => {
            if value["stop_reason"].as_str() == Some("refusal") {
                return Err(refusal(&value["stop_details"]));
            }
            let text = joined_text(value["content"].as_array(), |block| {
                (block["type"].as_str() == Some("text")).then(|| block["text"].as_str())?
            });
            if text.is_empty() {
                return Err(format!(
                    "{provider} reply has no text blocks (stop_reason {})",
                    value["stop_reason"].as_str().unwrap_or("unknown")
                ));
            }
            finish_text(config, &text)
        }
        (Api::Google, ModelType::Generative) => {
            let text = joined_text(
                value["candidates"][0]["content"]["parts"].as_array(),
                |part| part["text"].as_str(),
            );
            if text.is_empty() {
                let reason = value["promptFeedback"]["blockReason"]
                    .as_str()
                    .or_else(|| value["candidates"][0]["finishReason"].as_str())
                    .unwrap_or("no candidates");
                return Err(format!("{provider} returned no text: {reason}"));
            }
            finish_text(config, &text)
        }
        (Api::Google, ModelType::Embedding) => floats(&value["embedding"]["values"], provider),
        _ => Err(unsupported(config)),
    }
}

/// Text output in the configured shape: raw text, or validated and pretty-printed JSON.
fn finish_text(config: &AgentConfig, text: &str) -> Result<Vec<u8>, String> {
    match config.output {
        OutputType::Json => {
            let value = extract_json(text)?;
            serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())
        }
        _ => Ok(text.as_bytes().to_vec()),
    }
}

/// Pull one JSON value out of model text: the whole thing, a fenced block, or the span
/// between the first opening and last closing bracket.
pub fn extract_json(text: &str) -> Result<Value, String> {
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    if let Some(inner) = text.strip_prefix("```") {
        let inner = inner.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        if let Some(inner) = inner.trim().strip_suffix("```")
            && let Ok(value) = serde_json::from_str::<Value>(inner.trim())
        {
            return Ok(value);
        }
    }
    let start = text.find(['{', '[']);
    let end = text.rfind(['}', ']']);
    if let (Some(start), Some(end)) = (start, end)
        && start < end
        && let Ok(value) = serde_json::from_str::<Value>(&text[start..=end])
    {
        return Ok(value);
    }
    Err(format!(
        "the model did not return valid JSON: {}",
        excerpt(text)
    ))
}

fn floats(value: &Value, provider: &str) -> Result<Vec<u8>, String> {
    let numbers = value
        .as_array()
        .filter(|a| !a.is_empty() && a.iter().all(Value::is_number))
        .ok_or_else(|| format!("{provider} reply has no embedding vector"))?;
    serde_json::to_vec(numbers).map_err(|e| e.to_string())
}

fn joined_text<'a>(
    blocks: Option<&'a Vec<Value>>,
    pick: impl Fn(&'a Value) -> Option<&'a str>,
) -> String {
    blocks
        .map(|blocks| blocks.iter().filter_map(pick).collect::<Vec<_>>().join(""))
        .unwrap_or_default()
}

/// The provider's own error text, in the shapes OpenAI, Anthropic, Google and Ollama use.
fn error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(text) = error.as_str() {
        return Some(text.to_string());
    }
    error["message"].as_str().map(str::to_string)
}

/// The start of a body for error messages, with no risk of dumping a huge reply.
fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    let text = text.trim();
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

/// Whether this provider sends the configured temperature at all.
pub fn sends_temperature(provider: Provider) -> bool {
    provider.api() != Api::Anthropic
}

// ----- model listings -----

/// At most this many ids are offered from one listing.
const MAX_LISTED_MODELS: usize = 2000;

/// Where a provider lists its models: a GET under the same endpoint and key rules as
/// generation requests. The body is unused.
pub fn models_request(config: &AgentConfig, key: Option<&str>) -> Result<Request, String> {
    let endpoint = crate::validate::endpoint_url(config.endpoint_url())?;
    let base = endpoint.as_str().trim_end_matches('/').to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    let url = match config.provider.api() {
        Api::OpenAi => {
            if let Some(key) = key {
                headers.push(("authorization".into(), format!("Bearer {key}")));
            }
            format!("{base}/models")
        }
        Api::Ollama => format!("{base}/api/tags"),
        Api::Anthropic => {
            let key = key.ok_or("Anthropic needs an API key")?;
            headers.push(("x-api-key".into(), key.to_string()));
            headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
            format!("{base}/models?limit=1000")
        }
        Api::Google => {
            let key = key.ok_or("Google needs an API key")?;
            headers.push(("x-goog-api-key".into(), key.to_string()));
            format!("{base}/models?pageSize=1000")
        }
    };
    Ok(Request {
        url,
        headers,
        body: Value::Null,
    })
}

/// Model ids from a listing reply, sorted and deduplicated. Ids that could never be
/// used in a request are dropped rather than offered.
pub fn parse_models(config: &AgentConfig, reply: &Reply) -> Result<Vec<String>, String> {
    let provider = config.provider.label();
    let value: Value = match serde_json::from_str(&reply.body) {
        Ok(value) => value,
        Err(_) if reply.status >= 400 => {
            return Err(format!(
                "{provider} returned HTTP {}: {}",
                reply.status,
                excerpt(&reply.body)
            ));
        }
        Err(e) => {
            return Err(format!(
                "{provider} sent a reply that is not JSON ({e}): {}",
                excerpt(&reply.body)
            ));
        }
    };
    if reply.status >= 400 {
        let message = error_message(&value).unwrap_or_else(|| excerpt(&reply.body));
        return Err(format!(
            "{provider} returned HTTP {}: {message}",
            reply.status
        ));
    }
    let (list, field) = match config.provider.api() {
        Api::Ollama | Api::Google => (value.get("models"), "name"),
        Api::OpenAi | Api::Anthropic => (value.get("data"), "id"),
    };
    let Some(items) = list.and_then(Value::as_array) else {
        return Err(format!("{provider} listed no models in its reply"));
    };
    let mut names: Vec<String> = items
        .iter()
        .filter_map(|item| item.get(field).and_then(Value::as_str))
        .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
        .filter(|name| crate::validate::model_id(name).is_ok())
        .take(MAX_LISTED_MODELS)
        .collect();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

/// Ask the provider which models it offers.
pub fn list_models(
    http: &dyn Http,
    config: &AgentConfig,
    key: Option<&str>,
) -> Result<Vec<String>, String> {
    let request = models_request(config, key)?;
    let reply = http.get_json(&request)?;
    parse_models(config, &reply)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::node_data::Auth;
    use std::sync::Mutex;

    /// Records the request and answers with a canned reply.
    pub struct FakeHttp {
        pub reply: Reply,
        pub seen: Mutex<Vec<Request>>,
    }

    impl FakeHttp {
        pub fn new(status: u16, body: &str) -> Self {
            Self {
                reply: Reply {
                    status,
                    body: body.to_string(),
                },
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Http for FakeHttp {
        fn post_json(&self, request: &Request) -> Result<Reply, String> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(self.reply.clone())
        }
    }

    fn agent(provider: Provider, model_type: ModelType, output: OutputType) -> AgentConfig {
        AgentConfig {
            provider,
            model_type,
            model: "m".into(),
            auth: Auth::Env { var: "KEY".into() },
            prompt: "Be brief.".into(),
            temperature: 0.2,
            context_size: 4096,
            output,
            ..AgentConfig::default()
        }
    }

    #[test]
    fn openai_chat_request_shape() {
        let config = agent(
            Provider::OpenRouter,
            ModelType::Generative,
            OutputType::Json,
        );
        let req = build_request(&config, Some("sk-test"), "hello").unwrap();
        assert_eq!(req.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(
            req.headers,
            vec![("authorization".to_string(), "Bearer sk-test".to_string())]
        );
        assert_eq!(req.body["model"], "m");
        assert_eq!(req.body["temperature"], 0.2);
        assert_eq!(req.body["messages"][0]["role"], "system");
        assert_eq!(
            req.body["messages"][0]["content"],
            format!("Be brief.\n\n{JSON_INSTRUCTION}")
        );
        assert_eq!(req.body["messages"][1]["content"], "hello");
        assert_eq!(req.body["response_format"]["type"], "json_object");
    }

    #[test]
    fn lm_studio_asks_for_json_with_a_schema() {
        let config = AgentConfig {
            auth: Auth::None,
            ..agent(Provider::LmStudio, ModelType::Generative, OutputType::Json)
        };
        let req = build_request(&config, None, "hi").unwrap();
        let format = &req.body["response_format"];
        assert_eq!(format["type"], "json_schema");
        assert!(format["json_schema"]["name"].is_string());
        assert_eq!(format["json_schema"]["schema"], json!({}));
        assert!(
            req.body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .ends_with(JSON_INSTRUCTION)
        );

        let text = AgentConfig {
            output: OutputType::Text,
            ..config
        };
        let req = build_request(&text, None, "hi").unwrap();
        assert!(req.body.get("response_format").is_none());
        assert_eq!(req.body["messages"][0]["content"], "Be brief.");
    }

    #[test]
    fn local_servers_need_no_key_and_use_their_ports() {
        for (provider, url) in [
            (
                Provider::LmStudio,
                "http://localhost:1234/v1/chat/completions",
            ),
            (
                Provider::LlamaCpp,
                "http://localhost:8080/v1/chat/completions",
            ),
        ] {
            let config = AgentConfig {
                auth: Auth::None,
                ..agent(provider, ModelType::Generative, OutputType::Text)
            };
            let req = build_request(&config, None, "hi").unwrap();
            assert_eq!(req.url, url);
            assert!(req.headers.is_empty());
        }
        let config = AgentConfig {
            endpoint: "http://127.0.0.1:9999/v1/".into(),
            ..agent(Provider::Custom, ModelType::Generative, OutputType::Text)
        };
        let req = build_request(&config, None, "hi").unwrap();
        assert_eq!(req.url, "http://127.0.0.1:9999/v1/chat/completions");
    }

    #[test]
    fn prompt_alone_becomes_the_user_message() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let req = build_request(&config, Some("k"), "").unwrap();
        let messages = req.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Be brief.");

        let empty = AgentConfig {
            prompt: String::new(),
            ..config
        };
        assert!(build_request(&empty, Some("k"), "  ").is_err());
    }

    #[test]
    fn ollama_uses_native_api_with_context_size() {
        let config = AgentConfig {
            auth: Auth::None,
            ..agent(Provider::Ollama, ModelType::Generative, OutputType::Json)
        };
        let req = build_request(&config, None, "hi").unwrap();
        assert_eq!(req.url, "http://localhost:11434/api/chat");
        assert_eq!(req.body["stream"], false);
        assert_eq!(req.body["options"]["num_ctx"], 4096);
        assert_eq!(req.body["format"], "json");
        assert_eq!(
            req.body["messages"][0]["content"],
            format!("Be brief.\n\n{JSON_INSTRUCTION}")
        );

        let embed = AgentConfig {
            model_type: ModelType::Embedding,
            output: OutputType::Embedding,
            ..config
        };
        let req = build_request(&embed, None, "hi").unwrap();
        assert_eq!(req.url, "http://localhost:11434/api/embed");
        assert_eq!(req.body["input"], "Be brief.\n\nhi");
    }

    #[test]
    fn anthropic_sends_version_header_and_no_temperature() {
        let config = AgentConfig {
            model: "claude-opus-5-5".into(),
            ..agent(Provider::Anthropic, ModelType::Generative, OutputType::Json)
        };
        let req = build_request(&config, Some("sk-ant"), "hi").unwrap();
        assert_eq!(req.url, "https://api.anthropic.com/v1/messages");
        assert!(req.headers.contains(&("x-api-key".into(), "sk-ant".into())));
        assert!(
            req.headers
                .contains(&("anthropic-version".into(), ANTHROPIC_VERSION.into()))
        );
        assert!(
            req.headers
                .contains(&("anthropic-beta".into(), ANTHROPIC_FALLBACK_BETA.into()))
        );
        assert!(req.body.get("temperature").is_none());
        assert_eq!(req.body["max_tokens"], MAX_OUTPUT_TOKENS);
        assert_eq!(req.body["fallbacks"], "default");
        assert_eq!(req.body["messages"][0]["role"], "user");
        let system = req.body["system"].as_str().unwrap();
        assert!(system.starts_with("Be brief."));
        assert!(system.ends_with(JSON_INSTRUCTION));

        let older = AgentConfig {
            model: "claude-sonnet-4-6".into(),
            ..config.clone()
        };
        let req = build_request(&older, Some("sk-ant"), "hi").unwrap();
        assert!(req.body.get("fallbacks").is_none());
        assert!(!req.headers.iter().any(|(k, _)| k == "anthropic-beta"));

        assert!(build_request(&config, None, "hi").is_err(), "key required");
        assert!(!sends_temperature(Provider::Anthropic));
        assert!(sends_temperature(Provider::OpenAi));
    }

    #[test]
    fn google_puts_the_key_in_a_header_and_model_in_the_path() {
        let config = AgentConfig {
            model: "models/gemini-2.5-flash".into(),
            ..agent(Provider::Google, ModelType::Generative, OutputType::Json)
        };
        let req = build_request(&config, Some("g-key"), "hi").unwrap();
        assert_eq!(
            req.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(req.headers, vec![("x-goog-api-key".into(), "g-key".into())]);
        assert_eq!(
            req.body["system_instruction"]["parts"][0]["text"],
            format!("Be brief.\n\n{JSON_INSTRUCTION}")
        );
        assert_eq!(
            req.body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        let embed = AgentConfig {
            model_type: ModelType::Embedding,
            output: OutputType::Embedding,
            ..config
        };
        let req = build_request(&embed, Some("g-key"), "hi").unwrap();
        assert!(req.url.ends_with(":embedContent"));
    }

    #[test]
    fn openai_image_request_only_asks_dalle_for_base64() {
        let dalle = AgentConfig {
            model: "dall-e-3".into(),
            ..agent(Provider::OpenAi, ModelType::Image, OutputType::Image)
        };
        let req = build_request(&dalle, Some("k"), "a cat").unwrap();
        assert_eq!(req.url, "https://api.openai.com/v1/images/generations");
        assert_eq!(req.body["response_format"], "b64_json");
        let newer = AgentConfig {
            model: "gpt-image-1".into(),
            ..dalle
        };
        let req = build_request(&newer, Some("k"), "a cat").unwrap();
        assert!(req.body.get("response_format").is_none());
    }

    #[test]
    fn replies_are_parsed_per_protocol() {
        let openai = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 200,
            body: r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#.into(),
        };
        assert_eq!(parse_reply(&openai, &reply).unwrap(), b"hello");

        let ollama = agent(Provider::Ollama, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 200,
            body: r#"{"message":{"role":"assistant","content":"hi"}}"#.into(),
        };
        assert_eq!(parse_reply(&ollama, &reply).unwrap(), b"hi");

        let anthropic = agent(Provider::Anthropic, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 200,
            body: r#"{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"a"},{"type":"text","text":"b"}],"stop_reason":"end_turn"}"#.into(),
        };
        assert_eq!(parse_reply(&anthropic, &reply).unwrap(), b"ab");
        let refusal = Reply {
            status: 200,
            body: r#"{"content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"nope"}}"#.into(),
        };
        let err = parse_reply(&anthropic, &refusal).unwrap_err();
        assert!(err.contains("declined") && err.contains("cyber"), "{err}");

        let google = agent(Provider::Google, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 200,
            body: r#"{"candidates":[{"content":{"parts":[{"text":"x"},{"text":"y"}]}}]}"#.into(),
        };
        assert_eq!(parse_reply(&google, &reply).unwrap(), b"xy");
        let blocked = Reply {
            status: 200,
            body: r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#.into(),
        };
        assert!(
            parse_reply(&google, &blocked)
                .unwrap_err()
                .contains("SAFETY")
        );
    }

    #[test]
    fn embeddings_and_images_decode() {
        let embed = agent(
            Provider::OpenAi,
            ModelType::Embedding,
            OutputType::Embedding,
        );
        let reply = Reply {
            status: 200,
            body: r#"{"data":[{"embedding":[0.1,0.2,-1]}]}"#.into(),
        };
        assert_eq!(parse_reply(&embed, &reply).unwrap(), b"[0.1,0.2,-1]");
        let bad = Reply {
            status: 200,
            body: r#"{"data":[{"embedding":[]}]}"#.into(),
        };
        assert!(parse_reply(&embed, &bad).is_err());

        let google = agent(
            Provider::Google,
            ModelType::Embedding,
            OutputType::Embedding,
        );
        let reply = Reply {
            status: 200,
            body: r#"{"embedding":{"values":[1,2]}}"#.into(),
        };
        assert_eq!(parse_reply(&google, &reply).unwrap(), b"[1,2]");

        let image = agent(Provider::OpenAi, ModelType::Image, OutputType::Image);
        let reply = Reply {
            status: 200,
            body: r#"{"data":[{"b64_json":"aGVsbG8="}]}"#.into(),
        };
        assert_eq!(parse_reply(&image, &reply).unwrap(), b"hello");
        let url_only = Reply {
            status: 200,
            body: r#"{"data":[{"url":"https://x/y.png"}]}"#.into(),
        };
        assert!(parse_reply(&image, &url_only).unwrap_err().contains("URL"));
    }

    #[test]
    fn json_output_is_validated_and_pretty_printed() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Json);
        let reply = Reply {
            status: 200,
            body: r#"{"choices":[{"message":{"content":"Sure:\n```json\n{\"a\": 1}\n```"}}]}"#
                .into(),
        };
        assert_eq!(parse_reply(&config, &reply).unwrap(), b"{\n  \"a\": 1\n}");
        assert_eq!(extract_json("[1, 2]").unwrap(), json!([1, 2]));
        assert_eq!(
            extract_json("text {\"k\":true} more").unwrap(),
            json!({"k": true})
        );
        assert!(extract_json("no json here").is_err());
    }

    #[test]
    fn http_errors_surface_the_providers_message() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 401,
            body: r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error"}}"#
                .into(),
        };
        let err = parse_reply(&config, &reply).unwrap_err();
        assert_eq!(err, "OpenAI returned HTTP 401: Incorrect API key");

        let ollama = agent(Provider::Ollama, ModelType::Generative, OutputType::Text);
        let reply = Reply {
            status: 404,
            body: r#"{"error":"model 'm' not found"}"#.into(),
        };
        assert!(
            parse_reply(&ollama, &reply)
                .unwrap_err()
                .contains("not found")
        );

        let html = Reply {
            status: 502,
            body: "<html>Bad gateway</html>".into(),
        };
        let err = parse_reply(&config, &html).unwrap_err();
        assert!(err.starts_with("OpenAI returned HTTP 502"), "{err}");
        assert!(excerpt(&"x".repeat(1000)).ends_with('…'));
    }

    /// A one-shot HTTP/1.1 server on localhost that records the request and answers with a
    /// fixed status and JSON body.
    fn one_shot_server(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        one_shot_response(status, body, "")
    }

    fn one_shot_response(
        status: u16,
        body: &'static str,
        extra_headers: &str,
    ) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let extra_headers = extra_headers.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            // Poll with a deadline so a client that never connects fails the test
            // instead of hanging it on join.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("no client connected: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut payload = vec![0u8; length];
            reader.read_exact(&mut payload).unwrap();
            let reply = format!(
                "HTTP/1.1 {status} X\r\n{extra_headers}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            format!("{head}{}", String::from_utf8_lossy(&payload))
        });
        (url, handle)
    }

    #[test]
    fn provider_redirects_never_receive_keys_or_prompts() {
        for status in [301, 302, 303, 307, 308] {
            for streamed in [false, true] {
                let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                destination.set_nonblocking(true).unwrap();
                let location = format!(
                    "Location: http://{}/stolen\r\n",
                    destination.local_addr().unwrap()
                );
                let (url, server) = one_shot_response(status, "", &location);
                let request = Request {
                    url,
                    headers: vec![
                        ("x-api-key".into(), "test-secret".into()),
                        ("x-goog-api-key".into(), "test-secret".into()),
                    ],
                    body: json!({"prompt": "private input"}),
                };
                let http = UreqHttp::new(Duration::from_millis(500));
                let err = if streamed {
                    http.post_json_streaming(&request, &mut |_, _| true)
                        .unwrap_err()
                } else {
                    http.post_json(&request).unwrap_err()
                };
                server.join().unwrap();
                assert!(err.contains("redirects are refused"), "{err}");
                assert!(!err.contains("test-secret") && !err.contains("/stolen"));
                assert!(
                    matches!(destination.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
                    "a provider request followed HTTP {status}"
                );
            }
        }
    }

    #[test]
    fn ureq_client_sends_headers_and_json_and_reads_any_status() {
        let (url, server) = one_shot_server(200, r#"{"choices":[{"message":{"content":"real"}}]}"#);
        let config = AgentConfig {
            endpoint: url,
            auth: Auth::None,
            ..agent(Provider::Custom, ModelType::Generative, OutputType::Text)
        };
        let http = UreqHttp::new(Duration::from_secs(5));
        let out = generate(&http, &config, Some("k-123"), "hi").unwrap();
        assert_eq!(out, b"real");
        let seen = server.join().unwrap();
        assert!(
            seen.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
            "{seen}"
        );
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer k-123\r\n"),
            "{seen}"
        );
        assert!(
            seen.replace([' ', '\n'], "").contains(r#""content":"hi""#),
            "{seen}"
        );

        let (url, server) = one_shot_server(429, r#"{"error":{"message":"slow down"}}"#);
        let config = AgentConfig {
            endpoint: url,
            ..config
        };
        let err = generate(&http, &config, None, "hi").unwrap_err();
        assert_eq!(
            err,
            "Custom (OpenAI-compatible) returned HTTP 429: slow down"
        );
        server.join().unwrap();

        let unreachable = AgentConfig {
            endpoint: "http://127.0.0.1:1/v1".into(),
            ..config
        };
        let err = generate(&http, &unreachable, None, "hi").unwrap_err();
        assert!(err.contains("failed"), "{err}");
    }

    /// Streamed pieces and the final bytes from a fake server body.
    fn stream_with(
        config: &AgentConfig,
        status: u16,
        body: &str,
    ) -> (Result<Vec<u8>, String>, Vec<String>, Request) {
        let (result, pieces, request) = stream_full(config, status, body);
        (result.map(|s| s.bytes), pieces, request)
    }

    fn stream_full(
        config: &AgentConfig,
        status: u16,
        body: &str,
    ) -> (Result<Streamed, String>, Vec<String>, Request) {
        let http = FakeHttp::new(status, body);
        let mut pieces = Vec::new();
        let result = generate_streaming(&http, config, Some("k"), "hi", &mut |text| {
            pieces.push(text.to_string());
            true
        });
        let request = http.seen.lock().unwrap()[0].clone();
        (result, pieces, request)
    }

    #[test]
    fn streams_report_the_model_that_answered() {
        let openai = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let body = concat!(
            "data: {\"model\":\"other-model\",\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: {\"model\":\"other-model\",\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n",
        );
        let (result, _, _) = stream_full(&openai, 200, body);
        let streamed = result.unwrap();
        assert_eq!(streamed.bytes, b"ab");
        assert_eq!(streamed.model.as_deref(), Some("other-model"));

        // A whole reply names it too; a stream without a model field gives none.
        let (result, _, _) = stream_full(
            &openai,
            200,
            r#"{"model":"m-2024","choices":[{"message":{"content":"x"}}]}"#,
        );
        assert_eq!(result.unwrap().model.as_deref(), Some("m-2024"));
        let (result, _, _) = stream_full(
            &openai,
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
        );
        assert_eq!(result.unwrap().model, None);

        let anthropic = agent(Provider::Anthropic, ModelType::Generative, OutputType::Text);
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"y\"}}\n\n",
        );
        let (result, _, _) = stream_full(&anthropic, 200, body);
        assert_eq!(result.unwrap().model.as_deref(), Some("claude-x"));

        let google = agent(Provider::Google, ModelType::Generative, OutputType::Text);
        let body = "data: {\"modelVersion\":\"gemini-z\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"q\"}]}}]}\n";
        let (result, _, _) = stream_full(&google, 200, body);
        assert_eq!(result.unwrap().model.as_deref(), Some("gemini-z"));
    }

    #[test]
    fn openai_streams_sse_and_finishes_json_at_the_end() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Json);
        let body = concat!(
            ": keep-alive\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"a\\\":\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" 1}\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (result, pieces, request) = stream_with(&config, 200, body);
        assert_eq!(request.body["stream"], true);
        assert_eq!(pieces, ["{\"a\":", " 1}"]);
        assert_eq!(result.unwrap(), b"{\n  \"a\": 1\n}");

        // Bad JSON at the end is the usual error, after the pieces were seen.
        let (result, pieces, _) = stream_with(
            &config,
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"nope\"}}]}\n",
        );
        assert_eq!(pieces, ["nope"]);
        assert!(result.unwrap_err().contains("did not return valid JSON"));
    }

    #[test]
    fn ollama_streams_one_object_per_line() {
        let config = AgentConfig {
            auth: Auth::None,
            ..agent(Provider::Ollama, ModelType::Generative, OutputType::Text)
        };
        let body = "{\"message\":{\"role\":\"assistant\",\"content\":\"Bon\"},\"done\":false}\n\
                    {\"message\":{\"role\":\"assistant\",\"content\":\"jour\"},\"done\":false}\n\
                    {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n";
        let (result, pieces, request) = stream_with(&config, 200, body);
        assert_eq!(request.body["stream"], true);
        assert_eq!(pieces, ["Bon", "jour"]);
        assert_eq!(result.unwrap(), b"Bonjour");

        let (result, _, _) = stream_with(&config, 200, "{\"error\":\"model not found\"}\n");
        assert_eq!(result.unwrap_err(), "Ollama: model not found");
    }

    #[test]
    fn anthropic_streams_deltas_and_reports_refusals_and_errors() {
        let config = agent(Provider::Anthropic, ModelType::Generative, OutputType::Text);
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (result, pieces, request) = stream_with(&config, 200, body);
        assert_eq!(request.body["stream"], true);
        assert_eq!(pieces, ["Hi", " there"]);
        assert_eq!(result.unwrap(), b"Hi there");

        let refusal = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"I\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"category\":\"c\",\"explanation\":\"e\"}}}\n\n",
        );
        let (result, _, _) = stream_with(&config, 200, refusal);
        assert_eq!(result.unwrap_err(), "Anthropic declined the request (c): e");

        let error = "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let (result, _, _) = stream_with(&config, 200, error);
        assert_eq!(result.unwrap_err(), "Anthropic: Overloaded");
    }

    #[test]
    fn google_streams_on_its_stream_url() {
        let config = agent(Provider::Google, ModelType::Generative, OutputType::Text);
        let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Ol\"}]}}]}\n\n\
                    data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"á\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let (result, pieces, request) = stream_with(&config, 200, body);
        assert!(
            request
                .url
                .ends_with("/models/m:streamGenerateContent?alt=sse"),
            "{}",
            request.url
        );
        assert!(request.body.get("stream").is_none());
        assert_eq!(pieces, ["Ol", "á"]);
        assert_eq!(result.unwrap(), "Olá".as_bytes());
    }

    #[test]
    fn streaming_falls_back_to_whole_replies_and_errors() {
        let config = agent(Provider::Mistral, ModelType::Generative, OutputType::Text);
        // A server that ignored `stream`: the single reply is parsed as usual.
        let (result, pieces, _) = stream_with(
            &config,
            200,
            r#"{"choices":[{"message":{"content":"bonjour"}}]}"#,
        );
        assert!(pieces.is_empty());
        assert_eq!(result.unwrap(), b"bonjour");

        // Errors come back whole and keep their message.
        let (result, pieces, _) = stream_with(&config, 429, r#"{"error":{"message":"slow down"}}"#);
        assert!(pieces.is_empty());
        assert_eq!(result.unwrap_err(), "Mistral returned HTTP 429: slow down");

        // Embeddings are not streams.
        let embed = agent(
            Provider::OpenAi,
            ModelType::Embedding,
            OutputType::Embedding,
        );
        let (result, pieces, request) =
            stream_with(&embed, 200, r#"{"data":[{"embedding":[0.5,0.25]}]}"#);
        assert!(pieces.is_empty());
        assert!(request.body.get("stream").is_none());
        assert_eq!(result.unwrap(), b"[0.5,0.25]");
    }

    #[test]
    fn streamed_errors_and_corruption_never_become_partial_successes() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let prefix = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        for tail in [
            "data: {\"error\":{\"message\":\"late failure\"}}",
            "data: {not json}\n\n",
            "data: {\"choices\":",
        ] {
            let (result, pieces, _) = stream_with(&config, 200, &format!("{prefix}{tail}"));
            assert_eq!(pieces, ["partial"]);
            assert!(result.is_err(), "silently accepted {tail:?}");
        }
    }

    #[test]
    fn ndjson_keeps_the_last_record_without_a_newline() {
        let config = agent(Provider::Ollama, ModelType::Generative, OutputType::Text);
        let body = "{\"message\":{\"content\":\"first\"},\"done\":false}\n{\"message\":{\"content\":\"last\"},\"done\":true}";
        let (result, pieces, _) = stream_with(&config, 200, body);
        assert_eq!(result.unwrap(), b"firstlast");
        assert_eq!(pieces, ["first", "last"]);
    }

    #[test]
    fn sse_multiline_events_and_split_utf8_survive_chunk_boundaries() {
        struct Chunked(Vec<u8>);
        impl Http for Chunked {
            fn post_json(&self, _: &Request) -> Result<Reply, String> {
                unreachable!()
            }
            fn post_json_streaming(
                &self,
                _: &Request,
                sink: &mut dyn FnMut(u16, &[u8]) -> bool,
            ) -> Result<u16, String> {
                for byte in self.0.chunks(1) {
                    if !sink(200, byte) {
                        break;
                    }
                }
                Ok(200)
            }
        }
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let body = "event: message\r\ndata: {\"choices\":\r\ndata: [{\"delta\":{\"content\":\"café\"}}]}\r\n\r\ndata: [DONE]\n\n";
        let mut pieces = Vec::new();
        let out = generate_streaming(
            &Chunked(body.as_bytes().to_vec()),
            &config,
            Some("k"),
            "hi",
            &mut |text| {
                pieces.push(text.to_string());
                true
            },
        )
        .unwrap();
        assert_eq!(out.bytes, "café".as_bytes());
        assert_eq!(pieces, ["café"]);
        let bytes = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xff\"}}]}\n\n".to_vec();
        let err = generate_streaming(&Chunked(bytes), &config, Some("k"), "hi", &mut |_| true)
            .unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn streaming_stops_reading_at_the_body_cap_even_on_http_errors() {
        struct Endless {
            status: u16,
            delivered: Mutex<usize>,
        }
        impl Http for Endless {
            fn post_json(&self, _: &Request) -> Result<Reply, String> {
                unreachable!()
            }
            fn post_json_streaming(
                &self,
                _: &Request,
                sink: &mut dyn FnMut(u16, &[u8]) -> bool,
            ) -> Result<u16, String> {
                let mut chunk = vec![b'x'; 64 * 1024];
                chunk[0] = b':';
                *chunk.last_mut().unwrap() = b'\n';
                for _ in 0..MAX_REPLY_BYTES as usize / chunk.len() + 10 {
                    *self.delivered.lock().unwrap() += 1;
                    if !sink(self.status, &chunk) {
                        break;
                    }
                }
                Ok(self.status)
            }
        }
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        for status in [200, 500] {
            let http = Endless {
                status,
                delivered: Mutex::new(0),
            };
            let err =
                generate_streaming(&http, &config, Some("k"), "hi", &mut |_| true).unwrap_err();
            assert!(err.contains("larger than"), "{err}");
            assert_eq!(
                *http.delivered.lock().unwrap(),
                MAX_REPLY_BYTES as usize / (64 * 1024) + 1
            );
        }
    }

    #[test]
    fn request_helpers_validate_endpoints_and_model_paths_before_http() {
        let base = agent(Provider::Google, ModelType::Generative, OutputType::Text);
        for (endpoint, model) in [
            ("http://example.com", "m"),
            ("https://user:secret@example.com", "m"),
            ("https://example.com?secret=key", "m"),
            ("https://example.com", "m/../../other"),
        ] {
            let config = AgentConfig {
                endpoint: endpoint.into(),
                model: model.into(),
                ..base.clone()
            };
            let http = FakeHttp::new(
                200,
                r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
            );
            assert!(generate(&http, &config, Some("key"), "hi").is_err());
            assert!(http.seen.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn stopping_a_stream_returns_stopped() {
        let config = agent(Provider::OpenAi, ModelType::Generative, OutputType::Text);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
        );
        let http = FakeHttp::new(200, body);
        let mut seen = 0;
        let err = generate_streaming(&http, &config, Some("k"), "hi", &mut |_| {
            seen += 1;
            false
        })
        .unwrap_err();
        assert_eq!(err, STOPPED);
        assert_eq!(seen, 1);
    }

    #[test]
    fn ureq_client_streams_the_body_as_it_arrives() {
        use std::io::{BufRead, BufReader, Read, Write};
        let events: [&str; 2] = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"first \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\ndata: [DONE]\n\n",
        ];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut payload = vec![0u8; length];
            reader.read_exact(&mut payload).unwrap();
            let total: usize = events.iter().map(|e| e.len()).sum();
            let out = reader.get_mut();
            out.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            // Two writes a pause apart, so the client sees them separately.
            out.write_all(events[0].as_bytes()).unwrap();
            out.flush().unwrap();
            std::thread::sleep(Duration::from_millis(200));
            out.write_all(events[1].as_bytes()).unwrap();
            out.flush().unwrap();
        });
        let config = AgentConfig {
            endpoint: url,
            auth: Auth::None,
            ..agent(Provider::Custom, ModelType::Generative, OutputType::Text)
        };
        let http = UreqHttp::new(Duration::from_secs(5));
        let mut arrivals: Vec<(String, std::time::Instant)> = Vec::new();
        let out = generate_streaming(&http, &config, None, "hi", &mut |text| {
            arrivals.push((text.to_string(), std::time::Instant::now()));
            true
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(out.bytes, b"first second");
        assert_eq!(out.model, None);
        assert_eq!(arrivals.len(), 2, "{arrivals:?}");
        let gap = arrivals[1].1.duration_since(arrivals[0].1);
        assert!(
            gap >= Duration::from_millis(100),
            "{gap:?}: read as one piece"
        );
    }

    #[test]
    fn generate_runs_build_post_parse() {
        let config = agent(Provider::Mistral, ModelType::Generative, OutputType::Text);
        let http = FakeHttp::new(200, r#"{"choices":[{"message":{"content":"bonjour"}}]}"#);
        let out = generate(&http, &config, Some("k"), "hi").unwrap();
        assert_eq!(out, b"bonjour");
        let seen = http.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, "https://api.mistral.ai/v1/chat/completions");
    }
}

#[cfg(test)]
mod model_listing_tests {
    use super::*;
    use crate::node_data::Provider;

    fn config(provider: Provider) -> AgentConfig {
        AgentConfig {
            provider,
            ..AgentConfig::default()
        }
    }

    #[test]
    fn listing_urls_follow_each_api() {
        let ollama = models_request(&config(Provider::Ollama), None).unwrap();
        assert_eq!(ollama.url, "http://localhost:11434/api/tags");
        assert!(ollama.headers.is_empty());
        let lm = models_request(&config(Provider::LmStudio), None).unwrap();
        assert_eq!(lm.url, "http://localhost:1234/v1/models");
        let openai = models_request(&config(Provider::OpenAi), Some("sk")).unwrap();
        assert_eq!(openai.url, "https://api.openai.com/v1/models");
        assert_eq!(
            openai.headers,
            vec![("authorization".to_string(), "Bearer sk".to_string())]
        );
        assert!(
            models_request(&config(Provider::Anthropic), None)
                .unwrap_err()
                .contains("API key")
        );
        let google = models_request(&config(Provider::Google), Some("g")).unwrap();
        assert!(google.url.ends_with("/models?pageSize=1000"));
        assert_eq!(google.headers[0].0, "x-goog-api-key");
    }

    #[test]
    fn listings_are_parsed_sorted_and_filtered() {
        let reply = |body: &str| Reply {
            status: 200,
            body: body.into(),
        };
        assert_eq!(
            parse_models(
                &config(Provider::Ollama),
                &reply(
                    r#"{"models":[{"name":"llama3.1:latest"},{"name":"gemma"},{"name":"gemma"}]}"#
                )
            )
            .unwrap(),
            vec!["gemma", "llama3.1:latest"]
        );
        assert_eq!(
            parse_models(
                &config(Provider::LmStudio),
                &reply(r#"{"data":[{"id":"mistralai/ministral-3-3b"},{"id":"../bad"}]}"#)
            )
            .unwrap(),
            vec!["mistralai/ministral-3-3b"]
        );
        assert_eq!(
            parse_models(
                &config(Provider::Google),
                &reply(r#"{"models":[{"name":"models/gemini-2.5-pro"}]}"#)
            )
            .unwrap(),
            vec!["gemini-2.5-pro"]
        );
        let error = parse_models(
            &config(Provider::OpenAi),
            &Reply {
                status: 401,
                body: r#"{"error":{"message":"bad key"}}"#.into(),
            },
        )
        .unwrap_err();
        assert!(error.contains("401") && error.contains("bad key"));
        assert!(
            parse_models(&config(Provider::OpenAi), &reply("{}"))
                .unwrap_err()
                .contains("no models")
        );
    }
}
