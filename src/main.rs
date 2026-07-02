use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use gator::{
    copy_to_clipboard, ensure_tty_stdin, fuzzy_match, run_command_output, setup_terminal,
    write_selection, AppResult,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};
use tui_input::backend::crossterm::EventHandler;
use tui_input::{Input, InputRequest};

const INITIAL_ISSUE_LIST_LIMIT: &str = "30";
const FULL_ISSUE_LIST_LIMIT: &str = "1000";

fn main() -> AppResult<()> {
    ensure_tty_stdin()?;
    match run_issue_explorer()? {
        Some(issue_url) => write_selection(&issue_url),
        None => std::process::exit(1),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IssueStateFilter {
    Open,
    Closed,
    All,
}

impl IssueStateFilter {
    fn next(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::All,
            Self::All => Self::Open,
        }
    }

    fn gh_arg(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug)]
struct IssueEntry {
    number: u64,
    title: String,
    state: String,
    author: Option<String>,
    labels: Vec<String>,
    assignees: Vec<String>,
    milestone: Option<String>,
    comments: u64,
    created_at: Option<String>,
    updated_at: Option<String>,
    url: String,
    body: Option<String>,
    detail_loaded: bool,
    detail_error: Option<String>,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    state: String,
    author: Option<GhUser>,
    labels: Vec<GhLabel>,
    assignees: Vec<GhUser>,
    milestone: Option<GhMilestone>,
    #[serde(default)]
    comments: GhComments,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    url: String,
    body: Option<String>,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
struct GhMilestone {
    title: String,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum GhComments {
    Count(u64),
    Items(Vec<serde_json::Value>),
    #[default]
    Missing,
}

impl GhComments {
    fn count(&self) -> u64 {
        match self {
            Self::Count(count) => *count,
            Self::Items(items) => items.len() as u64,
            Self::Missing => 0,
        }
    }
}

struct IssueLoadResult {
    load_id: u64,
    state: IssueStateFilter,
    issues: Vec<IssueEntry>,
    error: Option<String>,
    done: bool,
}

struct IssueDetailResult {
    number: u64,
    body: Option<String>,
    milestone: Option<String>,
    comments: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct GhIssueDetail {
    body: Option<String>,
    milestone: Option<GhMilestone>,
    #[serde(default)]
    comments: GhComments,
}

pub(crate) fn run_issue_explorer() -> AppResult<Option<String>> {
    let cwd = env::current_dir()?;
    let repo = repo_for_path(&cwd).ok_or("current folder is not a GitHub repo with origin")?;
    select_issue(&repo)
}

fn select_issue(repo: &str) -> AppResult<Option<String>> {
    let (mut terminal, _guard) = setup_terminal()?;
    let accent = Color::Rgb(72, 166, 255);
    let warm = Color::Rgb(255, 181, 92);
    let text = Color::Black;
    let muted = text;
    let key_color = Color::Rgb(150, 150, 150);
    let mut input = Input::default();
    let mut selected = 0usize;
    let mut list_offset = 0usize;
    let mut detail_scroll = 0usize;
    let mut detail_max_scroll = 0usize;
    let mut detail_page_step = 5usize;
    let mut state_filter = IssueStateFilter::Open;
    let mut issues: Vec<IssueEntry> = Vec::new();
    let mut filtered: Vec<usize> = Vec::new();
    let mut loading = true;
    let mut loading_more = false;
    let mut load_id = 1u64;
    let mut error: Option<String> = None;
    let (tx, rx) = mpsc::channel::<IssueLoadResult>();
    let (detail_tx, detail_rx) = mpsc::channel::<IssueDetailResult>();
    let mut detail_in_flight = HashSet::new();

    spawn_issue_load(repo.to_string(), state_filter, load_id, tx.clone());

    loop {
        while let Ok(result) = rx.try_recv() {
            if result.state == state_filter && result.load_id == load_id {
                let selected_number =
                    selected_issue(&issues, &filtered, selected).map(|issue| issue.number);
                let mut next_issues = result.issues;
                merge_issue_details(&issues, &mut next_issues);
                issues = next_issues;
                error = result.error;
                filtered = filter_issues(&issues, input.value());
                selected = selected_number
                    .and_then(|number| selected_index_for_issue_number(&issues, &filtered, number))
                    .unwrap_or_else(|| selected.min(filtered.len().saturating_sub(1)));
                if !result.done {
                    list_offset = 0;
                    detail_scroll = 0;
                    detail_in_flight.clear();
                }
                loading = false;
                loading_more = !result.done;
            }
        }

        while let Ok(result) = detail_rx.try_recv() {
            detail_in_flight.remove(&result.number);
            if let Some(issue) = issues
                .iter_mut()
                .find(|issue| issue.number == result.number)
            {
                issue.detail_loaded = true;
                issue.detail_error = result.error;
                if issue.detail_error.is_none() {
                    issue.body = result.body;
                    issue.milestone = result.milestone;
                    if let Some(comments) = result.comments {
                        issue.comments = comments;
                    }
                }
            }
        }

        ensure_selected_issue_detail(
            repo,
            &issues,
            &filtered,
            selected,
            &mut detail_in_flight,
            &detail_tx,
        );

        let size = terminal.size()?;
        let ui = issue_layout(size.into());
        terminal.draw(|frame| {
            let list_title = if loading {
                format!("Issues {} loading", state_filter.label())
            } else if loading_more {
                format!(
                    "Issues {} {}/{} loading more",
                    state_filter.label(),
                    filtered.len(),
                    issues.len()
                )
            } else {
                format!(
                    "Issues {} {}/{}",
                    state_filter.label(),
                    filtered.len(),
                    issues.len()
                )
            };
            let left_block = Block::default()
                .borders(Borders::ALL)
                .title(format!("* {list_title}"))
                .border_style(Style::default().fg(accent))
                .border_type(BorderType::Rounded);
            frame.render_widget(left_block, ui.left);

            let search = Paragraph::new(input.value())
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false });
            frame.render_widget(search, ui.search);
            let cursor_x = input.visual_cursor().min(ui.search.width as usize);
            frame.set_cursor_position((ui.search.x + cursor_x as u16, ui.search.y));

            let list_height = ui.results.height as usize;
            list_offset = list_window_offset(selected, list_offset, list_height, filtered.len());
            let list_items = issue_list_items(
                &issues,
                &filtered,
                list_offset,
                list_height,
                ui.results.width as usize,
                text,
                muted,
            );
            let mut state = ListState::default();
            state.select(selected.checked_sub(list_offset));
            let list = List::new(list_items).highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(warm)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, ui.results, &mut state);

            let detail = selected_issue(&issues, &filtered, selected);
            let detail_text = issue_detail_text(
                repo,
                detail,
                error.as_deref(),
                loading && issues.is_empty(),
                accent,
                muted,
                text,
            );
            let detail_height = ui.detail.height.saturating_sub(2) as usize;
            detail_page_step = detail_height.max(1);
            detail_max_scroll = text_line_count(&detail_text).saturating_sub(detail_height);
            detail_scroll = detail_scroll.min(detail_max_scroll);
            let detail_title = detail
                .map(|issue| format!("#{}", issue.number))
                .unwrap_or_else(|| "Details".to_string());
            let detail_block = Block::default()
                .borders(Borders::ALL)
                .title(detail_title)
                .border_style(Style::default().fg(text))
                .border_type(BorderType::Rounded);
            let detail_widget = Paragraph::new(detail_text)
                .block(detail_block)
                .alignment(Alignment::Left)
                .scroll((detail_scroll as u16, 0))
                .wrap(Wrap { trim: false });
            frame.render_widget(detail_widget, ui.detail);

            let help = issue_help_line(state_filter, key_color, text, accent);
            frame.render_widget(
                Paragraph::new(Text::from(help))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Keys")
                            .border_style(Style::default().fg(muted))
                            .border_type(BorderType::Rounded),
                    )
                    .wrap(Wrap { trim: true }),
                ui.help,
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Esc => {
                        terminal.show_cursor()?;
                        return Ok(None);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        terminal.show_cursor()?;
                        return Ok(None);
                    }
                    KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(issue) = selected_issue(&issues, &filtered, selected) {
                            let _ = copy_to_clipboard(&issue.url);
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(issue) = selected_issue(&issues, &filtered, selected) {
                            terminal.show_cursor()?;
                            return Ok(Some(issue.url.clone()));
                        }
                    }
                    KeyCode::Tab => {
                        state_filter = state_filter.next();
                        selected = 0;
                        list_offset = 0;
                        detail_scroll = 0;
                        issues.clear();
                        filtered.clear();
                        detail_in_flight.clear();
                        error = None;
                        loading = true;
                        loading_more = false;
                        load_id = load_id.saturating_add(1);
                        spawn_issue_load(repo.to_string(), state_filter, load_id, tx.clone());
                    }
                    KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        loading = true;
                        loading_more = false;
                        error = None;
                        detail_in_flight.clear();
                        load_id = load_id.saturating_add(1);
                        spawn_issue_load(repo.to_string(), state_filter, load_id, tx.clone());
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        selected = (selected + 1).min(filtered.len().saturating_sub(1));
                    }
                    KeyCode::PageUp => {
                        detail_scroll = detail_scroll.saturating_sub(detail_page_step);
                    }
                    KeyCode::PageDown => {
                        detail_scroll = (detail_scroll + detail_page_step).min(detail_max_scroll);
                    }
                    KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        detail_scroll = 0;
                    }
                    KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        detail_scroll = detail_max_scroll;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.handle(InputRequest::DeleteLine);
                        filtered = filter_issues(&issues, input.value());
                        selected = 0;
                        list_offset = 0;
                    }
                    _ => {
                        let before = input.value().to_string();
                        let _ = input.handle_event(&Event::Key(key));
                        if input.value() != before {
                            filtered = filter_issues(&issues, input.value());
                            selected = 0;
                            list_offset = 0;
                            detail_scroll = 0;
                        }
                    }
                },
                Event::Paste(value) => {
                    insert_paste(&mut input, &value);
                    filtered = filter_issues(&issues, input.value());
                    selected = 0;
                    list_offset = 0;
                    detail_scroll = 0;
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if rect_contains(ui.results, mouse.column, mouse.row) {
                            let row = mouse.row.saturating_sub(ui.results.y) as usize;
                            selected = (list_offset + row).min(filtered.len().saturating_sub(1));
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if rect_contains(ui.detail, mouse.column, mouse.row) {
                            detail_scroll = detail_scroll.saturating_sub(1);
                        } else if rect_contains(ui.results, mouse.column, mouse.row) {
                            selected = selected.saturating_sub(1);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if rect_contains(ui.detail, mouse.column, mouse.row) {
                            detail_scroll = (detail_scroll + 1).min(detail_max_scroll);
                        } else if rect_contains(ui.results, mouse.column, mouse.row) {
                            selected = (selected + 1).min(filtered.len().saturating_sub(1));
                        }
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn insert_paste(input: &mut Input, value: &str) {
    for ch in value.chars().filter(|ch| *ch != '\r') {
        input.handle(InputRequest::InsertChar(ch));
    }
}

struct IssueLayout {
    left: Rect,
    search: Rect,
    results: Rect,
    detail: Rect,
    help: Rect,
}

fn issue_layout(size: Rect) -> IssueLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(size);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[0]);
    let left_inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .inner(body[0]);
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(left_inner);
    IssueLayout {
        left: body[0],
        search: left_chunks[0],
        results: left_chunks[1],
        detail: body[1],
        help: chunks[1],
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn text_line_count(text: &Text) -> usize {
    text.lines.len()
}

fn spawn_issue_load(
    repo: String,
    state: IssueStateFilter,
    load_id: u64,
    tx: mpsc::Sender<IssueLoadResult>,
) {
    thread::spawn(move || {
        let (issues, error) = load_issues(&repo, state, INITIAL_ISSUE_LIST_LIMIT)
            .map(|issues| (issues, None))
            .unwrap_or_else(|message| (Vec::new(), Some(message)));
        let initial_ok = error.is_none();
        let _ = tx.send(IssueLoadResult {
            load_id,
            state,
            issues,
            error,
            done: !initial_ok,
        });

        if initial_ok {
            let (issues, error) = load_issues(&repo, state, FULL_ISSUE_LIST_LIMIT)
                .map(|issues| (issues, None))
                .unwrap_or_else(|message| (Vec::new(), Some(message)));
            let _ = tx.send(IssueLoadResult {
                load_id,
                state,
                issues,
                error,
                done: true,
            });
        }
    });
}

fn load_issues(
    repo: &str,
    state: IssueStateFilter,
    limit: &str,
) -> Result<Vec<IssueEntry>, String> {
    let args = vec![
        "issue".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--state".to_string(),
        state.gh_arg().to_string(),
        "--limit".to_string(),
        limit.to_string(),
        "--json".to_string(),
        "number,title,state,author,labels,assignees,createdAt,updatedAt,url".to_string(),
    ];
    let output = run_command_output("gh", &args, None)
        .ok_or_else(|| "failed to load issues with gh".to_string())?;
    parse_issues_json(&output).map_err(|error| error.to_string())
}

fn ensure_selected_issue_detail(
    repo: &str,
    issues: &[IssueEntry],
    filtered: &[usize],
    selected: usize,
    in_flight: &mut HashSet<u64>,
    tx: &mpsc::Sender<IssueDetailResult>,
) {
    let Some(issue) = selected_issue(issues, filtered, selected) else {
        return;
    };
    if issue.detail_loaded || in_flight.contains(&issue.number) {
        return;
    }
    in_flight.insert(issue.number);
    spawn_issue_detail_load(repo.to_string(), issue.number, tx.clone());
}

fn spawn_issue_detail_load(repo: String, number: u64, tx: mpsc::Sender<IssueDetailResult>) {
    thread::spawn(move || {
        let result = load_issue_detail(&repo, number)
            .map(|detail| IssueDetailResult {
                number,
                body: detail.body,
                milestone: detail.milestone.map(|milestone| milestone.title),
                comments: Some(detail.comments.count()),
                error: None,
            })
            .unwrap_or_else(|message| IssueDetailResult {
                number,
                body: None,
                milestone: None,
                comments: None,
                error: Some(message),
            });
        let _ = tx.send(result);
    });
}

fn load_issue_detail(repo: &str, number: u64) -> Result<GhIssueDetail, String> {
    let args = vec![
        "issue".to_string(),
        "view".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--json".to_string(),
        "body,comments,milestone".to_string(),
    ];
    let output = run_command_output("gh", &args, None)
        .ok_or_else(|| format!("failed to load issue #{number} with gh"))?;
    serde_json::from_str::<GhIssueDetail>(&output).map_err(|error| error.to_string())
}

fn parse_issues_json(output: &str) -> serde_json::Result<Vec<IssueEntry>> {
    let issues = serde_json::from_str::<Vec<GhIssue>>(output)?;
    Ok(issues
        .into_iter()
        .map(|issue| IssueEntry {
            number: issue.number,
            title: issue.title,
            state: issue.state,
            author: issue.author.map(|author| author.login),
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            assignees: issue.assignees.into_iter().map(|user| user.login).collect(),
            milestone: issue.milestone.map(|milestone| milestone.title),
            comments: issue.comments.count(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            url: issue.url,
            body: issue.body,
            detail_loaded: false,
            detail_error: None,
        })
        .collect())
}

fn repo_for_path(path: &Path) -> Option<String> {
    let repo_dir = git_command_dir_for_path(path)?;
    let remote = run_git_command_allow_empty(&repo_dir, &["remote", "get-url", "origin"])?;
    github_repo_from_remote(&remote)
}

fn git_command_dir_for_path(path: &Path) -> Option<PathBuf> {
    if path.join(".git").is_dir() || path.join(".git").is_file() {
        return Some(path.to_path_buf());
    }
    let output = run_git_command_allow_empty(path, &["rev-parse", "--show-toplevel"])?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn run_git_command_allow_empty(repo_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("-c")
        .arg("color.ui=never")
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    )
}

fn github_repo_from_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = if let Some(value) = trimmed.strip_prefix("git@github.com:") {
        value
    } else {
        let (_, value) = trimmed.split_once("github.com/")?;
        value
    };
    github_repo_from_path(path)
}

fn github_repo_from_path(path: &str) -> Option<String> {
    let without_query = path.split('?').next().unwrap_or(path);
    let without_fragment = without_query.split('#').next().unwrap_or(without_query);
    let normalized = without_fragment
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| without_fragment.trim_matches('/'));
    let mut parts = normalized.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn filter_issues(issues: &[IssueEntry], query: &str) -> Vec<usize> {
    let tokens = query
        .split_whitespace()
        .filter(|token| !token.trim().is_empty())
        .collect::<Vec<&str>>();
    if tokens.is_empty() {
        return (0..issues.len()).collect();
    }

    issues
        .iter()
        .enumerate()
        .filter_map(|(index, issue)| {
            if tokens.iter().all(|token| issue_matches(issue, token)) {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

fn merge_issue_details(existing: &[IssueEntry], next: &mut [IssueEntry]) {
    for issue in next {
        let Some(existing_issue) = existing
            .iter()
            .find(|existing_issue| existing_issue.number == issue.number)
        else {
            continue;
        };
        if existing_issue.detail_loaded {
            issue.body = existing_issue.body.clone();
            issue.milestone = existing_issue
                .milestone
                .clone()
                .or_else(|| issue.milestone.clone());
            issue.comments = existing_issue.comments;
            issue.detail_loaded = true;
            issue.detail_error = existing_issue.detail_error.clone();
        }
    }
}

fn issue_matches(issue: &IssueEntry, token: &str) -> bool {
    if let Some(number) = token.strip_prefix('#') {
        return issue.number.to_string().contains(number)
            || issue.labels.iter().any(|label| fuzzy_match(number, label));
    }
    if let Some(author) = token.strip_prefix('@') {
        return issue
            .author
            .as_deref()
            .is_some_and(|value| fuzzy_match(author, value))
            || issue
                .assignees
                .iter()
                .any(|value| fuzzy_match(author, value));
    }
    fuzzy_match(token, &issue.title)
        || issue.number.to_string().contains(token)
        || issue.labels.iter().any(|label| fuzzy_match(token, label))
        || issue
            .body
            .as_deref()
            .is_some_and(|body| fuzzy_match(token, body))
}

fn selected_issue<'a>(
    issues: &'a [IssueEntry],
    filtered: &[usize],
    selected: usize,
) -> Option<&'a IssueEntry> {
    filtered.get(selected).and_then(|index| issues.get(*index))
}

fn selected_index_for_issue_number(
    issues: &[IssueEntry],
    filtered: &[usize],
    number: u64,
) -> Option<usize> {
    filtered.iter().position(|index| {
        issues
            .get(*index)
            .is_some_and(|issue| issue.number == number)
    })
}

fn issue_list_items(
    issues: &[IssueEntry],
    filtered: &[usize],
    offset: usize,
    height: usize,
    width: usize,
    text: Color,
    muted: Color,
) -> Vec<ListItem<'static>> {
    if filtered.is_empty() || height == 0 {
        return vec![ListItem::new(Line::from(Span::styled(
            "No issues",
            Style::default().fg(muted),
        )))];
    }
    let end = (offset + height).min(filtered.len());
    filtered[offset..end]
        .iter()
        .filter_map(|index| issues.get(*index))
        .map(|issue| {
            let state = if issue.state.eq_ignore_ascii_case("open") {
                "OPEN"
            } else {
                "DONE"
            };
            let labels = if issue.labels.is_empty() {
                String::new()
            } else {
                format!("[{}]", issue.labels.join(","))
            };
            let prefix = format!("#{:<5}{state:<5} ", issue.number);
            let prefix_len = prefix.chars().count();
            let labels_len = labels.chars().count();
            let label_gap = usize::from(!labels.is_empty());
            let title_width = width.saturating_sub(prefix_len + labels_len + label_gap);
            let title = truncate_with_ellipsis(&issue.title, title_width);
            let used = prefix_len + title.chars().count() + labels_len;
            let padding = width.saturating_sub(used);
            ListItem::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(muted)),
                Span::styled(title, Style::default().fg(text)),
                Span::raw(" ".repeat(padding)),
                Span::styled(labels, Style::default().fg(muted)),
            ]))
        })
        .collect()
}

fn truncate_with_ellipsis(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = value.chars().count();
    if count <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let trimmed = value.chars().take(max - 3).collect::<String>();
    format!("{trimmed}...")
}

fn issue_detail_text(
    repo: &str,
    issue: Option<&IssueEntry>,
    error: Option<&str>,
    loading: bool,
    accent: Color,
    muted: Color,
    text: Color,
) -> Text<'static> {
    let heading = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let value = Style::default().fg(text);
    let subtle = Style::default().fg(muted);
    if loading {
        return Text::from(Line::from(Span::styled(
            "Loading GitHub issues...",
            heading,
        )));
    }
    if let Some(error) = error {
        return Text::from(vec![
            Line::from(Span::styled("Unable to load issues", heading)),
            Line::from(""),
            Line::from(Span::styled(error.to_string(), subtle)),
        ]);
    }
    let Some(issue) = issue else {
        return Text::from(vec![
            Line::from(Span::styled(repo.to_string(), heading)),
            Line::from(""),
            Line::from(Span::styled("No issue selected", subtle)),
        ]);
    };

    let mut lines = vec![
        Line::from(Span::styled(
            format!("#{} {}", issue.number, issue.title),
            heading,
        )),
        Line::from(Span::styled(issue.url.clone(), subtle)),
        Line::from(""),
        Line::from(Span::styled(issue_metadata(issue), subtle)),
    ];
    if !issue.labels.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Labels: {}", issue.labels.join(", ")),
            subtle,
        )));
    }
    if !issue.assignees.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Assignees: {}", issue.assignees.join(", ")),
            subtle,
        )));
    }
    if let Some(milestone) = issue.milestone.as_ref() {
        lines.push(Line::from(Span::styled(
            format!("Milestone: {milestone}"),
            subtle,
        )));
    }
    lines.push(Line::from(""));
    if let Some(error) = issue.detail_error.as_ref() {
        lines.push(Line::from(Span::styled(
            format!("Unable to load description: {error}"),
            subtle,
        )));
    } else {
        match issue.body.as_deref().filter(|body| !body.trim().is_empty()) {
            Some(body) => lines.extend(
                body.lines()
                    .take(400)
                    .map(|line| Line::from(Span::styled(line.to_string(), value))),
            ),
            None if issue.detail_loaded => {
                lines.push(Line::from(Span::styled("No description", subtle)));
            }
            None => lines.push(Line::from(Span::styled("Loading description...", subtle))),
        }
    }
    Text::from(lines)
}

fn issue_metadata(issue: &IssueEntry) -> String {
    let mut parts = vec![
        format!("State: {}", issue.state),
        format!("Comments: {}", issue.comments),
    ];
    if let Some(author) = issue.author.as_ref() {
        parts.push(format!("Author: {author}"));
    }
    if let Some(created) = issue.created_at.as_ref() {
        parts.push(format!("Created: {}", short_timestamp(created)));
    }
    if let Some(updated) = issue.updated_at.as_ref() {
        parts.push(format!("Updated: {}", short_timestamp(updated)));
    }
    parts.join(" | ")
}

fn short_timestamp(value: &str) -> String {
    value
        .split('T')
        .next()
        .filter(|date| !date.is_empty())
        .unwrap_or(value)
        .to_string()
}

fn issue_help_line(
    state_filter: IssueStateFilter,
    key_color: Color,
    text: Color,
    accent: Color,
) -> Line<'static> {
    let key = Style::default().fg(key_color).add_modifier(Modifier::BOLD);
    let label = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let regular = Style::default().fg(text);
    Line::from(vec![
        Span::styled("Issues", label),
        Span::styled("  type filter  ", regular),
        Span::styled("#", key),
        Span::styled(" number/label  ", regular),
        Span::styled("@", key),
        Span::styled(" author/assignee  ", regular),
        Span::styled("Tab", key),
        Span::styled(format!(" {}  ", state_filter.label()), regular),
        Span::styled("r", key),
        Span::styled(" refresh  ", regular),
        Span::styled("Enter", key),
        Span::styled(" select URL  ", regular),
        Span::styled("Ctrl+Y", key),
        Span::styled(" copy URL", regular),
    ])
}

fn list_window_offset(
    selected: usize,
    current_offset: usize,
    height: usize,
    total: usize,
) -> usize {
    if total == 0 || height == 0 {
        return 0;
    }
    let mut offset = current_offset.min(total.saturating_sub(1));
    if selected < offset {
        offset = selected;
    } else if selected >= offset + height {
        offset = selected + 1 - height;
    }
    offset.min(total.saturating_sub(height))
}

#[cfg(test)]
mod tests {
    use super::{filter_issues, parse_issues_json};

    #[test]
    fn parses_issue_list_json() {
        let issues = parse_issues_json(
            r##"[
                {
                    "number": 12,
                    "title": "Add issue explorer",
                    "state": "OPEN",
                    "author": {"login": "yarden"},
                    "labels": [{"name": "feature"}],
                    "assignees": [{"login": "kcw"}],
                    "milestone": {"title": "v1"},
                    "comments": [{"id": "IC_1"}, {"id": "IC_2"}, {"id": "IC_3"}],
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-02T00:00:00Z",
                    "url": "https://github.com/o/r/issues/12",
                    "body": "Details"
                }
            ]"##,
        )
        .expect("valid issue JSON should parse");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 12);
        assert_eq!(issues[0].labels, vec!["feature"]);
        assert_eq!(issues[0].assignees, vec!["kcw"]);
        assert_eq!(issues[0].milestone.as_deref(), Some("v1"));
        assert_eq!(issues[0].comments, 3);
    }

    #[test]
    fn filters_issues_by_text_label_number_and_author() {
        let issues = parse_issues_json(
            r##"[
                {
                    "number": 12,
                    "title": "Add issue explorer",
                    "state": "OPEN",
                    "author": {"login": "yarden"},
                    "labels": [{"name": "feature"}],
                    "assignees": [],
                    "milestone": null,
                    "comments": 0,
                    "createdAt": null,
                    "updatedAt": null,
                    "url": "https://github.com/o/r/issues/12",
                    "body": "Details"
                }
            ]"##,
        )
        .expect("valid issue JSON should parse");

        assert_eq!(filter_issues(&issues, "explorer"), vec![0]);
        assert_eq!(filter_issues(&issues, "#feature"), vec![0]);
        assert_eq!(filter_issues(&issues, "#12"), vec![0]);
        assert_eq!(filter_issues(&issues, "@yarden"), vec![0]);
        assert!(filter_issues(&issues, "missing").is_empty());
    }
}
