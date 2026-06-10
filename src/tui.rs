// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use rusqlite::{Connection, OpenFlags};

const INDEX_DB: &str = ".hotpath/index.sqlite";
const METRIC_BAR_WIDTH: usize = 14;
const METRIC_LABEL_WIDTH: usize = 12;
const HOTSPOT_SELECTOR_WIDTH: usize = 2;
const HOTSPOT_SCORE_WIDTH: usize = 4;
const HOTSPOT_DEFAULT_TAG_WIDTH: usize = 22;
const HOTSPOT_NARROW_TAG_WIDTH: usize = 12;
const HOTSPOT_MIN_PATH_WIDTH: usize = 8;
const HOTSPOT_TAG_SEPARATOR: &str = " \u{00B7} ";
const OWNER_NAME_WIDTH: usize = 24;

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub fn run_tui() -> io::Result<()> {
    let root = env::current_dir()?;
    let snapshot = TuiDatabaseSnapshot::load_from_dir(root);
    let mut terminal = TerminalSession::enter()?;
    run_app(terminal.terminal_mut(), snapshot)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiDatabaseSnapshot {
    pub index_root: Option<PathBuf>,
    pub status: Option<String>,
    pub project: Option<ProjectRiskSummary>,
    pub git: GitMetadata,
    pub rows: Vec<RiskRow>,
}

impl TuiDatabaseSnapshot {
    pub fn load_from_dir(root: impl AsRef<Path>) -> Self {
        let Some(index_root) = find_index_root(root.as_ref()) else {
            return Self {
                index_root: None,
                status: Some("No Hotpath index found. Run hotpath scan first.".to_owned()),
                project: None,
                git: GitMetadata::default(),
                rows: Vec::new(),
            };
        };
        match Self::load_from_index_root(&index_root) {
            Ok(mut snapshot) => {
                if snapshot.rows.is_empty() && snapshot.status.is_none() {
                    snapshot.status = Some(
                        "No scored Go files found. Run hotpath scan after Go files are present."
                            .to_owned(),
                    );
                }
                snapshot
            }
            Err(error) => Self {
                index_root: Some(index_root),
                status: Some(format!("Could not read Hotpath index: {error}")),
                project: None,
                git: GitMetadata::default(),
                rows: Vec::new(),
            },
        }
    }

    fn load_from_index_root(index_root: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open_with_flags(
            index_root.join(INDEX_DB),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let project = load_project_summary(&connection)?;
        let git = load_git_metadata(&connection)?;
        let mut rows = load_risk_rows(&connection)?;
        let terms = load_terms(&connection)?;
        let facts = load_facts(&connection)?;
        let limitations = load_limitations(&connection)?;
        let owners = load_owners(&connection)?;

        for row in &mut rows {
            row.terms = terms.get(&row.relative_path).cloned().unwrap_or_default();
            row.facts = facts.get(&row.relative_path).cloned().unwrap_or_default();
            row.limitations = limitations
                .get(&row.relative_path)
                .cloned()
                .unwrap_or_default();
            row.owners = owners.get(&row.relative_path).cloned().unwrap_or_default();
            row.tags = tags_for_row(row);
        }

        Ok(Self {
            index_root: Some(index_root.to_path_buf()),
            status: None,
            project,
            git,
            rows,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitMetadata {
    pub mode: Option<String>,
    pub confidence: Option<String>,
    pub collection_mode: Option<String>,
    pub max_commits: Option<String>,
    pub max_age_days: Option<String>,
    pub first_parent: Option<String>,
    pub renames: Option<String>,
    pub cochange_max_files_per_commit: Option<String>,
    pub recent_churn_window_days: Option<String>,
    pub head_timestamp: Option<String>,
    pub first_parent_commit_count: Option<String>,
    pub all_reachable_commit_count: Option<String>,
    pub merge_commit_count: Option<String>,
    pub broad_commits_skipped_for_cochange: Option<String>,
    pub max_touched_files_in_commit: Option<String>,
    pub broadest_commit: Option<String>,
    pub likely_automated_author_count: Option<String>,
    pub top_author_touch_share_percent: Option<String>,
    pub author_identity_rule: Option<String>,
    pub mailmap: Option<String>,
    pub ownership_weighting: Option<String>,
    pub ownership_recency_half_life_days: Option<String>,
    pub warning: Option<String>,
    pub broad_commit_warning: Option<String>,
    pub author_concentration_warning: Option<String>,
    pub diagnostic: Option<String>,
    pub index_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRiskSummary {
    pub formula_id: String,
    pub score: f64,
    pub risk_10: f64,
    pub risk_band: String,
    pub confidence: String,
    pub active_file_count: u64,
    pub active_go_file_count: u64,
    pub scored_file_count: u64,
    pub scoring_coverage: f64,
    pub go_score_coverage: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskRow {
    pub rank: u64,
    pub relative_path: String,
    pub absolute_path: String,
    pub formula_id: String,
    pub score: f64,
    pub risk_10: f64,
    pub risk_band: String,
    pub is_generated: bool,
    pub is_vendor: bool,
    pub line_count: Option<u64>,
    pub byte_size: Option<u64>,
    pub language_id: Option<String>,
    pub complexity_pressure: Option<u64>,
    pub max_function_complexity_pressure: Option<u64>,
    pub source_coupling_pressure_in: Option<u64>,
    pub source_coupling_pressure_out: Option<u64>,
    pub co_changed_file_count: u64,
    pub total_churn_lines: u64,
    pub recent_churn_lines: u64,
    pub commits_per_file: u64,
    pub dominant_owner: Option<String>,
    pub dominant_owner_share: Option<f64>,
    pub owner_count: Option<u64>,
    pub author_count: u64,
    pub terms: Vec<RiskTerm>,
    pub facts: Vec<RiskFact>,
    pub limitations: Vec<RiskLimitation>,
    pub parser_diagnostics: Vec<RiskLimitation>,
    pub owners: Vec<RiskOwner>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskTerm {
    pub name: String,
    pub raw_value: Option<f64>,
    pub normalized_value: Option<f64>,
    pub weight: f64,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskFact {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskOwner {
    pub author: String,
    pub ownership_share: f64,
    pub touch_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiState {
    selected: usize,
    search_query: String,
    search_editing: bool,
    show_help: bool,
    should_exit: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TuiSeverity {
    High,
    Medium,
    Low,
    #[default]
    Neutral,
    Muted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiLayoutMode {
    Wide,
    Medium,
    Narrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiSizeBand {
    Small,
    Medium,
    Large,
    VeryLarge,
}

impl TuiSizeBand {
    fn label(self) -> &'static str {
        match self {
            Self::Small => "SMALL",
            Self::Medium => "MEDIUM",
            Self::Large => "LARGE",
            Self::VeryLarge => "VERY LARGE",
        }
    }

    fn bar_value(self) -> f64 {
        match self {
            Self::Small => 0.20,
            Self::Medium => 0.40,
            Self::Large => 0.60,
            Self::VeryLarge => 0.80,
        }
    }

    fn severity(self) -> TuiSeverity {
        match self {
            Self::Small => TuiSeverity::Low,
            Self::Medium | Self::Large => TuiSeverity::Medium,
            Self::VeryLarge => TuiSeverity::High,
        }
    }
}

impl TuiState {
    fn new() -> Self {
        Self {
            selected: 0,
            search_query: String::new(),
            search_editing: false,
            show_help: false,
            should_exit: false,
        }
    }

    fn filtered_indices(&self, snapshot: &TuiDatabaseSnapshot) -> Vec<usize> {
        if self.search_query.is_empty() {
            return (0..snapshot.rows.len()).collect();
        }
        let query = self.search_query.to_ascii_lowercase();
        snapshot
            .rows
            .iter()
            .enumerate()
            .filter(|(_index, row)| row.relative_path.to_ascii_lowercase().contains(&query))
            .map(|(index, _row)| index)
            .collect()
    }

    fn clamp(&mut self, row_count: usize) {
        if row_count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(row_count - 1);
        }
    }
}

fn run_app(terminal: &mut TuiTerminal, snapshot: TuiDatabaseSnapshot) -> io::Result<()> {
    let mut state = TuiState::new();
    loop {
        terminal.draw(|frame| render(frame, &snapshot, &state))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                reduce_key(&mut state, &snapshot, key);
                if state.should_exit {
                    return Ok(());
                }
            }
        }
    }
}

fn reduce_key(state: &mut TuiState, snapshot: &TuiDatabaseSnapshot, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if state.search_editing {
        match key.code {
            KeyCode::Esc => state.search_editing = false,
            KeyCode::Enter => state.search_editing = false,
            KeyCode::Backspace => {
                state.search_query.pop();
                state.clamp(state.filtered_indices(snapshot).len());
            }
            KeyCode::Char(character) => {
                state.search_query.push(character);
                state.clamp(state.filtered_indices(snapshot).len());
            }
            _ => {}
        }
        return;
    }

    let row_count = state.filtered_indices(snapshot).len();
    match key.code {
        KeyCode::Char('q') => state.should_exit = true,
        KeyCode::Char('?') => state.show_help = !state.show_help,
        KeyCode::Char('/') => state.search_editing = true,
        KeyCode::Esc => {
            if state.show_help {
                state.show_help = false;
            } else {
                state.search_query.clear();
                state.clamp(row_count);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected = (state.selected + 1).min(row_count.saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected = state.selected.saturating_sub(1);
        }
        KeyCode::Char('g') => state.selected = 0,
        KeyCode::Char('G') => state.selected = row_count.saturating_sub(1),
        _ => {}
    }
}

fn render(frame: &mut Frame<'_>, snapshot: &TuiDatabaseSnapshot, state: &TuiState) {
    let area = frame.area();
    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .areas(area);

    render_header(frame, header, snapshot);
    match layout_mode(area) {
        TuiLayoutMode::Wide => render_joined_body(frame, body, snapshot, state, [62, 38]),
        TuiLayoutMode::Medium => render_joined_body(frame, body, snapshot, state, [64, 36]),
        TuiLayoutMode::Narrow => render_narrow_body(frame, body, snapshot, state),
    }
    render_footer(frame, footer, state);
    if state.show_help {
        render_help(frame, area);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, snapshot: &TuiDatabaseSnapshot) {
    let project = snapshot.project.as_ref();
    let title = style(TuiSeverity::Low).add_modifier(Modifier::BOLD);
    let muted = style(TuiSeverity::Muted);
    let risk = project.map_or_else(
        || "risk unavailable".to_owned(),
        |project| format!("risk {:.1}/10 {}", project.risk_10, project.risk_band),
    );
    let confidence = project
        .map(|project| project.confidence.as_str())
        .unwrap_or("none");
    let active_files = project
        .map(|project| project.active_file_count)
        .unwrap_or(0);
    let active_go_files = project
        .map(|project| project.active_go_file_count)
        .unwrap_or(0);
    let scored_files = project
        .map(|project| project.scored_file_count)
        .unwrap_or(0);
    let coverage = project.map(project_coverage_percentage).unwrap_or(0.0);
    let formula = project
        .map(|project| project.formula_id.as_str())
        .unwrap_or("none");
    let git_line = git_status_line(&snapshot.git);

    let header = Paragraph::new(vec![
        Line::from(vec![Span::styled(
            "[1 Hotpath]",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        )]),
        Line::styled(horizontal_rule(area.width), style(TuiSeverity::Muted)),
        Line::from(vec![
            Span::styled("Hotpath", title),
            Span::raw("  "),
            Span::styled("local risk triage", muted),
            Span::raw("  "),
            Span::styled(risk, muted),
        ]),
        Line::from(vec![
            Span::styled("Hotpath", style(TuiSeverity::Medium)),
            Span::raw(" / "),
            Span::raw(format!(
                "active_file_count {active_files}  active_go_file_count {active_go_files}  scored_file_count {scored_files}  coverage {coverage:.1}%  confidence {confidence}"
            )),
            Span::raw("  "),
            Span::styled(format!("formula {formula}"), muted),
        ]),
        Line::styled(git_line, muted),
    ]);

    frame.render_widget(header, area);
}

fn git_status_line(git: &GitMetadata) -> String {
    let Some(confidence) = git.confidence.as_deref() else {
        return "git confidence none".to_owned();
    };
    let mut parts = vec![format!("git confidence {confidence}")];
    if let Some(mode) = &git.mode {
        parts.push(format!("mode {mode}"));
    }
    if let Some(max_commits) = &git.max_commits {
        parts.push(format!("max_commits {max_commits}"));
    }
    if let Some(max_age_days) = &git.max_age_days {
        parts.push(format!("max_age_days {max_age_days}"));
    }
    if let Some(first_parent) = &git.first_parent {
        parts.push(format!("first_parent {first_parent}"));
    }
    if let Some(renames) = &git.renames {
        parts.push(format!("renames {renames}"));
    }
    if let Some(limit) = &git.cochange_max_files_per_commit {
        parts.push(format!("cochange_max_files_per_commit {limit}"));
    }
    if let Some(window) = &git.recent_churn_window_days {
        parts.push(format!("recent_churn_window_days {window}"));
    }
    if let Some(head_timestamp) = &git.head_timestamp {
        parts.push(format!("head_timestamp {head_timestamp}"));
    }
    if let Some(first_parent_count) = &git.first_parent_commit_count {
        parts.push(format!("first_parent_commits {first_parent_count}"));
    }
    if let Some(all_reachable_count) = &git.all_reachable_commit_count {
        parts.push(format!("all_reachable_commits {all_reachable_count}"));
    }
    if let Some(merge_count) = &git.merge_commit_count {
        parts.push(format!("merge_commits {merge_count}"));
    }
    if let Some(broad_count) = &git.broad_commits_skipped_for_cochange {
        parts.push(format!("broad_commits_skipped {broad_count}"));
    }
    if let Some(max_touched) = &git.max_touched_files_in_commit {
        parts.push(format!("max_touched_files {max_touched}"));
    }
    if let Some(bot_count) = &git.likely_automated_author_count {
        parts.push(format!("likely_automated_authors {bot_count}"));
    }
    if let Some(top_share) = &git.top_author_touch_share_percent {
        parts.push(format!("top_author_touch_share_percent {top_share}"));
    }
    if let Some(author_rule) = &git.author_identity_rule {
        parts.push(format!("author_identity {author_rule}"));
    }
    if let Some(mailmap) = &git.mailmap {
        parts.push(format!("mailmap {mailmap}"));
    }
    if let Some(half_life) = &git.ownership_recency_half_life_days {
        parts.push(format!("ownership_half_life_days {half_life}"));
    }
    if let Some(warning) = &git.warning {
        parts.push(format!("warning {warning}"));
    }
    if let Some(warning) = &git.broad_commit_warning {
        if !warning.is_empty() {
            parts.push(format!("warning {warning}"));
        }
    }
    if let Some(warning) = &git.author_concentration_warning {
        if !warning.is_empty() {
            parts.push(format!("warning {warning}"));
        }
    }
    if let Some(diagnostic) = &git.diagnostic {
        parts.push(format!("diagnostic {diagnostic}"));
    }
    if let Some(index_action) = &git.index_action {
        parts.push(format!("index_action {index_action}"));
    }
    parts.join("  ")
}

fn render_joined_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiDatabaseSnapshot,
    state: &TuiState,
    split_percentages: [u16; 2],
) {
    let block = plain_panel_block(true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [main, divider, inspector] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(split_percentages[0]),
            Constraint::Length(1),
            Constraint::Percentage(split_percentages[1]),
        ])
        .areas(inner);
    render_main(frame, main, snapshot, state);
    render_divider(frame, divider);
    render_inspector(frame, inspector, snapshot, state);
}

fn render_narrow_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiDatabaseSnapshot,
    state: &TuiState,
) {
    let rows = visible_risk_lines(
        snapshot,
        state,
        area.width.saturating_sub(4),
        area.height as usize / 2,
    );
    let selected = selected_row(snapshot, state);
    let mut lines = rows;
    if let Some(row) = selected {
        lines.push(Line::raw(""));
        lines.extend(inspector_lines(row, area.width.saturating_sub(4)));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block("Hotpath", true))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_main(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiDatabaseSnapshot,
    state: &TuiState,
) {
    let content = padded_rect(area, 1, 1, 0, 0);
    let mut lines = vec![
        Line::styled(
            "Hotpath",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    lines.extend(hotpath_kpi_lines(snapshot));
    lines.push(hotspot_header_line(content.width));
    let header_height = lines.len();
    lines.extend(visible_risk_lines(
        snapshot,
        state,
        content.width.max(1),
        content.height.saturating_sub(header_height as u16) as usize,
    ));
    frame.render_widget(Paragraph::new(lines), content);
}

fn project_coverage_percentage(project: &ProjectRiskSummary) -> f64 {
    project
        .go_score_coverage
        .unwrap_or(project.scoring_coverage)
        .clamp(0.0, 1.0)
        * 100.0
}

fn hotpath_kpi_lines(snapshot: &TuiDatabaseSnapshot) -> Vec<Line<'static>> {
    let score = snapshot
        .project
        .as_ref()
        .map(|project| project.score)
        .or_else(|| snapshot.rows.first().map(|row| row.score))
        .unwrap_or(0.0);
    let coverage_line = snapshot.project.as_ref().map(|project| {
        Line::styled(
            format!(
                "Go coverage: {:.1}% (scored_file_count {} / active_go_file_count {}, active_file_count {})",
                project_coverage_percentage(project),
                project.scored_file_count,
                project.active_go_file_count,
                project.active_file_count
            ),
            style(TuiSeverity::Muted),
        )
    });
    let mut lines = vec![metric_bar_line(
        "Repo Risk",
        score,
        format!("{} {:.1}", severity_label(score), score * 10.0),
        severity_for_score(score),
    )];
    if let Some(coverage_line) = coverage_line {
        lines.push(coverage_line);
    }
    lines.push(Line::raw(""));
    lines
}

fn render_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiDatabaseSnapshot,
    state: &TuiState,
) {
    let content = padded_rect(area, 1, 1, 0, 0);
    let mut lines = vec![
        Line::styled(
            "Inspector",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    match selected_row(snapshot, state) {
        Some(row) => lines.extend(inspector_lines(row, content.width)),
        None => lines.extend(empty_state_lines(snapshot)),
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
}

fn render_divider(frame: &mut Frame<'_>, area: Rect) {
    let lines = (0..area.height)
        .map(|_| Line::styled("\u{2502}", style(TuiSeverity::Muted)))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = if state.search_editing {
        format!("/{}", state.search_query)
    } else if state.search_query.is_empty() {
        [
            "j/k or arrows move",
            "/ search",
            "g/G jump",
            "? help",
            "q quit",
        ]
        .join(HOTSPOT_TAG_SEPARATOR)
    } else {
        format!(
            "filter /{}{HOTSPOT_TAG_SEPARATOR}Esc clear{HOTSPOT_TAG_SEPARATOR}j/k move{HOTSPOT_TAG_SEPARATOR}? help{HOTSPOT_TAG_SEPARATOR}q quit",
            state.search_query,
        )
    };
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Left), area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let width = area.width.min(54);
    let height = area.height.min(10);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let lines = vec![
        Line::styled(
            "Hotpath keys",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("j/k or arrows  move selection"),
        Line::raw("g/G            first / last row"),
        Line::raw("/              search paths"),
        Line::raw("Esc            clear search or close help"),
        Line::raw("q              quit"),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Help", true)),
        popup,
    );
}

fn visible_risk_lines(
    snapshot: &TuiDatabaseSnapshot,
    state: &TuiState,
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let indices = state.filtered_indices(snapshot);
    if snapshot.rows.is_empty() {
        return empty_state_lines(snapshot);
    }
    if indices.is_empty() {
        return vec![Line::styled(
            "No rows match the current search.",
            Style::default().fg(Color::DarkGray),
        )];
    }
    let selected = state.selected.min(indices.len().saturating_sub(1));
    let start = selected
        .saturating_add(1)
        .saturating_sub(max_rows)
        .min(indices.len());
    indices
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows.max(1))
        .filter_map(|(display_index, row_index)| {
            snapshot
                .rows
                .get(*row_index)
                .map(|row| risk_row_line(row, display_index == selected, width))
        })
        .collect()
}

fn risk_row_line(row: &RiskRow, selected: bool, width: u16) -> Line<'static> {
    let marker = if selected { "\u{258C}" } else { " " };
    let row_style = if selected {
        selected_row_style(severity_for_score(row.score))
    } else {
        style(severity_for_score(row.score))
    };
    let muted_style = if selected {
        selected_row_style(TuiSeverity::Muted)
    } else {
        style(TuiSeverity::Muted)
    };
    let gap_style = if selected {
        selected_gap_style()
    } else {
        Style::default()
    };
    let tags = hotspot_tag_text(&row.tags);
    let (path_width, tag_width) = hotspot_column_widths(width as usize);
    let path = pad_truncated_path(&row.relative_path, path_width);

    let mut spans = vec![
        Span::styled(
            format!("{marker:<HOTSPOT_SELECTOR_WIDTH$}"),
            marker_style(selected),
        ),
        Span::styled(path, row_style),
        Span::styled("  ", gap_style),
    ];
    if selected {
        spans.extend(selected_score_bar_spans(
            row.score,
            METRIC_BAR_WIDTH,
            row_style,
        ));
    } else {
        spans.extend(score_bar_spans(row.score, METRIC_BAR_WIDTH, row_style));
    }
    spans.push(Span::styled(
        format!(" {:>HOTSPOT_SCORE_WIDTH$.1}", row.risk_10),
        row_style,
    ));
    if tag_width > 0 && !tags.is_empty() {
        spans.push(Span::styled("  ", gap_style));
        spans.push(Span::styled(
            pad_truncated_end(&tags, tag_width),
            muted_style,
        ));
    }

    Line::from(spans)
}

fn inspector_lines(row: &RiskRow, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(
            truncate_middle(&row.relative_path, width.saturating_sub(4) as usize),
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        metric_bar_line(
            "RISK",
            row.score,
            severity_label(row.score).to_owned(),
            severity_for_score(row.score),
        ),
        metric_bar_line(
            "FRAGILITY",
            ownership_risk(row),
            ownership_risk_label(row).to_owned(),
            severity_for_score(ownership_risk(row)),
        ),
        metric_bar_line(
            "COORD PRESS",
            coordination_pressure(row),
            coordination_severity_label(coordination_pressure(row)).to_owned(),
            severity_for_score(coordination_pressure(row)),
        ),
        metric_bar_line(
            "CMPX PRESS",
            complexity_pressure(row),
            complexity_severity_label(row.max_function_complexity_pressure.unwrap_or(0) as f64)
                .to_owned(),
            severity_for_complexity_score(row.max_function_complexity_pressure.unwrap_or(0) as f64),
        ),
    ];
    if let Some(line_count) = row.line_count {
        let band = line_size_band(line_count);
        lines.push(metric_bar_line(
            "SIZE",
            band.bar_value(),
            format!(
                "{} {} lines",
                band.label(),
                format_compact_count(line_count)
            ),
            band.severity(),
        ));
    }

    let tags = hotspot_tag_text(&inspector_tags(row));
    if !tags.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(tags, style(TuiSeverity::Muted)));
    }

    if !row.parser_diagnostics.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section_divider(width));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Parser diagnostics",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ));
        for diagnostic in &row.parser_diagnostics {
            lines.push(Line::styled(
                format!("  - {}: {}", diagnostic.code, diagnostic.message),
                style(TuiSeverity::Muted),
            ));
        }
    }

    lines.push(Line::raw(""));
    lines.push(section_divider(width));
    lines.extend(risk_driver_lines(row));

    lines.push(Line::raw(""));
    lines.push(section_divider(width));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("Ownership ({})", ownership_shape_label(row)),
        style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
    ));
    lines.extend(ownership_distribution_lines(row));

    if !row.limitations.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section_divider(width));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Limitations",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ));
        for limitation in &row.limitations {
            lines.push(Line::styled(
                format!("  - {}: {}", limitation.code, limitation.message),
                style(TuiSeverity::Muted),
            ));
        }
    }
    lines
}

fn empty_state_lines(snapshot: &TuiDatabaseSnapshot) -> Vec<Line<'static>> {
    vec![Line::styled(
        snapshot
            .status
            .as_deref()
            .unwrap_or("No scored Go files found. Run hotpath scan after Go files are present.")
            .to_owned(),
        Style::default().fg(Color::DarkGray),
    )]
}

fn selected_row<'a>(snapshot: &'a TuiDatabaseSnapshot, state: &TuiState) -> Option<&'a RiskRow> {
    let indices = state.filtered_indices(snapshot);
    indices
        .get(state.selected.min(indices.len().saturating_sub(1)))
        .and_then(|index| snapshot.rows.get(*index))
}

fn load_project_summary(connection: &Connection) -> rusqlite::Result<Option<ProjectRiskSummary>> {
    let mut statement = connection.prepare(
        "
        SELECT
            formula_id,
            score,
            risk_10,
            risk_band,
            confidence,
            active_file_count,
            active_go_file_count,
            scored_file_count,
            scoring_coverage,
            go_score_coverage
        FROM project_risk_summary
        ORDER BY formula_id
        LIMIT 1
        ",
    )?;
    let result = statement.query_row([], |row| {
        Ok(ProjectRiskSummary {
            formula_id: row.get(0)?,
            score: row.get(1)?,
            risk_10: row.get(2)?,
            risk_band: row.get(3)?,
            confidence: row.get(4)?,
            active_file_count: i64_to_u64(row.get::<_, i64>(5)?),
            active_go_file_count: i64_to_u64(row.get::<_, i64>(6)?),
            scored_file_count: i64_to_u64(row.get::<_, i64>(7)?),
            scoring_coverage: row.get(8)?,
            go_score_coverage: row.get(9)?,
        })
    });
    match result {
        Ok(summary) => Ok(Some(summary)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_git_metadata(connection: &Connection) -> rusqlite::Result<GitMetadata> {
    if !table_exists(connection, "stage_metadata")? {
        return Ok(GitMetadata::default());
    }

    let mut statement = connection.prepare(
        "
        SELECT key, value
        FROM stage_metadata
        WHERE key LIKE 'git_%'
        ORDER BY key ASC
        ",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut values = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        values.insert(key, value);
    }

    Ok(GitMetadata {
        mode: values.get("git_scan_mode").cloned(),
        confidence: values.get("git_confidence").cloned(),
        collection_mode: values.get("git_collection_mode").cloned(),
        max_commits: values.get("git_max_commits").cloned(),
        max_age_days: values.get("git_max_age_days").cloned(),
        first_parent: values.get("git_first_parent").cloned(),
        renames: values.get("git_renames").cloned(),
        cochange_max_files_per_commit: values.get("git_cochange_max_files_per_commit").cloned(),
        recent_churn_window_days: values.get("git_recent_churn_window_days").cloned(),
        head_timestamp: values.get("git_head_timestamp").cloned(),
        first_parent_commit_count: values.get("git_first_parent_commit_count").cloned(),
        all_reachable_commit_count: values.get("git_all_reachable_commit_count").cloned(),
        merge_commit_count: values.get("git_merge_commit_count").cloned(),
        broad_commits_skipped_for_cochange: values
            .get("git_broad_commits_skipped_for_cochange")
            .cloned(),
        max_touched_files_in_commit: values.get("git_max_touched_files_in_commit").cloned(),
        broadest_commit: values.get("git_broadest_commit").cloned(),
        likely_automated_author_count: values.get("git_likely_automated_author_count").cloned(),
        top_author_touch_share_percent: values.get("git_top_author_touch_share_percent").cloned(),
        author_identity_rule: values.get("git_author_identity_rule").cloned(),
        mailmap: values.get("git_mailmap").cloned(),
        ownership_weighting: values.get("git_ownership_weighting").cloned(),
        ownership_recency_half_life_days: values
            .get("git_ownership_recency_half_life_days")
            .cloned(),
        warning: values.get("git_merge_heavy_warning").cloned(),
        broad_commit_warning: values.get("git_broad_commit_warning").cloned(),
        author_concentration_warning: values.get("git_author_concentration_warning").cloned(),
        diagnostic: values
            .get("git_diagnostic_message")
            .or_else(|| values.get("git_diagnostic"))
            .cloned(),
        index_action: values.get("git_index_action").cloned(),
    })
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| Ok(row.get::<_, i64>(0)? > 0),
    )
}

fn load_risk_rows(connection: &Connection) -> rusqlite::Result<Vec<RiskRow>> {
    let mut statement = connection.prepare(
        "
        SELECT
            score.rank,
            score.relative_path,
            score.path,
            score.formula_id,
            score.score,
            score.risk_10,
            score.risk_band,
            score.is_generated,
            score.is_vendor,
            facts.line_count,
            facts.byte_size,
            facts.language_id,
            facts.complexity_pressure,
            facts.max_function_complexity_pressure,
            facts.source_coupling_pressure_in,
            facts.source_coupling_pressure_out,
            facts.co_changed_file_count,
            facts.total_churn_lines,
            facts.recent_churn_lines,
            facts.commits_per_file,
            facts.dominant_owner,
            facts.dominant_owner_share,
            facts.owner_count,
            facts.author_count,
            facts.diagnostics
        FROM file_risk_scores score
        LEFT JOIN file_facts facts
            ON facts.relative_path = score.relative_path
        ORDER BY score.score DESC, score.relative_path ASC
        ",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(RiskRow {
            rank: i64_to_u64(row.get::<_, i64>(0)?),
            relative_path: row.get(1)?,
            absolute_path: row.get(2)?,
            formula_id: row.get(3)?,
            score: row.get(4)?,
            risk_10: row.get(5)?,
            risk_band: row.get(6)?,
            is_generated: row.get::<_, i64>(7)? != 0,
            is_vendor: row.get::<_, i64>(8)? != 0,
            line_count: optional_i64_to_u64(row.get::<_, Option<i64>>(9)?),
            byte_size: optional_i64_to_u64(row.get::<_, Option<i64>>(10)?),
            language_id: row.get(11)?,
            complexity_pressure: optional_i64_to_u64(row.get::<_, Option<i64>>(12)?),
            max_function_complexity_pressure: optional_i64_to_u64(row.get::<_, Option<i64>>(13)?),
            source_coupling_pressure_in: optional_i64_to_u64(row.get::<_, Option<i64>>(14)?),
            source_coupling_pressure_out: optional_i64_to_u64(row.get::<_, Option<i64>>(15)?),
            co_changed_file_count: i64_to_u64(row.get::<_, i64>(16)?),
            total_churn_lines: i64_to_u64(row.get::<_, i64>(17)?),
            recent_churn_lines: i64_to_u64(row.get::<_, i64>(18)?),
            commits_per_file: i64_to_u64(row.get::<_, i64>(19)?),
            dominant_owner: row.get(20)?,
            dominant_owner_share: row.get(21)?,
            owner_count: optional_i64_to_u64(row.get::<_, Option<i64>>(22)?),
            author_count: i64_to_u64(row.get::<_, i64>(23)?),
            terms: Vec::new(),
            facts: Vec::new(),
            limitations: Vec::new(),
            parser_diagnostics: parse_diagnostics_json(row.get::<_, String>(24)?.as_str()),
            owners: Vec::new(),
            tags: Vec::new(),
        })
    })?;
    rows.collect()
}

fn load_terms(connection: &Connection) -> rusqlite::Result<BTreeMap<String, Vec<RiskTerm>>> {
    let mut statement = connection.prepare(
        "
        SELECT relative_path, term_name, raw_value, normalized_value, weight, contribution
        FROM file_risk_terms
        ORDER BY relative_path, term_name
        ",
    )?;
    let mut grouped = BTreeMap::new();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RiskTerm {
                name: row.get(1)?,
                raw_value: row.get(2)?,
                normalized_value: row.get(3)?,
                weight: row.get(4)?,
                contribution: row.get(5)?,
            },
        ))
    })?;
    for row in rows {
        let (path, term) = row?;
        grouped.entry(path).or_insert_with(Vec::new).push(term);
    }
    Ok(grouped)
}

fn load_facts(connection: &Connection) -> rusqlite::Result<BTreeMap<String, Vec<RiskFact>>> {
    let mut statement = connection.prepare(
        "
        SELECT relative_path, fact_kind, message
        FROM file_risk_facts
        ORDER BY relative_path, fact_index
        ",
    )?;
    let mut grouped = BTreeMap::new();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RiskFact {
                kind: row.get(1)?,
                message: row.get(2)?,
            },
        ))
    })?;
    for row in rows {
        let (path, fact) = row?;
        grouped.entry(path).or_insert_with(Vec::new).push(fact);
    }
    Ok(grouped)
}

fn load_limitations(
    connection: &Connection,
) -> rusqlite::Result<BTreeMap<String, Vec<RiskLimitation>>> {
    let mut statement = connection.prepare(
        "
        SELECT relative_path, code, message
        FROM file_risk_limitations
        ORDER BY relative_path, limitation_index
        ",
    )?;
    let mut grouped = BTreeMap::new();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RiskLimitation {
                code: row.get(1)?,
                message: row.get(2)?,
            },
        ))
    })?;
    for row in rows {
        let (path, limitation) = row?;
        grouped
            .entry(path)
            .or_insert_with(Vec::new)
            .push(limitation);
    }
    Ok(grouped)
}

fn load_owners(connection: &Connection) -> rusqlite::Result<BTreeMap<String, Vec<RiskOwner>>> {
    let mut statement = connection.prepare(
        "
        SELECT path, author, ownership_share, touch_count
        FROM git_file_owners
        ORDER BY path, owner_rank
        ",
    )?;
    let mut grouped = BTreeMap::new();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RiskOwner {
                author: row.get(1)?,
                ownership_share: row.get(2)?,
                touch_count: i64_to_u64(row.get::<_, i64>(3)?),
            },
        ))
    })?;
    for row in rows {
        let (path, owner) = row?;
        grouped.entry(path).or_insert_with(Vec::new).push(owner);
    }
    Ok(grouped)
}

fn parse_diagnostics_json(value: &str) -> Vec<RiskLimitation> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(value) else {
        return Vec::new();
    };

    items
        .into_iter()
        .filter_map(|item| {
            Some(RiskLimitation {
                code: item.get("code")?.as_str()?.to_owned(),
                message: item.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

fn find_index_root(current_dir: &Path) -> Option<PathBuf> {
    current_dir
        .ancestors()
        .find(|candidate| candidate.join(INDEX_DB).is_file())
        .map(Path::to_path_buf)
}

fn tags_for_row(row: &RiskRow) -> Vec<String> {
    inspector_tags(row).into_iter().take(1).collect()
}

fn inspector_tags(row: &RiskRow) -> Vec<String> {
    let mut signals = Vec::new();
    if row.is_generated {
        signals.push(("GEN", 1.0, 10));
    }
    if row.is_vendor {
        signals.push(("VENDOR", 1.0, 10));
    }
    if !row.parser_diagnostics.is_empty() {
        signals.push(("PARSER", 1.0, 95));
    }
    for term in &row.terms {
        let value = term.normalized_value.unwrap_or_default();
        if value < 0.60 {
            continue;
        }
        match term.name.as_str() {
            "churn" => signals.push(("CHURN", value, 90)),
            "recent_churn" => signals.push(("VOLATILITY", value, 65)),
            "size" => signals.push(("SIZE", value, 75)),
            "ownership_risk" => signals.push(("FRAGILITY", value, 80)),
            "cochange_pressure" | "source_coupling_pressure" => {
                signals.push(("COORD PRESS", value, 85))
            }
            "complexity_pressure" => signals.push(("CMPX PRESS", value, 70)),
            _ => {}
        }
    }

    signals.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| {
                right
                    .1
                    .partial_cmp(&left.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.0.cmp(right.0))
    });
    let mut tags = Vec::new();
    for (label, _, _) in signals {
        push_tag(&mut tags, label);
    }
    tags
}

fn push_tag(tags: &mut Vec<String>, tag: &str) {
    if !tags.iter().any(|existing| existing == tag) {
        tags.push(tag.to_owned());
    }
}

fn panel_block(title: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        style(TuiSeverity::Medium)
    } else {
        style(TuiSeverity::Muted)
    };
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
}

fn plain_panel_block(focused: bool) -> Block<'static> {
    let border_style = if focused {
        style(TuiSeverity::Medium)
    } else {
        style(TuiSeverity::Muted)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
}

fn metric_bar_line(label: &str, value: f64, text: String, severity: TuiSeverity) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<METRIC_LABEL_WIDTH$} "),
        style(TuiSeverity::Muted),
    )];
    spans.extend(score_bar_spans(value, METRIC_BAR_WIDTH, style(severity)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(text, style(severity)));

    Line::from(spans)
}

fn section_divider(width: u16) -> Line<'static> {
    let inner_width = width.saturating_sub(4).max(12) as usize;
    Line::styled(
        "\u{2500}".repeat(inner_width.min(48)),
        style(TuiSeverity::Muted),
    )
}

fn layout_mode(area: Rect) -> TuiLayoutMode {
    if area.width >= 120 && area.height >= 30 {
        TuiLayoutMode::Wide
    } else if area.width >= 90 {
        TuiLayoutMode::Medium
    } else {
        TuiLayoutMode::Narrow
    }
}

fn horizontal_rule(width: u16) -> String {
    "\u{2500}".repeat(width as usize)
}

fn hotspot_header_line(width: u16) -> Line<'static> {
    let (path_width, tag_width) = hotspot_column_widths(width as usize);
    let risk_width = METRIC_BAR_WIDTH + 1 + HOTSPOT_SCORE_WIDTH;
    let header_style = style(TuiSeverity::Muted).add_modifier(Modifier::BOLD);

    let mut spans = vec![
        Span::raw(" ".repeat(HOTSPOT_SELECTOR_WIDTH)),
        Span::styled(pad_truncated_end("Path", path_width), header_style),
        Span::raw("  "),
        Span::styled(format!("{:<risk_width$}", "Risk"), header_style),
    ];
    if tag_width > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            pad_truncated_end("Top Factor", tag_width),
            header_style,
        ));
    }

    Line::from(spans)
}

fn hotspot_tag_text(tags: &[String]) -> String {
    tags.join(HOTSPOT_TAG_SEPARATOR)
}

fn hotspot_column_widths(total_width: usize) -> (usize, usize) {
    let base_width = HOTSPOT_SELECTOR_WIDTH + 2 + METRIC_BAR_WIDTH + 1 + HOTSPOT_SCORE_WIDTH;
    let full_tag_width = 2 + HOTSPOT_DEFAULT_TAG_WIDTH;
    let narrow_tag_width = 2 + HOTSPOT_NARROW_TAG_WIDTH;

    if total_width >= base_width + HOTSPOT_MIN_PATH_WIDTH + full_tag_width {
        (
            total_width.saturating_sub(base_width + full_tag_width),
            HOTSPOT_DEFAULT_TAG_WIDTH,
        )
    } else if total_width >= base_width + HOTSPOT_MIN_PATH_WIDTH + narrow_tag_width {
        (
            total_width.saturating_sub(base_width + narrow_tag_width),
            HOTSPOT_NARROW_TAG_WIDTH,
        )
    } else {
        (
            total_width
                .saturating_sub(base_width)
                .max(HOTSPOT_MIN_PATH_WIDTH),
            0,
        )
    }
}

fn style(severity: TuiSeverity) -> Style {
    let color = match severity {
        TuiSeverity::High => Color::Red,
        TuiSeverity::Medium => Color::Yellow,
        TuiSeverity::Low => Color::Green,
        TuiSeverity::Neutral => Color::White,
        TuiSeverity::Muted => Color::DarkGray,
    };

    Style::default().fg(color)
}

fn selected_row_style(severity: TuiSeverity) -> Style {
    style(severity)
        .add_modifier(Modifier::BOLD)
        .bg(Color::Rgb(32, 32, 32))
}

fn selected_gap_style() -> Style {
    Style::default().bg(Color::Rgb(32, 32, 32))
}

fn marker_style(selected: bool) -> Style {
    if selected {
        selected_row_style(TuiSeverity::Medium)
    } else {
        Style::default()
    }
}

fn score_bar_parts(score: f64, width: usize) -> (String, String) {
    let filled = ((score.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    (
        "\u{25A0}".repeat(filled),
        "\u{25A1}".repeat(width.saturating_sub(filled)),
    )
}

fn score_bar_spans(score: f64, width: usize, active_style: Style) -> Vec<Span<'static>> {
    score_bar_spans_with_inactive(score, width, active_style, inactive_bar_style())
}

fn selected_score_bar_spans(score: f64, width: usize, active_style: Style) -> Vec<Span<'static>> {
    score_bar_spans_with_inactive(score, width, active_style, selected_inactive_bar_style())
}

fn score_bar_spans_with_inactive(
    score: f64,
    width: usize,
    active_style: Style,
    inactive_style: Style,
) -> Vec<Span<'static>> {
    let (filled, empty) = score_bar_parts(score, width);
    vec![
        Span::styled(filled, active_style),
        Span::styled(empty, inactive_style),
    ]
}

fn inactive_bar_style() -> Style {
    Style::default().fg(Color::Rgb(82, 82, 82)).bg(Color::Black)
}

fn selected_inactive_bar_style() -> Style {
    Style::default()
        .fg(Color::Rgb(118, 118, 118))
        .bg(Color::Rgb(32, 32, 32))
}

fn severity_for_score(score: f64) -> TuiSeverity {
    if score >= 0.70 {
        TuiSeverity::High
    } else if score >= 0.40 {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Low
    }
}

fn severity_label(value: f64) -> &'static str {
    if value >= 0.85 {
        "EXTREME"
    } else if value >= 0.70 {
        "HIGH"
    } else if value >= 0.40 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

fn severity_for_complexity_score(score: f64) -> TuiSeverity {
    if score >= 20.0 {
        TuiSeverity::High
    } else if score >= 10.0 {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Low
    }
}

fn complexity_severity_label(score: f64) -> &'static str {
    if score >= 35.0 {
        "EXTREME"
    } else if score >= 20.0 {
        "HIGH"
    } else if score >= 10.0 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

fn line_size_band(line_count: u64) -> TuiSizeBand {
    if line_count >= 2_000 {
        TuiSizeBand::VeryLarge
    } else if line_count >= 1_000 {
        TuiSizeBand::Large
    } else if line_count >= 250 {
        TuiSizeBand::Medium
    } else {
        TuiSizeBand::Small
    }
}

fn ownership_risk(row: &RiskRow) -> f64 {
    term_value(row, "ownership_risk").unwrap_or_else(|| {
        row.dominant_owner_share
            .map(|share| share.clamp(0.0, 1.0))
            .unwrap_or(0.0)
    })
}

fn ownership_risk_label(row: &RiskRow) -> &'static str {
    if ownership_risk(row) >= 0.70 {
        "CONCENTRATED"
    } else if ownership_risk(row) >= 0.40 {
        "SHARED"
    } else {
        "DISTRIBUTED"
    }
}

fn ownership_shape_label(row: &RiskRow) -> &'static str {
    let concentration = row
        .dominant_owner_share
        .unwrap_or_else(|| ownership_risk(row));
    if concentration > 0.90 {
        "single owner"
    } else if concentration >= 0.70 {
        "concentrated"
    } else if concentration >= 0.40 {
        "shared"
    } else {
        "distributed"
    }
}

fn coordination_pressure(row: &RiskRow) -> f64 {
    let source = term_value(row, "source_coupling_pressure").unwrap_or_else(|| {
        let incoming = row.source_coupling_pressure_in.unwrap_or(0) as f64 / 25.0;
        let outgoing = row.source_coupling_pressure_out.unwrap_or(0) as f64 / 15.0;
        incoming.max(outgoing).clamp(0.0, 1.0)
    });
    let cochange =
        term_value(row, "cochange_pressure").unwrap_or(row.co_changed_file_count as f64 / 25.0);
    source.max(cochange).clamp(0.0, 1.0)
}

fn coordination_severity_label(value: f64) -> &'static str {
    if value >= 0.85 {
        "CENTRAL"
    } else if value >= 0.60 {
        "HIGH"
    } else if value >= 0.35 {
        "MODERATE"
    } else {
        "LOW"
    }
}

fn complexity_pressure(row: &RiskRow) -> f64 {
    term_value(row, "complexity_pressure").unwrap_or_else(|| {
        let file = row.complexity_pressure.unwrap_or(0) as f64 / 150.0;
        let function = row.max_function_complexity_pressure.unwrap_or(0) as f64 / 30.0;
        file.max(function).clamp(0.0, 1.0)
    })
}

fn term_value(row: &RiskRow, name: &str) -> Option<f64> {
    row.terms
        .iter()
        .find(|term| term.name == name)
        .and_then(|term| term.normalized_value)
}

fn risk_driver_lines(row: &RiskRow) -> Vec<Line<'static>> {
    let mut messages = row
        .facts
        .iter()
        .map(|fact| fact.message.clone())
        .collect::<Vec<_>>();
    if ownership_risk(row) >= 0.60 {
        messages.push("Concentrated ownership / low maintainer redundancy".to_owned());
    }
    if coordination_pressure(row) >= 0.60 {
        messages.push("High source coupling pressure from resolved local imports".to_owned());
    }
    if complexity_pressure(row) >= 0.50 {
        messages.push("High approximate cognitive complexity pressure".to_owned());
    }
    if row
        .line_count
        .is_some_and(|line_count| line_size_band(line_count).bar_value() >= 0.60)
    {
        messages.push("Expensive to review or reason about due to scale".to_owned());
    }
    messages.sort();
    messages.dedup();

    if messages.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            "Why This File Matters",
            style(TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
    ];
    lines.extend(
        messages
            .into_iter()
            .take(4)
            .map(|message| Line::raw(format!("  - {message}"))),
    );

    lines
}

fn ownership_distribution_lines(row: &RiskRow) -> Vec<Line<'static>> {
    if row.owners.is_empty() {
        return vec![Line::styled("  unavailable", style(TuiSeverity::Muted))];
    }

    let mut visible = Vec::new();
    let mut others_share = 0.0;
    let mut others_touches = 0;
    for owner in &row.owners {
        if visible.len() < 3 && owner.author != "others" {
            visible.push(owner);
        } else {
            others_share += owner.ownership_share;
            others_touches += owner.touch_count;
        }
    }

    let mut lines = visible
        .into_iter()
        .map(|owner| owner_share_line(&display_author(&owner.author), owner.ownership_share))
        .collect::<Vec<_>>();
    if others_touches > 0 && rounded_percent(others_share) >= 1.0 {
        lines.push(owner_share_line("others", others_share));
    }

    lines
}

fn owner_share_line(author: &str, share: f64) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(
                "  {:<OWNER_NAME_WIDTH$}",
                truncate_end(author, OWNER_NAME_WIDTH)
            ),
            style(TuiSeverity::Muted),
        ),
        Span::raw(format!(" {:>3.0}%", share * 100.0)),
    ])
}

fn rounded_percent(share: f64) -> f64 {
    (share * 100.0).round()
}

fn format_compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn padded_rect(area: Rect, left: u16, right: u16, top: u16, bottom: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(left.min(area.width)),
        y: area.y.saturating_add(top.min(area.height)),
        width: area.width.saturating_sub(left.saturating_add(right)),
        height: area.height.saturating_sub(top.saturating_add(bottom)),
    }
}

fn truncate_middle(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let left = (width - 1) / 2;
    let right = width - 1 - left;
    let prefix = value.chars().take(left).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn truncate_end(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    format!("{}…", value.chars().take(width - 1).collect::<String>())
}

fn pad_truncated_end(value: &str, width: usize) -> String {
    format!("{:<width$}", truncate_end(value, width))
}

fn pad_truncated_path(value: &str, width: usize) -> String {
    format!("{:<width$}", truncate_middle(value, width))
}

fn display_author(author: &str) -> String {
    author
        .split_once(" <")
        .map(|(name, _email)| name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(author)
        .to_owned()
}

fn optional_i64_to_u64(value: Option<i64>) -> Option<u64> {
    value.map(i64_to_u64)
}

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

struct TerminalSession {
    terminal: TuiTerminal,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }

    fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ratatui::backend::TestBackend;
    use rusqlite::Connection;

    use super::*;

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path =
                env::temp_dir().join(format!("hotpath-tui-{name}-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join(".hotpath")).expect("fixture dir should exist");
            Self { path }
        }

        fn db_path(&self) -> PathBuf {
            self.path.join(INDEX_DB)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn missing_db_returns_empty_state() {
        let fixture = env::temp_dir().join("hotpath-tui-missing-db");
        let _ = fs::remove_dir_all(&fixture);
        fs::create_dir_all(&fixture).expect("fixture should exist");

        let snapshot = TuiDatabaseSnapshot::load_from_dir(&fixture);

        assert!(snapshot.index_root.is_none());
        assert!(snapshot
            .status
            .as_deref()
            .is_some_and(|status| status.contains("Run hotpath scan first")));
        let _ = fs::remove_dir_all(&fixture);
    }

    #[test]
    fn loads_project_summary_and_ranked_rows() {
        let fixture = Fixture::new("load");
        create_tui_db(&fixture.db_path());

        let snapshot = TuiDatabaseSnapshot::load_from_dir(&fixture.path);

        assert_eq!(
            snapshot
                .project
                .as_ref()
                .map(|project| project.risk_band.as_str()),
            Some("high")
        );
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[0].relative_path, "src/risky.go");
        assert!(snapshot.rows[0].is_generated);
        assert!(inspector_tags(&snapshot.rows[0])
            .iter()
            .any(|tag| tag == "GEN"));
        assert_eq!(snapshot.rows[0].terms.len(), 2);
        assert_eq!(snapshot.rows[0].facts.len(), 1);
        assert_eq!(snapshot.rows[0].limitations.len(), 1);
        assert_eq!(snapshot.rows[0].parser_diagnostics.len(), 1);
        assert_eq!(snapshot.rows[0].parser_diagnostics[0].code, "parse_error");
        assert_eq!(snapshot.rows[0].owners.len(), 1);
    }

    #[test]
    fn loads_risk_rows_by_score_descending_then_path_ascending() {
        let fixture = Fixture::new("risk-sort");
        create_tui_db(&fixture.db_path());
        let connection = Connection::open(fixture.db_path()).expect("db should open");
        connection
            .execute(
                "UPDATE file_risk_scores SET rank = 1, score = 0.8, risk_10 = 8.0 WHERE relative_path = 'src/safe.go'",
                [],
            )
            .expect("safe row should update");
        connection
            .execute(
                "UPDATE file_risk_scores SET rank = 2, score = 0.8, risk_10 = 8.0 WHERE relative_path = 'src/risky.go'",
                [],
            )
            .expect("risky row should update");

        let snapshot = TuiDatabaseSnapshot::load_from_dir(&fixture.path);

        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[0].relative_path, "src/risky.go");
        assert_eq!(snapshot.rows[1].relative_path, "src/safe.go");
    }

    #[test]
    fn empty_risk_tables_return_scored_go_empty_state() {
        let fixture = Fixture::new("empty-risk");
        create_empty_tui_db(&fixture.db_path());

        let snapshot = TuiDatabaseSnapshot::load_from_dir(&fixture.path);

        assert!(snapshot.rows.is_empty());
        assert!(snapshot
            .status
            .as_deref()
            .is_some_and(|status| status.contains("No scored Go files found")));
    }

    #[test]
    fn search_filters_rows_and_clamps_selection() {
        let snapshot = sample_snapshot();
        let mut state = TuiState::new();

        reduce_key(&mut state, &snapshot, KeyEvent::from(KeyCode::Char('/')));
        reduce_key(&mut state, &snapshot, KeyEvent::from(KeyCode::Char('s')));
        reduce_key(&mut state, &snapshot, KeyEvent::from(KeyCode::Char('a')));
        reduce_key(&mut state, &snapshot, KeyEvent::from(KeyCode::Char('f')));
        reduce_key(&mut state, &snapshot, KeyEvent::from(KeyCode::Enter));

        let filtered = state.filtered_indices(&snapshot);
        assert_eq!(filtered.len(), 1);
        assert_eq!(snapshot.rows[filtered[0]].relative_path, "src/safe.go");
    }

    #[test]
    fn renders_hotpath_view_with_inspector() {
        let snapshot = sample_snapshot();
        let state = TuiState::new();
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        terminal
            .draw(|frame| render(frame, &snapshot, &state))
            .expect("render should succeed");
        let output = buffer_text(&terminal);

        assert!(output.contains("Hotpath"));
        assert!(output.contains("Repo Risk"));
        assert!(output.contains("active_file_count 2"));
        assert!(output.contains("active_go_file_count 2"));
        assert!(output.contains("scored_file_count 2"));
        assert!(output.contains("coverage 100.0%"));
        assert!(output.contains("git confidence bounded"));
        assert!(output.contains("max_commits 50000"));
        assert!(output.contains("all_reachable_commits 7"));
        assert!(output.contains("undercount side-branch work"));
        assert!(output.contains("broad_commits_skipped 1"));
        assert!(output.contains("likely_automated_authors 1"));
        assert!(output.contains("author_identity exact_author_string_name_email"));
        assert!(output.contains("mailmap ignored"));
        assert!(output.contains("index_action fully_rebuilt"));
        assert!(output.contains("Go coverage: 100.0%"));
        assert!(output.contains("Top Factor"));
        assert!(output.contains("Inspector"));
        assert!(output.contains("src/risky.go"));
        assert!(output.contains("CHURN"));
        assert!(output.contains("Parser diagnostics"));
        assert!(output.contains("parse_error"));
    }

    #[test]
    fn renders_missing_index_empty_state() {
        let snapshot = TuiDatabaseSnapshot {
            index_root: None,
            status: Some("No Hotpath index found. Run hotpath scan first.".to_owned()),
            project: None,
            git: GitMetadata::default(),
            rows: Vec::new(),
        };
        let state = TuiState::new();
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        terminal
            .draw(|frame| render(frame, &snapshot, &state))
            .expect("render should succeed");
        let output = buffer_text(&terminal);

        assert!(output.contains("No Hotpath index found"));
    }

    fn create_empty_tui_db(path: &Path) {
        let connection = Connection::open(path).expect("db should open");
        connection
            .execute_batch(
                "
                CREATE TABLE project_risk_summary (
                    formula_id TEXT PRIMARY KEY NOT NULL,
                    active_scan_id INTEGER NOT NULL,
                    score REAL NOT NULL,
                    risk_10 REAL NOT NULL,
                    risk_band TEXT NOT NULL,
                    confidence TEXT NOT NULL,
                    active_file_count INTEGER NOT NULL,
                    active_go_file_count INTEGER NOT NULL,
                    scored_file_count INTEGER NOT NULL,
                    scoring_coverage REAL NOT NULL,
                    go_score_coverage REAL
                );
                CREATE TABLE file_risk_scores (
                    relative_path TEXT NOT NULL,
                    path TEXT NOT NULL,
                    active_scan_id INTEGER NOT NULL,
                    formula_id TEXT NOT NULL,
                    rank INTEGER NOT NULL,
                    score REAL NOT NULL,
                    risk_10 REAL NOT NULL,
                    risk_band TEXT NOT NULL,
                    is_generated INTEGER NOT NULL,
                    is_vendor INTEGER NOT NULL
                );
                CREATE TABLE file_facts (
                    relative_path TEXT,
                    line_count INTEGER,
                    byte_size INTEGER,
                    language_id TEXT,
                    complexity_pressure INTEGER,
                    max_function_complexity_pressure INTEGER,
                    source_coupling_pressure_in INTEGER,
                    source_coupling_pressure_out INTEGER,
                    co_changed_file_count INTEGER NOT NULL,
                    total_churn_lines INTEGER NOT NULL,
                    recent_churn_lines INTEGER NOT NULL,
                    commits_per_file INTEGER NOT NULL,
                    dominant_owner TEXT,
                    dominant_owner_share REAL,
                    owner_count INTEGER,
                    author_count INTEGER NOT NULL,
                    diagnostics TEXT NOT NULL
                );
                CREATE TABLE file_risk_terms (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    term_name TEXT NOT NULL,
                    raw_value REAL,
                    normalized_value REAL,
                    weight REAL NOT NULL,
                    contribution REAL NOT NULL
                );
                CREATE TABLE file_risk_facts (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    fact_index INTEGER NOT NULL,
                    fact_kind TEXT NOT NULL,
                    message TEXT NOT NULL
                );
                CREATE TABLE file_risk_limitations (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    limitation_index INTEGER NOT NULL,
                    code TEXT NOT NULL,
                    message TEXT NOT NULL
                );
                CREATE TABLE git_file_owners (
                    path TEXT NOT NULL,
                    owner_rank INTEGER NOT NULL,
                    author TEXT NOT NULL,
                    ownership_score REAL NOT NULL,
                    ownership_share REAL NOT NULL,
                    touch_count INTEGER NOT NULL
                );
                CREATE TABLE stage_metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                ",
            )
            .expect("fixture schema should be created");
    }

    #[test]
    fn narrow_layout_renders_without_overlap_sensitive_panic() {
        let snapshot = sample_snapshot();
        let state = TuiState::new();
        let backend = TestBackend::new(70, 28);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        terminal
            .draw(|frame| render(frame, &snapshot, &state))
            .expect("narrow render should succeed");
        let output = buffer_text(&terminal);
        assert!(output.contains("src/risky.go"));
        assert!(output.contains("File"));
    }

    fn create_tui_db(path: &Path) {
        let connection = Connection::open(path).expect("db should open");
        connection
            .execute_batch(
                "
                CREATE TABLE project_risk_summary (
                    formula_id TEXT PRIMARY KEY NOT NULL,
                    active_scan_id INTEGER NOT NULL,
                    score REAL NOT NULL,
                    risk_10 REAL NOT NULL,
                    risk_band TEXT NOT NULL,
                    confidence TEXT NOT NULL,
                    active_file_count INTEGER NOT NULL,
                    active_go_file_count INTEGER NOT NULL,
                    scored_file_count INTEGER NOT NULL,
                    scoring_coverage REAL NOT NULL,
                    go_score_coverage REAL,
                    max_file_score REAL NOT NULL,
                    top_10_mean_score REAL NOT NULL,
                    high_risk_file_count INTEGER NOT NULL,
                    medium_risk_file_count INTEGER NOT NULL,
                    dominant_dimension TEXT,
                    dominant_dimension_pressure REAL NOT NULL,
                    git_index_status TEXT NOT NULL
                );
                CREATE TABLE file_risk_scores (
                    relative_path TEXT NOT NULL,
                    path TEXT NOT NULL,
                    active_scan_id INTEGER NOT NULL,
                    formula_id TEXT NOT NULL,
                    rank INTEGER NOT NULL,
                    score REAL NOT NULL,
                    risk_10 REAL NOT NULL,
                    risk_band TEXT NOT NULL,
                    is_generated INTEGER NOT NULL,
                    is_vendor INTEGER NOT NULL
                );
                CREATE TABLE file_facts (
                    relative_path TEXT,
                    line_count INTEGER,
                    byte_size INTEGER,
                    language_id TEXT,
                    complexity_pressure INTEGER,
                    max_function_complexity_pressure INTEGER,
                    source_coupling_pressure_in INTEGER,
                    source_coupling_pressure_out INTEGER,
                    co_changed_file_count INTEGER NOT NULL,
                    total_churn_lines INTEGER NOT NULL,
                    recent_churn_lines INTEGER NOT NULL,
                    commits_per_file INTEGER NOT NULL,
                    dominant_owner TEXT,
                    dominant_owner_share REAL,
                    owner_count INTEGER,
                    author_count INTEGER NOT NULL,
                    diagnostics TEXT NOT NULL
                );
                CREATE TABLE file_risk_terms (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    term_name TEXT NOT NULL,
                    raw_value REAL,
                    normalized_value REAL,
                    weight REAL NOT NULL,
                    contribution REAL NOT NULL
                );
                CREATE TABLE file_risk_facts (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    fact_index INTEGER NOT NULL,
                    fact_kind TEXT NOT NULL,
                    message TEXT NOT NULL
                );
                CREATE TABLE file_risk_limitations (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    limitation_index INTEGER NOT NULL,
                    code TEXT NOT NULL,
                    message TEXT NOT NULL
                );
                CREATE TABLE git_file_owners (
                    path TEXT NOT NULL,
                    owner_rank INTEGER NOT NULL,
                    author TEXT NOT NULL,
                    ownership_score REAL NOT NULL,
                    ownership_share REAL NOT NULL,
                    touch_count INTEGER NOT NULL
                );

                INSERT INTO project_risk_summary VALUES
                    ('hotpath.project_risk.go.v1', 1, 0.72, 7.2, 'high', 'high', 2, 2, 2, 1.0, 1.0, 0.90, 0.55, 1, 1, 'churn', 0.8, 'available');
                INSERT INTO file_risk_scores VALUES
                    ('src/risky.go', 'C:/repo/src/risky.go', 1, 'hotpath.score.go.v1', 1, 0.9, 9.0, 'extreme', 1, 0),
                    ('src/safe.go', 'C:/repo/src/safe.go', 1, 'hotpath.score.go.v1', 2, 0.2, 2.0, 'low', 0, 0);
                INSERT INTO file_facts VALUES
                    ('src/risky.go', 1200, 48000, 'go', 220, 45, 12, 8, 20, 2300, 1000, 3, 'Alice <a@example.invalid>', 0.9, 1, 1, '[{\"code\":\"parse_error\",\"message\":\"Go source contains syntax errors\"}]'),
                    ('src/safe.go', 10, 400, 'go', 1, 1, 0, 0, 0, 1, 0, 1, NULL, NULL, NULL, 1, '[]');
                INSERT INTO file_risk_terms VALUES
                    ('src/risky.go', 'hotpath.score.go.v1', 'churn', 2300, 1.0, 0.18, 0.18),
                    ('src/risky.go', 'hotpath.score.go.v1', 'complexity_pressure', 220, 1.0, 0.16, 0.16),
                    ('src/safe.go', 'hotpath.score.go.v1', 'churn', 1, 0.0, 0.18, 0.0);
                INSERT INTO file_risk_facts VALUES
                    ('src/risky.go', 'hotpath.score.go.v1', 0, 'high_churn', 'High total churn');
                INSERT INTO file_risk_limitations VALUES
                    ('src/risky.go', 'hotpath.score.go.v1', 0, 'test_limit', 'fixture limitation');
                INSERT INTO git_file_owners VALUES
                    ('src/risky.go', 1, 'Alice <a@example.invalid>', 10.0, 0.9, 3);
                INSERT INTO stage_metadata VALUES
                    ('git_confidence', 'bounded'),
                    ('git_scan_mode', 'full'),
                    ('git_collection_mode', 'bounded_recent_stream'),
                    ('git_max_commits', '50000'),
                    ('git_max_age_days', '730'),
                    ('git_first_parent', 'true'),
                    ('git_renames', 'true'),
                    ('git_cochange_max_files_per_commit', '100'),
                    ('git_recent_churn_window_days', '90'),
                    ('git_head_timestamp', '1700000000'),
                    ('git_first_parent_commit_count', '4'),
                    ('git_all_reachable_commit_count', '7'),
                    ('git_merge_commit_count', '1'),
                    ('git_merge_heavy_warning', 'all reachable history is much larger than first-parent history; Git metrics may undercount side-branch work'),
                    ('git_broad_commits_skipped_for_cochange', '1'),
                    ('git_max_touched_files_in_commit', '101'),
                    ('git_broadest_commit', 'abc123'),
                    ('git_broad_commit_warning', 'commits over the co-change file limit skipped co-change pair generation but still counted churn and ownership'),
                    ('git_likely_automated_author_count', '1'),
                    ('git_top_author_touch_share_percent', '84'),
                    ('git_author_identity_rule', 'exact_author_string_name_email'),
                    ('git_mailmap', 'ignored'),
                    ('git_ownership_weighting', 'changed_lines_with_recency_half_life_bulk_change_dampening_sustained_activity_and_others_grouping'),
                    ('git_ownership_recency_half_life_days', '730'),
                    ('git_ownership_others_grouping', 'authors outside top retained contributors are grouped as others'),
                    ('git_index_action', 'fully_rebuilt'),
                    ('git_author_concentration_warning', 'one author accounts for at least 80 percent of Git file touches; ownership may be distorted by bulk or automated changes');
                ",
            )
            .expect("fixture schema should be created");
    }

    fn sample_snapshot() -> TuiDatabaseSnapshot {
        let fixture = Fixture::new("sample");
        create_tui_db(&fixture.db_path());
        TuiDatabaseSnapshot::load_from_dir(&fixture.path)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }
}
