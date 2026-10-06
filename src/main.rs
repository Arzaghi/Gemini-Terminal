use std::{
    collections::VecDeque,
    env, fs,
    io::{self, Write, stdout},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::MoveTo,
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use eframe::egui::{self, Align, FontData, FontDefinitions, FontFamily, Key, Layout, RichText};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

const MODEL_REFRESH_INTERVAL_SECS: i64 = 3 * 24 * 60 * 60;
const MAX_RESPONSE_CHAR_DELAY_MS: u64 = 60_000;

#[derive(Deserialize, Serialize)]
struct Config {
    api_key: String,
    model: String,
    #[serde(default = "default_models")]
    models: Vec<String>,
    #[serde(default)]
    models_updated_at: Option<i64>,
    #[serde(default)]
    font_path: String,
    #[serde(default)]
    right_to_left: bool,
    #[serde(default = "default_response_char_delay_ms")]
    response_char_delay_ms: u64,
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
    gui: bool,
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

struct GeminiApp {
    config: Config,
    config_path: PathBuf,
    status: String,
    page: Page,
    prompt: String,
    messages: Vec<Message>,
    sending: bool,
    response_receiver: Option<Receiver<Result<String, String>>>,
    typing_response: Option<VecDeque<char>>,
    next_character_at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Options,
}

struct Message {
    from_user: bool,
    text: String,
}

fn main() -> Result<()> {
    let options = parse_args(env::args().skip(1))?;
    if options.help {
        println!(
            "Usage: gemini-terminal [--gui] [--config PATH] [--model ID] [--init]\n\n\
                 --init       Create a private config file at the standard XDG path\n\
                  --gui        Launch the desktop GUI instead of the terminal app\n\
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
    if config.api_key == "YOUR_GEMINI_API_KEY" {
        config.api_key.clear();
    }
    if config.model.trim().is_empty() {
        bail!("The `model` must be set in the config file");
    }
    if config.models.iter().any(|model| model.trim().is_empty()) {
        bail!("The `models` array cannot contain an empty model name");
    }
    if config.response_char_delay_ms > MAX_RESPONSE_CHAR_DELAY_MS {
        bail!("`response_char_delay_ms` must not exceed {MAX_RESPONSE_CHAR_DELAY_MS}");
    }

    let mut status = String::from("Ready");
    let now = unix_time_now();
    if !config.api_key.is_empty() && should_refresh_models(config.models_updated_at, now) {
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
        status = if config.api_key.is_empty() {
            String::from("Add your Gemini API key in Options")
        } else {
            String::from("No cached models; using built-in model list")
        };
    } else if config.api_key.is_empty() {
        status = String::from("Add your Gemini API key in Options");
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

    if options.gui && config.font_path.is_empty() {
        config.font_path = default_persian_font_path().unwrap_or_default();
    }

    if options.gui {
        run_gui(config, config_path, status)
    } else {
        if config.api_key.trim().is_empty() {
            bail!(
                "Set `api_key` in the config file or run `gemini-terminal --gui` to enter it in Options"
            );
        }
        run_terminal(config, config_path, status)
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<CliOptions> {
    let mut options = CliOptions {
        config_path: None,
        model: None,
        initialize: false,
        gui: false,
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
            "--gui" => options.gui = true,
            "-h" | "--help" => options.help = true,
            _ => bail!(
                "Unknown argument: {argument}\nUsage: gemini-terminal [--gui] [--config PATH] [--model ID] [--init]"
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
response_char_delay_ms = 5
"#;

fn default_response_char_delay_ms() -> u64 {
    5
}

fn print_response_with_delay(response: &str, delay_ms: u64) -> io::Result<()> {
    let mut output = stdout().lock();
    if delay_ms == 0 {
        write!(output, "  {}", response.replace('\n', "\n  "))?;
        if !response.ends_with('\n') {
            writeln!(output)?;
        }
        return writeln!(output);
    }

    write!(output, "  ")?;
    let mut characters = response.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\n' {
            write!(output, "\n  ")?;
        } else {
            write!(output, "{character}")?;
        }
        output.flush()?;
        if characters.peek().is_some() && delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    if !response.ends_with('\n') {
        writeln!(output)?;
    }
    writeln!(output)
}

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

fn run_terminal(config: Config, config_path: PathBuf, status: String) -> Result<()> {
    let mut app = App {
        config,
        config_path,
    };
    println!(
        "Gemini Terminal | {} | Enter: send | Ctrl+J: newline | Ctrl+L: clear | F2: models | Ctrl+C: quit",
        app.config.model
    );
    if status != "Ready" {
        println!("{status}");
    }

    let mut prompt = String::new();
    loop {
        print!("\n\nPrompt:\n  {}", prompt.replace('\n', "\n  "));
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
                print!("Waiting for Gemini... |");
                stdout().flush()?;
                let api_key = app.config.api_key.clone();
                let model = app.config.model.clone();
                let (sender, receiver) = mpsc::channel();
                thread::spawn(move || {
                    let _ = sender.send(generate_content(&api_key, &model, &prompt));
                });
                let frames = ['|', '/', '-', '\\'];
                let mut frame = 0;
                let result = loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(response) => break response,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            print!("\rWaiting for Gemini... {}", frames[frame % frames.len()]);
                            stdout().flush()?;
                            frame += 1;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            break Err(String::from("Request stopped unexpectedly"));
                        }
                    }
                };
                crossterm::execute!(
                    stdout(),
                    crossterm::cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
                println!("Response:");
                let response = result.unwrap_or_else(|error| format!("Error: {error}"));
                print_response_with_delay(&response, app.config.response_char_delay_ms)?;
            }
        }
    }
}

fn read_prompt(prompt: String) -> Result<InputAction> {
    enable_raw_mode().context("Could not enable terminal input mode")?;
    if let Err(error) = crossterm::execute!(stdout(), EnableBracketedPaste) {
        let _ = disable_raw_mode();
        return Err(error).context("Could not enable bracketed paste");
    }
    let input = read_prompt_raw(prompt);
    let paste_restore = crossterm::execute!(stdout(), DisableBracketedPaste)
        .context("Could not disable bracketed paste");
    let restore = disable_raw_mode().context("Could not restore terminal input mode");
    paste_restore?;
    restore?;
    input
}

fn read_prompt_raw(mut prompt: String) -> Result<InputAction> {
    loop {
        let event = event::read()?;
        if let Event::Paste(text) = event {
            let pasted = text.replace("\r\n", "\n").replace('\r', "\n");
            prompt.push_str(&pasted);
            print!("{}", pasted.replace('\n', "\r\n  "));
            stdout().flush()?;
            continue;
        }
        let Event::Key(key) = event else { continue };
        match key.code {
            KeyCode::F(2) => {
                print!("\r\n");
                stdout().flush()?;
                return Ok(InputAction::SelectModel(prompt));
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crossterm::execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
                print!("Prompt:\n  {}", prompt.replace('\n', "\n  "));
                stdout().flush()?;
            }
            KeyCode::Char('\x0C') => {
                crossterm::execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
                print!("Prompt:\n  {}", prompt.replace('\n', "\n  "));
                stdout().flush()?;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                prompt.push('\n');
                print!("\r\n  ");
                stdout().flush()?;
            }
            KeyCode::Char('\n') | KeyCode::Char('j')
                if key.code == KeyCode::Char('\n')
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                prompt.push('\n');
                print!("\r\n  ");
                stdout().flush()?;
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

fn run_gui(config: Config, config_path: PathBuf, status: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([940.0, 720.0])
            .with_min_inner_size([680.0, 520.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Gemini Terminal",
        options,
        Box::new(move |creation_context| {
            creation_context
                .egui_ctx
                .set_visuals(egui::Visuals::light());
            let font_status = apply_persian_font(&creation_context.egui_ctx, &config.font_path)
                .err()
                .map(|error| format!("Could not load Persian font: {error}"));
            Ok(Box::new(GeminiApp {
                config,
                config_path,
                status: font_status.unwrap_or(status),
                page: Page::Chat,
                prompt: String::new(),
                messages: Vec::new(),
                sending: false,
                response_receiver: None,
                typing_response: None,
                next_character_at: Instant::now(),
            }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Could not launch desktop window: {error}"))
}

impl eframe::App for GeminiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let received = self
            .response_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());
        match received {
            Some(Ok(Ok(response))) => {
                self.messages.push(Message {
                    from_user: false,
                    text: String::new(),
                });
                self.sending = false;
                self.typing_response = Some(response.chars().collect());
                self.next_character_at = Instant::now();
                self.response_receiver = None;
            }
            Some(Ok(Err(error))) => {
                self.messages.push(Message {
                    from_user: false,
                    text: format!("Error: {error}"),
                });
                self.status = String::from("Request failed");
                self.sending = false;
                self.response_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.status = String::from("Request stopped unexpectedly");
                self.sending = false;
                self.response_receiver = None;
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }

        let typing_finished = if let Some(characters) = &mut self.typing_response {
            let delay = Duration::from_millis(self.config.response_char_delay_ms);
            let now = Instant::now();
            while !characters.is_empty() && now >= self.next_character_at {
                if let Some(character) = characters.pop_front() {
                    if let Some(message) = self.messages.last_mut() {
                        message.text.push(character);
                    }
                }
                self.next_character_at += delay;
            }
            characters.is_empty()
        } else {
            false
        };
        if typing_finished {
            self.typing_response = None;
            self.status = String::from("Ready");
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Gemini Terminal");
                ui.separator();
                ui.label(&self.config.model);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .selectable_label(self.page == Page::Options, "Options")
                        .clicked()
                    {
                        self.page = Page::Options;
                    }
                    if ui
                        .selectable_label(self.page == Page::Chat, "Chat")
                        .clicked()
                    {
                        self.page = Page::Chat;
                    }
                });
            });
            if !self.status.is_empty() {
                ui.small(&self.status);
            }
            if self.sending && self.page == Page::Options {
                ui.spinner();
            }
        });

        if self.page == Page::Chat {
            egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
                ui.add_space(6.0);
                let text_alignment = if self.config.right_to_left {
                    Align::RIGHT
                } else {
                    Align::LEFT
                };
                let editor_width = ui.available_width();
                let editor = egui::TextEdit::multiline(&mut self.prompt)
                    .desired_rows(4)
                    .desired_width(editor_width)
                    .hint_text("Write a message...")
                    .horizontal_align(text_alignment);
                egui::ScrollArea::vertical()
                    .id_salt("prompt-editor-scroll")
                    .max_height(112.0)
                    .min_scrolled_height(112.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(editor_width);
                        ui.add(editor);
                    });
                ui.horizontal(|ui| {
                    ui.label("Enter adds a line; Ctrl+Enter sends");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let send = ui.add_enabled(
                            !self.sending && !self.prompt.trim().is_empty(),
                            egui::Button::new("Send"),
                        );
                        let send_from_keyboard =
                            ui.input(|input| input.key_pressed(Key::Enter) && input.modifiers.ctrl);
                        if send.clicked() || send_from_keyboard {
                            self.submit_prompt();
                        }
                    });
                });
                ui.add_space(4.0);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.page {
            Page::Chat => self.show_chat(ui),
            Page::Options => self.show_options(ui, ctx),
        });

        if self.sending || self.typing_response.is_some() {
            let repaint_delay = if self.typing_response.is_some() {
                1
            } else {
                100
            };
            ctx.request_repaint_after(Duration::from_millis(repaint_delay));
        }
    }
}

impl GeminiApp {
    fn show_chat(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.add_space(48.0);
                    ui.heading("What would you like to explore?");
                    ui.label("Ask Gemini a question or paste a longer prompt below.");
                    return;
                }
                for message in &self.messages {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(if message.from_user { "You" } else { "Gemini" }).strong(),
                    );
                    let alignment = if self.config.right_to_left {
                        Align::RIGHT
                    } else {
                        Align::LEFT
                    };
                    ui.with_layout(Layout::top_down(alignment), |ui| {
                        ui.label(&message.text);
                    });
                    ui.separator();
                }
                if self.sending {
                    ui.add_space(12.0);
                    ui.label(RichText::new("Gemini").strong());
                    ui.spinner();
                }
            });
    }

    fn show_options(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(20.0);
        ui.heading("Options");
        ui.label("Connection and text display");
        ui.add_space(20.0);
        ui.set_max_width(620.0);

        ui.label("Gemini API key");
        ui.add_sized(
            [ui.available_width(), 32.0],
            egui::TextEdit::singleline(&mut self.config.api_key)
                .password(true)
                .hint_text("Paste your API key"),
        );
        ui.add_space(12.0);

        egui::ComboBox::from_label("Model")
            .selected_text(&self.config.model)
            .show_ui(ui, |ui| {
                for model in &self.config.models {
                    ui.selectable_value(&mut self.config.model, model.clone(), model);
                }
            });
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.label("Response character delay");
            ui.add(
                egui::DragValue::new(&mut self.config.response_char_delay_ms)
                    .range(0..=MAX_RESPONSE_CHAR_DELAY_MS)
                    .suffix(" ms"),
            );
        });
        ui.add_space(12.0);

        ui.checkbox(&mut self.config.right_to_left, "Right-to-left layout");
        ui.add_space(8.0);
        ui.label("Persian font file (.ttf or .otf)");
        ui.add_sized(
            [ui.available_width(), 32.0],
            egui::TextEdit::singleline(&mut self.config.font_path)
                .hint_text("For example: /usr/share/fonts/.../NotoNaskhArabic-Regular.ttf"),
        );
        ui.add_space(4.0);
        ui.small("Install a Persian-capable font on your system, then enter its file path here.");
        ui.add_space(20.0);

        if ui.button("Save options").clicked() {
            match apply_persian_font(ctx, &self.config.font_path) {
                Ok(()) => match save_config(&self.config_path, &self.config) {
                    Ok(()) => self.status = String::from("Options saved"),
                    Err(error) => self.status = format!("Could not save options: {error}"),
                },
                Err(error) => self.status = format!("Could not load font: {error}"),
            }
        }
    }

    fn submit_prompt(&mut self) {
        if self.sending {
            return;
        }
        if self.config.api_key.trim().is_empty() {
            self.status = String::from("Add your Gemini API key in Options");
            self.page = Page::Options;
            return;
        }
        let prompt = std::mem::take(&mut self.prompt);
        if prompt.trim().is_empty() {
            return;
        }
        self.messages.push(Message {
            from_user: true,
            text: prompt.clone(),
        });
        self.status = String::from("Waiting for Gemini...");
        self.sending = true;

        let api_key = self.config.api_key.clone();
        let model = self.config.model.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(generate_content(&api_key, &model, &prompt));
        });
        self.response_receiver = Some(receiver);
    }
}

fn default_persian_font_path() -> Option<String> {
    [
        "/usr/share/fonts/noto/NotoNaskhArabic-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoNaskhArabic-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansArabic-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf",
    ]
    .iter()
    .find(|path| Path::new(path).is_file())
    .map(|path| String::from(*path))
}

fn apply_persian_font(ctx: &egui::Context, path: &str) -> Result<()> {
    if path.trim().is_empty() {
        ctx.set_fonts(FontDefinitions::default());
        return Ok(());
    }
    let font_data = fs::read(path).with_context(|| format!("Could not read font file {path}"))?;
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        String::from("persian-user-font"),
        std::sync::Arc::new(FontData::from_owned(font_data)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, String::from("persian-user-font"));
    }
    ctx.set_fonts(fonts);
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
        Config, ModelCatalog, extract_text, generate_content, parse_args, persist_model_selection,
        save_config, should_refresh_models, text_generation_models, unix_time_now,
    };
    use std::{env, fs};

    #[test]
    fn reads_key_and_model_from_toml() {
        let config: Config =
            toml::from_str("api_key = \"test-key\"\nmodel = \"gemini-test\"\n").unwrap();

        assert_eq!(config.api_key, "test-key");
        assert_eq!(config.model, "gemini-test");
        assert_eq!(config.models, ["gemini-3.5-flash-lite", "gemini-3.6-flash"]);
        assert!(config.font_path.is_empty());
        assert!(!config.right_to_left);
        assert_eq!(config.response_char_delay_ms, 5);
    }

    #[test]
    fn gui_is_opt_in_and_parsed_as_a_flag() {
        let terminal_options = parse_args(std::iter::empty()).unwrap();
        assert!(!terminal_options.gui);

        let gui_options = parse_args([String::from("--gui")].into_iter()).unwrap();
        assert!(gui_options.gui);
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
            font_path: String::new(),
            right_to_left: false,
            response_char_delay_ms: 5,
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
            font_path: String::new(),
            right_to_left: false,
            response_char_delay_ms: 5,
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
