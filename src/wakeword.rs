use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use strsim::levenshtein;
use vosk::{CompleteResult, DecodingState, Model, Recognizer};

use crate::dialog::{DialogConfig, DialogManager, DialogOutcome};

#[derive(Debug)]
struct MatchInfo {
    variant: String,
    method: &'static str,
}

#[derive(Debug, Clone)]
struct TtsConfig {
    enabled: bool,
    text: String,
    voice: String,
    speed: u32,
    output_device: String,
    cooldown_ms: u64,
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
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

fn extract_complete_texts(result: CompleteResult<'_>) -> Vec<String> {
    match result {
        CompleteResult::Single(single) => vec![single.text.to_string()],
        CompleteResult::Multiple(multi) => multi
            .alternatives
            .into_iter()
            .map(|a| a.text.to_string())
            .collect(),
    }
}

fn parse_wake_variants(base: &str, aliases_env: &str) -> Vec<String> {
    let mut raw = vec![base.to_string()];
    for alias in aliases_env.split(',') {
        let v = alias.trim();
        if !v.is_empty() {
            raw.push(v.to_string());
        }
    }

    let mut normalized = Vec::<String>::new();
    for v in raw {
        let n = normalize_text(&v);
        if !n.is_empty() && !normalized.iter().any(|x| x == &n) {
            normalized.push(n);
        }
    }
    normalized
}

fn detect_wake_word(text: &str, variants: &[String], max_distance: usize) -> Option<MatchInfo> {
    let norm = normalize_text(text);
    if norm.is_empty() {
        return None;
    }

    for v in variants {
        if norm.contains(v) {
            return Some(MatchInfo {
                variant: v.clone(),
                method: "contains",
            });
        }
    }

    let tokens: Vec<&str> = norm.split_whitespace().collect();
    for v in variants {
        let v_tokens: Vec<&str> = v.split_whitespace().collect();
        if v_tokens.is_empty() {
            continue;
        }

        if v_tokens.len() == 1 {
            let target = v_tokens[0];
            for token in &tokens {
                if levenshtein(token, target) <= max_distance {
                    return Some(MatchInfo {
                        variant: v.clone(),
                        method: "fuzzy-token",
                    });
                }
            }
        } else if tokens.len() >= v_tokens.len() {
            let window = v_tokens.len();
            for i in 0..=(tokens.len() - window) {
                let candidate = tokens[i..i + window].join(" ");
                let threshold = max_distance + (v.len() / 8);
                if levenshtein(&candidate, v) <= threshold {
                    return Some(MatchInfo {
                        variant: v.clone(),
                        method: "fuzzy-phrase",
                    });
                }
            }
        }
    }

    None
}

fn spawn_arecord(alsa_device: &str, sample_rate: u32) -> Result<(Child, ChildStdout)> {
    let mut child = Command::new("arecord")
        .arg("-D")
        .arg(alsa_device)
        .arg("-q")
        .arg("-f")
        .arg("S16_LE")
        .arg("-r")
        .arg(sample_rate.to_string())
        .arg("-c")
        .arg("1")
        .arg("-t")
        .arg("raw")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("arecord konnte nicht gestartet werden")?;

    let stdout = child
        .stdout
        .take()
        .context("arecord stdout nicht verfuegbar")?;

    Ok((child, stdout))
}

fn probe_alsa_device(alsa_device: &str, sample_rate: u32) -> bool {
    Command::new("arecord")
        .arg("-D")
        .arg(alsa_device)
        .arg("-q")
        .arg("-f")
        .arg("S16_LE")
        .arg("-r")
        .arg(sample_rate.to_string())
        .arg("-c")
        .arg("1")
        .arg("-d")
        .arg("1")
        .arg("-t")
        .arg("raw")
        .arg("/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn select_alsa_device(preferred: &str, sample_rate: u32) -> Option<String> {
    let mut candidates = vec![
        preferred.to_string(),
        "default".to_string(),
        "sysdefault".to_string(),
        "plughw:1,0".to_string(),
        "hw:1,0".to_string(),
        "plughw:0,0".to_string(),
        "hw:0,0".to_string(),
    ];
    candidates.dedup();
    candidates
        .into_iter()
        .find(|dev| probe_alsa_device(dev, sample_rate))
}

fn speak_tts_text(config: &TtsConfig, text: &str, debug: bool) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let wav_path = "/tmp/wakeword_tts.wav";

    let status = Command::new("espeak-ng")
        .arg("-v")
        .arg(&config.voice)
        .arg("-s")
        .arg(config.speed.to_string())
        .arg("-w")
        .arg(wav_path)
        .arg(text)
        .status()
        .context("espeak-ng konnte nicht gestartet werden")?;
    if !status.success() {
        anyhow::bail!("espeak-ng ist mit Fehler beendet");
    }

    let play = Command::new("aplay")
        .arg("-q")
        .arg("-D")
        .arg(&config.output_device)
        .arg(wav_path)
        .status()
        .context("aplay konnte nicht gestartet werden")?;
    if !play.success() {
        anyhow::bail!("aplay ist mit Fehler beendet");
    }

    if debug {
        println!(
            "[debug] tts gesprochen voice={} speed={} device={} text=\"{}\"",
            config.voice, config.speed, config.output_device, text
        );
    }
    Ok(())
}

fn speak_tts(config: &TtsConfig, debug: bool) -> Result<()> {
    speak_tts_text(config, &config.text, debug)
}

fn speak_openrouter_answer(config: &TtsConfig, answer: &str, debug: bool) -> Result<()> {
    let max_chars: usize = std::env::var("OPENROUTER_TTS_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(280);
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let speakable: String = trimmed.chars().take(max_chars).collect();
    speak_tts_text(config, &speakable, debug)
}

pub fn run() -> Result<()> {
    let wake_word = std::env::var("WAKE_WORD").unwrap_or_else(|_| "marvin".to_string());
    let wake_word_aliases = std::env::var("WAKE_WORD_ALIASES").unwrap_or_default();
    let wake_fuzzy_distance: usize = std::env::var("WAKE_FUZZY_DISTANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let use_wake_grammar = env_flag("WAKE_USE_GRAMMAR", true);
    let debug_wakeword = env_flag("DEBUG_WAKEWORD", false);
    let wake_cooldown_ms: u64 = std::env::var("WAKE_COOLDOWN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let openrouter_api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    let dialog_config = DialogConfig {
        enabled: !openrouter_api_key.trim().is_empty(),
        api_key: openrouter_api_key,
        model: std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openrouter/free".to_string()),
        base_url: std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1/chat/completions".to_string()),
        site_url: std::env::var("OPENROUTER_SITE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        site_name: std::env::var("OPENROUTER_SITE_NAME")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        system_prompt: std::env::var("OPENROUTER_SYSTEM_PROMPT").unwrap_or_else(|_| {
            "Du bist ein hilfreicher Sprachassistent. Antworte kurz und auf Deutsch.".to_string()
        }),
        question_window_ms: std::env::var("QUESTION_WINDOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8000),
        max_history_turns: std::env::var("OPENROUTER_MAX_HISTORY_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6),
    };

    let model_path = std::env::var("VOSK_MODEL_PATH")
        .unwrap_or_else(|_| "/models/vosk-model-small-de-0.15".to_string());
    let alsa_device = std::env::var("ALSA_DEVICE").unwrap_or_else(|_| "default".to_string());
    let sample_rate_hz: u32 = std::env::var("SAMPLE_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16_000);

    let wake_variants = parse_wake_variants(&wake_word, &wake_word_aliases);

    println!("Wake-Word: {wake_word}");
    println!("Wake-Word Varianten (normalisiert): {:?}", wake_variants);
    println!("Fuzzy-Distanz: {wake_fuzzy_distance}");
    println!("Wake-Cooldown: {wake_cooldown_ms} ms");
    println!("Grammar-Modus: {use_wake_grammar}");
    println!("Debug: {debug_wakeword}");
    println!(
        "OpenRouter: enabled={} model={} question_window_ms={} history_turns={}",
        dialog_config.enabled,
        dialog_config.model,
        dialog_config.question_window_ms,
        dialog_config.max_history_turns
    );
    println!("Vosk model: {model_path}");
    println!("ALSA device (preferred): {alsa_device}");
    println!("Sample rate: {} Hz, channels: 1", sample_rate_hz);

    let model = Model::new(&model_path).context("Vosk-Modell konnte nicht geladen werden")?;
    let mut recognizer = if use_wake_grammar {
        let mut grammar = wake_variants.clone();
        grammar.push("[unk]".to_string());
        Recognizer::new_with_grammar(&model, sample_rate_hz as f32, &grammar)
            .context("Recognizer (grammar) konnte nicht erstellt werden")?
    } else {
        Recognizer::new(&model, sample_rate_hz as f32)
            .context("Recognizer konnte nicht erstellt werden")?
    };
    recognizer.set_max_alternatives(5);
    let mut question_recognizer = Recognizer::new(&model, sample_rate_hz as f32)
        .context("Recognizer (question) konnte nicht erstellt werden")?;
    question_recognizer.set_max_alternatives(3);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .context("Ctrl+C Handler konnte nicht gesetzt werden")?;

    let selected_alsa_device = select_alsa_device(&alsa_device, sample_rate_hz).context(
        "Kein verwendbares ALSA-Eingabegeraet gefunden. Setze ALSA_DEVICE (z.B. hw:1,0).",
    )?;
    println!("ALSA device (aktiv): {selected_alsa_device}");

    let tts_output_device = std::env::var("ALSA_OUTPUT_DEVICE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| selected_alsa_device.clone());
    let tts_config = TtsConfig {
        enabled: env_flag("TTS_ENABLED", true),
        text: std::env::var("TTS_TEXT").unwrap_or_else(|_| "ja hallo hier bin ich".to_string()),
        voice: std::env::var("TTS_VOICE").unwrap_or_else(|_| "de".to_string()),
        speed: std::env::var("TTS_SPEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(165),
        output_device: tts_output_device,
        cooldown_ms: std::env::var("TTS_COOLDOWN_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000),
    };
    println!(
        "TTS: enabled={} voice={} speed={} output_device={} cooldown_ms={}",
        tts_config.enabled,
        tts_config.voice,
        tts_config.speed,
        tts_config.output_device,
        tts_config.cooldown_ms
    );

    let (mut arecord_child, mut arecord_out) =
        spawn_arecord(&selected_alsa_device, sample_rate_hz)?;

    let mut bytes = vec![0u8; 4096];
    let mut last_partial = String::new();
    let mut chunk_count: u64 = 0;
    let mut last_tts: Option<Instant> = None;
    let mut last_wake: Option<Instant> = None;
    let mut dialog = DialogManager::new(dialog_config);
    let openrouter_http = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("HTTP-Client fuer OpenRouter konnte nicht erstellt werden")?;

    println!("Laeuft. Sprich das Wake-Word und beende mit Ctrl+C.");
    while running.load(Ordering::SeqCst) {
        let read = match arecord_out.read(&mut bytes) {
            Ok(0) => {
                eprintln!("arecord liefert keine Daten mehr");
                break;
            }
            Ok(n) => n,
            Err(err) => {
                eprintln!("arecord read Fehler: {err}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        let mut frame_mono_i16 = Vec::with_capacity(read / 2);
        for chunk in bytes[..read].chunks_exact(2) {
            frame_mono_i16.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }

        chunk_count += 1;
        if debug_wakeword && chunk_count % 25 == 0 {
            let avg_abs = if frame_mono_i16.is_empty() {
                0.0
            } else {
                frame_mono_i16
                    .iter()
                    .map(|s| (*s as i32).unsigned_abs() as f32)
                    .sum::<f32>()
                    / frame_mono_i16.len() as f32
            };
            println!("[debug] chunk={chunk_count} avg_abs={avg_abs:.1}");
        }

        match recognizer.accept_waveform(&frame_mono_i16) {
            Ok(DecodingState::Finalized) => {
                let finals = extract_complete_texts(recognizer.result());
                for text in finals {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let matched = detect_wake_word(trimmed, &wake_variants, wake_fuzzy_distance);
                    if debug_wakeword {
                        println!(
                            "[debug] final=\"{}\" normalized=\"{}\" matched={}",
                            trimmed,
                            normalize_text(trimmed),
                            matched.is_some()
                        );
                    }
                    if let Some(info) = matched {
                        if dialog.is_active() {
                            if debug_wakeword {
                                println!("[debug] wake ignoriert (dialog aktiv)");
                            }
                            continue;
                        }
                        let allow_wake = last_wake
                            .map(|ts| ts.elapsed() >= Duration::from_millis(wake_cooldown_ms))
                            .unwrap_or(true);
                        if !allow_wake {
                            if debug_wakeword {
                                println!("[debug] wake cooldown aktiv");
                            }
                            continue;
                        }
                        last_wake = Some(Instant::now());
                        println!(
                            "WAKE WORD erkannt: variant=\"{}\" methode={} text=\"{}\"",
                            info.variant, info.method, trimmed
                        );
                        dialog.begin_question_window();
                        question_recognizer.reset();
                        if debug_wakeword {
                            println!(
                                "[debug] fragefenster aktiv fuer {} ms",
                                dialog.config().question_window_ms
                            );
                        }
                        let allow_tts = last_tts
                            .map(|ts| ts.elapsed() >= Duration::from_millis(tts_config.cooldown_ms))
                            .unwrap_or(true);
                        if allow_tts {
                            if let Err(err) = speak_tts(&tts_config, debug_wakeword) {
                                eprintln!("TTS Fehler: {err}");
                            } else {
                                last_tts = Some(Instant::now());
                            }
                        } else if debug_wakeword {
                            println!("[debug] tts cooldown aktiv");
                        }
                        break;
                    }
                }
            }
            Ok(DecodingState::Running) => {
                let partial = recognizer.partial_result();
                let partial_text = partial.partial.trim();
                if !partial_text.is_empty() {
                    if debug_wakeword && partial_text != last_partial {
                        println!(
                            "[debug] partial=\"{}\" normalized=\"{}\"",
                            partial_text,
                            normalize_text(partial_text)
                        );
                        last_partial = partial_text.to_string();
                    }
                    if let Some(info) =
                        detect_wake_word(partial_text, &wake_variants, wake_fuzzy_distance)
                    {
                        if dialog.is_active() {
                            if debug_wakeword {
                                println!("[debug] wake ignoriert (dialog aktiv)");
                            }
                            continue;
                        }
                        let allow_wake = last_wake
                            .map(|ts| ts.elapsed() >= Duration::from_millis(wake_cooldown_ms))
                            .unwrap_or(true);
                        if !allow_wake {
                            if debug_wakeword {
                                println!("[debug] wake cooldown aktiv");
                            }
                            continue;
                        }
                        last_wake = Some(Instant::now());
                        println!(
                            "WAKE WORD erkannt: variant=\"{}\" methode={} text=\"{}\"",
                            info.variant, info.method, partial_text
                        );
                        dialog.begin_question_window();
                        question_recognizer.reset();
                        if debug_wakeword {
                            println!(
                                "[debug] fragefenster aktiv fuer {} ms",
                                dialog.config().question_window_ms
                            );
                        }
                        let allow_tts = last_tts
                            .map(|ts| ts.elapsed() >= Duration::from_millis(tts_config.cooldown_ms))
                            .unwrap_or(true);
                        if allow_tts {
                            if let Err(err) = speak_tts(&tts_config, debug_wakeword) {
                                eprintln!("TTS Fehler: {err}");
                            } else {
                                last_tts = Some(Instant::now());
                            }
                        } else if debug_wakeword {
                            println!("[debug] tts cooldown aktiv");
                        }
                    }
                }
            }
            Ok(DecodingState::Failed) => {
                eprintln!("Recognizer meldet DecodingState::Failed");
            }
            Err(err) => {
                eprintln!("accept_waveform Fehler: {err}");
            }
        }

        if dialog.is_active() {
            match question_recognizer.accept_waveform(&frame_mono_i16) {
                Ok(DecodingState::Finalized) => {
                    match dialog.process_final(question_recognizer.result(), &openrouter_http) {
                        Ok(DialogOutcome::None) => {}
                        Ok(DialogOutcome::Exited { phrase }) => {
                            println!("Dialog beendet: \"{}\"", phrase);
                            question_recognizer.reset();
                            if let Err(err) = speak_tts_text(
                                &tts_config,
                                "Alles klar, bis spaeter.",
                                debug_wakeword,
                            ) {
                                eprintln!("TTS Fehler (Dialog-Ende): {err}");
                            }
                        }
                        Ok(DialogOutcome::Answered { question, answer }) => {
                            println!("Frage erkannt: \"{}\"", question);
                            println!("OpenRouter: {}", answer);
                            if let Err(err) =
                                speak_openrouter_answer(&tts_config, &answer, debug_wakeword)
                            {
                                eprintln!("TTS Fehler (OpenRouter): {err}");
                            }
                        }
                        Err(err) => {
                            eprintln!("OpenRouter Fehler: {err}");
                        }
                    }
                }
                Ok(DecodingState::Running) => {
                    let partial = question_recognizer.partial_result();
                    let partial_text = partial.partial.trim();
                    let changed = dialog.observe_partial(partial_text);
                    if changed && debug_wakeword {
                        println!(
                            "[debug] frage-partial=\"{}\" normalized=\"{}\"",
                            partial_text,
                            normalize_text(partial_text)
                        );
                    }
                }
                Ok(DecodingState::Failed) => {
                    eprintln!("Question recognizer meldet DecodingState::Failed");
                }
                Err(err) => {
                    eprintln!("question accept_waveform Fehler: {err}");
                }
            }
        } else if dialog.is_window_expired() {
            dialog.clear_activity();
            question_recognizer.reset();
            if debug_wakeword {
                println!("[debug] fragefenster beendet");
            }
        }
    }

    let _ = arecord_child.kill();
    let _ = arecord_child.wait();
    println!("Beendet.");
    Ok(())
}
