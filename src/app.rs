use std::{
    io::{self, Stdout},
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};
use tokio::{sync::mpsc, time};

use crate::{
    config::{self, Config},
    context::PromptContext,
    model::{ModelInfo, select_model},
    openrouter::{ModelDecision, OpenRouterClient, PlanningInput},
    shell::{ShellRunResult, build_attempt_summary, run_command, should_retry},
};

const MAX_RETRY_DEPTH: usize = 3;
const SPINNER_FRAMES: &[&str] = &["|", "/", "-", "\\"];

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub initial_query: Option<String>,
    pub initial_model: Option<String>,
    pub force_model_refresh: bool,
}

#[derive(Clone)]
struct RuntimeContext {
    client: Option<OpenRouterClient>,
    allow_paid_models: bool,
    cache_path: PathBuf,
    shell_program: String,
    cwd: PathBuf,
}

#[derive(Debug)]
enum AppEvent {
    Input(KeyEvent),
    Tick,
    QueryFinished(Result<QueryOutcome>),
    ModelsRefreshed(Result<Vec<ModelInfo>>),
}

#[derive(Debug, Clone)]
enum HistoryKind {
    Command {
        command: String,
        output: String,
        exit_code: i32,
        retry_depth: usize,
        reasoning: Option<String>,
        open_target: Option<String>,
        saved_output_path: Option<PathBuf>,
    },
    Question {
        question: String,
        reasoning: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    intent: String,
    kind: HistoryKind,
}

#[derive(Debug, Clone)]
struct PendingClarification {
    original_intent: String,
    question: String,
    attempt_summaries: Vec<String>,
}

#[derive(Debug)]
enum QueryOutcome {
    Completed {
        entries: Vec<HistoryEntry>,
        status: String,
        new_cwd: PathBuf,
    },
    NeedsClarification {
        entries: Vec<HistoryEntry>,
        pending: PendingClarification,
        status: String,
        new_cwd: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct QueryRequest {
    original_intent: String,
    current_user_message: String,
    intent_label: String,
    clarification_answer: Option<String>,
    attempt_summaries: Vec<String>,
}

#[derive(Debug, Default)]
struct ModelPicker {
    open: bool,
    filter: String,
    selected: usize,
}

struct App {
    history: Vec<HistoryEntry>,
    input: String,
    cursor: usize,
    history_scroll: u16,
    status: String,
    busy: bool,
    spinner_index: usize,
    models: Vec<ModelInfo>,
    selected_model: Option<String>,
    model_picker: ModelPicker,
    pending_clarification: Option<PendingClarification>,
    shell_program: String,
    highlighter: ShellHighlighter,
}

struct ShellHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl ShellHighlighter {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_default();

        Self { syntax_set, theme }
    }

    fn highlight(&self, command: &str) -> Line<'static> {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension("sh")
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);

        match highlighter.highlight_line(command, &self.syntax_set) {
            Ok(ranges) => {
                let spans = ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let mut line_style = Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        ));

                        if style.font_style.contains(FontStyle::BOLD) {
                            line_style = line_style.add_modifier(Modifier::BOLD);
                        }

                        Span::styled(text.to_string(), line_style)
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            }
            Err(_) => Line::from(command.to_string()),
        }
    }
}

impl App {
    fn new(
        models: Vec<ModelInfo>,
        selected_model: Option<String>,
        shell_program: String,
        status: String,
    ) -> Self {
        Self {
            history: Vec::new(),
            input: String::new(),
            cursor: 0,
            history_scroll: 0,
            status,
            busy: false,
            spinner_index: 0,
            models,
            selected_model,
            model_picker: ModelPicker::default(),
            pending_clarification: None,
            shell_program,
            highlighter: ShellHighlighter::new(),
        }
    }

    fn active_model_label(&self) -> String {
        self.selected_model
            .clone()
            .unwrap_or_else(|| "no-model".to_string())
    }

    fn spinner_frame(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_index % SPINNER_FRAMES.len()]
    }

    fn visible_model_indices(&self) -> Vec<usize> {
        self.models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                model
                    .matches_filter(&self.model_picker.filter)
                    .then_some(index)
            })
            .collect()
    }

    fn clamp_model_picker_selection(&mut self) {
        let len = self.visible_model_indices().len();
        if len == 0 {
            self.model_picker.selected = 0;
        } else if self.model_picker.selected >= len {
            self.model_picker.selected = len - 1;
        }
    }

    fn push_entries(&mut self, entries: Vec<HistoryEntry>) {
        self.history.extend(entries);
    }

    fn latest_open_target(&self) -> Option<String> {
        self.history
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                HistoryKind::Command { open_target, .. } => open_target.clone(),
                HistoryKind::Question { .. } => None,
            })
    }

    fn replace_models(&mut self, models: Vec<ModelInfo>) {
        self.models = models;
        self.selected_model = select_model(&self.models, self.selected_model.as_deref());
        self.clamp_model_picker_selection();
    }
}

impl HistoryEntry {
    fn command(
        intent: String,
        command: String,
        result: &ShellRunResult,
        reasoning: Option<String>,
        retry_depth: usize,
    ) -> Self {
        Self {
            intent,
            kind: HistoryKind::Command {
                command,
                output: result.display_output.clone(),
                exit_code: result.exit_code,
                retry_depth,
                reasoning,
                open_target: result.open_target.clone(),
                saved_output_path: result.saved_output_path.clone(),
            },
        }
    }

    fn question(intent: String, question: String, reasoning: Option<String>) -> Self {
        Self {
            intent,
            kind: HistoryKind::Question {
                question,
                reasoning,
            },
        }
    }

    fn collapsed_summary(&self) -> String {
        match &self.kind {
            HistoryKind::Command {
                command,
                exit_code,
                retry_depth,
                ..
            } => {
                if *retry_depth == 0 {
                    format!("{} -> {} [exit {}]", self.intent, command, exit_code)
                } else {
                    format!(
                        "{} -> {} [retry {}, exit {}]",
                        self.intent, command, retry_depth, exit_code
                    )
                }
            }
            HistoryKind::Question { question, .. } => format!("{} -> {}", self.intent, question),
        }
    }

    fn expanded_lines(&self, highlighter: &ShellHighlighter) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled("Intent: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(self.intent.clone()),
        ])];

        match &self.kind {
            HistoryKind::Command {
                command,
                output,
                exit_code,
                retry_depth,
                reasoning,
                saved_output_path,
                ..
            } => {
                let label = if *retry_depth == 0 {
                    format!("Command [exit {}]:", exit_code)
                } else {
                    format!("Command [retry {}, exit {}]:", retry_depth, exit_code)
                };
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(Color::Cyan),
                )));
                lines.push(highlighter.highlight(command));

                if let Some(reasoning) = reasoning.as_deref().filter(|value| !value.is_empty()) {
                    lines.push(Line::from(vec![
                        Span::styled("Why: ", Style::default().fg(Color::Yellow)),
                        Span::raw(reasoning.to_string()),
                    ]));
                }

                if let Some(path) = saved_output_path {
                    lines.push(Line::from(vec![
                        Span::styled("Output: ", Style::default().fg(Color::Green)),
                        Span::raw(format!("long output saved to {}", path.display())),
                    ]));
                }

                lines.push(Line::from(Span::styled(
                    "Result:",
                    Style::default().fg(Color::Green),
                )));
                lines.extend(output.lines().map(|line| Line::from(line.to_string())));
            }
            HistoryKind::Question {
                question,
                reasoning,
            } => {
                lines.push(Line::from(Span::styled(
                    "Clarification:",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(question.clone()));
                if let Some(reasoning) = reasoning.as_deref().filter(|value| !value.is_empty()) {
                    lines.push(Line::from(vec![
                        Span::styled("Why: ", Style::default().fg(Color::Yellow)),
                        Span::raw(reasoning.to_string()),
                    ]));
                }
            }
        }

        lines
    }
}

pub async fn run(config: Config, launch: LaunchOptions) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current working directory")?;
    let shell_program = config.default_shell();
    let cache_path = config::models_cache_path()?;
    let allow_paid_models = config.has_user_api_key();
    let client = config
        .effective_api_key()
        .zip(config.api_key_source())
        .map(|(api_key, source)| OpenRouterClient::new(api_key, source))
        .transpose()?;

    let (models, startup_status) = match &client {
        Some(client) => match client
            .load_model_catalog(&cache_path, launch.force_model_refresh, allow_paid_models)
            .await
        {
            Ok(models) if models.is_empty() => (
                Vec::new(),
                "No models were returned by OpenRouter.".to_string(),
            ),
            Ok(models) => (
                models,
                if allow_paid_models {
                    "Paid mode active. Free and paid models available.".to_string()
                } else {
                    "Free mode active. Using cached/dynamic free models.".to_string()
                },
            ),
            Err(error) => (
                Vec::new(),
                format!("Model load failed: {error}"),
            ),
        },
        None => (
            Vec::new(),
            "No OpenRouter key available. Add one to ~/.config/ash/config.toml or compile with ASH_EMBEDDED_OPENROUTER_KEY.".to_string(),
        ),
    };

    let selected_model = select_model(
        &models,
        launch
            .initial_model
            .as_deref()
            .or(config.default_model.as_deref()),
    );

    let runtime = RuntimeContext {
        client,
        allow_paid_models,
        cache_path,
        shell_program: shell_program.clone(),
        cwd,
    };

    let mut app = App::new(models, selected_model, shell_program, startup_status);
    let initial_query = launch.initial_query;

    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal")?;

    let run_result = run_loop(&mut terminal, &mut app, runtime, initial_query).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    run_result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    mut runtime: RuntimeContext,
    initial_query: Option<String>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();

    spawn_input_reader(tx.clone());
    spawn_tick(tx.clone());

    if let Some(query) = initial_query {
        app.input = query;
        app.cursor = app.input.chars().count();
        submit_query(app, &runtime, &tx)?;
    }

    loop {
        terminal.draw(|frame| render(frame, app, &runtime))?;

        let Some(event) = rx.recv().await else {
            break;
        };

        match event {
            AppEvent::Input(key) => {
                if handle_key_event(app, &runtime, &tx, key)? {
                    break;
                }
            }
            AppEvent::Tick => {
                if app.busy {
                    app.spinner_index = (app.spinner_index + 1) % SPINNER_FRAMES.len();
                }
            }
            AppEvent::QueryFinished(result) => {
                app.busy = false;
                match result {
                    Ok(QueryOutcome::Completed { entries, status, new_cwd }) => {
                        app.pending_clarification = None;
                        app.push_entries(entries);
                        app.status = status;
                        runtime.cwd = new_cwd;
                    }
                    Ok(QueryOutcome::NeedsClarification {
                        entries,
                        pending,
                        status,
                        new_cwd,
                    }) => {
                        app.push_entries(entries);
                        app.status = status;
                        app.pending_clarification = Some(pending);
                        runtime.cwd = new_cwd;
                    }
                    Err(error) => {
                        app.status = format!("Request failed: {error}");
                    }
                }
            }
            AppEvent::ModelsRefreshed(result) => match result {
                Ok(models) => {
                    app.replace_models(models);
                    app.status = if runtime.allow_paid_models {
                        "Model list refreshed. Free and paid models available.".to_string()
                    } else {
                        "Model list refreshed. Free models only.".to_string()
                    };
                }
                Err(error) => {
                    app.status = format!("Model refresh failed: {error}");
                }
            },
        }
    }

    Ok(())
}

fn spawn_input_reader(tx: mpsc::UnboundedSender<AppEvent>) {
    thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    if let Ok(Event::Key(key)) = event::read() {
                        if tx.send(AppEvent::Input(key)).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

fn spawn_tick(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(120));
        loop {
            interval.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

fn handle_key_event(
    app: &mut App,
    runtime: &RuntimeContext,
    tx: &mpsc::UnboundedSender<AppEvent>,
    key: KeyEvent,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Ok(true),
            KeyCode::Char('r') => {
                refresh_models(app, runtime, tx);
                Ok(false)
            }
            KeyCode::Char('o') => {
                open_latest_target(app)?;
                Ok(false)
            }
            _ => Ok(false),
        };
    }

    if app.model_picker.open {
        return handle_model_picker_input(app, key);
    }

    match key.code {
        KeyCode::F(2) => {
            app.model_picker.open = true;
            app.clamp_model_picker_selection();
        }
        KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.model_picker.open = true;
            app.clamp_model_picker_selection();
        }
        KeyCode::Esc => {
            if app.pending_clarification.is_some() {
                app.pending_clarification = None;
                app.status = "Clarification dismissed.".to_string();
            } else {
                app.input.clear();
                app.cursor = 0;
            }
        }
        KeyCode::Enter => {
            submit_query(app, runtime, tx)?;
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let remove_at = app.cursor - 1;
                remove_char(&mut app.input, remove_at);
                app.cursor -= 1;
            }
        }
        KeyCode::Delete => {
            if app.cursor < app.input.chars().count() {
                remove_char(&mut app.input, app.cursor);
            }
        }
        KeyCode::Left => {
            app.cursor = app.cursor.saturating_sub(1);
        }
        KeyCode::Right => {
            let len = app.input.chars().count();
            app.cursor = (app.cursor + 1).min(len);
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        KeyCode::Up => {
            app.history_scroll = app.history_scroll.saturating_sub(1);
        }
        KeyCode::Down => {
            app.history_scroll = app.history_scroll.saturating_add(1);
        }
        KeyCode::PageUp => {
            app.history_scroll = app.history_scroll.saturating_sub(5);
        }
        KeyCode::PageDown => {
            app.history_scroll = app.history_scroll.saturating_add(5);
        }
        KeyCode::Char(character) => {
            insert_char(&mut app.input, app.cursor, character);
            app.cursor += 1;
        }
        _ => {}
    }

    Ok(false)
}

fn handle_model_picker_input(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.model_picker.open = false;
        }
        KeyCode::Backspace => {
            app.model_picker.filter.pop();
            app.clamp_model_picker_selection();
        }
        KeyCode::Enter => {
            let visible = app.visible_model_indices();
            if let Some(model_index) = visible.get(app.model_picker.selected) {
                app.selected_model = Some(app.models[*model_index].id.clone());
                app.status = format!("Model selected: {}", app.models[*model_index].id);
            }
            app.model_picker.open = false;
        }
        KeyCode::Up => {
            app.model_picker.selected = app.model_picker.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            let len = app.visible_model_indices().len();
            if len > 0 {
                app.model_picker.selected = (app.model_picker.selected + 1).min(len - 1);
            }
        }
        KeyCode::Char(character) => {
            app.model_picker.filter.push(character);
            app.model_picker.selected = 0;
            app.clamp_model_picker_selection();
        }
        _ => {}
    }

    Ok(false)
}

fn submit_query(
    app: &mut App,
    runtime: &RuntimeContext,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    if app.busy {
        app.status = "ash is already working on a request.".to_string();
        return Ok(());
    }

    let Some(client) = runtime.client.clone() else {
        app.status = "No OpenRouter key configured, so queries cannot run yet.".to_string();
        return Ok(());
    };

    let Some(model_id) = app.selected_model.clone() else {
        app.status = "No model selected.".to_string();
        return Ok(());
    };

    let input = app.input.trim().to_string();
    if input.is_empty() {
        return Ok(());
    }

    let request = if let Some(pending) = app.pending_clarification.take() {
        QueryRequest {
            intent_label: format!("{} | clarification: {}", pending.original_intent, input),
            original_intent: pending.original_intent,
            current_user_message: input.clone(),
            clarification_answer: Some(input),
            attempt_summaries: pending.attempt_summaries,
        }
    } else {
        QueryRequest {
            intent_label: input.clone(),
            original_intent: input.clone(),
            current_user_message: input.clone(),
            clarification_answer: None,
            attempt_summaries: Vec::new(),
        }
    };

    app.input.clear();
    app.cursor = 0;
    app.busy = true;
    app.status = format!("{} {}", app.spinner_frame(), model_id);

    let tx = tx.clone();
    let shell_program = runtime.shell_program.clone();
    let cwd = runtime.cwd.clone();

    tokio::spawn(async move {
        let result = process_query(client, model_id, shell_program, cwd, request).await;
        let _ = tx.send(AppEvent::QueryFinished(result));
    });

    Ok(())
}

fn refresh_models(app: &mut App, runtime: &RuntimeContext, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(client) = runtime.client.clone() else {
        app.status = "No OpenRouter key configured, so there is nothing to refresh.".to_string();
        return;
    };

    app.status = "Refreshing model list...".to_string();

    let tx = tx.clone();
    let cache_path = runtime.cache_path.clone();
    let allow_paid = runtime.allow_paid_models;

    tokio::spawn(async move {
        let result = client
            .load_model_catalog(&cache_path, true, allow_paid)
            .await;
        let _ = tx.send(AppEvent::ModelsRefreshed(result));
    });
}

fn open_latest_target(app: &mut App) -> Result<()> {
    let Some(target) = app.latest_open_target() else {
        app.status = "No openable link or saved output available yet.".to_string();
        return Ok(());
    };

    open::that(&target).with_context(|| format!("failed to open {target}"))?;
    app.status = format!("Opened {target}");
    Ok(())
}

async fn process_query(
    client: OpenRouterClient,
    model_id: String,
    shell_program: String,
    mut cwd: PathBuf,
    request: QueryRequest,
) -> Result<QueryOutcome> {
    let mut last_result: Option<ShellRunResult> = None;
    let mut attempt_summaries = request.attempt_summaries.clone();
    let mut entries = Vec::new();

    for retry_depth in 0..=MAX_RETRY_DEPTH {
        let prompt_context = PromptContext::capture(&shell_program, &cwd, last_result.as_ref());
        let planning = PlanningInput {
            original_intent: &request.original_intent,
            user_input: &request.current_user_message,
            clarification_answer: request.clarification_answer.as_deref(),
            attempt_summaries: &attempt_summaries,
            prompt_context: &prompt_context,
        };

        match client.plan_command(&model_id, &planning).await? {
            ModelDecision::Run { command, reasoning } => {
                let result = run_command(&shell_program, &command, &cwd).await?;
                cwd = result.new_cwd.clone();
                let should_try_again =
                    retry_depth < MAX_RETRY_DEPTH && should_retry(&command, &result);

                entries.push(HistoryEntry::command(
                    request.intent_label.clone(),
                    command.clone(),
                    &result,
                    reasoning,
                    retry_depth,
                ));
                attempt_summaries.push(build_attempt_summary(&command, &result));

                if should_try_again {
                    last_result = Some(result);
                    continue;
                }

                let status = if result.exit_code == 0 {
                    format!("Completed with {model_id}")
                } else if retry_depth >= MAX_RETRY_DEPTH {
                    format!("Stopped after {MAX_RETRY_DEPTH} retries with {model_id}")
                } else {
                    format!("Completed with exit code {}", result.exit_code)
                };

                return Ok(QueryOutcome::Completed { entries, status, new_cwd: cwd });
            }
            ModelDecision::Ask {
                question,
                reasoning,
            } => {
                entries.push(HistoryEntry::question(
                    request.intent_label.clone(),
                    question.clone(),
                    reasoning,
                ));

                return Ok(QueryOutcome::NeedsClarification {
                    entries,
                    pending: PendingClarification {
                        original_intent: request.original_intent.clone(),
                        question,
                        attempt_summaries,
                    },
                    status: "Need clarification.".to_string(),
                    new_cwd: cwd,
                });
            }
        }
    }

    Err(anyhow!("retry loop exited unexpectedly"))
}

fn render(frame: &mut Frame<'_>, app: &App, runtime: &RuntimeContext) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(6)])
        .split(frame.area());

    render_history(frame, app, areas[0]);
    render_input(frame, app, runtime, areas[1]);

    if app.model_picker.open {
        render_model_picker(frame, app);
    }
}

fn render_history(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let history_text = build_history_text(app);
    let history = Paragraph::new(history_text)
        .block(
            Block::default()
                .title(" ash history ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.history_scroll, 0));

    frame.render_widget(history, area);
}

fn render_input(frame: &mut Frame<'_>, app: &App, runtime: &RuntimeContext, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let heading = format!(
        " {} {}  shell:{}  cwd:{} ",
        if app.busy {
            app.spinner_frame()
        } else {
            "ready"
        },
        app.active_model_label(),
        app.shell_program,
        runtime.cwd.display()
    );

    frame.render_widget(Block::default().title(heading).borders(Borders::ALL), area);

    let clarification_text = app
        .pending_clarification
        .as_ref()
        .map(|pending| format!("Clarify: {}", pending.question))
        .unwrap_or_else(|| "Enter a request in plain English.".to_string());
    frame.render_widget(
        Paragraph::new(clarification_text).style(Style::default().fg(Color::Yellow)),
        sections[0],
    );

    frame.render_widget(
        Paragraph::new(format!("> {}", app.input)).wrap(Wrap { trim: false }),
        sections[1],
    );
    frame.render_widget(
        Paragraph::new(app.status.clone()).style(Style::default().fg(Color::Cyan)),
        sections[2],
    );
    frame.render_widget(
        Paragraph::new(
            "Enter submit  Alt+M/F2 models  Ctrl+R refresh  Ctrl+O open output/link  Ctrl+C quit",
        ),
        sections[3],
    );

    let cursor_x = sections[1]
        .x
        .saturating_add(2)
        .saturating_add(app.cursor as u16);
    let cursor_y = sections[1].y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_model_picker(frame: &mut Frame<'_>, app: &App) {
    let popup = centered_rect(80, 65, frame.area());
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(" model picker ")
            .borders(Borders::ALL),
        popup,
    );

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(popup);

    frame.render_widget(
        Paragraph::new(format!("Search: {}", app.model_picker.filter)),
        inner[0],
    );

    let visible = app.visible_model_indices();
    let items = if visible.is_empty() {
        vec![ListItem::new("No models match the current filter.")]
    } else {
        visible
            .iter()
            .map(|index| {
                let model = &app.models[*index];
                let mut lines = vec![Line::from(vec![
                    Span::styled(model.title(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!(
                        "  {} ctx  {}",
                        model.context_label(),
                        model.cost_label()
                    )),
                ])];

                if model.title() != model.id {
                    lines.push(Line::from(Span::styled(
                        model.id.clone(),
                        Style::default().fg(Color::DarkGray),
                    )));
                }

                ListItem::new(lines)
            })
            .collect::<Vec<_>>()
    };

    let mut state = ListState::default();
    if !visible.is_empty() {
        state.select(Some(app.model_picker.selected.min(visible.len() - 1)));
    }

    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::Black))
            .highlight_symbol("> "),
        inner[1],
        &mut state,
    );

    frame.render_widget(
        Paragraph::new("Type to filter. Enter selects. Esc closes."),
        inner[2],
    );
}

fn build_history_text(app: &App) -> Text<'static> {
    if app.history.is_empty() {
        return Text::from(vec![
            Line::from("ash turns natural language into shell commands."),
            Line::from("Recent command attempts and clarifications will appear here."),
        ]);
    }

    let mut lines = Vec::new();
    let expanded_from = app.history.len().saturating_sub(4);

    for (index, entry) in app.history.iter().enumerate() {
        if index < expanded_from {
            lines.push(Line::from(Span::styled(
                entry.collapsed_summary(),
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.extend(entry.expanded_lines(&app.highlighter));
        }

        if index + 1 < app.history.len() {
            lines.push(Line::from(""));
        }
    }

    Text::from(lines)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn insert_char(input: &mut String, index: usize, character: char) {
    let byte_index = char_to_byte_index(input, index);
    input.insert(byte_index, character);
}

fn remove_char(input: &mut String, index: usize) {
    let start = char_to_byte_index(input, index);
    let end = char_to_byte_index(input, index + 1);
    input.replace_range(start..end, "");
}

fn char_to_byte_index(input: &str, index: usize) -> usize {
    input
        .char_indices()
        .nth(index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or_else(|| input.len())
}
