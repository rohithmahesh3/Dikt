use crate::settings::{PostProcessProvider, Settings};
use log::debug;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/rohithmahesh3/Dikt"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Dikt/1.0 (+https://github.com/rohithmahesh3/Dikt)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("Dikt"));

    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key)
                    .map_err(|e| format!("Invalid API key header value: {}", e))?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .map_err(|e| format!("Invalid authorization header value: {}", e))?,
            );
        }
    }

    Ok(headers)
}

fn create_client(provider: &PostProcessProvider, api_key: &str) -> Result<reqwest::Client, String> {
    let headers = build_headers(provider, api_key)?;
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

fn get_provider(settings: &Settings) -> Option<PostProcessProvider> {
    let provider_id = settings.post_process_provider_id();
    let base_urls = settings.post_process_base_urls();

    let base_url =
        base_urls
            .get(&provider_id)
            .cloned()
            .unwrap_or_else(|| match provider_id.as_str() {
                "openai" => "https://api.openai.com/v1".to_string(),
                "anthropic" => "https://api.anthropic.com/v1".to_string(),
                "openrouter" => "https://openrouter.ai/api/v1".to_string(),
                "groq" => "https://api.groq.com/openai/v1".to_string(),
                "cerebras" => "https://api.cerebras.ai/v1".to_string(),
                _ => "http://localhost:11434/v1".to_string(),
            });

    Some(PostProcessProvider {
        id: provider_id.clone(),
        label: provider_id.clone(),
        base_url,
        allow_base_url_edit: provider_id == "custom",
    })
}

pub async fn call_llm(settings: &Settings, prompt: &str) -> Option<String> {
    let provider = get_provider(settings)?;
    let api_keys = settings.post_process_api_keys();
    let api_key = api_keys.get(&provider.id)?.clone();

    if api_key.is_empty() {
        debug!("No API key for provider {}", provider.id);
        return None;
    }

    let models = settings.post_process_models();
    let model = models.get(&provider.id).cloned().unwrap_or_default();

    if model.is_empty() {
        debug!("No model selected for provider {}", provider.id);
        return None;
    }

    send_chat_completion(&provider, api_key, &model, prompt.to_string())
        .await
        .ok()
        .flatten()
}

pub async fn send_chat_completion(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    prompt: String,
) -> Result<Option<String>, String> {
    let base_url = provider.base_url.trim_end_matches('/');
    let client = create_client(provider, &api_key)?;

    if provider.id == "anthropic" {
        // Anthropic uses /v1/messages with a different request/response format
        let url = format!("{}/messages", base_url);
        debug!("Sending Anthropic messages request to: {}", url);

        let request_body = serde_json::json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": prompt
            }]
        });

        let response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error response".to_string());
            return Err(format!(
                "API request failed with status {}: {}",
                status, error_text
            ));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse API response: {}", e))?;

        // Anthropic response: { "content": [{ "type": "text", "text": "..." }] }
        let text = body["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .map(|s| s.to_string());

        Ok(text)
    } else {
        // OpenAI-compatible endpoint
        let url = format!("{}/chat/completions", base_url);
        debug!("Sending chat completion request to: {}", url);

        let request_body = ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }],
        };

        let response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error response".to_string());
            return Err(format!(
                "API request failed with status {}: {}",
                status, error_text
            ));
        }

        let completion: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse API response: {}", e))?;

        Ok(completion
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone()))
    }
}

pub async fn fetch_models(settings: &Settings) -> Result<Vec<String>, String> {
    let provider = get_provider(settings).ok_or("No provider configured")?;
    let api_keys = settings.post_process_api_keys();
    let api_key = api_keys.get(&provider.id).cloned().unwrap_or_default();

    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/models", base_url);

    debug!("Fetching models from: {}", url);

    let client = create_client(&provider, &api_key)?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch models: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(format!(
            "Model list request failed ({}): {}",
            status, error_text
        ));
    }

    let parsed: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    let mut models = Vec::new();

    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    } else if let Some(array) = parsed.as_array() {
        for entry in array {
            if let Some(model) = entry.as_str() {
                models.push(model.to_string());
            }
        }
    }

    Ok(models)
}
