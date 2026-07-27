/// AI-powered command suggestions: natural language → shell command.
/// Supports OpenAI, Anthropic, and Ollama (local) providers.
/// Runs inference in a background thread, communicates via channels.
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiProvider {
    OpenAI,
    Anthropic,
    Ollama,
}

#[derive(Debug, Clone)]
pub struct AiConfig {
    pub provider: AiProvider,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
    /// Whether cloud providers may receive recent history and Git status.
    pub share_context: bool,
}

impl AiConfig {
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Option<Self> {
        let provider_name = get("RSH_AI_PROVIDER").unwrap_or_default();
        let provider_name = provider_name.trim();
        let explicitly_enabled = !provider_name.is_empty()
            || get("RSH_AI_ENABLED")
                .as_deref()
                .is_some_and(env_value_is_truthy);
        if !explicitly_enabled {
            return None;
        }

        let provider = if provider_name.is_empty() {
            // Provider auto-detection is only reached after RSH_AI_ENABLED opted in.
            if get("OPENAI_API_KEY").as_deref().is_some_and(nonempty) {
                AiProvider::OpenAI
            } else if get("ANTHROPIC_API_KEY").as_deref().is_some_and(nonempty) {
                AiProvider::Anthropic
            } else {
                AiProvider::Ollama
            }
        } else {
            match provider_name.to_ascii_lowercase().as_str() {
                "openai" => AiProvider::OpenAI,
                "anthropic" => AiProvider::Anthropic,
                "ollama" => AiProvider::Ollama,
                _ => return None,
            }
        };

        let (api_key, default_model, default_url) = match &provider {
            AiProvider::OpenAI => (
                get("OPENAI_API_KEY").or_else(|| get("RSH_AI_API_KEY")),
                "gpt-4o-mini".to_string(),
                "https://api.openai.com/v1".to_string(),
            ),
            AiProvider::Anthropic => (
                get("ANTHROPIC_API_KEY").or_else(|| get("RSH_AI_API_KEY")),
                "claude-sonnet-4-20250514".to_string(),
                "https://api.anthropic.com".to_string(),
            ),
            AiProvider::Ollama => (
                None,
                "codellama:7b".to_string(),
                "http://localhost:11434".to_string(),
            ),
        };

        let model = get("RSH_AI_MODEL")
            .filter(|value| nonempty(value))
            .unwrap_or(default_model);
        let base_url = get("RSH_AI_BASE_URL")
            .filter(|value| nonempty(value))
            .unwrap_or(default_url);
        let share_context = get("RSH_AI_SHARE_CONTEXT")
            .as_deref()
            .is_some_and(env_value_is_truthy);

        Some(AiConfig {
            provider,
            api_key,
            model,
            base_url,
            share_context,
        })
    }

    /// Local inference stays local. Cloud inference only gets optional shell
    /// context after the user explicitly opts in with RSH_AI_SHARE_CONTEXT.
    pub fn allows_extended_context(&self) -> bool {
        self.provider == AiProvider::Ollama || self.share_context
    }
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn env_value_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug)]
pub struct AiRequest {
    pub prompt: String,
    pub context: AiContext,
}

#[derive(Debug, Clone)]
pub struct AiContext {
    pub cwd: String,
    pub os: String,
    pub recent_history: Vec<String>,
    pub git_status: Option<String>,
    pub last_error: Option<(String, String, i32)>, // (command, stderr, exit_code)
}

#[derive(Debug)]
pub enum AiResponse {
    Suggestion(String),
    Error(String),
}

pub struct AiWorker {
    tx: mpsc::Sender<AiRequest>,
    pub rx: mpsc::Receiver<AiResponse>,
}

impl AiWorker {
    pub fn new(config: AiConfig) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<AiRequest>();
        let (resp_tx, resp_rx) = mpsc::channel::<AiResponse>();

        thread::spawn(move || {
            while let Ok(request) = req_rx.recv() {
                let response = process_request(&config, &request);
                if resp_tx.send(response).is_err() {
                    break;
                }
            }
        });

        AiWorker {
            tx: req_tx,
            rx: resp_rx,
        }
    }

    pub fn request(&self, req: AiRequest) {
        let _ = self.tx.send(req);
    }

    pub fn try_recv(&self) -> Option<AiResponse> {
        self.rx.try_recv().ok()
    }
}

fn build_system_prompt(ctx: &AiContext) -> String {
    let mut sys = String::from(
        "You are a shell command generator. Given a natural language description, \
         output ONLY the shell command. No explanation, no markdown, no quotes around it. \
         Just the raw command that can be executed directly.\n\n",
    );
    sys.push_str(&format!("OS: {}\n", ctx.os));
    sys.push_str(&format!("Current directory: {}\n", ctx.cwd));
    if let Some(ref git) = ctx.git_status {
        sys.push_str(&format!("Git status: {}\n", git));
    }
    if !ctx.recent_history.is_empty() {
        sys.push_str("Recent commands:\n");
        for cmd in ctx.recent_history.iter().rev().take(5) {
            sys.push_str(&format!("  {}\n", cmd));
        }
    }
    sys
}

fn build_fix_prompt(ctx: &AiContext) -> String {
    let mut sys = String::from(
        "You are a shell command fixer. Given a failed command with its error output, \
         output ONLY the corrected shell command. No explanation, no markdown, no quotes. \
         Just the raw fixed command.\n\n",
    );
    sys.push_str(&format!("OS: {}\n", ctx.os));
    sys.push_str(&format!("Current directory: {}\n", ctx.cwd));
    if let Some((ref cmd, ref stderr, code)) = ctx.last_error {
        sys.push_str(&format!("Failed command: {}\n", cmd));
        sys.push_str(&format!("Exit code: {}\n", code));
        sys.push_str(&format!("Error output:\n{}\n", stderr));
    }
    sys
}

#[cfg(feature = "ai")]
fn process_request(config: &AiConfig, request: &AiRequest) -> AiResponse {
    let is_fix = request.context.last_error.is_some() && request.prompt.is_empty();
    let system_prompt = if is_fix {
        build_fix_prompt(&request.context)
    } else {
        build_system_prompt(&request.context)
    };
    let user_msg = if is_fix {
        "Fix the failed command.".to_string()
    } else {
        request.prompt.clone()
    };

    match &config.provider {
        AiProvider::OpenAI => call_openai(config, &system_prompt, &user_msg),
        AiProvider::Anthropic => call_anthropic(config, &system_prompt, &user_msg),
        AiProvider::Ollama => call_ollama(config, &system_prompt, &user_msg),
    }
}

#[cfg(not(feature = "ai"))]
fn process_request(_config: &AiConfig, _request: &AiRequest) -> AiResponse {
    AiResponse::Error("AI feature not enabled. Rebuild with --features ai".to_string())
}

#[cfg(feature = "ai")]
fn ai_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(30)))
        .timeout_send_body(Some(std::time::Duration::from_secs(10)))
        .build()
        .into()
}

#[cfg(feature = "ai")]
fn call_openai(config: &AiConfig, system: &str, user: &str) -> AiResponse {
    let url = format!("{}/chat/completions", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "max_tokens": 200,
        "temperature": 0.1
    });

    let mut req = ai_agent()
        .post(&url)
        .header("Content-Type", "application/json");
    if let Some(ref key) = config.api_key {
        req = req.header("Authorization", &format!("Bearer {}", key));
    }

    match req.send(body.to_string()) {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(text) => parse_openai_response(&text),
            Err(e) => AiResponse::Error(format!("Read error: {}", e)),
        },
        Err(e) => AiResponse::Error(format!("Request failed: {}", e)),
    }
}

#[cfg(feature = "ai")]
fn call_anthropic(config: &AiConfig, system: &str, user: &str) -> AiResponse {
    let url = format!("{}/v1/messages", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": 200,
        "system": system,
        "messages": [
            {"role": "user", "content": user}
        ]
    });

    let mut req = ai_agent()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01");
    if let Some(ref key) = config.api_key {
        req = req.header("x-api-key", key);
    }

    match req.send(body.to_string()) {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(text) => parse_anthropic_response(&text),
            Err(e) => AiResponse::Error(format!("Read error: {}", e)),
        },
        Err(e) => AiResponse::Error(format!("Request failed: {}", e)),
    }
}

#[cfg(feature = "ai")]
fn call_ollama(config: &AiConfig, system: &str, user: &str) -> AiResponse {
    let url = format!("{}/api/generate", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "system": system,
        "prompt": user,
        "stream": false,
        "options": {
            "temperature": 0.1,
            "num_predict": 200
        }
    });

    match ai_agent()
        .post(&url)
        .header("Content-Type", "application/json")
        .send(body.to_string())
    {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(text) => parse_ollama_response(&text),
            Err(e) => AiResponse::Error(format!("Read error: {}", e)),
        },
        Err(e) => AiResponse::Error(format!("Request failed: {}", e)),
    }
}

#[cfg(feature = "ai")]
fn parse_openai_response(text: &str) -> AiResponse {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => {
            if let Some(content) = v["choices"][0]["message"]["content"].as_str() {
                AiResponse::Suggestion(content.trim().to_string())
            } else if let Some(err) = v["error"]["message"].as_str() {
                AiResponse::Error(err.to_string())
            } else {
                AiResponse::Error("Unexpected response format".to_string())
            }
        }
        Err(e) => AiResponse::Error(format!("Parse error: {}", e)),
    }
}

#[cfg(feature = "ai")]
fn parse_anthropic_response(text: &str) -> AiResponse {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => {
            if let Some(content) = v["content"][0]["text"].as_str() {
                AiResponse::Suggestion(content.trim().to_string())
            } else if let Some(err) = v["error"]["message"].as_str() {
                AiResponse::Error(err.to_string())
            } else {
                AiResponse::Error("Unexpected response format".to_string())
            }
        }
        Err(e) => AiResponse::Error(format!("Parse error: {}", e)),
    }
}

#[cfg(feature = "ai")]
fn parse_ollama_response(text: &str) -> AiResponse {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => {
            if let Some(response) = v["response"].as_str() {
                AiResponse::Suggestion(response.trim().to_string())
            } else if let Some(err) = v["error"].as_str() {
                AiResponse::Error(err.to_string())
            } else {
                AiResponse::Error("Unexpected response format".to_string())
            }
        }
        Err(e) => AiResponse::Error(format!("Parse error: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Option<AiConfig> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AiConfig::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn api_keys_do_not_enable_ai_implicitly() {
        assert!(config(&[("OPENAI_API_KEY", "secret")]).is_none());
        assert!(config(&[("RSH_AI_ENABLED", "false"), ("OPENAI_API_KEY", "secret")]).is_none());
    }

    #[test]
    fn provider_or_truthy_enabled_flag_explicitly_enables_ai() {
        let explicit = config(&[("RSH_AI_PROVIDER", "ollama")]).unwrap();
        assert_eq!(explicit.provider, AiProvider::Ollama);

        let detected = config(&[("RSH_AI_ENABLED", "YeS"), ("OPENAI_API_KEY", "secret")]).unwrap();
        assert_eq!(detected.provider, AiProvider::OpenAI);

        let local_default = config(&[("RSH_AI_ENABLED", "1")]).unwrap();
        assert_eq!(local_default.provider, AiProvider::Ollama);
    }

    #[test]
    fn cloud_extended_context_requires_separate_opt_in() {
        let private = config(&[("RSH_AI_PROVIDER", "openai")]).unwrap();
        assert!(!private.allows_extended_context());

        let shared = config(&[
            ("RSH_AI_PROVIDER", "anthropic"),
            ("RSH_AI_SHARE_CONTEXT", "on"),
        ])
        .unwrap();
        assert!(shared.allows_extended_context());

        let local = config(&[("RSH_AI_PROVIDER", "ollama")]).unwrap();
        assert!(local.allows_extended_context());
    }
}
