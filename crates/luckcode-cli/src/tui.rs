use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use luckcode_storage::{read_session_events, sessions_root};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    fs,
    io::{self, Stdout},
    path::PathBuf,
    time::{Duration, SystemTime},
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(200);

pub fn run_tui() -> Result<()> {
    let sessions = load_sessions()?;
    let mut state = TuiState::new(sessions);
    load_selected_events(&mut state)?;

    enable_raw_mode().context("failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal")?;

    let result = run_tui_loop(&mut terminal, &mut state);

    disable_raw_mode().context("failed to disable terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")?;

    result
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiState,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| render(frame, state))
            .context("failed to draw TUI")?;

        if !event::poll(EVENT_POLL_INTERVAL).context("failed to poll terminal event")? {
            continue;
        }

        let Event::Key(key) = event::read().context("failed to read terminal event")? else {
            continue;
        };

        let old_session = state.selected_session;
        match key_to_action(key) {
            Some(TuiAction::Quit) => return Ok(()),
            Some(action) => reduce(state, action),
            None => {}
        }
        if old_session != state.selected_session {
            load_selected_events(state)?;
        }
    }
}

fn key_to_action(key: KeyEvent) -> Option<TuiAction> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(TuiAction::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(TuiAction::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(TuiAction::Up),
        KeyCode::Tab => Some(TuiAction::NextPanel),
        KeyCode::BackTab => Some(TuiAction::PreviousPanel),
        KeyCode::Char('r') => Some(TuiAction::Reload),
        _ => None,
    }
}

fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(35),
            Constraint::Percentage(35),
        ])
        .split(outer[0]);

    render_sessions(frame, body[0], state);
    render_timeline(frame, body[1], state);
    render_detail(frame, body[2], state);
    render_status(frame, outer[1], state);
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let items = if state.sessions.is_empty() {
        vec![ListItem::new("No sessions found")]
    } else {
        state
            .sessions
            .iter()
            .map(|session| {
                ListItem::new(vec![
                    Line::from(vec![Span::styled(
                        session.session_id.clone(),
                        Style::default().fg(Color::Cyan),
                    )]),
                    Line::from(format!("{} {}", session.project_hash, session.updated_at)),
                ])
            })
            .collect()
    };
    let mut list_state = ListState::default();
    if !state.sessions.is_empty() {
        list_state.select(Some(state.selected_session));
    }
    let block = panel_block("Sessions", state.panel == ActivePanel::Sessions);
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_timeline(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let items = if state.events.is_empty() {
        vec![ListItem::new("No events")]
    } else {
        state
            .events
            .iter()
            .map(|event| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:>3} ", event.index)),
                    Span::styled(
                        event.kind.clone(),
                        Style::default().fg(kind_color(&event.kind)),
                    ),
                    Span::raw(" "),
                    Span::raw(event.title.clone()),
                ]))
            })
            .collect()
    };
    let mut list_state = ListState::default();
    if !state.events.is_empty() {
        list_state.select(Some(state.selected_event));
    }
    let block = panel_block("Timeline", state.panel == ActivePanel::Timeline);
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = state
        .events
        .get(state.selected_event)
        .map(|event| event.detail.clone())
        .or_else(|| {
            state
                .sessions
                .get(state.selected_session)
                .map(SessionRow::detail)
        })
        .unwrap_or_else(|| "No session selected.".to_string());
    let paragraph = Paragraph::new(text)
        .block(panel_block("Detail", state.panel == ActivePanel::Detail))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let project = state
        .sessions
        .get(state.selected_session)
        .map(|session| session.project_hash.as_str())
        .unwrap_or("-");
    let line = Line::from(vec![
        Span::raw("q quit  "),
        Span::raw("up/down move  "),
        Span::raw("tab panel  "),
        Span::raw("r reload  "),
        Span::raw(format!("project {project}")),
    ]);
    let paragraph = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

fn panel_block(title: &str, active: bool) -> Block<'_> {
    let style = if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "user" => Color::Green,
        "assistant" => Color::Blue,
        "tool_call" => Color::Yellow,
        "tool_result" => Color::Magenta,
        "checkpoint" => Color::Red,
        "compact_summary" => Color::Cyan,
        _ => Color::White,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiState {
    sessions: Vec<SessionRow>,
    selected_session: usize,
    events: Vec<TimelineEvent>,
    selected_event: usize,
    panel: ActivePanel,
}

impl TuiState {
    fn new(sessions: Vec<SessionRow>) -> Self {
        Self {
            sessions,
            selected_session: 0,
            events: Vec::new(),
            selected_event: 0,
            panel: ActivePanel::Sessions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRow {
    session_id: String,
    project_hash: String,
    updated_at: String,
    path: PathBuf,
}

impl SessionRow {
    fn detail(&self) -> String {
        format!(
            "session: {}\nproject: {}\nupdated: {}\npath: {}",
            self.session_id,
            self.project_hash,
            self.updated_at,
            self.path.display()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineEvent {
    index: usize,
    kind: String,
    title: String,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivePanel {
    Sessions,
    Timeline,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiAction {
    Up,
    Down,
    NextPanel,
    PreviousPanel,
    Reload,
    Quit,
}

pub(crate) fn reduce(state: &mut TuiState, action: TuiAction) {
    match action {
        TuiAction::Up => match state.panel {
            ActivePanel::Sessions => {
                state.selected_session = state.selected_session.saturating_sub(1);
                state.selected_event = 0;
            }
            ActivePanel::Timeline | ActivePanel::Detail => {
                state.selected_event = state.selected_event.saturating_sub(1);
            }
        },
        TuiAction::Down => match state.panel {
            ActivePanel::Sessions => {
                if !state.sessions.is_empty() {
                    state.selected_session =
                        (state.selected_session + 1).min(state.sessions.len() - 1);
                    state.selected_event = 0;
                }
            }
            ActivePanel::Timeline | ActivePanel::Detail => {
                if !state.events.is_empty() {
                    state.selected_event = (state.selected_event + 1).min(state.events.len() - 1);
                }
            }
        },
        TuiAction::NextPanel => {
            state.panel = match state.panel {
                ActivePanel::Sessions => ActivePanel::Timeline,
                ActivePanel::Timeline => ActivePanel::Detail,
                ActivePanel::Detail => ActivePanel::Sessions,
            };
        }
        TuiAction::PreviousPanel => {
            state.panel = match state.panel {
                ActivePanel::Sessions => ActivePanel::Detail,
                ActivePanel::Timeline => ActivePanel::Sessions,
                ActivePanel::Detail => ActivePanel::Timeline,
            };
        }
        TuiAction::Reload | TuiAction::Quit => {}
    }
}

fn load_selected_events(state: &mut TuiState) -> Result<()> {
    let Some(session) = state.sessions.get(state.selected_session) else {
        state.events.clear();
        state.selected_event = 0;
        return Ok(());
    };
    let events = read_session_events(&session.project_hash, &session.session_id)?;
    state.events = events
        .iter()
        .enumerate()
        .map(|(idx, event)| timeline_event(idx + 1, event))
        .collect();
    state.selected_event = state
        .selected_event
        .min(state.events.len().saturating_sub(1));
    Ok(())
}

fn load_sessions() -> Result<Vec<SessionRow>> {
    let root = sessions_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for project_dir in fs::read_dir(&root)
        .with_context(|| format!("failed to read sessions root {}", root.display()))?
    {
        let project_dir = project_dir?;
        if !project_dir.file_type()?.is_dir() {
            continue;
        }
        let project_hash = project_dir.file_name().to_string_lossy().to_string();
        for session_file in fs::read_dir(project_dir.path())? {
            let session_file = session_file?;
            let path = session_file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = session_file.metadata()?.modified()?;
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string();
            sessions.push((
                modified,
                SessionRow {
                    session_id,
                    project_hash: project_hash.clone(),
                    updated_at: format_system_time(modified),
                    path,
                },
            ));
        }
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(sessions.into_iter().map(|(_, session)| session).collect())
}

fn timeline_event(index: usize, event: &Value) -> TimelineEvent {
    let kind = event_kind(event).to_string();
    let title = event_title(&kind, event);
    let detail = event_detail(event);
    TimelineEvent {
        index,
        kind,
        title,
        detail,
    }
}

fn event_kind(event: &Value) -> &str {
    event
        .get("type")
        .or_else(|| event.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn event_title(kind: &str, event: &Value) -> String {
    match kind {
        "user" | "assistant" | "compact_summary" => event
            .get("content")
            .and_then(Value::as_str)
            .map(compact_title)
            .unwrap_or_default(),
        "tool_call" => event
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        "tool_result" => {
            let name = event.get("name").and_then(Value::as_str).unwrap_or("tool");
            let status = event
                .get("metadata")
                .and_then(|metadata| metadata.get("success"))
                .and_then(Value::as_bool)
                .map(|success| if success { "ok" } else { "failed" })
                .unwrap_or("result");
            format!("{name} {status}")
        }
        "checkpoint" => event
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("checkpoint")
            .to_string(),
        _ => compact_json(event),
    }
}

fn event_detail(event: &Value) -> String {
    serde_json::to_string_pretty(event).unwrap_or_else(|_| event.to_string())
}

fn compact_title(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= 80 {
        return first_line.to_string();
    }
    let mut out = first_line.chars().take(77).collect::<String>();
    out.push_str("...");
    out
}

fn compact_json(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    compact_title(&text)
}

fn format_system_time(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_state() -> TuiState {
        TuiState {
            sessions: vec![
                SessionRow {
                    session_id: "ses_1".to_string(),
                    project_hash: "p1".to_string(),
                    updated_at: "2026-01-01 00:00:00".to_string(),
                    path: PathBuf::from("a.jsonl"),
                },
                SessionRow {
                    session_id: "ses_2".to_string(),
                    project_hash: "p2".to_string(),
                    updated_at: "2026-01-02 00:00:00".to_string(),
                    path: PathBuf::from("b.jsonl"),
                },
            ],
            selected_session: 0,
            events: vec![
                TimelineEvent {
                    index: 1,
                    kind: "user".to_string(),
                    title: "one".to_string(),
                    detail: "one".to_string(),
                },
                TimelineEvent {
                    index: 2,
                    kind: "assistant".to_string(),
                    title: "two".to_string(),
                    detail: "two".to_string(),
                },
            ],
            selected_event: 0,
            panel: ActivePanel::Sessions,
        }
    }

    #[test]
    fn reduce_moves_between_sessions_and_resets_event_selection() {
        let mut state = sample_state();
        state.selected_event = 1;

        reduce(&mut state, TuiAction::Down);

        assert_eq!(state.selected_session, 1);
        assert_eq!(state.selected_event, 0);

        reduce(&mut state, TuiAction::Down);
        assert_eq!(state.selected_session, 1);

        reduce(&mut state, TuiAction::Up);
        assert_eq!(state.selected_session, 0);
    }

    #[test]
    fn reduce_moves_timeline_selection_when_timeline_is_active() {
        let mut state = sample_state();
        state.panel = ActivePanel::Timeline;

        reduce(&mut state, TuiAction::Down);
        assert_eq!(state.selected_event, 1);

        reduce(&mut state, TuiAction::Down);
        assert_eq!(state.selected_event, 1);

        reduce(&mut state, TuiAction::Up);
        assert_eq!(state.selected_event, 0);
    }

    #[test]
    fn reduce_cycles_panels() {
        let mut state = sample_state();

        reduce(&mut state, TuiAction::NextPanel);
        assert_eq!(state.panel, ActivePanel::Timeline);
        reduce(&mut state, TuiAction::NextPanel);
        assert_eq!(state.panel, ActivePanel::Detail);
        reduce(&mut state, TuiAction::NextPanel);
        assert_eq!(state.panel, ActivePanel::Sessions);
    }

    #[test]
    fn timeline_event_formats_tool_result_status() {
        let event = timeline_event(
            3,
            &json!({
                "type": "tool_result",
                "name": "run_shell",
                "metadata": { "success": false }
            }),
        );

        assert_eq!(event.index, 3);
        assert_eq!(event.kind, "tool_result");
        assert_eq!(event.title, "run_shell failed");
    }

    #[test]
    fn compact_title_truncates_long_lines() {
        let title = compact_title(&"x".repeat(100));

        assert_eq!(title.chars().count(), 80);
        assert!(title.ends_with("..."));
    }
}
