use std::{
    env, fs,
    io::{self, Write, stdout},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

const MODEL_REFRESH_INTERVAL_SECS: i64 = 3 * 24 * 60 * 60;

#[derive(Deserialize, Serialize)]
struct Config {
    api_key: String,
    model: String,
    #[serde(default = "default_models")]
    models: Vec<String>,
    #[serde(default)]
    models_updated_at: Option<i64>,
}

#[derive(Deserialize)]
struct ModelCatalog {
    #[serde(default)]
    models: Vec<ModelInfo>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct ModelInfo {
    name: String,
    #[serde(rename = "supportedGenerationMethods", default)]
    supported_generation_methods: Vec<String>,
}

struct CliOptions {
    config_path: Option<PathBuf>,
    model: Option<String>,
    initialize: bool,
    help: bool,
}

struct App {
    config: Config,
    config_path: PathBuf,
}

enum InputAction {
    Submit(String),
    SelectModel(String),
    Quit,
}

fn main() -> Result<()> {
    let options = parse_args(env::args().skip(1))?;
    if options.help {
        println!(
            "Usage: gemini-terminal [--config PATH] [--model ID] [--init]\n\n\
                 --init       Create a private config file at the standard XDG path\n\
                 --model ID   Start with one of the configured models\n\
                 --config     Read a config file from a custom path"
        );
        return Ok(());
    }
    let config_path = match options.config_path {
        Some(path) => path,
        None => default_config_path()?,
    };
    if options.initialize {
        initialize_config(&config_path)?;
        println!(
            "Created {}. Add your Gemini API key, then run gemini-terminal.",
            config_path.display()
        );
        return Ok(());
    }

    let config_text = fs::read_to_string(&config_path).with_context(|| {
        format!(
            "Could not read config file: {} (run `gemini-terminal --init` to create it)",
            config_path.display()
        )
    })?;
    let mut config: Config = toml::from_str(&config_text)
        .with_context(|| format!("Could not parse config file: {}", config_path.display()))?;
    if config.api_key.trim().is_empty() || config.model.trim().is_empty() {
        bail!("Both `api_key` and `model` must be set in the config file");
    }
    if config.models.iter().any(|model| model.trim().is_empty()) {
        bail!("The `models` array cannot contain an empty model name");
    }

    let mut status = String::from("Ready");
    let now = unix_time_now();
    if should_refresh_models(config.models_updated_at, now) {
        match list_available_models(&config.api_key) {
            Ok(models) if !models.is_empty() => {
                config.models = models;
                config.models_updated_at = Some(now);
                status = match save_config(&config_path, &config) {
                    Ok(()) => String::from("Model list refreshed from Google"),
                    Err(_) => String::from("Model list refreshed; could not save cache"),
                };
            }
            Ok(_) => status = String::from("Google returned no text models; using cached list"),
            Err(_) => status = String::from("Could not refresh models; using cached list"),
        }
    }
    if config.models.is_empty() {
        config.models = default_models();
        status = String::from("No cached models; using built-in model list");
    }

    if let Some(model) = options.model {
        if !config.models.contains(&model) {
            bail!("Model `{model}` is not available in the cached model list");
        }
        persist_model_selection(&config_path, &mut config, &model)?;
        status = String::from("Selected model saved");
    } else if !config.models.contains(&config.model) {
        let fallback = config.models[0].clone();
        persist_model_selection(&config_path, &mut config, &fallback)?;
        status = String::from("Saved model unavailable; selected the first available model");
    }

    run_app(config, config_path, status)
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<CliOptions> {
    let mut options = CliOptions {
        config_path: None,
        model: None,
        initialize: false,
        help: false,
    };
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--config" => {
                options.config_path = Some(PathBuf::from(
                    args.next().context("Expected a path after --config")?,
                ));
            }
            "--model" => {
                options.model = Some(args.next().context("Expected a model ID after --model")?);
            }
            "--init" => options.initialize = true,
            "-h" | "--help" => options.help = true,
            _ => bail!(
                "Unknown argument: {argument}\nUsage: gemini-terminal [--config PATH] [--model ID] [--init]"
            ),
        }
    }
    Ok(options)
}

fn default_config_path() -> Result<PathBuf> {
    let config_home = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").context("Could not determine home directory")?)
            .join(".config"),
    };
    Ok(config_home.join("gemini-terminal").join("config.toml"))
}

fn initialize_config(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("Config already exists: {}", path.display());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let mut file_options = fs::OpenOptions::new();
    file_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file_options.mode(0o600);
    }
    file_options
        .open(path)
        .with_context(|| format!("Could not create config file: {}", path.display()))?
        .write_all(CONFIG_TEMPLATE.as_bytes())?;
    Ok(())
}

const CONFIG_TEMPLATE: &str = r#"api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3.6-flash"
models = ["gemini-3.5-flash-lite", "gemini-3.6-flash"]
"#;

fn default_models() -> Vec<String> {
    vec![
        String::from("gemini-3.5-flash-lite"),
        String::from("gemini-3.6-flash"),
    ]
}

fn unix_time_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn should_refresh_models(last_updated: Option<i64>, now: i64) -> bool {
    match last_updated {
        Some(last_updated) if last_updated <= now => {
            now.saturating_sub(last_updated) >= MODEL_REFRESH_INTERVAL_SECS
        }
        _ => true,
    }
}

fn list_available_models(api_key: &str) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| String::from("Could not initialize Gemini client"))?;
    let mut page_token: Option<String> = None;
    let mut models = Vec::new();

    loop {
        let mut request = client
            .get("https://generativelanguage.googleapis.com/v1beta/models")
            .header("x-goog-api-key", api_key)
            .query(&[("pageSize", "1000")]);
        if let Some(token) = &page_token {
            request = request.query(&[("pageToken", token)]);
        }
        let response = request
            .send()
            .map_err(|_| String::from("Could not reach Gemini model catalog"))?;
        if !response.status().is_success() {
            return Err(String::from("Gemini model catalog request failed"));
        }
        let catalog: ModelCatalog = response
            .json()
            .map_err(|_| String::from("Gemini returned an unreadable model catalog"))?;
        models.extend(text_generation_models(catalog.models));
        page_token = catalog.next_page_token.filter(|token| !token.is_empty());
        if page_token.is_none() {
            break;
        }
    }

    models.sort();
    models.dedup();
    Ok(models)
}

fn text_generation_models(models: Vec<ModelInfo>) -> Vec<String> {
    let mut names = models
        .into_iter()
        .filter(|model| {
            model
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
        })
        .map(|model| {
            model
                .name
                .strip_prefix("models/")
                .unwrap_or(&model.name)
                .to_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn save_config(path: &Path, config: &Config) -> Result<()> {
    let serialized = toml::to_string_pretty(config).context("Could not serialize config")?;
    let file_name = path
        .file_name()
        .context("Config path must include a file name")?
        .to_string_lossy();
    let temporary_path = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| -> Result<()> {
        let mut file = options
            .open(&temporary_path)
            .with_context(|| format!("Could not create {}", temporary_path.display()))?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)
            .with_context(|| format!("Could not update {}", path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

fn persist_model_selection(path: &Path, config: &mut Config, model: &str) -> Result<()> {
    if !config.models.iter().any(|available| available == model) {
        bail!("Model `{model}` is not available in the cached model list");
    }
    let previous_model = std::mem::replace(&mut config.model, model.to_owned());
    if let Err(error) = save_config(path, config) {
        config.model = previous_model;
        return Err(error);
    }
    Ok(())
}

fn run_app(config: Config, config_path: PathBuf, status: String) -> Result<()> {
    let mut app = App {
        config,
        config_path,
    };
    println!(
        "Gemini Terminal | {} | F2: models | Ctrl+C: quit",
        app.config.model
    );
    if status != "Ready" {
        println!("{status}");
    }

    let mut prompt = String::new();
    loop {
        print!("\n------------\nPrompt: {prompt}");
        stdout().flush()?;
        match read_prompt(std::mem::take(&mut prompt))? {
            InputAction::Quit => return Ok(()),
            InputAction::SelectModel(unfinished_prompt) => {
                prompt = unfinished_prompt;
                choose_model(&mut app)?;
            }
            InputAction::Submit(prompt) if prompt.trim().is_empty() => {
                println!("Prompt cannot be empty.\n");
            }
            InputAction::Submit(prompt) => {
                print!("Response: ");
                stdout().flush()?;
                let response =
                    match generate_content(&app.config.api_key, &app.config.model, &prompt) {
                        Ok(response) => response,
                        Err(error) => format!("Error: {error}"),
                    };
                print!("{response}");
                if !response.ends_with('\n') {
                    println!();
                }
                println!();
            }
        }
    }
}

fn read_prompt(prompt: String) -> Result<InputAction> {
    enable_raw_mode().context("Could not enable terminal input mode")?;
    let input = read_prompt_raw(prompt);
    let restore = disable_raw_mode().context("Could not restore terminal input mode");
    restore?;
    input
}

fn read_prompt_raw(mut prompt: String) -> Result<InputAction> {
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match key.code {
            KeyCode::F(2) => {
                print!("\r\n");
                stdout().flush()?;
                return Ok(InputAction::SelectModel(prompt));
            }
            KeyCode::Enter => {
                print!("\r\n");
                stdout().flush()?;
                return Ok(InputAction::Submit(prompt));
            }
            KeyCode::Esc | KeyCode::Char('d')
                if key.code == KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                print!("\r\n");
                stdout().flush()?;
                return Ok(InputAction::Quit);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                print!("\r\n^C\r\n");
                stdout().flush()?;
                return Ok(InputAction::Quit);
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                prompt.push(character);
                print!("{character}");
                stdout().flush()?;
            }
            KeyCode::Backspace => {
                if let Some(character) = prompt.pop() {
                    let width = UnicodeWidthChar::width(character).unwrap_or(0);
                    if width > 0 {
                        print!("\x08{}\x08", " ".repeat(width));
                        stdout().flush()?;
                    }
                }
            }
            _ => {}
        }
    }
}

fn choose_model(app: &mut App) -> Result<()> {
    println!("\nAvailable models:");
    for (index, model) in app.config.models.iter().enumerate() {
        let marker = if model == &app.config.model {
            " (current)"
        } else {
            ""
        };
        println!("  {}. {model}{marker}", index + 1);
    }
    print!("Select a number, or press Enter to cancel: ");
    stdout().flush()?;
    let mut selection = String::new();
    io::stdin().read_line(&mut selection)?;
    let selection = selection.trim();
    if selection.is_empty() {
        return Ok(());
    }
    let index = selection
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .filter(|index| *index < app.config.models.len());
    let Some(index) = index else {
        println!("Invalid model number; keeping {}.\n", app.config.model);
        return Ok(());
    };

    let model = app.config.models[index].clone();
    match persist_model_selection(&app.config_path, &mut app.config, &model) {
        Ok(()) => println!("Using {model}.\n"),
        Err(_) => println!("Could not save selection; keeping {}.\n", app.config.model),
    }
    Ok(())
}

fn generate_content(api_key: &str, model: &str, prompt: &str) -> Result<String, String> {
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(url)
        .header("x-goog-api-key", api_key)
        .json(&serde_json::json!({
            "contents": [{ "parts": [{ "text": prompt }] }]
        }))
        .send()
        .map_err(|error| format!("Could not reach Gemini: {error}"))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .map_err(|error| format!("Gemini returned an unreadable response: {error}"))?;
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("No error details were provided.");
        return Err(format!("Gemini API error ({}): {message}", status.as_u16()));
    }

    extract_text(&body)
        .ok_or_else(|| String::from("Gemini returned no text. The response may have been blocked."))
}

fn extract_text(body: &serde_json::Value) -> Option<String> {
    body.pointer("/candidates/0/content/parts")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        Config, ModelCatalog, extract_text, generate_content, persist_model_selection, save_config,
        should_refresh_models, text_generation_models, unix_time_now,
    };
    use std::{env, fs};

    #[test]
    fn reads_key_and_model_from_toml() {
        let config: Config =
            toml::from_str("api_key = \"test-key\"\nmodel = \"gemini-test\"\n").unwrap();

        assert_eq!(config.api_key, "test-key");
        assert_eq!(config.model, "gemini-test");
        assert_eq!(config.models, ["gemini-3.5-flash-lite", "gemini-3.6-flash"]);
    }

    #[test]
    fn selected_model_is_persisted_to_config() {
        let directory = env::temp_dir().join(format!(
            "gemini-terminal-selection-test-{}-{}",
            std::process::id(),
            unix_time_now()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let mut config = Config {
            api_key: String::from("test-key"),
            model: String::from("gemini-3.5-flash-lite"),
            models: vec![
                String::from("gemini-3.5-flash-lite"),
                String::from("gemini-3.6-flash"),
            ],
            models_updated_at: Some(1234),
        };

        persist_model_selection(&path, &mut config, "gemini-3.6-flash").unwrap();
        let saved: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(saved.model, "gemini-3.6-flash");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn joins_text_parts_from_gemini_response() {
        let body = serde_json::json!({
            "candidates": [{
                "content": { "parts": [{ "text": "First" }, { "text": "Second" }] }
            }]
        });

        assert_eq!(extract_text(&body).as_deref(), Some("First\nSecond"));
    }

    #[test]
    fn catalog_keeps_only_models_that_generate_content() {
        let catalog: ModelCatalog = serde_json::from_value(serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-z",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/gemini-embedding",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/gemini-a",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/gemini-z",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ],
            "nextPageToken": "page-two"
        }))
        .unwrap();

        assert_eq!(
            text_generation_models(catalog.models),
            ["gemini-a", "gemini-z"]
        );
        assert_eq!(catalog.next_page_token.as_deref(), Some("page-two"));
    }

    #[test]
    fn refreshes_models_when_missing_or_at_least_three_days_old() {
        let now = 10_000_000;
        assert!(should_refresh_models(None, now));
        assert!(!should_refresh_models(
            Some(now - 3 * 24 * 60 * 60 + 1),
            now
        ));
        assert!(should_refresh_models(Some(now - 3 * 24 * 60 * 60), now));
        assert!(should_refresh_models(Some(now + 1), now));
    }

    #[test]
    fn saves_refreshed_model_ids_and_timestamp_to_toml() {
        let directory = env::temp_dir().join(format!(
            "gemini-terminal-test-{}-{}",
            std::process::id(),
            unix_time_now()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let config = Config {
            api_key: String::from("test-key"),
            model: String::from("gemini-3.6-flash"),
            models: vec![String::from("gemini-3.6-flash")],
            models_updated_at: Some(1234),
        };

        save_config(&path, &config).unwrap();
        let saved: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(saved.models, ["gemini-3.6-flash"]);
        assert_eq!(saved.models_updated_at, Some(1234));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "makes a real Gemini API request; requires GEMINI_TEST_CONFIG"]
    fn gemini_live_request_returns_text() {
        let config_path = env::var("GEMINI_TEST_CONFIG")
            .expect("set GEMINI_TEST_CONFIG to a TOML config file path");
        let config_text = fs::read_to_string(config_path).expect("could not read test config");
        let config: Config = toml::from_str(&config_text).expect("could not parse test config");
        let model = env::var("GEMINI_TEST_MODEL").unwrap_or(config.model);

        let response = generate_content(
            &config.api_key,
            &model,
            "Reply with exactly this text and nothing else: GEMINI_CONNECTION_OK",
        )
        .unwrap_or_else(|_| {
            panic!("Gemini request failed; details omitted to protect credentials")
        });

        assert!(response.contains("GEMINI_CONNECTION_OK"));
    }

    #[test]
    #[ignore = "makes a real Gemini API request; requires GEMINI_TEST_CONFIG"]
    fn gemini_live_catalog_contains_generation_models() {
        let config_path = env::var("GEMINI_TEST_CONFIG")
            .expect("set GEMINI_TEST_CONFIG to a TOML config file path");
        let config_text = fs::read_to_string(config_path).expect("could not read test config");
        let config: Config = toml::from_str(&config_text).expect("could not parse test config");
        let models = super::list_available_models(&config.api_key)
            .unwrap_or_else(|_| panic!("Gemini model catalog request failed; details omitted"));

        assert!(!models.is_empty());
        assert!(models.iter().all(|model| !model.starts_with("models/")));
    }
}
