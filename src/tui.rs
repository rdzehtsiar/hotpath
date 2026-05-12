// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::complexity::{self, ComplexityReport, ComplexitySummary, ComplexitySymbolRecord};
use crate::dependency::{self, FileDependencyFan, ResolvedDependencyEdge};
use crate::git;
use crate::report::{Report, ReportContext, ReportFinding, ReportGitSummary, ReportHotspot};
use crate::storage;
use crate::{
    estimate_context, parse, ranked_hotspot_scores_from_scan_and_git, ContextOptions, FileRecord,
    ParseImportRecord, ParseReport, ParseSummary, ParseSymbolRecord, ScanError, ScanReport,
    ScanSummary, REPORT_SCHEMA_VERSION,
};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub fn run_tui() -> io::Result<()> {
    let snapshot = TuiSnapshot::load_current_dir().map_err(io::Error::other)?;
    let mut terminal = TerminalSession::enter()?;
    run_app(terminal.terminal_mut(), &snapshot)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiSnapshot {
    pub report: Report,
    pub scan: TuiScanSnapshot,
    pub symbols: TuiSymbolSnapshot,
    pub complexity: TuiComplexitySnapshot,
    pub coupling: TuiCouplingSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiScanSnapshot {
    pub summary: ScanSummary,
    pub files: Vec<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiSymbolSnapshot {
    pub summary: ParseSummary,
    pub symbols: Vec<ParseSymbolRecord>,
    pub imports: Vec<ParseImportRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiComplexitySnapshot {
    pub summary: ComplexitySummary,
    pub symbols: Vec<ComplexitySymbolRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiCouplingSnapshot {
    pub edges: Vec<ResolvedDependencyEdge>,
    pub fan_by_file: Vec<TuiFileFan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiFileFan {
    pub path: String,
    pub fan_in: u64,
    pub fan_out: u64,
}

#[derive(Debug)]
pub enum TuiSnapshotError {
    CurrentDir(io::Error),
    Git(git::GitHistoryError),
    Scan(ScanError),
    PersistGitAnalysis(storage::index::IndexError),
    PersistHotspots(storage::index::IndexError),
    PersistScan(storage::index::IndexError),
    PersistSymbols(storage::index::IndexError),
}

impl TuiSnapshot {
    pub fn load_current_dir() -> Result<Self, TuiSnapshotError> {
        let current_dir = env::current_dir().map_err(TuiSnapshotError::CurrentDir)?;
        let analysis = git::analyze_from_head_at(&current_dir)?;
        let scan = crate::scan_repository(&analysis.worktree_root)?;
        let parse = parse::report_from_scan(&analysis.worktree_root, &scan);
        let complexity = complexity::report_from_parse(&parse);
        let ranked = ranked_hotspot_scores_from_scan_and_git(
            &scan.files,
            &analysis.file_metrics,
            &analysis.co_changes,
        );
        let context = estimate_context(&scan.files, ContextOptions::default());
        let context_estimated_tokens = context.summary.estimated_tokens;
        let hotspots = ranked.iter().map(ReportHotspot::from).collect::<Vec<_>>();
        let findings = hotspots.iter().map(ReportFinding::from).collect::<Vec<_>>();
        let mut index = storage::index::IndexStore::open(&analysis.worktree_root)
            .map_err(TuiSnapshotError::PersistScan)?;
        let scan_run = index
            .persist_scan(&scan)
            .map_err(TuiSnapshotError::PersistScan)?;
        index
            .persist_symbols(&parse)
            .map_err(TuiSnapshotError::PersistSymbols)?;
        index
            .persist_git_analysis(
                &analysis.worktree_root,
                &analysis.head_commit_id,
                analysis.head_commit_time,
                analysis.recent_window_days as u64,
                &analysis.file_metrics,
                &analysis.co_changes,
            )
            .map_err(TuiSnapshotError::PersistGitAnalysis)?;
        index
            .persist_hotspots(scan_run.id, &ranked)
            .map_err(TuiSnapshotError::PersistHotspots)?;
        let report = Report {
            schema_version: REPORT_SCHEMA_VERSION,
            summary: crate::ReportSummary {
                scan: scan.summary(),
                git: ReportGitSummary {
                    head_commit_id: analysis.head_commit_id,
                    recent_window_days: analysis.recent_window_days as u64,
                    file_metric_count: analysis.file_metrics.len() as u64,
                    co_change_count: analysis.co_changes.len() as u64,
                },
                hotspot_count: hotspots.len() as u64,
                context_estimated_tokens,
            },
            hotspots,
            context: ReportContext {
                options: context.options,
                summary: context.summary,
                groups: context.groups,
                skipped: context.skipped,
                budget: context.budget,
            },
            findings,
        };

        Ok(Self::from_parts(report, scan, parse, complexity))
    }

    pub fn from_parts(
        report: Report,
        scan: ScanReport,
        parse: ParseReport,
        complexity: ComplexityReport,
    ) -> Self {
        let edges = dependency::resolve_dependencies(&parse);
        let fan = dependency::fan_metrics(&parse.files, &edges);

        Self {
            report: sorted_report(report),
            scan: TuiScanSnapshot::from_scan(scan),
            symbols: TuiSymbolSnapshot::from_parse(parse),
            complexity: TuiComplexitySnapshot::from_complexity(complexity),
            coupling: TuiCouplingSnapshot::from_dependency_facts(edges, fan.by_path),
        }
    }
}

impl TuiScanSnapshot {
    fn from_scan(scan: ScanReport) -> Self {
        let mut files = scan.files;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let summary = ScanReport {
            status: scan.status,
            file_walking: scan.file_walking,
            classification: scan.classification,
            warnings: scan.warnings,
            files: files.clone(),
        }
        .summary();

        Self { summary, files }
    }
}

impl TuiSymbolSnapshot {
    fn from_parse(parse: ParseReport) -> Self {
        let mut symbols = parse.symbols;
        let mut imports = parse.imports;
        symbols.sort_by(|left, right| {
            (
                &left.path,
                left.start_line,
                left.end_line,
                &left.kind,
                &left.name,
            )
                .cmp(&(
                    &right.path,
                    right.start_line,
                    right.end_line,
                    &right.kind,
                    &right.name,
                ))
        });
        imports.sort_by(|left, right| {
            (
                &left.path,
                left.start_line,
                left.end_line,
                &left.kind,
                &left.target,
            )
                .cmp(&(
                    &right.path,
                    right.start_line,
                    right.end_line,
                    &right.kind,
                    &right.target,
                ))
        });
        let summary = ParseReport {
            warnings: parse.warnings,
            files: parse.files,
            symbols: symbols.clone(),
            imports: imports.clone(),
        }
        .summary();

        Self {
            summary,
            symbols,
            imports,
        }
    }
}

impl TuiComplexitySnapshot {
    fn from_complexity(complexity: ComplexityReport) -> Self {
        let mut symbols = complexity.symbols;
        symbols.sort_by(|left, right| {
            (
                &left.path,
                left.start_line,
                left.end_line,
                &left.kind,
                &left.name,
            )
                .cmp(&(
                    &right.path,
                    right.start_line,
                    right.end_line,
                    &right.kind,
                    &right.name,
                ))
        });

        Self {
            summary: complexity.summary,
            symbols,
        }
    }
}

impl TuiCouplingSnapshot {
    fn from_dependency_facts(
        mut edges: Vec<ResolvedDependencyEdge>,
        fan_by_path: BTreeMap<String, FileDependencyFan>,
    ) -> Self {
        edges.sort();
        let fan_by_file = fan_by_path
            .into_iter()
            .map(|(path, fan)| TuiFileFan {
                path,
                fan_in: fan.fan_in,
                fan_out: fan.fan_out,
            })
            .collect();

        Self { edges, fan_by_file }
    }
}

fn sorted_report(mut report: Report) -> Report {
    report.hotspots.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.path.cmp(&right.path))
    });
    report.context.groups.sort_by(|left, right| {
        right
            .estimated_tokens
            .cmp(&left.estimated_tokens)
            .then_with(|| left.path.cmp(&right.path))
    });
    report
        .context
        .skipped
        .sort_by(|left, right| left.path.cmp(&right.path));
    report.findings.sort_by(|left, right| {
        (&left.path, left.code, left.rank).cmp(&(&right.path, right.code, right.rank))
    });

    report
}

impl fmt::Display for TuiSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Git(source) => write!(f, "{source}"),
            Self::Scan(source) => write!(f, "{source}"),
            Self::PersistGitAnalysis(source) => {
                write!(f, "failed to persist TUI Git analysis: {source}")
            }
            Self::PersistHotspots(source) => {
                write!(f, "failed to persist TUI hotspot scores: {source}")
            }
            Self::PersistScan(source) => {
                write!(f, "failed to persist TUI scan results: {source}")
            }
            Self::PersistSymbols(source) => {
                write!(f, "failed to persist TUI parser symbols: {source}")
            }
        }
    }
}

impl StdError for TuiSnapshotError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::PersistGitAnalysis(source)
            | Self::PersistHotspots(source)
            | Self::PersistScan(source)
            | Self::PersistSymbols(source) => Some(source),
        }
    }
}

impl From<git::GitHistoryError> for TuiSnapshotError {
    fn from(source: git::GitHistoryError) -> Self {
        Self::Git(source)
    }
}

impl From<ScanError> for TuiSnapshotError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

fn run_app(terminal: &mut TuiTerminal, snapshot: &TuiSnapshot) -> io::Result<()> {
    let mut state = TuiAppState::default();

    loop {
        terminal.draw(|frame| render(frame, snapshot, &state))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                reduce_key_with_editor(&mut state, snapshot, key, |name| env::var_os(name));
                if state.should_exit {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TuiView {
    Overview,
    Detail,
}

impl TuiView {
    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Detail => "Detail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    DrillDown { from: TuiView, row_text: String },
    ExplainScore { view: TuiView, row_text: String },
    OpenEditor { command: String, row_text: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListSelection {
    selected: usize,
}

impl ListSelection {
    pub fn selected(&self) -> usize {
        self.selected
    }

    fn move_next(&mut self, row_count: usize) {
        if row_count == 0 {
            self.selected = 0;
            return;
        }

        self.selected = (self.selected + 1).min(row_count - 1);
    }

    fn move_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn clamp(&mut self, row_count: usize) {
        if row_count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(row_count - 1);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchState {
    query: String,
}

impl SearchState {
    pub fn query(&self) -> &str {
        &self.query
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiAppState {
    current_view: TuiView,
    back_stack: Vec<TuiView>,
    selections: BTreeMap<TuiView, ListSelection>,
    search: Option<SearchState>,
    status: Option<String>,
    last_action: Option<TuiAction>,
    should_exit: bool,
}

impl Default for TuiAppState {
    fn default() -> Self {
        let mut selections = BTreeMap::new();
        selections.insert(TuiView::Overview, ListSelection::default());
        selections.insert(TuiView::Detail, ListSelection::default());

        Self {
            current_view: TuiView::Overview,
            back_stack: Vec::new(),
            selections,
            search: None,
            status: None,
            last_action: None,
            should_exit: false,
        }
    }
}

impl TuiAppState {
    pub fn current_view(&self) -> TuiView {
        self.current_view
    }

    pub fn selected_index(&self) -> usize {
        self.selection_for_current_view().selected()
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(SearchState::query)
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn last_action(&self) -> Option<&TuiAction> {
        self.last_action.as_ref()
    }

    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    fn selection_for_current_view(&self) -> &ListSelection {
        self.selections
            .get(&self.current_view)
            .expect("all TUI views have selection state")
    }

    fn selection_for_current_view_mut(&mut self) -> &mut ListSelection {
        self.selections
            .get_mut(&self.current_view)
            .expect("all TUI views have selection state")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorResolution {
    Command(String),
    Missing,
}

pub fn resolve_editor_from_env<F>(mut get_env: F) -> EditorResolution
where
    F: FnMut(&str) -> Option<String>,
{
    get_env("VISUAL")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| get_env("EDITOR").filter(|value| !value.trim().is_empty()))
        .map(EditorResolution::Command)
        .unwrap_or(EditorResolution::Missing)
}

fn reduce_key_with_editor<F>(
    state: &mut TuiAppState,
    snapshot: &TuiSnapshot,
    key: KeyEvent,
    mut get_env: F,
) where
    F: FnMut(&str) -> Option<std::ffi::OsString>,
{
    if key.kind != KeyEventKind::Press {
        return;
    }

    let rows = filtered_visible_rows(snapshot, state);

    if state.search.is_some() {
        match key.code {
            KeyCode::Esc => reduce_escape(state),
            KeyCode::Backspace => {
                if let Some(search) = &mut state.search {
                    search.query.pop();
                }
                clamp_current_selection(state, snapshot);
            }
            KeyCode::Char(character) => {
                if let Some(search) = &mut state.search {
                    search.query.push(character);
                }
                clamp_current_selection(state, snapshot);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => state.should_exit = true,
        KeyCode::Char('/') => {
            state.search = Some(SearchState::default());
            state.status = Some("Search active".to_owned());
            clamp_current_selection(state, snapshot);
        }
        KeyCode::Esc => reduce_escape(state),
        KeyCode::Char('j') => {
            state.selection_for_current_view_mut().move_next(rows.len());
        }
        KeyCode::Char('k') => {
            state.selection_for_current_view_mut().move_previous();
        }
        KeyCode::Enter => drill_down(state, &rows),
        KeyCode::Char('x') => explain_score(state, &rows),
        KeyCode::Char('e') => {
            let resolution = resolve_editor_from_env(|name| {
                get_env(name).and_then(|value| value.into_string().ok())
            });
            resolve_editor_action(state, &rows, resolution);
        }
        _ => {}
    }
}

fn reduce_escape(state: &mut TuiAppState) {
    if state.search.take().is_some() {
        state.status = Some("Search cleared".to_owned());
        return;
    }

    if let Some(previous) = state.back_stack.pop() {
        state.current_view = previous;
        state.status = Some(format!("Back to {}", previous.title()));
    } else {
        state.should_exit = true;
    }
}

fn drill_down(state: &mut TuiAppState, rows: &[String]) {
    let Some(row_text) = selected_row_text(state, rows) else {
        state.status = Some("No row selected".to_owned());
        return;
    };

    let from = state.current_view;
    if from == TuiView::Detail {
        state.status = Some("Already in detail view".to_owned());
        return;
    }

    state.back_stack.push(from);
    state.current_view = TuiView::Detail;
    state.last_action = Some(TuiAction::DrillDown {
        from,
        row_text: row_text.clone(),
    });
    state.status = Some(format!("Opened {row_text}"));
}

fn explain_score(state: &mut TuiAppState, rows: &[String]) {
    let Some(row_text) = selected_row_text(state, rows) else {
        state.status = Some("No row selected".to_owned());
        return;
    };

    state.last_action = Some(TuiAction::ExplainScore {
        view: state.current_view,
        row_text: row_text.clone(),
    });
    state.status = Some(format!("Explain score: {row_text}"));
}

fn resolve_editor_action(state: &mut TuiAppState, rows: &[String], resolution: EditorResolution) {
    let Some(row_text) = selected_row_text(state, rows) else {
        state.status = Some("No row selected".to_owned());
        return;
    };

    match resolution {
        EditorResolution::Command(command) => {
            state.last_action = Some(TuiAction::OpenEditor {
                command: command.clone(),
                row_text: row_text.clone(),
            });
            state.status = Some(format!("Editor action: {command} {row_text}"));
        }
        EditorResolution::Missing => {
            state.status = Some("Set VISUAL or EDITOR to open a row in an editor".to_owned());
        }
    }
}

fn selected_row_text(state: &TuiAppState, rows: &[String]) -> Option<String> {
    rows.get(state.selected_index()).cloned()
}

fn clamp_current_selection(state: &mut TuiAppState, snapshot: &TuiSnapshot) {
    let row_count = filtered_visible_rows(snapshot, state).len();
    state.selection_for_current_view_mut().clamp(row_count);
}

fn filtered_visible_rows(snapshot: &TuiSnapshot, state: &TuiAppState) -> Vec<String> {
    let rows = visible_rows(snapshot, state.current_view);
    let Some(search) = &state.search else {
        return rows;
    };
    let query = search.query.trim().to_lowercase();
    if query.is_empty() {
        return rows;
    }

    rows.into_iter()
        .filter(|row| row.to_lowercase().contains(&query))
        .collect()
}

fn visible_rows(snapshot: &TuiSnapshot, view: TuiView) -> Vec<String> {
    match view {
        TuiView::Overview => vec![
            format!("Files: {}", snapshot.scan.summary.total_files),
            format!("Hotspots: {}", snapshot.report.summary.hotspot_count),
            format!("Symbols: {}", snapshot.symbols.summary.symbol_count),
            format!("Dependencies: {}", snapshot.coupling.edges.len()),
        ],
        TuiView::Detail => snapshot
            .report
            .hotspots
            .iter()
            .map(|hotspot| {
                format!(
                    "#{rank} {path} score {score:.2}",
                    rank = hotspot.rank,
                    path = hotspot.path,
                    score = hotspot.score
                )
            })
            .chain(snapshot.scan.files.iter().map(|file| file.path.clone()))
            .collect(),
    }
}

fn visible_row_window(
    rows: &[String],
    selected: usize,
    limit: usize,
) -> impl Iterator<Item = (usize, &String)> {
    let start = selected
        .saturating_add(1)
        .saturating_sub(limit)
        .min(rows.len());

    rows.iter().enumerate().skip(start).take(limit)
}

fn render(frame: &mut Frame<'_>, snapshot: &TuiSnapshot, state: &TuiAppState) {
    let area = frame.area();
    let [_, content, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Fill(1),
        ])
        .areas(area);
    let rows = filtered_visible_rows(snapshot, state);
    let selected = state.selected_index();
    let mut body_lines = vec![
        Line::from(vec![
            Span::styled(
                "Hotpath",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" local codebase intelligence"),
        ]),
        Line::raw(""),
        Line::raw(format!("View: {}", state.current_view.title())),
    ];

    if rows.is_empty() {
        body_lines.push(Line::raw("No rows."));
    } else {
        body_lines.extend(visible_row_window(&rows, selected, 4).map(|(index, row)| {
            if index == selected {
                Line::raw(format!("> {row}"))
            } else {
                Line::raw(format!("  {row}"))
            }
        }));
    }

    let footer = match state.search_query() {
        Some(query) => format!("/{query}"),
        None => state.status().map(str::to_owned).unwrap_or_else(|| {
            "j/k move, Enter drill down, / search, x explain, e editor, q quit".to_owned()
        }),
    };
    body_lines.push(Line::raw(""));
    body_lines.push(Line::raw(footer));

    let body = Paragraph::new(body_lines)
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .title(" Hotpath TUI ")
                .borders(Borders::ALL),
        );

    frame.render_widget(body, content);
}

#[cfg(test)]
fn should_quit(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }

    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
}

struct TerminalSession {
    terminal: TuiTerminal,
    raw_mode_enabled: bool,
    alternate_screen_enabled: bool,
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
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            raw_mode_enabled: true,
            alternate_screen_enabled: true,
        })
    }

    fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.alternate_screen_enabled {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
            self.alternate_screen_enabled = false;
        }

        if self.raw_mode_enabled {
            let _ = disable_raw_mode();
            self.raw_mode_enabled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextSkippedReason, ContextSkippedRow, ContextSummary};
    use crate::parse::{ParseFileReason, ParseFileStatus};
    use crate::report::{ReportFindingLevel, ReportSummary};
    use crate::{
        ContentKind, FileRecord, ParseFileRecord, ParseImportRecord, ParseSymbolRecord, ScanReport,
    };

    #[test]
    fn quit_keys_are_q_and_escape_key_presses() {
        assert!(should_quit(KeyEvent::from(KeyCode::Char('q'))));
        assert!(should_quit(KeyEvent::from(KeyCode::Esc)));
        assert!(!should_quit(KeyEvent::from(KeyCode::Char('Q'))));
        assert!(!should_quit(KeyEvent::from(KeyCode::Enter)));
    }

    #[test]
    fn reducer_moves_selection_with_j_and_k() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('j'), None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('j'), None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('k'), None);

        assert_eq!(state.selected_index(), 1);
    }

    #[test]
    fn reducer_drills_down_and_escape_navigates_back() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::Detail);
        assert_eq!(
            state.last_action(),
            Some(&TuiAction::DrillDown {
                from: TuiView::Overview,
                row_text: "Files: 2".to_owned(),
            })
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert_eq!(state.current_view(), TuiView::Overview);
        assert!(!state.should_exit());
    }

    #[test]
    fn reducer_search_filters_current_view_and_escape_clears_search_first() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('/'), None);
        for character in "hot".chars() {
            reduce_test_key(&mut state, &snapshot, KeyCode::Char(character), None);
        }

        assert_eq!(state.search_query(), Some("hot"));
        assert_eq!(
            filtered_visible_rows(&snapshot, &state),
            vec!["Hotspots: 0"]
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert_eq!(state.search_query(), None);
        assert_eq!(state.current_view(), TuiView::Overview);
        assert!(!state.should_exit());
    }

    #[test]
    fn reducer_search_accepts_q_as_query_text() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('/'), None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('q'), None);

        assert_eq!(state.search_query(), Some("q"));
        assert!(!state.should_exit());
    }

    #[test]
    fn reducer_does_not_create_self_history_from_detail_view() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert_eq!(state.current_view(), TuiView::Overview);
        assert!(!state.should_exit());
    }

    #[test]
    fn visible_row_window_keeps_selection_rendered() {
        let rows = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let window = visible_row_window(&rows, 5, 4)
            .map(|(index, row)| (index, row.as_str()))
            .collect::<Vec<_>>();

        assert_eq!(window, vec![(2, "c"), (3, "d"), (4, "e"), (5, "f")]);
    }

    #[test]
    fn reducer_q_exits_and_escape_at_root_preserves_existing_exit_behavior() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert!(state.should_exit());

        let mut state = TuiAppState::default();
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('q'), None);

        assert!(state.should_exit());
    }

    #[test]
    fn editor_resolution_prefers_visual_then_editor() {
        let resolved = resolve_editor_from_env(|name| match name {
            "VISUAL" => Some("code".to_owned()),
            "EDITOR" => Some("vim".to_owned()),
            _ => None,
        });

        assert_eq!(resolved, EditorResolution::Command("code".to_owned()));

        let resolved = resolve_editor_from_env(|name| match name {
            "EDITOR" => Some("vim".to_owned()),
            _ => None,
        });

        assert_eq!(resolved, EditorResolution::Command("vim".to_owned()));
    }

    #[test]
    fn reducer_editor_action_sets_status_when_editor_is_missing() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('e'), None);

        assert_eq!(
            state.status(),
            Some("Set VISUAL or EDITOR to open a row in an editor")
        );
        assert_eq!(state.last_action(), None);
    }

    #[test]
    fn reducer_editor_action_records_resolved_command_without_spawning() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('e'), Some("vim"));

        assert_eq!(
            state.last_action(),
            Some(&TuiAction::OpenEditor {
                command: "vim".to_owned(),
                row_text: "Files: 2".to_owned(),
            })
        );
    }

    #[test]
    fn reducer_explain_score_records_selected_row() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('j'), None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('x'), None);

        assert_eq!(
            state.last_action(),
            Some(&TuiAction::ExplainScore {
                view: TuiView::Overview,
                row_text: "Hotspots: 0".to_owned(),
            })
        );
    }

    #[test]
    fn snapshot_constructor_sorts_repository_relative_data() {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("src/z.rs"), file_record("src/a.rs")],
        );
        let parse = ParseReport {
            warnings: Vec::new(),
            files: vec![parse_file("src/z.rs"), parse_file("src/a.rs")],
            symbols: vec![symbol("src/z.rs", "zed", 5), symbol("src/a.rs", "alpha", 1)],
            imports: vec![
                import("src/z.rs", "crate::a::alpha", "use"),
                import("src/a.rs", "z", "mod"),
            ],
        };
        let complexity = complexity::report_from_parse(&parse);

        let snapshot = TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity);

        assert_eq!(
            snapshot
                .scan
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/z.rs"]
        );
        assert_eq!(
            snapshot
                .symbols
                .symbols
                .iter()
                .map(|symbol| symbol.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/z.rs"]
        );
        assert_eq!(
            snapshot
                .symbols
                .imports
                .iter()
                .map(|import| import.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/z.rs"]
        );
        assert_eq!(
            snapshot
                .coupling
                .fan_by_file
                .iter()
                .map(|fan| fan.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/z.rs"]
        );
    }

    #[test]
    fn snapshot_constructor_preserves_basic_shape() {
        let scan = ScanReport::from_parts(Vec::new(), vec![file_record("src/lib.rs")]);
        let parse = ParseReport {
            warnings: Vec::new(),
            files: vec![parse_file("src/lib.rs")],
            symbols: vec![symbol("src/lib.rs", "run", 1)],
            imports: Vec::new(),
        };
        let complexity = complexity::report_from_parse(&parse);

        let snapshot = TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity);

        assert_eq!(snapshot.scan.summary.total_files, 1);
        assert_eq!(snapshot.symbols.summary.symbol_count, 1);
        assert_eq!(snapshot.complexity.summary.total_files, 1);
        assert!(snapshot.coupling.edges.is_empty());
        assert_eq!(snapshot.report.summary.scan.total_files, 1);
    }

    fn reduce_test_key(
        state: &mut TuiAppState,
        snapshot: &TuiSnapshot,
        code: KeyCode,
        editor: Option<&str>,
    ) {
        reduce_key_with_editor(state, snapshot, KeyEvent::from(code), |name| match name {
            "VISUAL" => editor.map(Into::into),
            _ => None,
        });
    }

    fn test_snapshot() -> TuiSnapshot {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("src/lib.rs"), file_record("src/main.rs")],
        );
        let parse = ParseReport {
            warnings: Vec::new(),
            files: vec![parse_file("src/lib.rs"), parse_file("src/main.rs")],
            symbols: vec![symbol("src/lib.rs", "run", 1)],
            imports: Vec::new(),
        };
        let complexity = complexity::report_from_parse(&parse);

        TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity)
    }

    fn empty_report(scan: &ScanReport) -> Report {
        Report {
            schema_version: REPORT_SCHEMA_VERSION,
            summary: ReportSummary {
                scan: scan.summary(),
                git: ReportGitSummary {
                    head_commit_id: "HEAD".to_owned(),
                    recent_window_days: 90,
                    file_metric_count: 0,
                    co_change_count: 0,
                },
                hotspot_count: 0,
                context_estimated_tokens: 0,
            },
            hotspots: Vec::new(),
            context: ReportContext {
                options: ContextOptions::default(),
                summary: ContextSummary {
                    total_files: scan.files.len() as u64,
                    included_files: 0,
                    skipped_files: 0,
                    estimated_tokens: 0,
                    included_bytes: 0,
                    filtered_generated_files: 0,
                    filtered_vendor_files: 0,
                },
                groups: Vec::new(),
                skipped: vec![ContextSkippedRow {
                    path: "src/z.rs".to_owned(),
                    reason: ContextSkippedReason::Unreadable,
                }],
                budget: None,
            },
            findings: vec![ReportFinding {
                code: "hotpath.test",
                level: ReportFindingLevel::Info,
                path: Some("src/z.rs".to_owned()),
                message: "test".to_owned(),
                rank: None,
                score: None,
            }],
        }
    }

    fn file_record(path: &str) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size: Some(10),
            extension: Some("rs".to_owned()),
            language: Some("Rust"),
            line_count: Some(1),
            is_vendor: false,
            is_generated: false,
            content: ContentKind::Text,
            is_symlink: false,
            classification: "source",
            warnings: Vec::new(),
        }
    }

    fn parse_file(path: &str) -> ParseFileRecord {
        ParseFileRecord {
            path: path.to_owned(),
            language: Some("Rust"),
            content: ContentKind::Text,
            status: ParseFileStatus::Parsed,
            reason: Some(ParseFileReason::ParserExtractionPending),
            symbol_count: 1,
            import_count: 0,
        }
    }

    fn symbol(path: &str, name: &str, start_line: u64) -> ParseSymbolRecord {
        ParseSymbolRecord {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: "function".to_owned(),
            start_line,
            end_line: start_line,
            signature: None,
            nesting_depth: 0,
            parent: None,
            cyclomatic_complexity: Some(1),
            max_control_flow_nesting: Some(0),
        }
    }

    fn import(path: &str, target: &str, kind: &str) -> ParseImportRecord {
        ParseImportRecord {
            path: path.to_owned(),
            target: target.to_owned(),
            kind: kind.to_owned(),
            start_line: 1,
            end_line: 1,
        }
    }
}
