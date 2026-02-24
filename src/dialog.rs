use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::json;
use vosk::CompleteResult;

#[derive(Debug, Clone)]
pub struct DialogConfig {
    pub enabled: bool,
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub site_url: Option<String>,
    pub site_name: Option<String>,
    pub system_prompt: String,
    pub question_window_ms: u64,
    pub max_history_turns: usize,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

impl ChatMessage {
    fn user(content: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: content.to_string(),
        }
    }

    fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum DialogOutcome {
    None,
    Answered { question: String, answer: String },
    Exited { phrase: String },
}

#[derive(Debug)]
pub struct DialogManager {
    cfg: DialogConfig,
    chat_mode: bool,
    question_mode_until: Option<Instant>,
    history: Vec<ChatMessage>,
    last_partial: String,
}

impl DialogManager {
    pub fn new(cfg: DialogConfig) -> Self {
        Self {
            cfg,
            chat_mode: false,
            question_mode_until: None,
            history: Vec::new(),
            last_partial: String::new(),
        }
    }

    pub fn config(&self) -> &DialogConfig {
        &self.cfg
    }

    pub fn is_active(&self) -> bool {
        self.chat_mode
            || self
                .question_mode_until
                .map(|deadline| Instant::now() <= deadline)
                .unwrap_or(false)
    }

    pub fn is_window_expired(&self) -> bool {
        self.question_mode_until.is_some() && !self.is_active()
    }

    pub fn begin_question_window(&mut self) {
        self.chat_mode = false;
        self.history.clear();
        self.question_mode_until =
            Some(Instant::now() + std::time::Duration::from_millis(self.cfg.question_window_ms));
        self.last_partial.clear();
    }

    pub fn clear_activity(&mut self) {
        self.question_mode_until = None;
        self.chat_mode = false;
        self.history.clear();
        self.last_partial.clear();
    }

    pub fn observe_partial(&mut self, partial: &str) -> bool {
        let p = partial.trim();
        if p.is_empty() {
            return false;
        }
        if p != self.last_partial {
            self.last_partial = p.to_string();
            return true;
        }
        false
    }

    pub fn process_final(
        &mut self,
        result: CompleteResult<'_>,
        client: &Client,
    ) -> Result<DialogOutcome> {
        let candidates = extract_candidates(result);
        let mut chosen = choose_best_candidate(&candidates);
        if chosen.is_none() && !self.last_partial.is_empty() {
            chosen = Some(self.last_partial.clone());
        }
        let Some(text) = chosen else {
            return Ok(DialogOutcome::None);
        };
        let utterance = text.trim().to_string();
        if utterance.is_empty() {
            return Ok(DialogOutcome::None);
        }

        if self.chat_mode && is_exit_phrase(&utterance) {
            self.clear_activity();
            return Ok(DialogOutcome::Exited { phrase: utterance });
        }

        let tokens = token_count(&utterance);
        let should_send = if self.chat_mode {
            tokens >= 2 && !is_short_ack(&utterance)
        } else {
            looks_like_question(&utterance) || tokens >= 4
        };
        if !should_send {
            return Ok(DialogOutcome::None);
        }

        if !self.cfg.enabled {
            return Ok(DialogOutcome::None);
        }

        let answer = ask_openrouter(client, &self.cfg, &self.history, &utterance)?;
        self.history.push(ChatMessage::user(&utterance));
        self.history.push(ChatMessage::assistant(&answer));
        while self.history.len() > self.cfg.max_history_turns.saturating_mul(2) {
            self.history.remove(0);
        }
        self.chat_mode = true;
        self.question_mode_until = None;
        self.last_partial.clear();
        Ok(DialogOutcome::Answered {
            question: utterance,
            answer,
        })
    }
}

fn extract_candidates(result: CompleteResult<'_>) -> Vec<(String, f32)> {
    match result {
        CompleteResult::Single(single) => {
            let t = single.text.trim().to_string();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![(t, 0.5)]
            }
        }
        CompleteResult::Multiple(multi) => multi
            .alternatives
            .into_iter()
            .filter_map(|a| {
                let t = a.text.trim();
                if t.is_empty() {
                    None
                } else {
                    Some((t.to_string(), a.confidence))
                }
            })
            .collect(),
    }
}

fn choose_best_candidate(candidates: &[(String, f32)]) -> Option<String> {
    candidates
        .iter()
        .max_by(|(ta, ca), (tb, cb)| {
            score_candidate(ta, *ca)
                .partial_cmp(&score_candidate(tb, *cb))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(t, _)| t.clone())
}

fn score_candidate(text: &str, confidence: f32) -> f32 {
    let norm = normalize_text(text);
    let tokens = norm.split_whitespace().count() as f32;
    let question_bonus = if looks_like_question(text) { 2.5 } else { 0.0 };
    let length_bonus = (norm.len().min(80) as f32) / 40.0;
    (confidence * 3.0) + tokens + question_bonus + length_bonus
}

fn ask_openrouter(
    client: &Client,
    cfg: &DialogConfig,
    history: &[ChatMessage],
    question: &str,
) -> Result<String> {
    let mut headers = HeaderMap::new();
    let auth = format!("Bearer {}", cfg.api_key);
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&auth).context("ungueltiger OpenRouter API-Key Header")?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(site_url) = &cfg.site_url {
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_str(site_url).context("ungueltiger HTTP-Referer Header")?,
        );
    }
    if let Some(site_name) = &cfg.site_name {
        headers.insert(
            "X-Title",
            HeaderValue::from_str(site_name).context("ungueltiger X-Title Header")?,
        );
    }

    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
    for msg in history {
        messages.push(json!({ "role": msg.role, "content": msg.content }));
    }
    messages.push(json!({ "role": "user", "content": question }));

    let body = json!({
        "model": cfg.model,
        "messages": messages,
    });

    let response = client
        .post(&cfg.base_url)
        .headers(headers)
        .json(&body)
        .send()
        .context("OpenRouter Request fehlgeschlagen")?;
    let status = response.status();
    let response_text = response
        .text()
        .context("OpenRouter Antwort konnte nicht gelesen werden")?;
    if !status.is_success() {
        anyhow::bail!(
            "OpenRouter API Status {}: {}",
            status.as_u16(),
            response_text
        );
    }
    let value: serde_json::Value = serde_json::from_str(&response_text)
        .context("OpenRouter Antwort ist kein gueltiges JSON")?;

    let answer = value
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .context("OpenRouter Antwort enthaelt keinen Text in choices[0].message.content")?;

    Ok(answer)
}

fn token_count(text: &str) -> usize {
    normalize_text(text).split_whitespace().count()
}

fn is_short_ack(text: &str) -> bool {
    matches!(
        normalize_text(text).as_str(),
        "ja" | "okay" | "ok" | "gut" | "alles klar" | "danke"
    )
}

fn is_exit_phrase(text: &str) -> bool {
    let norm = normalize_text(text);
    norm.contains("danke das wars")
        || norm.contains("danke das war es")
        || norm.contains("das wars danke")
        || norm.contains("danke war s")
}

fn looks_like_question(text: &str) -> bool {
    let raw = text.trim();
    if raw.is_empty() {
        return false;
    }
    if raw.ends_with('?') {
        return true;
    }

    let norm = normalize_text(raw);
    let mut tokens = norm.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };

    if matches!(
        first,
        "wer"
            | "wie"
            | "was"
            | "wann"
            | "wo"
            | "warum"
            | "wieso"
            | "weshalb"
            | "welche"
            | "welcher"
            | "welches"
            | "kann"
            | "kannst"
            | "koenntest"
            | "ist"
            | "sind"
            | "gibt"
            | "erklaere"
            | "erklaer"
    ) {
        return true;
    }

    norm.contains("kannst du") || norm.contains("weisst du")
}

fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | '0'..='9' | ' ' => out.push(ch),
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
