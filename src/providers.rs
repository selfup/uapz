//! Talking to model providers.
//!
//! [`build_request`] turns an [`AgentConfig`] plus the text going in into one HTTP request,
//! [`parse_reply`] turns the provider's answer into output bytes, and [`generate`] runs the
//! two around an [`Http`] client. The client is a trait so tests can use a fake; the real
//! one is [`UreqHttp`].
//!
//! Protocols:
//! - OpenAI-compatible servers (OpenAI, OpenRouter, Mistral, LM Studio, llama.cpp, custom)
//!   share `/chat/completions`, `/embeddings` and `/images/generations`.
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
}

pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Http for UreqHttp {
    fn post_json(&self, request: &Request) -> Result<Reply, String> {
        let mut req = self.agent.post(&request.url);
        for (key, value) in &request.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        let mut response = req
            .send_json(&request.body)
            .map_err(|e| format!("request to {} failed: {e}", request.url))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(|e| format!("reading the reply from {} failed: {e}", request.url))?;
        Ok(Reply { status, body })
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
    let base = config.endpoint_url().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err("the agent has no endpoint".into());
    }
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
                    let mut body = json!({
                        "model": model,
                        "messages": chat_messages(system.as_deref(), &user),
                        "temperature": temperature(config),
                    });
                    if wants_json {
                        body["response_format"] = json!({"type": "json_object"});
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
                let details = &value["stop_details"];
                return Err(format!(
                    "Anthropic declined the request ({}): {}",
                    details["category"].as_str().unwrap_or("no category"),
                    details["explanation"].as_str().unwrap_or("no explanation")
                ));
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
        assert_eq!(req.body["messages"][0]["content"], "Be brief.");
        assert_eq!(req.body["messages"][1]["content"], "hello");
        assert_eq!(req.body["response_format"]["type"], "json_object");
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
            "Be brief."
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
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
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
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            format!("{head}{}", String::from_utf8_lossy(&payload))
        });
        (url, handle)
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
