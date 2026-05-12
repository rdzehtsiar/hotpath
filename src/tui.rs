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
    loop {
        terminal.draw(|frame| render(frame, snapshot))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if should_quit(key) => return Ok(()),
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame<'_>, snapshot: &TuiSnapshot) {
    let area = frame.area();
    let [_, content, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Fill(1),
        ])
        .areas(area);

    let title = Line::from(vec![
        Span::styled(
            "Hotpath",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" local codebase intelligence"),
    ]);
    let body = Paragraph::new(vec![
        title,
        Line::raw(""),
        Line::raw(format!(
            "Snapshot loaded: {} files, {} hotspots, {} symbols, {} dependencies.",
            snapshot.scan.summary.total_files,
            snapshot.report.summary.hotspot_count,
            snapshot.symbols.summary.symbol_count,
            snapshot.coupling.edges.len()
        )),
        Line::raw("Press q or Esc to exit."),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .title(" Hotpath TUI ")
            .borders(Borders::ALL),
    );

    frame.render_widget(body, content);
}

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
