use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result, anyhow};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    config::ApiKeySource,
    context::PromptContext,
    model::{ModelCache, ModelInfo, now_unix_seconds, sort_and_filter_models},
};

const MODELS_ENDPOINT: &str = "https://openrouter.ai/api/v1/models";
const CHAT_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
const SYSTEM_PROMPT: &str = "You translate natural language into safe, practical shell commands for a terminal UI.

Return JSON only. Never use markdown fences.

Schema:
{
  \"action\": \"run\" | \"ask\",
  \"command\": \"shell command when action=run\",
  \"question\": \"concise clarification question when action=ask\",
  \"reasoning\": \"short explanation\"
}

Rules:
- This is not a chatbot. Prefer a single concrete command or a short non-interactive command chain.
- Use only tools that are present in the PATH hint when possible.
- Avoid destructive commands, interactive prompts, editors, or commands that need sudo unless the user clearly asked for them.
- If the request is ambiguous, risky, or underspecified, ask one short clarification question instead of guessing.
- When a previous command failed, use the full stdout/stderr and exit code to repair it before asking again.
- Assume the command will be executed in the provided working directory.
- Prefer portable commands and avoid unnecessary verbosity.";

#[derive(Debug, Clone)]
pub struct OpenRouterClient {
    client: Client,
    _api_key_source: ApiKeySource,
}

#[derive(Debug, Clone)]
pub struct PlanningInput<'a> {
    pub original_intent: &'a str,
    pub user_input: &'a str,
    pub clarification_answer: Option<&'a str>,
    pub attempt_summaries: &'a [String],
    pub prompt_context: &'a PromptContext,
}

#[derive(Debug, Clone)]
pub enum ModelDecision {
    Run {
        command: String,
        reasoning: Option<String>,
    },
    Ask {
        question: String,
        reasoning: Option<String>,
    },
}

impl OpenRouterClient {
    pub fn new(api_key: String, api_key_source: ApiKeySource) -> Result<Self> {
        let trimmed_key = api_key.trim();
        if trimmed_key.is_empty() {
            return Err(anyhow!("OpenRouter API key is empty"));
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {trimmed_key}"))
                .context("failed to build OpenRouter authorization header")?,
        );
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/openai/codex"),
        );
        headers.insert("X-Title", HeaderValue::from_static("ash"));

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to build OpenRouter HTTP client")?;

        Ok(Self {
            client,
            _api_key_source: api_key_source,
        })
    }

    pub async fn load_model_catalog(
        &self,
        cache_path: &Path,
        force_refresh: bool,
        include_paid: bool,
    ) -> Result<Vec<ModelInfo>> {
        if !force_refresh {
            if let Some(cache) = read_cache(cache_path)? {
                if cache.is_fresh() {
                    return Ok(sort_and_filter_models(cache.models, include_paid));
                }
            }
        }

        match self.fetch_models().await {
            Ok(models) => {
                write_cache(
                    cache_path,
                    &ModelCache {
                        fetched_at_unix_seconds: now_unix_seconds(),
                        models: models.clone(),
                    },
                )?;
                Ok(sort_and_filter_models(models, include_paid))
            }
            Err(error) => {
                if let Some(cache) = read_cache(cache_path)? {
                    Ok(sort_and_filter_models(cache.models, include_paid))
                } else {
                    Err(error)
                }
            }
        }
    }

    pub async fn plan_command(
        &self,
        model: &str,
        input: &PlanningInput<'_>,
    ) -> Result<ModelDecision> {
        let prompt = build_user_prompt(input);
        let body = json!({
            "model": model,
            "temperature": 0.1,
            "messages": [
                {
                    "role": "system",
                    "content": SYSTEM_PROMPT
                },
                {
                    "role": "user",
                    "content": prompt
                }
            ]
        });

        let response = self
            .client
            .post(CHAT_ENDPOINT)
            .json(&body)
            .send()
            .await
            .context("failed to contact OpenRouter")?
            .error_for_status()
            .context("OpenRouter returned an error")?;

        let payload = response
            .json::<ChatCompletionResponse>()
            .await
            .context("failed to decode OpenRouter response")?;

        let content = payload
            .choices
            .first()
            .map(|choice| extract_message_content(&choice.message.content))
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("OpenRouter returned an empty completion"))?;

        parse_decision(&content)
    }

    async fn fetch_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(MODELS_ENDPOINT)
            .send()
            .await
            .context("failed to fetch model list from OpenRouter")?
            .error_for_status()
            .context("OpenRouter returned an error while loading models")?;

        let payload = response
            .json::<ModelsResponse>()
            .await
            .context("failed to decode OpenRouter model list")?;

        Ok(payload.data)
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Value,
}

#[derive(Debug, Deserialize)]
struct DecisionEnvelope {
    action: String,
    command: Option<String>,
    question: Option<String>,
    reasoning: Option<String>,
}

fn build_user_prompt(input: &PlanningInput<'_>) -> String {
    let attempts = if input.attempt_summaries.is_empty() {
        "None".to_string()
    } else {
        input.attempt_summaries.join("\n\n---\n\n")
    };

    let clarification = input
        .clarification_answer
        .map(|answer| format!("User clarification:\n{answer}\n\n"))
        .unwrap_or_default();

    format!(
        "Original intent:\n{}\n\nLatest user message:\n{}\n\n{}Context block:\n{}\nPrevious attempts:\n{}\n",
        input.original_intent,
        input.user_input,
        clarification,
        input.prompt_context.to_block(),
        attempts
    )
}

fn parse_decision(content: &str) -> Result<ModelDecision> {
    let json_slice = extract_json_slice(content).unwrap_or(content);
    let envelope: DecisionEnvelope = serde_json::from_str(json_slice)
        .with_context(|| format!("failed to parse model JSON: {content}"))?;

    match envelope.action.as_str() {
        "run" => {
            let command = envelope
                .command
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("model returned action=run without a command"))?;

            Ok(ModelDecision::Run {
                command,
                reasoning: envelope.reasoning.map(|value| value.trim().to_string()),
            })
        }
        "ask" => {
            let question = envelope
                .question
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("model returned action=ask without a question"))?;

            Ok(ModelDecision::Ask {
                question,
                reasoning: envelope.reasoning.map(|value| value.trim().to_string()),
            })
        }
        other => Err(anyhow!("unsupported model action: {other}")),
    }
}

fn extract_message_content(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

fn extract_json_slice(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    content.get(start..=end)
}

fn read_cache(path: &Path) -> Result<Option<ModelCache>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read model cache {}", path.display()))?;
    let cache = serde_json::from_str::<ModelCache>(&content)
        .with_context(|| format!("failed to parse model cache {}", path.display()))?;
    Ok(Some(cache))
}

fn write_cache(path: &Path, cache: &ModelCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create model cache directory {}",
                parent.display()
            )
        })?;
    }

    let content = serde_json::to_string_pretty(cache).context("failed to serialize model cache")?;
    fs::write(path, content)
        .with_context(|| format!("failed to write model cache {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{ModelDecision, parse_decision};

    #[test]
    fn parses_run_decision() {
        let decision = parse_decision(r#"{"action":"run","command":"ls -la"}"#).unwrap();
        match decision {
            ModelDecision::Run { command, .. } => assert_eq!(command, "ls -la"),
            _ => panic!("expected a run decision"),
        }
    }

    #[test]
    fn parses_ask_decision_inside_code_fences() {
        let decision =
            parse_decision("```json\n{\"action\":\"ask\",\"question\":\"Which file?\"}\n```")
                .unwrap();
        match decision {
            ModelDecision::Ask { question, .. } => assert_eq!(question, "Which file?"),
            _ => panic!("expected an ask decision"),
        }
    }
}
