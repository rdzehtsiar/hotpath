// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};

use crate::complexity::{self, ComplexityReport, ComplexitySummary, ComplexitySymbolRecord};
use crate::dependency::{self, FileDependencyFan, ResolvedDependencyEdge};
use crate::git;
use crate::ownership::OperationalOwnershipSnapshot;
use crate::report::{self, Report, ReportFinding, ReportHotspot};
use crate::scoring::{NormalizedMetric, WeightedTerm};
use crate::storage;
use crate::{
    estimate_context, parse, ranked_hotspot_scores_from_scan_and_git, ContextBudgetStatus,
    ContextOptions, ContextSkippedReason, FileRecord, ParseImportRecord, ParseReport, ParseSummary,
    ParseSymbolRecord, ScanError, ScanReport, ScanSummary,
};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;
const TUI_PROGRESS_THROTTLE: Duration = Duration::from_millis(750);
const TUI_PROGRESS_PERCENT_STEP: u64 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiOptions {
    pub context: ContextOptions,
    pub include_generated_hotspots: bool,
    pub ascii: bool,
    pub no_color: bool,
}

pub fn run_tui() -> io::Result<()> {
    run_tui_with_options(TuiOptions::default())
}

pub fn run_tui_with_options(options: TuiOptions) -> io::Result<()> {
    let snapshot = TuiSnapshot::loading_with_options(options);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut progress = TuiProgressEmitter::new(sender.clone());
        let result = TuiSnapshot::load_current_dir_with_progress(options, |update| {
            progress.emit(update);
        })
        .map_err(|error| error.to_string());
        let _ = match result {
            Ok(snapshot) => sender.send(TuiWorkerMessage::Completed(Box::new(snapshot))),
            Err(error) => sender.send(TuiWorkerMessage::Failed(error)),
        };
    });
    let mut terminal = TerminalSession::enter()?;
    run_app(terminal.terminal_mut(), snapshot, Some(receiver), options)
}

#[derive(Debug)]
enum TuiWorkerMessage {
    Progress(TuiProgressUpdate),
    Completed(Box<TuiSnapshot>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiProgressUpdate {
    phase: &'static str,
    detail: String,
    completed: Option<u64>,
    total: Option<u64>,
    unit: &'static str,
    rate: Option<TuiProgressRate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TuiProgressRate {
    completed_at_start: u64,
    started_at: Instant,
}

impl TuiProgressUpdate {
    fn indeterminate(phase: &'static str, detail: impl Into<String>) -> Self {
        Self {
            phase,
            detail: detail.into(),
            completed: None,
            total: None,
            unit: "",
            rate: None,
        }
    }

    fn measured(
        phase: &'static str,
        detail: impl Into<String>,
        completed: u64,
        total: u64,
        unit: &'static str,
    ) -> Self {
        Self {
            phase,
            detail: detail.into(),
            completed: Some(completed),
            total: Some(total),
            unit,
            rate: None,
        }
    }
}

struct TuiProgressEmitter {
    sender: mpsc::Sender<TuiWorkerMessage>,
    last_emit: Option<Instant>,
    last_phase: Option<&'static str>,
    last_percent: Option<u64>,
    phase_started_at: Option<Instant>,
    phase_started_completed: Option<u64>,
}

impl TuiProgressEmitter {
    fn new(sender: mpsc::Sender<TuiWorkerMessage>) -> Self {
        Self {
            sender,
            last_emit: None,
            last_phase: None,
            last_percent: None,
            phase_started_at: None,
            phase_started_completed: None,
        }
    }

    fn emit(&mut self, mut update: TuiProgressUpdate) {
        if !self.should_emit(&update) {
            return;
        }

        let now = Instant::now();
        if self.last_phase != Some(update.phase) {
            self.phase_started_at = None;
            self.phase_started_completed = None;
        }
        if let Some(completed) = update.completed {
            if self.phase_started_at.is_none() || self.phase_started_completed.is_none() {
                self.phase_started_at = Some(now);
                self.phase_started_completed = Some(0);
            }
            if let (Some(started_at), Some(started_completed)) =
                (self.phase_started_at, self.phase_started_completed)
            {
                update.rate = Some(TuiProgressRate {
                    completed_at_start: started_completed.min(completed),
                    started_at,
                });
            }
        }

        self.last_emit = Some(now);
        self.last_phase = Some(update.phase);
        self.last_percent = progress_percent(&update);
        let _ = self.sender.send(TuiWorkerMessage::Progress(update));
    }

    fn should_emit(&self, update: &TuiProgressUpdate) -> bool {
        if self.last_phase != Some(update.phase) {
            return true;
        }

        if progress_percent(update) == Some(100) {
            return true;
        }

        if let (Some(current), Some(previous)) = (progress_percent(update), self.last_percent) {
            if current.saturating_sub(previous) >= TUI_PROGRESS_PERCENT_STEP {
                return true;
            }
        }

        self.last_emit
            .is_none_or(|last| last.elapsed() >= TUI_PROGRESS_THROTTLE)
    }
}

fn progress_percent(update: &TuiProgressUpdate) -> Option<u64> {
    let completed = update.completed?;
    let total = update.total?;
    completed
        .saturating_mul(100)
        .checked_div(total)
        .map(|percent| percent.min(100))
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiSnapshot {
    pub report: Report,
    pub scan: TuiScanSnapshot,
    pub symbols: TuiSymbolSnapshot,
    pub complexity: TuiComplexitySnapshot,
    pub coupling: TuiCouplingSnapshot,
    pub ownership: TuiOwnershipSnapshot,
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TuiOwnershipSnapshot {
    pub by_file: Vec<TuiFileOwnership>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiFileOwnership {
    pub path: String,
    pub owners: Vec<TuiOwnerShare>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiOwnerShare {
    pub author: String,
    pub touch_count: u64,
    pub share: f64,
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
    pub fn loading_with_options(options: TuiOptions) -> Self {
        let scan = ScanReport {
            status: "loading",
            file_walking: "pending",
            classification: "pending",
            warnings: Vec::new(),
            files: Vec::new(),
        };
        let parse = parse::scaffold_report_from_scan(&scan);
        let complexity = complexity::report_from_parse(&parse);
        let context = estimate_context(&scan.files, options.context);
        let report = report::report_from_scan_analysis(
            &scan,
            &git::GitAnalysis {
                worktree_root: env::current_dir().unwrap_or_else(|_| ".".into()),
                head_commit_id: "loading".to_owned(),
                head_commit_time: 0,
                recent_window_days: git::RECENT_CHURN_WINDOW_DAYS,
                changes: Vec::new(),
                file_metrics: Vec::new(),
                co_changes: Vec::new(),
                ownership: OperationalOwnershipSnapshot::default(),
            },
            context,
            Vec::new(),
            vec![ReportFinding {
                code: "hotpath.tui.loading",
                level: report::ReportFindingLevel::Info,
                path: None,
                message: "Repository analysis is running in the background".to_owned(),
                rank: None,
                score: None,
            }],
        );

        Self::from_parts(report, scan, parse, complexity)
    }

    pub fn load_current_dir() -> Result<Self, TuiSnapshotError> {
        Self::load_current_dir_with_options(TuiOptions::default())
    }

    pub fn load_current_dir_with_options(options: TuiOptions) -> Result<Self, TuiSnapshotError> {
        Self::load_current_dir_with_progress(options, |_| {})
    }

    fn load_current_dir_with_progress<F>(
        options: TuiOptions,
        mut progress: F,
    ) -> Result<Self, TuiSnapshotError>
    where
        F: FnMut(TuiProgressUpdate),
    {
        let current_dir = env::current_dir().map_err(TuiSnapshotError::CurrentDir)?;
        progress(TuiProgressUpdate::indeterminate(
            "Opening repository",
            "discovering Git worktree",
        ));
        let worktree_root = git::worktree_root_at(&current_dir)?;
        let read_git_cache = RefCell::new(
            storage::index::IndexStore::open(&worktree_root)
                .map_err(TuiSnapshotError::PersistScan)?,
        );
        let (cache_write_sender, cache_write_receiver) =
            crossbeam_channel::bounded::<Vec<(String, Vec<git::GitFileChange>)>>(8);
        let cache_write_root = worktree_root.clone();
        let cache_writer = thread::spawn(move || {
            let Ok(mut write_cache) = storage::index::IndexStore::open(&cache_write_root) else {
                return;
            };
            for commits in cache_write_receiver {
                let _ = write_cache
                    .persist_git_commit_changes_batch(env!("CARGO_PKG_VERSION"), &commits);
            }
        });
        progress(TuiProgressUpdate::indeterminate(
            "Git history",
            "counting reachable commits",
        ));
        let analyzer_version = env!("CARGO_PKG_VERSION");
        let analysis = git::analyze_from_head_at_with_progress_and_cache_batches(
            &worktree_root,
            |git_progress| {
                progress(TuiProgressUpdate::measured(
                    "Git history",
                    "diffing reachable commits",
                    git_progress.completed_commits as u64,
                    git_progress.total_commits as u64,
                    "commits",
                ));
            },
            |commit_ids| {
                read_git_cache
                    .borrow()
                    .cached_git_commit_changes_batch(commit_ids, analyzer_version)
                    .ok()
                    .unwrap_or_default()
            },
            |commits| {
                if !commits.is_empty() {
                    let _ = cache_write_sender.send(commits.to_vec());
                }
            },
        );
        drop(cache_write_sender);
        let _ = cache_writer.join();
        let analysis = analysis?;
        progress(TuiProgressUpdate::measured(
            "Git history",
            "diffing reachable commits",
            analysis.changes.len() as u64,
            analysis.changes.len() as u64,
            "changes",
        ));
        progress(TuiProgressUpdate::indeterminate(
            "Scanning repository",
            "walking files and classifying content",
        ));
        let scan = crate::scan_repository(&analysis.worktree_root)?;
        progress(TuiProgressUpdate::measured(
            "Scanning repository",
            "walking files and classifying content",
            scan.files.len() as u64,
            scan.files.len() as u64,
            "files",
        ));
        progress(TuiProgressUpdate::indeterminate(
            "Parsing symbols",
            "preparing parser candidates",
        ));
        let parse = parse::report_from_scan_with_progress(
            &analysis.worktree_root,
            &scan,
            |parse_progress| {
                progress(TuiProgressUpdate::measured(
                    "Parsing symbols",
                    parse_progress.path,
                    parse_progress.completed_files as u64,
                    parse_progress.total_files as u64,
                    "files",
                ));
            },
        );
        progress(TuiProgressUpdate::indeterminate(
            "Complexity",
            "building symbol, complexity, and coupling facts",
        ));
        let complexity = complexity::report_from_parse(&parse);
        progress(TuiProgressUpdate::indeterminate(
            "Scoring hotspots",
            "ranking files by advisory risk",
        ));
        let ranked = ranked_hotspot_scores_from_scan_and_git(&scan.files, &analysis);
        let context = estimate_context(&scan.files, options.context);
        let hotspots = ranked.iter().map(ReportHotspot::from).collect::<Vec<_>>();
        let findings = hotspots.iter().map(ReportFinding::from).collect::<Vec<_>>();
        progress(TuiProgressUpdate::indeterminate(
            "Writing index",
            "persisting scan, parser, Git, and hotspot facts",
        ));
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
        let mut report =
            report::report_from_scan_analysis(&scan, &analysis, context, hotspots, findings);
        if !options.include_generated_hotspots {
            suppress_generated_hotspots(&scan.files, &mut report);
        }

        let mut snapshot = Self::from_parts(report, scan, parse, complexity);
        snapshot.ownership = TuiOwnershipSnapshot::from_operational_ownership(&analysis.ownership);
        progress(TuiProgressUpdate::indeterminate(
            "Ready",
            "repository analysis complete",
        ));

        Ok(snapshot)
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
            ownership: TuiOwnershipSnapshot::default(),
        }
    }
}

impl TuiOwnershipSnapshot {
    fn from_operational_ownership(ownership: &OperationalOwnershipSnapshot) -> Self {
        let by_file = ownership
            .by_file
            .iter()
            .map(|file| {
                let mut owners = file
                    .owners
                    .iter()
                    .map(|owner| TuiOwnerShare {
                        author: owner.author.clone(),
                        touch_count: owner.meaningful_commits,
                        share: owner.share,
                    })
                    .collect::<Vec<_>>();
                if file.others_share > 0.0 {
                    owners.push(TuiOwnerShare {
                        author: "others".to_owned(),
                        touch_count: 1,
                        share: file.others_share,
                    });
                }

                TuiFileOwnership {
                    path: file.path.clone(),
                    owners,
                }
            })
            .collect();

        Self { by_file }
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
        parse::sort_symbol_records(&mut symbols);
        parse::sort_import_records(&mut imports);
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
        complexity::sort_symbol_records(&mut symbols);

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

fn suppress_generated_hotspots(files: &[FileRecord], report: &mut Report) {
    report.hotspots.retain(|hotspot| {
        files
            .iter()
            .find(|file| file.path == hotspot.path)
            .is_none_or(|file| !is_suppressed_hotspot_file(file))
    });
    for (index, hotspot) in report.hotspots.iter_mut().enumerate() {
        hotspot.rank = index as u64 + 1;
    }
    report.summary.hotspot_count = report.hotspots.len() as u64;
    report.findings = report.hotspots.iter().map(ReportFinding::from).collect();
}

fn is_suppressed_hotspot_file(file: &FileRecord) -> bool {
    file.is_generated
        || file.is_vendor
        || is_lockfile_path(&file.path)
        || is_minified_path(&file.path)
}

fn is_lockfile_path(path: &str) -> bool {
    matches!(
        path.rsplit('/').next().unwrap_or(path),
        "Cargo.lock"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lockb"
            | "Gemfile.lock"
            | "poetry.lock"
            | "Pipfile.lock"
            | "composer.lock"
            | "go.sum"
            | "flake.lock"
    )
}

fn is_minified_path(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).contains(".min.")
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

fn run_app(
    terminal: &mut TuiTerminal,
    mut snapshot: TuiSnapshot,
    receiver: Option<Receiver<TuiWorkerMessage>>,
    options: TuiOptions,
) -> io::Result<()> {
    let mut state = TuiAppState {
        status: Some("Analyzing repository in background".to_owned()),
        analysis_running: receiver.is_some(),
        ..TuiAppState::default()
    };

    loop {
        if let Some(receiver) = &receiver {
            loop {
                match receiver.try_recv() {
                    Ok(TuiWorkerMessage::Progress(update)) => {
                        state.background_status = Some(update);
                    }
                    Ok(TuiWorkerMessage::Completed(loaded)) => {
                        snapshot = *loaded;
                        state.status = Some("Repository analysis ready".to_owned());
                        state.analysis_running = false;
                        state.background_status = Some(TuiProgressUpdate::indeterminate(
                            "Ready",
                            "repository analysis complete",
                        ));
                        clamp_current_selection(&mut state, &snapshot);
                    }
                    Ok(TuiWorkerMessage::Failed(error)) => {
                        state.status = Some(format!("Analysis failed: {error}"));
                        state.analysis_running = false;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if state.analysis_running {
                            state.status = Some("Analysis stopped before completing".to_owned());
                            state.analysis_running = false;
                            state.background_status = None;
                        }
                        break;
                    }
                }
            }
        }

        terminal.draw(|frame| render_with_options(frame, &snapshot, &state, options))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                reduce_key_with_editor(&mut state, &snapshot, key, |name| env::var_os(name));
                if state.should_exit {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TuiView {
    Hotspots,
    RepoTree,
    FileDetail,
    SymbolDetail,
    GitDetail,
    CouplingGraph,
    ContextBudgeting,
    ExplainScore,
}

const PRIMARY_VIEWS: [TuiView; 4] = [
    TuiView::Hotspots,
    TuiView::RepoTree,
    TuiView::CouplingGraph,
    TuiView::ContextBudgeting,
];
const METRIC_BAR_WIDTH: usize = 14;
const METRIC_LABEL_WIDTH: usize = 12;
const HOTSPOT_SELECTOR_WIDTH: usize = 2;
const HOTSPOT_SCORE_WIDTH: usize = 4;
const HOTSPOT_DEFAULT_TAG_WIDTH: usize = 22;
const HOTSPOT_NARROW_TAG_WIDTH: usize = 12;
const HOTSPOT_MIN_PATH_WIDTH: usize = 8;
const HOTSPOT_TAG_SEPARATOR: &str = " \u{00B7} ";
const OWNER_NAME_WIDTH: usize = 24;

impl TuiView {
    fn title(self) -> &'static str {
        match self {
            Self::Hotspots => "Hotspots",
            Self::RepoTree => "Repo Tree",
            Self::FileDetail => "File Detail",
            Self::SymbolDetail => "Symbol Detail",
            Self::GitDetail => "Git Detail",
            Self::CouplingGraph => "Coupling Graph",
            Self::ContextBudgeting => "Context Budgeting",
            Self::ExplainScore => "Explain Score",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TuiPaneFocus {
    Nav,
    #[default]
    Main,
    Inspector,
}

impl TuiPaneFocus {
    fn next(self) -> Self {
        match self {
            Self::Nav => Self::Main,
            Self::Main => Self::Inspector,
            Self::Inspector => Self::Nav,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Nav => Self::Inspector,
            Self::Main => Self::Nav,
            Self::Inspector => Self::Main,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Nav => "Nav",
            Self::Main => "Main",
            Self::Inspector => "Inspector",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolKey {
    path: String,
    start_line: u64,
    end_line: u64,
    kind: String,
    name: String,
}

impl SymbolKey {
    fn from_parse(symbol: &ParseSymbolRecord) -> Self {
        Self {
            path: symbol.path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            kind: symbol.kind.clone(),
            name: symbol.name.clone(),
        }
    }

    fn from_complexity(symbol: &ComplexitySymbolRecord) -> Self {
        Self {
            path: symbol.path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            kind: symbol.kind.clone(),
            name: symbol.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    DrillDown {
        from: TuiView,
        to: TuiView,
        path: String,
    },
    ExplainScore {
        view: TuiView,
        path: String,
    },
    OpenEditor {
        command: String,
        row_text: String,
    },
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
    current_path: Option<String>,
    current_symbol: Option<SymbolKey>,
    back_stack: Vec<TuiView>,
    path_stack: Vec<Option<String>>,
    symbol_stack: Vec<Option<SymbolKey>>,
    selections: BTreeMap<TuiView, ListSelection>,
    search: Option<SearchState>,
    search_editing: bool,
    pane_focus: TuiPaneFocus,
    show_help: bool,
    command_palette: bool,
    status: Option<String>,
    background_status: Option<TuiProgressUpdate>,
    analysis_running: bool,
    last_action: Option<TuiAction>,
    should_exit: bool,
}

impl Default for TuiAppState {
    fn default() -> Self {
        let mut selections = BTreeMap::new();
        selections.insert(TuiView::Hotspots, ListSelection::default());
        selections.insert(TuiView::RepoTree, ListSelection::default());
        selections.insert(TuiView::FileDetail, ListSelection::default());
        selections.insert(TuiView::SymbolDetail, ListSelection::default());
        selections.insert(TuiView::GitDetail, ListSelection::default());
        selections.insert(TuiView::CouplingGraph, ListSelection::default());
        selections.insert(TuiView::ContextBudgeting, ListSelection::default());
        selections.insert(TuiView::ExplainScore, ListSelection::default());

        Self {
            current_view: TuiView::Hotspots,
            current_path: None,
            current_symbol: None,
            back_stack: Vec::new(),
            path_stack: Vec::new(),
            symbol_stack: Vec::new(),
            selections,
            search: None,
            search_editing: false,
            pane_focus: TuiPaneFocus::Main,
            show_help: false,
            command_palette: false,
            status: None,
            background_status: None,
            analysis_running: false,
            last_action: None,
            should_exit: false,
        }
    }
}

impl TuiAppState {
    pub fn current_view(&self) -> TuiView {
        self.current_view
    }

    pub fn current_path(&self) -> Option<&str> {
        self.current_path.as_deref()
    }

    pub fn selected_index(&self) -> usize {
        self.selection_for_current_view().selected()
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(SearchState::query)
    }

    pub fn is_search_editing(&self) -> bool {
        self.search_editing
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn pane_focus(&self) -> TuiPaneFocus {
        self.pane_focus
    }

    pub fn show_help(&self) -> bool {
        self.show_help
    }

    pub fn command_palette(&self) -> bool {
        self.command_palette
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

    if state.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => state.show_help = false,
            _ => {}
        }
        return;
    }

    if state.command_palette {
        match key.code {
            KeyCode::Esc => state.command_palette = false,
            KeyCode::Char('1') => {
                state.command_palette = false;
                open_hotspots(state);
            }
            KeyCode::Char('2') => {
                state.command_palette = false;
                open_repo_tree(state);
            }
            KeyCode::Char('3') => {
                state.command_palette = false;
                let rows = filtered_visible_rows(snapshot, state);
                open_coupling_graph(state, snapshot, &rows);
            }
            KeyCode::Char('4') => {
                state.command_palette = false;
                open_context_budgeting(state);
            }
            KeyCode::Char('/') => {
                state.command_palette = false;
                state.search = Some(SearchState::default());
                state.search_editing = true;
                state.status = Some("Search active".to_owned());
            }
            _ => {}
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
        state.command_palette = true;
        state.status = Some("Command palette".to_owned());
        return;
    }

    let rows = filtered_visible_rows(snapshot, state);

    if state.search_editing {
        match key.code {
            KeyCode::Esc => clear_search(state),
            KeyCode::Enter => confirm_search(state, snapshot),
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
        KeyCode::Char('?') => state.show_help = true,
        KeyCode::Char('/') => {
            state.search = Some(SearchState::default());
            state.search_editing = true;
            state.status = Some("Search active".to_owned());
            clamp_current_selection(state, snapshot);
        }
        KeyCode::Esc => reduce_escape(state),
        KeyCode::Tab => {
            state.pane_focus = state.pane_focus.next();
            state.status = Some(format!("Focus: {}", state.pane_focus.title()));
        }
        KeyCode::BackTab => {
            state.pane_focus = state.pane_focus.previous();
            state.status = Some(format!("Focus: {}", state.pane_focus.title()));
        }
        KeyCode::Left | KeyCode::Char('h') => reduce_escape(state),
        KeyCode::Right | KeyCode::Char('l') => drill_down(state, snapshot, &rows),
        KeyCode::Down | KeyCode::Char('j') => {
            state.selection_for_current_view_mut().move_next(rows.len());
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.selection_for_current_view_mut().move_previous();
        }
        KeyCode::Enter => drill_down(state, snapshot, &rows),
        KeyCode::Char('1') => open_hotspots(state),
        KeyCode::Char('2') => open_repo_tree(state),
        KeyCode::Char('3') => open_coupling_graph(state, snapshot, &rows),
        KeyCode::Char('4') => open_context_budgeting(state),
        KeyCode::Char('t') => open_repo_tree(state),
        KeyCode::Char('g') => open_coupling_graph(state, snapshot, &rows),
        KeyCode::Char('c') => open_context_budgeting(state),
        KeyCode::Char('x') => explain_score(state, snapshot, &rows),
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
        state.search_editing = false;
        state.status = Some("Filter cleared".to_owned());
        return;
    }

    if let Some(previous) = state.back_stack.pop() {
        state.current_path = state.path_stack.pop().flatten();
        state.current_symbol = state.symbol_stack.pop().flatten();
        state.current_view = previous;
        state.status = Some(format!("Back to {}", previous.title()));
    } else {
        state.should_exit = true;
    }
}

fn clear_search(state: &mut TuiAppState) {
    state.search = None;
    state.search_editing = false;
    state.status = Some("Filter cleared".to_owned());
}

fn confirm_search(state: &mut TuiAppState, snapshot: &TuiSnapshot) {
    state.search_editing = false;
    let row_count = filtered_visible_rows(snapshot, state).len();
    state.status = Some(match state.search_query() {
        Some(query) if !query.trim().is_empty() => {
            format!("Filter active: {query} ({row_count} rows)")
        }
        _ => "Filter active: all rows".to_owned(),
    });
    clamp_current_selection(state, snapshot);
}

fn drill_down(state: &mut TuiAppState, snapshot: &TuiSnapshot, rows: &[String]) {
    let Some(row_text) = selected_row_text(state, rows) else {
        state.status = Some("No row selected".to_owned());
        return;
    };

    let from = state.current_view;
    let next = match from {
        TuiView::Hotspots => {
            hotspot_path_from_row(snapshot, &row_text).map(|path| (TuiView::FileDetail, path))
        }
        TuiView::RepoTree => repo_tree_path_from_row(snapshot, &row_text)
            .filter(|path| file_for_path(snapshot, path).is_some())
            .map(|path| (TuiView::FileDetail, path)),
        TuiView::FileDetail => state
            .current_path
            .as_ref()
            .filter(|path| {
                is_git_detail_row(&row_text) && hotspot_for_path(snapshot, path).is_some()
            })
            .cloned()
            .map(|path| (TuiView::GitDetail, path))
            .or_else(|| {
                let symbol = state
                    .current_path
                    .as_deref()
                    .and_then(|path| symbol_from_file_detail_row(snapshot, path, &row_text));
                symbol.map(|symbol| {
                    let path = symbol.path.clone();
                    push_symbol_view(state, symbol);
                    (TuiView::SymbolDetail, path)
                })
            }),
        TuiView::CouplingGraph => {
            coupling_graph_path_from_row(snapshot, state.current_path.as_deref(), &row_text)
                .map(|path| (TuiView::FileDetail, path))
        }
        TuiView::SymbolDetail
        | TuiView::GitDetail
        | TuiView::ContextBudgeting
        | TuiView::ExplainScore => None,
    };

    let Some((to, path)) = next else {
        state.status = Some("No drilldown available for this row".to_owned());
        return;
    };

    if to != TuiView::SymbolDetail {
        push_view(state, to, Some(path.clone()));
    }
    state.last_action = Some(TuiAction::DrillDown {
        from,
        to,
        path: path.clone(),
    });
    state.status = Some(format!("Opened {row_text}"));
}

fn explain_score(state: &mut TuiAppState, snapshot: &TuiSnapshot, rows: &[String]) {
    let Some(row_text) = selected_row_text(state, rows) else {
        state.status = Some("No row selected".to_owned());
        return;
    };

    let path = match state.current_view {
        TuiView::Hotspots => hotspot_path_from_row(snapshot, &row_text),
        TuiView::RepoTree => repo_tree_path_from_row(snapshot, &row_text),
        TuiView::FileDetail
        | TuiView::SymbolDetail
        | TuiView::GitDetail
        | TuiView::CouplingGraph
        | TuiView::ContextBudgeting
        | TuiView::ExplainScore => state.current_path.clone(),
    };
    let Some(path) = path.filter(|path| hotspot_for_path(snapshot, path).is_some()) else {
        state.status = Some("No hotspot score available for this file".to_owned());
        return;
    };

    let from = state.current_view;
    if from != TuiView::ExplainScore {
        push_view(state, TuiView::ExplainScore, Some(path.clone()));
    }
    state.last_action = Some(TuiAction::ExplainScore {
        view: from,
        path: path.clone(),
    });
    state.status = Some(format!("Explain score: {path}"));
}

fn open_hotspots(state: &mut TuiAppState) {
    if state.current_view != TuiView::Hotspots {
        push_view(state, TuiView::Hotspots, None);
    }
    state.status = Some("Hotspots".to_owned());
}

fn open_repo_tree(state: &mut TuiAppState) {
    if state.current_view != TuiView::RepoTree {
        push_view(state, TuiView::RepoTree, None);
    }
    state.status = Some("Repo tree".to_owned());
}

fn open_coupling_graph(state: &mut TuiAppState, snapshot: &TuiSnapshot, rows: &[String]) {
    let selected_path = selected_path_for_view(state, snapshot, rows);
    if state.current_view != TuiView::CouplingGraph || state.current_path != selected_path {
        push_view(state, TuiView::CouplingGraph, selected_path.clone());
    }
    state.status = Some(match selected_path {
        Some(path) => format!("Coupling graph: {path}"),
        None => "Coupling graph".to_owned(),
    });
}

fn open_context_budgeting(state: &mut TuiAppState) {
    if state.current_view != TuiView::ContextBudgeting {
        push_view(state, TuiView::ContextBudgeting, None);
    }
    state.status = Some("Context budgeting".to_owned());
}

fn selected_path_for_view(
    state: &TuiAppState,
    snapshot: &TuiSnapshot,
    rows: &[String],
) -> Option<String> {
    let row_text = selected_row_text(state, rows);
    match state.current_view {
        TuiView::Hotspots => row_text.and_then(|row| hotspot_path_from_row(snapshot, &row)),
        TuiView::RepoTree => row_text.and_then(|row| repo_tree_path_from_row(snapshot, &row)),
        TuiView::CouplingGraph => row_text.and_then(|row| {
            coupling_graph_path_from_row(snapshot, state.current_path.as_deref(), &row)
        }),
        TuiView::FileDetail
        | TuiView::SymbolDetail
        | TuiView::GitDetail
        | TuiView::ExplainScore => state.current_path.clone(),
        TuiView::ContextBudgeting => None,
    }
}

fn push_view(state: &mut TuiAppState, next_view: TuiView, next_path: Option<String>) {
    if state.current_view == next_view && state.current_path == next_path {
        return;
    }

    state.back_stack.push(state.current_view);
    state.path_stack.push(state.current_path.clone());
    state.symbol_stack.push(state.current_symbol.clone());
    state.current_view = next_view;
    state.current_path = next_path;
    state.current_symbol = None;
    state.selection_for_current_view_mut().selected = 0;
    state.search = None;
    state.search_editing = false;
}

fn push_symbol_view(state: &mut TuiAppState, symbol: &ParseSymbolRecord) {
    state.back_stack.push(state.current_view);
    state.path_stack.push(state.current_path.clone());
    state.symbol_stack.push(state.current_symbol.clone());
    state.current_view = TuiView::SymbolDetail;
    state.current_path = Some(symbol.path.clone());
    state.current_symbol = Some(SymbolKey::from_parse(symbol));
    state.selection_for_current_view_mut().selected = 0;
    state.search = None;
    state.search_editing = false;
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
    let rows = visible_rows(snapshot, state);
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

fn visible_rows(snapshot: &TuiSnapshot, state: &TuiAppState) -> Vec<String> {
    match state.current_view {
        TuiView::Hotspots => hotspot_rows(snapshot),
        TuiView::RepoTree => repo_tree_rows(snapshot)
            .into_iter()
            .map(|row| row.text)
            .collect(),
        TuiView::FileDetail => state
            .current_path
            .as_deref()
            .map(|path| file_detail_rows(snapshot, path))
            .unwrap_or_else(|| vec!["No file selected.".to_owned()]),
        TuiView::SymbolDetail => state
            .current_symbol
            .as_ref()
            .map(|symbol| symbol_detail_rows(snapshot, symbol))
            .unwrap_or_else(|| vec!["No symbol selected.".to_owned()]),
        TuiView::GitDetail => state
            .current_path
            .as_deref()
            .map(|path| git_detail_rows(snapshot, path))
            .unwrap_or_else(|| vec!["No file selected.".to_owned()]),
        TuiView::CouplingGraph => coupling_graph_rows(snapshot, state.current_path.as_deref()),
        TuiView::ContextBudgeting => context_budgeting_rows(snapshot),
        TuiView::ExplainScore => state
            .current_path
            .as_deref()
            .map(|path| explain_score_rows(snapshot, path))
            .unwrap_or_else(|| vec!["No file selected.".to_owned()]),
    }
}

fn hotspot_rows(snapshot: &TuiSnapshot) -> Vec<String> {
    if snapshot.report.hotspots.is_empty() {
        return vec!["No current files were ranked as hotspots.".to_owned()];
    }

    snapshot
        .report
        .hotspots
        .iter()
        .map(|hotspot| {
            format!(
                "#{rank} {path} score {score:.3}",
                rank = hotspot.rank,
                path = hotspot.path,
                score = hotspot.score
            )
        })
        .collect()
}

fn file_detail_rows(snapshot: &TuiSnapshot, path: &str) -> Vec<String> {
    let mut rows = vec![format!("File: {path}")];
    rows.push("Repo tree: press t".to_owned());
    rows.push("Coupling graph: press g".to_owned());

    if let Some(file) = file_for_path(snapshot, path) {
        rows.extend([
            format!("Language: {}", file.language.unwrap_or("unknown")),
            format!("Classification: {}", file.classification),
            format!("Content: {:?}", file.content),
            format!("Bytes: {}", optional_u64(file.byte_size)),
            format!("Lines: {}", optional_u64(file.line_count)),
            format!("Generated: {}", file.is_generated),
            format!("Vendor: {}", file.is_vendor),
            format!("Symlink: {}", file.is_symlink),
        ]);
        if !file.warnings.is_empty() {
            rows.push(format!("Warnings: {:?}", file.warnings));
        }
    } else {
        rows.push("Scan facts: file not present in current scan snapshot".to_owned());
    }

    if let Some(hotspot) = hotspot_for_path(snapshot, path) {
        rows.extend([
            format!("Hotspot score: {:.3}", hotspot.score),
            format!("Rank: #{}", hotspot.rank),
            "Git detail: press Enter".to_owned(),
        ]);
        rows.extend(
            raw_metric_rows(hotspot)
                .into_iter()
                .map(|row| format!("Metric: {row}")),
        );
        rows.extend(limitation_rows(hotspot));
    } else {
        rows.push("Hotspot score: not ranked".to_owned());
        rows.push("Limitations: no score explanation is available for this file".to_owned());
    }

    let symbols = symbols_for_path(snapshot, path);
    if symbols.is_empty() {
        rows.push("Related symbols: none".to_owned());
    } else {
        rows.push(format!("Related symbols: {}", symbols.len()));
        rows.extend(symbols.into_iter().take(8).map(symbol_row_text));
    }

    rows
}

fn coupling_graph_rows(snapshot: &TuiSnapshot, path: Option<&str>) -> Vec<String> {
    match path {
        Some(path) => coupling_graph_file_rows(snapshot, path),
        None => coupling_graph_overview_rows(snapshot),
    }
}

fn coupling_graph_file_rows(snapshot: &TuiSnapshot, path: &str) -> Vec<String> {
    let fan = fan_for_path(snapshot, path);
    let incoming = incoming_edges_for_path(snapshot, path);
    let outgoing = outgoing_edges_for_path(snapshot, path);
    let mut rows = vec![
        format!("File: {path}"),
        format!(
            "Matched current file: {}",
            file_for_path(snapshot, path).is_some()
        ),
        format!(
            "Coupling: {} dependencies, {} dependents",
            fan.map_or(0, |fan| fan.fan_out),
            fan.map_or(0, |fan| fan.fan_in)
        ),
    ];

    rows.push("Incoming edges:".to_owned());
    if incoming.is_empty() {
        rows.push("Incoming: none".to_owned());
    } else {
        rows.extend(incoming.into_iter().map(|edge| {
            format!(
                "Incoming: {} -> {} ({})",
                edge.source_path, edge.target_path, edge.kind
            )
        }));
    }

    rows.push("Outgoing edges:".to_owned());
    if outgoing.is_empty() {
        rows.push("Outgoing: none".to_owned());
    } else {
        rows.extend(outgoing.into_iter().map(|edge| {
            format!(
                "Outgoing: {} -> {} ({})",
                edge.source_path, edge.target_path, edge.kind
            )
        }));
    }

    rows
}

fn coupling_graph_overview_rows(snapshot: &TuiSnapshot) -> Vec<String> {
    let mut rows = vec![format!(
        "Coupling graph: {} resolved dependency edges",
        snapshot.coupling.edges.len()
    )];

    if snapshot.coupling.fan_by_file.is_empty() {
        rows.push("Coupling graph: no parsed current files".to_owned());
        return rows;
    }

    rows.push("Files by coupling:".to_owned());
    rows.extend(snapshot.coupling.fan_by_file.iter().map(|fan| {
        format!(
            "File: {} dependencies {} dependents {}",
            fan.path, fan.fan_out, fan.fan_in
        )
    }));

    if snapshot.coupling.edges.is_empty() {
        rows.push("Edges: none resolved from parser imports".to_owned());
    }

    rows
}

fn context_budgeting_rows(snapshot: &TuiSnapshot) -> Vec<String> {
    let context = &snapshot.report.context;
    let summary = &context.summary;
    let mut rows = vec![
        format!("Total estimated tokens: {}", summary.estimated_tokens),
        format!("Included files: {}", summary.included_files),
        format!("Skipped files: {}", summary.skipped_files),
        format!("Included bytes: {}", summary.included_bytes),
    ];

    if let Some(budget) = &context.budget {
        rows.push(format!("Budget: {}", context_budget_status_text(budget)));
    } else {
        rows.push("Budget: none configured".to_owned());
    }

    rows.push("Top groups:".to_owned());
    if context.groups.is_empty() {
        rows.push("Group: none".to_owned());
    } else {
        rows.extend(context.groups.iter().map(|group| {
            format!(
                "Group: {} tokens {} bytes {} files {}",
                group.path, group.estimated_tokens, group.byte_size, group.file_count
            )
        }));
    }

    rows.push("Skipped files:".to_owned());
    if context.skipped.is_empty() {
        rows.push("Skipped: none".to_owned());
    } else {
        rows.extend(context.skipped.iter().map(|skipped| {
            format!(
                "Skipped: {} ({})",
                skipped.path,
                context_skipped_reason_label(skipped.reason)
            )
        }));
    }

    rows.extend([
        "Approximation: estimated tokens = ceil(byte_size / 4) for UTF-8 text files".to_owned(),
        "Approximation: tokenizer-specific counts vary by model and language".to_owned(),
    ]);

    rows
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoTreeRow {
    path: String,
    text: String,
    is_file: bool,
}

#[derive(Debug, Default)]
struct RepoTreeNode {
    children: BTreeMap<String, RepoTreeNode>,
    file_path: Option<String>,
}

impl Drop for RepoTreeNode {
    fn drop(&mut self) {
        let mut children = std::mem::take(&mut self.children)
            .into_values()
            .collect::<Vec<_>>();

        while let Some(mut child) = children.pop() {
            children.extend(std::mem::take(&mut child.children).into_values());
        }
    }
}

fn repo_tree_rows(snapshot: &TuiSnapshot) -> Vec<RepoTreeRow> {
    let mut root = RepoTreeNode::default();
    for file in &snapshot.scan.files {
        insert_repo_tree_path(&mut root, &file.path);
    }

    let mut rows = Vec::new();
    append_repo_tree_rows(&root, 0, &mut rows);
    if rows.is_empty() {
        rows.push(RepoTreeRow {
            path: String::new(),
            text: "Repository tree: no scanned files".to_owned(),
            is_file: false,
        });
    }
    rows
}

fn insert_repo_tree_path(root: &mut RepoTreeNode, path: &str) {
    let mut node = root;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        node = node.children.entry(part.to_owned()).or_default();
    }
    node.file_path = Some(path.to_owned());
}

enum RepoTreeFrame<'a> {
    Children(&'a RepoTreeNode, usize),
    Directory(&'a str, &'a RepoTreeNode, usize),
    File(&'a RepoTreeNode, usize),
}

fn append_repo_tree_rows(root: &RepoTreeNode, depth: usize, rows: &mut Vec<RepoTreeRow>) {
    let mut stack = vec![RepoTreeFrame::Children(root, depth)];

    while let Some(frame) = stack.pop() {
        match frame {
            RepoTreeFrame::Children(node, depth) => {
                let (dirs, files): (Vec<_>, Vec<_>) = node
                    .children
                    .iter()
                    .partition(|(_, child)| child.file_path.is_none());

                for (_name, child) in files.into_iter().rev() {
                    stack.push(RepoTreeFrame::File(child, depth));
                }

                for (name, child) in dirs.into_iter().rev() {
                    stack.push(RepoTreeFrame::Directory(name, child, depth));
                }
            }
            RepoTreeFrame::Directory(name, child, depth) => {
                let path = repo_tree_display_path(child);
                rows.push(RepoTreeRow {
                    path,
                    text: format!("{}[dir] {name}/", "  ".repeat(depth)),
                    is_file: false,
                });
                stack.push(RepoTreeFrame::Children(child, depth + 1));
            }
            RepoTreeFrame::File(child, depth) => {
                if let Some(path) = &child.file_path {
                    rows.push(RepoTreeRow {
                        path: path.clone(),
                        text: format!("{}[file] {path}", "  ".repeat(depth)),
                        is_file: true,
                    });
                }
            }
        }
    }
}

fn repo_tree_display_path(node: &RepoTreeNode) -> String {
    node.file_path.clone().unwrap_or_default()
}

fn symbol_detail_rows(snapshot: &TuiSnapshot, key: &SymbolKey) -> Vec<String> {
    let parse_symbol = parse_symbol_for_key(snapshot, key);
    let complexity_symbol = complexity_symbol_for_key(snapshot, key);
    let Some(symbol) = parse_symbol else {
        return vec![
            format!("File: {}", key.path),
            format!("Symbol: {} {}", key.kind, key.name),
            "Parser facts: symbol not present in current snapshot".to_owned(),
        ];
    };
    let length_lines = complexity_symbol
        .map(|symbol| symbol.length_lines)
        .unwrap_or(symbol.end_line - symbol.start_line + 1);
    let function_length_lines = complexity_symbol.and_then(|symbol| symbol.function_length_lines);
    let is_large_symbol = complexity_symbol.map(|symbol| symbol.is_large_symbol);

    vec![
        format!("File: {}", symbol.path),
        format!("Symbol: {} {}", symbol.kind, symbol.name),
        format!("Kind: {}", symbol.kind),
        format!("Range: lines {}-{}", symbol.start_line, symbol.end_line),
        format!("Parent: {}", optional_string(symbol.parent.as_deref())),
        format!("Nesting depth: {}", symbol.nesting_depth),
        format!("Length lines: {length_lines}"),
        format!(
            "Function length lines: {}",
            optional_u64(function_length_lines)
        ),
        format!(
            "Cyclomatic complexity: {}",
            optional_u64(symbol.cyclomatic_complexity)
        ),
        format!(
            "Max control flow nesting: {}",
            optional_u64(symbol.max_control_flow_nesting)
        ),
        format!("Large symbol: {}", optional_bool(is_large_symbol)),
    ]
}

fn git_detail_rows(snapshot: &TuiSnapshot, path: &str) -> Vec<String> {
    let Some(hotspot) = hotspot_for_path(snapshot, path) else {
        return vec![
            format!("File: {path}"),
            "Git metrics: no hotspot score raw metrics available".to_owned(),
        ];
    };

    let raw = &hotspot.raw_metrics;
    vec![
        format!("File: {path}"),
        format!("Commits: {}", optional_u64(raw.commits_per_file)),
        format!("Total churn lines: {}", optional_u64(raw.total_churn_lines)),
        format!("Contributors: {}", optional_u64(raw.author_count)),
        format!("Owners: {}", optional_u64(raw.owner_count)),
        format!(
            "Dominant ownership: {}",
            optional_percent(raw.dominant_owner_share)
        ),
        format!(
            "Co-changed file count: {}",
            optional_u64(raw.co_changed_file_count)
        ),
    ]
}

fn explain_score_rows(snapshot: &TuiSnapshot, path: &str) -> Vec<String> {
    let Some(hotspot) = hotspot_for_path(snapshot, path) else {
        return vec![
            format!("File: {path}"),
            "Score explanation: no hotspot score available".to_owned(),
        ];
    };

    let mut rows = vec![
        format!("File: {path}"),
        format!("Score: {:.3}", hotspot.score),
        format!("Formula: {}", hotspot.formula_version.id),
        format!(
            "Formula version: {}.{}",
            hotspot.formula_version.major, hotspot.formula_version.minor
        ),
    ];
    rows.extend(hotspot.weighted_terms.iter().map(|term| {
        format!(
            "Term: {} weight {:.2} input {} contribution {:.3}",
            term.name,
            term.weight,
            optional_f64(term.normalized_input),
            term.weighted_contribution
        )
    }));
    rows.extend(limitation_rows(hotspot));

    rows
}

fn raw_metric_rows(hotspot: &ReportHotspot) -> Vec<String> {
    let raw = &hotspot.raw_metrics;
    vec![
        format!("bytes {}", optional_u64(raw.byte_size)),
        format!("lines {}", optional_u64(raw.line_count)),
        format!("commits {}", optional_u64(raw.commits_per_file)),
        format!("total churn lines {}", optional_u64(raw.total_churn_lines)),
        format!(
            "authors {}  owners {}",
            optional_u64(raw.author_count),
            optional_u64(raw.owner_count)
        ),
        format!(
            "dominant ownership {}",
            optional_percent(raw.dominant_owner_share)
        ),
        format!(
            "co-changed files {}",
            optional_u64(raw.co_changed_file_count)
        ),
    ]
}

fn limitation_rows(hotspot: &ReportHotspot) -> Vec<String> {
    if hotspot.limitations.is_empty() {
        return vec!["Limitations: none recorded".to_owned()];
    }

    hotspot
        .limitations
        .iter()
        .map(|limitation| format!("Limitation: {} - {}", limitation.code, limitation.message))
        .collect()
}

fn hotspot_path_from_row(snapshot: &TuiSnapshot, row_text: &str) -> Option<String> {
    snapshot
        .report
        .hotspots
        .iter()
        .find(|hotspot| {
            row_text.starts_with(&format!("#{} {}", hotspot.rank, hotspot.path))
                || row_text == hotspot.path
        })
        .map(|hotspot| hotspot.path.clone())
}

fn repo_tree_path_from_row(snapshot: &TuiSnapshot, row_text: &str) -> Option<String> {
    repo_tree_rows(snapshot)
        .into_iter()
        .find(|row| row.is_file && row.text == row_text)
        .map(|row| row.path)
}

fn coupling_graph_path_from_row(
    snapshot: &TuiSnapshot,
    current_path: Option<&str>,
    row_text: &str,
) -> Option<String> {
    if let Some(path) = row_text
        .strip_prefix("File: ")
        .and_then(|rest| {
            rest.split_once(" fan-in ")
                .map(|(path, _)| path)
                .or(Some(rest))
        })
        .filter(|path| file_for_path(snapshot, path).is_some())
    {
        return Some(path.to_owned());
    }

    if let Some((source, target)) = parse_coupling_edge_row(row_text, "Incoming: ") {
        return coupling_edge_drilldown_target(snapshot, current_path, source, target);
    }

    if let Some((source, target)) = parse_coupling_edge_row(row_text, "Outgoing: ") {
        return coupling_edge_drilldown_target(snapshot, current_path, source, target);
    }

    None
}

fn parse_coupling_edge_row<'a>(row_text: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = row_text.strip_prefix(prefix)?;
    let (source, rest) = rest.split_once(" -> ")?;
    let (target, _) = rest.rsplit_once(" (")?;

    Some((source, target))
}

fn coupling_edge_drilldown_target(
    snapshot: &TuiSnapshot,
    current_path: Option<&str>,
    source: &str,
    target: &str,
) -> Option<String> {
    let next = match current_path {
        Some(current) if current == source => target,
        Some(current) if current == target => source,
        _ => target,
    };

    file_for_path(snapshot, next).map(|_| next.to_owned())
}

fn hotspot_for_path<'a>(snapshot: &'a TuiSnapshot, path: &str) -> Option<&'a ReportHotspot> {
    snapshot
        .report
        .hotspots
        .iter()
        .find(|hotspot| hotspot.path == path)
}

fn file_for_path<'a>(snapshot: &'a TuiSnapshot, path: &str) -> Option<&'a FileRecord> {
    snapshot.scan.files.iter().find(|file| file.path == path)
}

fn symbols_for_path<'a>(snapshot: &'a TuiSnapshot, path: &str) -> Vec<&'a ParseSymbolRecord> {
    snapshot
        .symbols
        .symbols
        .iter()
        .filter(|symbol| symbol.path == path)
        .collect()
}

fn fan_for_path<'a>(snapshot: &'a TuiSnapshot, path: &str) -> Option<&'a TuiFileFan> {
    snapshot
        .coupling
        .fan_by_file
        .iter()
        .find(|fan| fan.path == path)
}

fn ownership_for_path<'a>(snapshot: &'a TuiSnapshot, path: &str) -> Option<&'a TuiFileOwnership> {
    snapshot
        .ownership
        .by_file
        .iter()
        .find(|ownership| ownership.path == path)
}

fn incoming_edges_for_path<'a>(
    snapshot: &'a TuiSnapshot,
    path: &str,
) -> Vec<&'a ResolvedDependencyEdge> {
    snapshot
        .coupling
        .edges
        .iter()
        .filter(|edge| edge.target_path == path)
        .collect()
}

fn outgoing_edges_for_path<'a>(
    snapshot: &'a TuiSnapshot,
    path: &str,
) -> Vec<&'a ResolvedDependencyEdge> {
    snapshot
        .coupling
        .edges
        .iter()
        .filter(|edge| edge.source_path == path)
        .collect()
}

fn symbol_from_file_detail_row<'a>(
    snapshot: &'a TuiSnapshot,
    path: &str,
    row_text: &str,
) -> Option<&'a ParseSymbolRecord> {
    symbols_for_path(snapshot, path)
        .into_iter()
        .find(|symbol| symbol_row_text(symbol) == row_text)
}

fn parse_symbol_for_key<'a>(
    snapshot: &'a TuiSnapshot,
    key: &SymbolKey,
) -> Option<&'a ParseSymbolRecord> {
    snapshot
        .symbols
        .symbols
        .iter()
        .find(|symbol| SymbolKey::from_parse(symbol) == *key)
}

fn complexity_symbol_for_key<'a>(
    snapshot: &'a TuiSnapshot,
    key: &SymbolKey,
) -> Option<&'a ComplexitySymbolRecord> {
    snapshot
        .complexity
        .symbols
        .iter()
        .find(|symbol| SymbolKey::from_complexity(symbol) == *key)
}

fn symbol_row_text(symbol: &ParseSymbolRecord) -> String {
    format!(
        "Symbol: {} {} lines {}-{}",
        symbol.kind, symbol.name, symbol.start_line, symbol.end_line
    )
}

fn is_git_detail_row(row_text: &str) -> bool {
    row_text == "Git detail: press Enter"
}

fn context_budget_status_text(budget: &ContextBudgetStatus) -> String {
    match (budget.remaining_tokens, budget.over_budget_tokens) {
        (Some(remaining), _) => format!(
            "within budget by {remaining} tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
        (_, Some(over)) => format!(
            "over budget by {over} tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
        (None, None) => format!(
            "within budget by 0 tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
    }
}

fn context_skipped_reason_label(reason: ContextSkippedReason) -> &'static str {
    match reason {
        ContextSkippedReason::Binary => "binary",
        ContextSkippedReason::UnknownContent => "unknown content",
        ContextSkippedReason::MissingByteSize => "missing byte size",
        ContextSkippedReason::Unreadable => "unreadable",
        ContextSkippedReason::ExcludedGenerated => "excluded generated",
        ContextSkippedReason::ExcludedVendor => "excluded vendor",
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn optional_f64(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn optional_percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.1}%", value * 100.0))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn optional_string(value: Option<&str>) -> String {
    value
        .map(str::to_owned)
        .unwrap_or_else(|| "none".to_owned())
}

fn optional_bool(value: Option<bool>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
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

#[derive(Debug, Clone, PartialEq)]
struct DisplayRow {
    text: String,
    label: String,
    meta: String,
    severity: TuiSeverity,
    hotspot: Option<DisplayHotspotRow>,
}

#[derive(Debug, Clone, PartialEq)]
struct DisplayHotspotRow {
    path: String,
    score: f64,
    tags: Vec<String>,
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

fn layout_mode(area: Rect) -> TuiLayoutMode {
    if area.width >= 120 && area.height >= 30 {
        TuiLayoutMode::Wide
    } else if area.width >= 90 {
        TuiLayoutMode::Medium
    } else {
        TuiLayoutMode::Narrow
    }
}

fn display_rows(snapshot: &TuiSnapshot, state: &TuiAppState) -> Vec<DisplayRow> {
    filtered_visible_rows(snapshot, state)
        .into_iter()
        .map(|text| display_row_from_text(snapshot, state.current_view, text))
        .collect()
}

fn display_row_from_text(snapshot: &TuiSnapshot, view: TuiView, text: String) -> DisplayRow {
    match view {
        TuiView::Hotspots => hotspot_display_row(snapshot, text),
        TuiView::RepoTree => repo_tree_display_row(snapshot, text),
        TuiView::CouplingGraph => coupling_display_row(text),
        TuiView::ContextBudgeting => context_display_row(text),
        TuiView::ExplainScore => explain_display_row(text),
        TuiView::FileDetail | TuiView::SymbolDetail | TuiView::GitDetail => {
            detail_display_row(text)
        }
    }
}

fn hotspot_display_row(snapshot: &TuiSnapshot, text: String) -> DisplayRow {
    let hotspot =
        hotspot_path_from_row(snapshot, &text).and_then(|path| hotspot_for_path(snapshot, &path));
    let severity = hotspot
        .map(|hotspot| severity_for_score(hotspot.score))
        .unwrap_or(TuiSeverity::Neutral);
    let meta = hotspot
        .map(|hotspot| {
            let risk = (hotspot.score * 10.0).round() as u64;
            format!(
                "risk {risk}/10  churn {}  authors {}",
                optional_u64(hotspot.raw_metrics.total_churn_lines),
                optional_u64(hotspot.raw_metrics.author_count)
            )
        })
        .unwrap_or_default();

    DisplayRow {
        hotspot: hotspot.map(|hotspot| DisplayHotspotRow {
            path: hotspot.path.clone(),
            score: hotspot.score,
            tags: hotspot_driver_tags(snapshot, hotspot),
        }),
        label: String::new(),
        text,
        meta,
        severity,
    }
}

fn repo_tree_display_row(snapshot: &TuiSnapshot, text: String) -> DisplayRow {
    let severity = repo_tree_path_from_row(snapshot, &text)
        .and_then(|path| {
            hotspot_for_path(snapshot, &path).map(|hotspot| severity_for_score(hotspot.score))
        })
        .unwrap_or(TuiSeverity::Muted);
    let label = if text.contains("[dir]") {
        "dir"
    } else {
        "file"
    }
    .to_owned();

    DisplayRow {
        text,
        label,
        meta: String::new(),
        severity,
        hotspot: None,
    }
}

fn detail_display_row(text: String) -> DisplayRow {
    let severity = if text.starts_with("Hotspot score:")
        || text.starts_with("Rank:")
        || text.starts_with("Large symbol: true")
    {
        TuiSeverity::Medium
    } else if text.starts_with("Limitation:") || text.starts_with("Warnings:") {
        TuiSeverity::High
    } else if text.contains("unknown") || text.contains("not ranked") {
        TuiSeverity::Muted
    } else {
        TuiSeverity::Neutral
    };
    let label = text
        .split_once(':')
        .map(|(label, _)| label.to_ascii_lowercase())
        .unwrap_or_else(|| "detail".to_owned());

    DisplayRow {
        text,
        label,
        meta: String::new(),
        severity,
        hotspot: None,
    }
}

fn coupling_display_row(text: String) -> DisplayRow {
    let severity = if text.starts_with("Incoming: none")
        || text.starts_with("Outgoing: none")
        || text.starts_with("Edges: none")
    {
        TuiSeverity::Muted
    } else if text.contains("fan-in") || text.contains("fan-out") {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Neutral
    };
    let label = if text.starts_with("Incoming:") {
        "incoming"
    } else if text.starts_with("Outgoing:") {
        "outgoing"
    } else {
        "coupling"
    };

    DisplayRow {
        text,
        label: label.to_owned(),
        meta: String::new(),
        severity,
        hotspot: None,
    }
}

fn context_display_row(text: String) -> DisplayRow {
    let severity = if text.contains("over budget") {
        TuiSeverity::High
    } else if text.starts_with("Skipped:") || text.starts_with("Approximation:") {
        TuiSeverity::Muted
    } else if text.starts_with("Group:") || text.starts_with("Budget:") {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Neutral
    };

    DisplayRow {
        text,
        label: "context".to_owned(),
        meta: String::new(),
        severity,
        hotspot: None,
    }
}

fn explain_display_row(text: String) -> DisplayRow {
    let severity = if text.starts_with("Limitation:") {
        TuiSeverity::High
    } else if text.starts_with("Term:") {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Neutral
    };

    DisplayRow {
        text,
        label: "score".to_owned(),
        meta: String::new(),
        severity,
        hotspot: None,
    }
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

#[cfg(test)]
fn render(frame: &mut Frame<'_>, snapshot: &TuiSnapshot, state: &TuiAppState) {
    render_with_options(frame, snapshot, state, TuiOptions::default());
}

fn render_with_options(
    frame: &mut Frame<'_>,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    options: TuiOptions,
) {
    let area = frame.area();
    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .areas(area);
    let rows = display_rows(snapshot, state);
    let mode = layout_mode(area);

    render_header(frame, header, snapshot, state, options);
    match mode {
        TuiLayoutMode::Wide => render_wide_body(frame, body, snapshot, state, &rows, options),
        TuiLayoutMode::Medium => render_medium_body(frame, body, snapshot, state, &rows, options),
        TuiLayoutMode::Narrow => render_narrow_body(frame, body, snapshot, state, &rows, options),
    }
    render_footer(frame, footer, state, options);

    if state.show_help {
        render_help_overlay(frame, area, options);
    }
    if state.command_palette {
        render_command_palette(frame, area, options);
    }
}

fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    options: TuiOptions,
) {
    let head = short_commit(&snapshot.report.summary.git.head_commit_id);
    let path = state.current_path().unwrap_or("repo");
    let title_style = style(options, TuiSeverity::Low).add_modifier(Modifier::BOLD);
    let muted = style(options, TuiSeverity::Muted);
    let nav = navigation_line(state, options);
    let header = Paragraph::new(vec![
        nav,
        Line::styled(
            horizontal_rule(area.width, options),
            style(options, TuiSeverity::Muted),
        ),
        Line::from(vec![
            Span::styled("Hotpath", title_style),
            Span::raw("  "),
            Span::styled("local risk triage", muted),
            Span::raw("  "),
            Span::styled(format!("HEAD {head}"), muted),
        ]),
        Line::from(vec![
            Span::styled(
                state.current_view.title(),
                style(options, TuiSeverity::Medium),
            ),
            Span::raw(" / "),
            Span::raw(truncate_middle(
                path,
                area.width.saturating_sub(48) as usize,
            )),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} files  {} hotspots  {} tokens",
                    snapshot.scan.summary.total_files,
                    snapshot.report.summary.hotspot_count,
                    format_compact_count(snapshot.report.summary.context_estimated_tokens)
                ),
                muted,
            ),
        ]),
    ]);

    frame.render_widget(header, area);
}

fn render_wide_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    render_joined_body(frame, area, snapshot, state, rows, [62, 38], options);
}

fn render_medium_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    render_joined_body(frame, area, snapshot, state, rows, [64, 36], options);
}

fn render_narrow_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    render_main_panel(frame, area, snapshot, state, rows, options);
}

fn navigation_line(state: &TuiAppState, options: TuiOptions) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, view) in PRIMARY_VIEWS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let active = *view == state.current_view;
        let label = if active {
            format!("[{} {}]", index + 1, view.title())
        } else {
            format!("{} {}", index + 1, view.title())
        };
        let mut view_style = if active {
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD)
        } else {
            style(options, TuiSeverity::Muted)
        };
        if state.pane_focus == TuiPaneFocus::Nav {
            view_style = view_style.add_modifier(Modifier::UNDERLINED);
        }
        spans.push(Span::styled(label, view_style));
    }

    Line::from(spans)
}

fn horizontal_rule(width: u16, options: TuiOptions) -> String {
    let glyph = if options.ascii { '-' } else { '\u{2500}' };
    glyph.to_string().repeat(width as usize)
}

fn render_joined_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    split_percentages: [u16; 2],
    options: TuiOptions,
) {
    let focused = matches!(
        state.pane_focus,
        TuiPaneFocus::Main | TuiPaneFocus::Inspector
    );
    let block = plain_panel_block(focused, options);
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

    render_main_panel_content(frame, main, snapshot, state, rows, options);
    render_vertical_divider(frame, divider, options);
    render_inspector_content(frame, inspector, snapshot, state, rows, options);
}

fn render_main_panel_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    let content = padded_rect(area, 1, 1, 0, 0);
    let content_width = content.width.max(1);
    let mut lines = vec![
        Line::styled(
            state.current_view.title(),
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    let body_height = content.height.saturating_sub(lines.len() as u16).max(1) as usize;
    lines.extend(main_panel_lines(
        snapshot,
        state,
        rows,
        content_width,
        body_height,
        options,
    ));
    frame.render_widget(Paragraph::new(lines), content);
}

fn render_inspector_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    let content = padded_rect(area, 1, 1, 0, 0);
    let mut lines = vec![
        Line::styled(
            "Inspector",
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    let body_width = content.width.max(1);
    lines.extend(inspector_lines(snapshot, state, rows, body_width, options));
    frame.render_widget(Paragraph::new(lines), content);
}

fn render_vertical_divider(frame: &mut Frame<'_>, area: Rect, options: TuiOptions) {
    let glyph = if options.ascii { "|" } else { "\u{2502}" };
    let lines = (0..area.height)
        .map(|_| Line::styled(glyph, style(options, TuiSeverity::Muted)))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn padded_rect(area: Rect, left: u16, right: u16, top: u16, bottom: u16) -> Rect {
    let x_offset = left.min(area.width);
    let y_offset = top.min(area.height);
    Rect {
        x: area.x.saturating_add(x_offset),
        y: area.y.saturating_add(y_offset),
        width: area.width.saturating_sub(left.saturating_add(right)),
        height: area.height.saturating_sub(top.saturating_add(bottom)),
    }
}

fn render_main_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    options: TuiOptions,
) {
    let top_padding = u16::from(state.current_view == TuiView::Hotspots);
    let inner_height = area.height.saturating_sub(2 + top_padding).max(1) as usize;
    let horizontal_padding = u16::from(state.current_view == TuiView::Hotspots);
    let inner_width = area
        .width
        .saturating_sub(2 + horizontal_padding.saturating_mul(2))
        .max(1);
    let lines = main_panel_lines(snapshot, state, rows, inner_width, inner_height, options);
    let mut block = panel_block(
        state.current_view.title(),
        state.pane_focus == TuiPaneFocus::Main,
        options,
    );
    if horizontal_padding > 0 {
        block = block.padding(Padding::new(
            horizontal_padding,
            horizontal_padding,
            top_padding,
            0,
        ));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn main_panel_lines(
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    width: u16,
    height: usize,
    options: TuiOptions,
) -> Vec<Line<'static>> {
    let selected = state.selected_index();
    let mut lines = match state.current_view {
        TuiView::Hotspots => hotspot_kpi_lines(snapshot, options),
        TuiView::ContextBudgeting => context_kpi_lines(snapshot, options),
        TuiView::CouplingGraph => coupling_kpi_lines(snapshot, options),
        _ => Vec::new(),
    };
    if state.current_view == TuiView::Hotspots {
        lines.push(hotspot_header_line(width, options));
    }
    let remaining = height.saturating_sub(lines.len()).max(1);
    if rows.is_empty() {
        lines.push(Line::styled("No rows.", style(options, TuiSeverity::Muted)));
    } else {
        lines.extend(
            visible_display_row_window(rows, selected, remaining)
                .map(|(index, row)| render_display_row(row, index == selected, width, options)),
        );
    }
    lines
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState, options: TuiOptions) {
    let text = if state.search_editing {
        format!("/{}", state.search_query().unwrap_or(""))
    } else if state.analysis_running {
        state
            .background_status
            .as_ref()
            .map(|update| progress_status_text(update, area.width as usize, options))
            .or_else(|| state.status().map(str::to_owned))
            .unwrap_or_else(|| "Analyzing repository in background".to_owned())
    } else {
        let default_status = || {
            [
                "j/k or arrows move",
                "Enter drill",
                "/ search",
                "1-4 views",
                "? help",
                "Ctrl-P commands",
                "e editor",
                "q quit",
            ]
            .join(HOTSPOT_TAG_SEPARATOR)
        };
        state
            .status()
            .map(str::to_owned)
            .unwrap_or_else(default_status)
    };
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Left), area);
}

fn progress_status_text(update: &TuiProgressUpdate, width: usize, options: TuiOptions) -> String {
    progress_status_text_at(update, width, options, Instant::now())
}

fn progress_status_text_at(
    update: &TuiProgressUpdate,
    width: usize,
    options: TuiOptions,
    now: Instant,
) -> String {
    let mut text = if let (Some(completed), Some(total)) = (update.completed, update.total) {
        let percent = progress_percent(update).unwrap_or(0);
        let bar = progress_bar(percent, options);
        let unit = if update.unit.is_empty() {
            String::new()
        } else {
            format!(" {}", update.unit)
        };
        let rate = if update.unit.is_empty() {
            String::new()
        } else {
            let rate = progress_rate_per_second(update, now).unwrap_or(0);
            format!("{HOTSPOT_TAG_SEPARATOR}{rate}{unit}/s")
        };
        let detail = if update.detail.is_empty() {
            String::new()
        } else {
            format!("{HOTSPOT_TAG_SEPARATOR}{}", update.detail)
        };
        format!(
            "{} [{}] {:>3}%  {}/{}{}{}{}",
            update.phase, bar, percent, completed, total, unit, rate, detail
        )
    } else if update.detail.is_empty() {
        update.phase.to_owned()
    } else {
        format!("{}... {}", update.phase, update.detail)
    };

    if text.chars().count() > width {
        text = truncate_end(&text, width);
    }

    text
}

fn progress_rate_per_second(update: &TuiProgressUpdate, now: Instant) -> Option<u64> {
    let completed = update.completed?;
    let rate = update.rate?;
    let elapsed = now.duration_since(rate.started_at).as_secs_f64();
    if elapsed <= 0.0 {
        return None;
    }
    Some(((completed.saturating_sub(rate.completed_at_start) as f64) / elapsed).round() as u64)
}

fn progress_bar(percent: u64, options: TuiOptions) -> String {
    let (filled, empty) = score_bar_parts(percent.min(100) as f64 / 100.0, 10, options);
    format!("{filled}{empty}")
}

fn visible_display_row_window(
    rows: &[DisplayRow],
    selected: usize,
    limit: usize,
) -> impl Iterator<Item = (usize, &DisplayRow)> {
    let start = selected
        .saturating_add(1)
        .saturating_sub(limit)
        .min(rows.len());

    rows.iter().enumerate().skip(start).take(limit)
}

fn render_display_row(
    row: &DisplayRow,
    selected: bool,
    width: u16,
    options: TuiOptions,
) -> Line<'static> {
    if let Some(hotspot) = &row.hotspot {
        return render_hotspot_display_row(hotspot, selected, width, options);
    }

    let marker = if selected { "\u{258C}" } else { " " };
    let label_width = 9usize;
    let meta_width = row.meta.len().min(32);
    let text_limit = (width as usize)
        .saturating_sub(label_width + meta_width + 8)
        .max(12);
    let row_style = if selected {
        style(options, row.severity).add_modifier(Modifier::BOLD)
    } else {
        style(options, row.severity)
    };

    let mut spans = vec![
        Span::styled(marker.to_owned(), marker_style(selected, options)),
        Span::raw(" "),
        Span::styled(
            format!("{:<label_width$}", row.label),
            style(options, TuiSeverity::Muted),
        ),
        Span::raw(" "),
        Span::styled(truncate_middle(&row.text, text_limit), row_style),
    ];
    if !row.meta.is_empty() {
        spans.extend([
            Span::raw("  "),
            Span::styled(
                truncate_middle(&row.meta, meta_width.max(1)),
                style(options, TuiSeverity::Muted),
            ),
        ]);
    }

    Line::from(spans)
}

fn render_hotspot_display_row(
    hotspot: &DisplayHotspotRow,
    selected: bool,
    width: u16,
    options: TuiOptions,
) -> Line<'static> {
    let marker = if selected { "\u{258C}" } else { " " };
    let row_style = if selected {
        selected_row_style(options, severity_for_score(hotspot.score))
    } else {
        style(options, severity_for_score(hotspot.score))
    };
    let muted_style = if selected {
        selected_row_style(options, TuiSeverity::Muted)
    } else {
        style(options, TuiSeverity::Muted)
    };
    let gap_style = if selected {
        selected_gap_style(options)
    } else {
        Style::default()
    };
    let tags = hotspot_tag_text(&hotspot.tags);
    let (path_width, tag_width) = hotspot_column_widths(width as usize);
    let risk = hotspot.score * 10.0;
    let path = pad_truncated_path(&hotspot.path, path_width);

    let mut spans = vec![
        Span::styled(
            format!("{marker:<HOTSPOT_SELECTOR_WIDTH$}"),
            marker_style(selected, options),
        ),
        Span::styled(path, row_style),
        Span::styled("  ", gap_style),
    ];
    if selected {
        spans.extend(selected_score_bar_spans(
            hotspot.score,
            METRIC_BAR_WIDTH,
            row_style,
            options,
        ));
    } else {
        spans.extend(score_bar_spans(
            hotspot.score,
            METRIC_BAR_WIDTH,
            row_style,
            options,
        ));
    }
    spans.push(Span::styled(
        format!(" {:>HOTSPOT_SCORE_WIDTH$.1}", risk),
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

fn hotspot_header_line(width: u16, options: TuiOptions) -> Line<'static> {
    let (path_width, tag_width) = hotspot_column_widths(width as usize);
    let risk_width = METRIC_BAR_WIDTH + 1 + HOTSPOT_SCORE_WIDTH;
    let header_style = style(options, TuiSeverity::Muted).add_modifier(Modifier::BOLD);

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

#[derive(Debug, Clone, PartialEq)]
struct DriverSignal {
    label: &'static str,
    strength: f64,
    percentile: f64,
    priority: u8,
}

fn driver_signal(label: &'static str, strength: f64, priority: u8) -> DriverSignal {
    DriverSignal {
        label,
        strength,
        percentile: 0.0,
        priority,
    }
}

fn hotspot_driver_tags(snapshot: &TuiSnapshot, hotspot: &ReportHotspot) -> Vec<String> {
    qualified_driver_tags(snapshot, hotspot)
        .into_iter()
        .filter(|signal| signal_qualifies_for_row_tag(hotspot, signal))
        .map(|signal| signal.label.to_owned())
        .take(1)
        .collect()
}

fn inspector_driver_tags(snapshot: &TuiSnapshot, hotspot: &ReportHotspot) -> Vec<String> {
    qualified_driver_tags(snapshot, hotspot)
        .into_iter()
        .filter(signal_qualifies_for_inspector_tag)
        .map(|signal| signal.label.to_owned())
        .take(3)
        .collect()
}

fn qualified_driver_tags(snapshot: &TuiSnapshot, hotspot: &ReportHotspot) -> Vec<DriverSignal> {
    let mut signals = collect_driver_signals(snapshot, hotspot)
        .into_iter()
        .filter_map(|mut signal| {
            signal.percentile =
                driver_signal_percentile_rank(snapshot, signal.label, signal.strength);
            signal_qualifies_for_tag(&signal).then_some(signal)
        })
        .collect::<Vec<_>>();

    signals.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| {
                right
                    .strength
                    .partial_cmp(&left.strength)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                right
                    .percentile
                    .partial_cmp(&left.percentile)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.label.cmp(right.label))
    });

    let mut tags = Vec::new();
    for signal in signals {
        if !tags
            .iter()
            .any(|existing: &DriverSignal| existing.label == signal.label)
        {
            tags.push(signal);
        }
    }

    tags
}

fn signal_qualifies_for_row_tag(hotspot: &ReportHotspot, signal: &DriverSignal) -> bool {
    if hotspot.score >= 0.70 {
        return signal.strength >= 0.70 && signal.percentile >= 0.90;
    }

    hotspot.score >= 0.55 && signal.strength >= 0.85 && signal.percentile >= 0.95
}

fn signal_qualifies_for_inspector_tag(signal: &DriverSignal) -> bool {
    signal.strength >= 0.65 && signal.percentile >= 0.75
}

fn signal_qualifies_for_tag(signal: &DriverSignal) -> bool {
    signal.strength >= 0.60 || signal.percentile >= 0.75
}

fn driver_signal_percentile_rank(snapshot: &TuiSnapshot, label: &str, strength: f64) -> f64 {
    let values = snapshot
        .report
        .hotspots
        .iter()
        .filter(|hotspot| hotspot.score >= 0.55)
        .flat_map(|hotspot| collect_driver_signals(snapshot, hotspot))
        .filter(|signal| signal.label == label)
        .map(|signal| signal.strength)
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();

    percentile_rank(&values, strength)
}

fn percentile_rank(values: &[f64], value: f64) -> f64 {
    if !value.is_finite() || values.is_empty() {
        return 0.0;
    }
    if values.len() == 1 {
        return if value > 0.0 { 1.0 } else { 0.0 };
    }

    let below_or_equal = values
        .iter()
        .filter(|candidate| candidate.is_finite() && **candidate <= value)
        .count();

    ((below_or_equal.saturating_sub(1)) as f64 / (values.len() - 1) as f64).clamp(0.0, 1.0)
}

fn collect_driver_signals(snapshot: &TuiSnapshot, hotspot: &ReportHotspot) -> Vec<DriverSignal> {
    let mut signals = hotspot
        .weighted_terms
        .iter()
        .filter_map(driver_signal_from_term)
        .collect::<Vec<_>>();
    if let Some(fan) = fan_for_path(snapshot, &hotspot.path) {
        let fanout = normalized_u64(fan.fan_out, 25);
        if fanout >= 0.45 {
            signals.push(driver_signal("CORE", fanout, 80));
        }
        let fanin = normalized_u64(fan.fan_in, 25);
        if fanin >= 0.45 {
            signals.push(driver_signal("CORE", fanin, 95));
        }
    }
    if let Some(complexity) = complexity_pressure_for_path(snapshot, &hotspot.path) {
        if complexity >= 0.50 {
            signals.push(driver_signal("COMPLEXITY", complexity, 70));
        }
    }
    if let Some(file) = file_for_path(snapshot, &hotspot.path) {
        if let Some(byte_size) = file.byte_size {
            let context = normalized_context_tokens(byte_size.div_ceil(4), 100_000);
            if context >= 0.65 {
                signals.push(driver_signal("SIZE", context, 75));
            }
        }
    }

    signals.sort_by(|left, right| {
        right
            .strength
            .partial_cmp(&left.strength)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.label.cmp(right.label))
    });
    let mut unique_signals = Vec::new();
    for signal in signals {
        if unique_signals
            .iter()
            .all(|existing: &DriverSignal| existing.label != signal.label)
        {
            unique_signals.push(signal);
        }
    }

    unique_signals
}

fn driver_signal_from_term(term: &WeightedTerm) -> Option<DriverSignal> {
    let input = term.normalized_input.unwrap_or(0.0);
    let label = match term.metric {
        NormalizedMetric::Churn if input >= 0.60 => "CHURN",
        NormalizedMetric::Size if input >= 0.65 => "SIZE",
        NormalizedMetric::RecentChurn if input >= 0.75 => "VOLATILITY",
        NormalizedMetric::Ownership if input >= 0.70 => "FRAGILITY",
        NormalizedMetric::Coupling if input >= 0.60 => "COUPLING",
        _ => return None,
    };

    Some(driver_signal(label, input, driver_signal_priority(label)))
}

fn driver_signal_priority(label: &str) -> u8 {
    match label {
        "CORE" => 95,
        "CHURN" => 90,
        "COUPLING" => 85,
        "FRAGILITY" => 80,
        "SIZE" => 75,
        "COMPLEXITY" => 70,
        "VOLATILITY" => 65,
        _ => 0,
    }
}

fn hotspot_kpi_lines(snapshot: &TuiSnapshot, options: TuiOptions) -> Vec<Line<'static>> {
    let top_score = snapshot
        .report
        .hotspots
        .first()
        .map(|hotspot| hotspot.score)
        .unwrap_or(0.0);
    vec![
        metric_bar_line(
            "Repo Risk",
            top_score,
            format!("{} {:.1}", severity_label(top_score), top_score * 10.0),
            severity_for_score(top_score),
            options,
        ),
        Line::raw(""),
    ]
}

fn context_kpi_lines(snapshot: &TuiSnapshot, options: TuiOptions) -> Vec<Line<'static>> {
    let context = &snapshot.report.context;
    let budget = context
        .budget
        .as_ref()
        .map(context_budget_status_text)
        .unwrap_or_else(|| "no budget".to_owned());
    vec![
        metric_bar_line(
            "Context",
            context_pressure(snapshot),
            format!("{} tokens", context.summary.estimated_tokens),
            severity_for_score(context_pressure(snapshot)),
            options,
        ),
        Line::styled(
            format!("Budget     {budget}"),
            style(options, TuiSeverity::Muted),
        ),
        Line::raw(""),
    ]
}

fn coupling_kpi_lines(snapshot: &TuiSnapshot, options: TuiOptions) -> Vec<Line<'static>> {
    vec![
        metric_bar_line(
            "Coupling",
            coupling_pressure(snapshot),
            format!("{} edges", snapshot.coupling.edges.len()),
            severity_for_score(coupling_pressure(snapshot)),
            options,
        ),
        Line::styled(
            format!(
                "Files      {} with fan metrics",
                snapshot.coupling.fan_by_file.len()
            ),
            style(options, TuiSeverity::Muted),
        ),
        Line::raw(""),
    ]
}

fn metric_bar_line(
    label: &str,
    value: f64,
    text: String,
    severity: TuiSeverity,
    options: TuiOptions,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<METRIC_LABEL_WIDTH$} "),
        style(options, TuiSeverity::Muted),
    )];
    spans.extend(score_bar_spans(
        value,
        METRIC_BAR_WIDTH,
        style(options, severity),
        options,
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(text, style(options, severity)));

    Line::from(spans)
}

fn section_divider(width: u16, options: TuiOptions) -> Line<'static> {
    let inner_width = width.saturating_sub(4).max(12) as usize;
    let glyph = if options.ascii { "-" } else { "\u{2500}" };

    Line::styled(
        glyph.repeat(inner_width.min(48)),
        style(options, TuiSeverity::Muted),
    )
}

fn coupling_pressure(snapshot: &TuiSnapshot) -> f64 {
    let max_fan = snapshot
        .coupling
        .fan_by_file
        .iter()
        .map(|fan| fan.fan_in.max(fan.fan_out))
        .max()
        .unwrap_or(0);
    let edge_pressure = normalized_u64(snapshot.coupling.edges.len() as u64, 100);
    let fan_pressure = normalized_u64(max_fan, 25);

    edge_pressure.max(fan_pressure)
}

fn context_pressure(snapshot: &TuiSnapshot) -> f64 {
    let estimated = snapshot.report.context.summary.estimated_tokens;
    snapshot
        .report
        .context
        .budget
        .as_ref()
        .map(|budget| normalized_u64(estimated, budget.budget_tokens.max(1)))
        .unwrap_or_else(|| normalized_context_tokens(estimated, 500_000))
}

fn context_pressure_for_path(snapshot: &TuiSnapshot, path: &str) -> Option<(f64, u64)> {
    let bytes = file_for_path(snapshot, path)?.byte_size?;
    let tokens = bytes.div_ceil(4);

    Some((normalized_context_tokens(tokens, 100_000), tokens))
}

fn coupling_pressure_for_path(snapshot: &TuiSnapshot, path: &str) -> Option<f64> {
    let fan = fan_for_path(snapshot, path)?;
    let degree = fan.fan_in.saturating_add(fan.fan_out);
    let cochange_count = hotspot_for_path(snapshot, path)
        .and_then(|hotspot| hotspot.raw_metrics.co_changed_file_count)
        .unwrap_or(0);
    if degree == 0 && cochange_count == 0 {
        return Some(0.0);
    }

    let fan_in_values = snapshot
        .coupling
        .fan_by_file
        .iter()
        .map(|fan| fan.fan_in as f64)
        .collect::<Vec<_>>();
    let fan_out_values = snapshot
        .coupling
        .fan_by_file
        .iter()
        .map(|fan| fan.fan_out as f64)
        .collect::<Vec<_>>();
    let degree_values = snapshot
        .coupling
        .fan_by_file
        .iter()
        .map(|fan| fan.fan_in.saturating_add(fan.fan_out) as f64)
        .collect::<Vec<_>>();
    let cochange_values = snapshot
        .report
        .hotspots
        .iter()
        .filter_map(|hotspot| {
            hotspot
                .raw_metrics
                .co_changed_file_count
                .map(|value| value as f64)
        })
        .collect::<Vec<_>>();

    let fan_in_rank = percentile_rank(&fan_in_values, fan.fan_in as f64);
    let fan_out_rank = percentile_rank(&fan_out_values, fan.fan_out as f64);
    let degree_rank = percentile_rank(&degree_values, degree as f64);
    let cochange_rank = percentile_rank(&cochange_values, cochange_count as f64);
    let edge_share = normalized_u64(degree, snapshot.coupling.edges.len().max(1) as u64);

    let mut pressure = (degree_rank * 0.45)
        .max(fan_in_rank * 0.35 + fan_out_rank * 0.20)
        .max(cochange_rank * 0.45)
        + edge_share * 0.10
        + normalized_u64(cochange_count, 25) * 0.10;
    pressure = pressure.clamp(0.0, 1.0);

    if degree <= 1 && cochange_count <= 1 {
        pressure = pressure.min(0.25);
    } else if degree <= 3 && cochange_count <= 2 {
        pressure = pressure.min(0.55);
    }

    Some(pressure)
}

fn complexity_pressure_for_path(snapshot: &TuiSnapshot, path: &str) -> Option<f64> {
    Some((complexity_score_for_path(snapshot, path)? / 35.0).clamp(0.0, 1.0))
}

fn complexity_score_for_path(snapshot: &TuiSnapshot, path: &str) -> Option<f64> {
    let max_complexity = snapshot
        .complexity
        .symbols
        .iter()
        .filter(|symbol| symbol.path == path)
        .filter_map(|symbol| symbol.cyclomatic_complexity)
        .max()?;

    Some(max_complexity as f64)
}

fn normalized_u64(value: u64, saturation: u64) -> f64 {
    if saturation == 0 {
        return 0.0;
    }

    (value as f64 / saturation as f64).clamp(0.0, 1.0)
}

fn normalized_context_tokens(tokens: u64, large_threshold: u64) -> f64 {
    normalized_u64(tokens, large_threshold)
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

fn severity_for_complexity_score(score: f64) -> TuiSeverity {
    if score >= 20.0 {
        TuiSeverity::High
    } else if score >= 10.0 {
        TuiSeverity::Medium
    } else {
        TuiSeverity::Low
    }
}

fn coupling_severity_label(value: f64) -> &'static str {
    if value >= 0.90 {
        "EXTREME"
    } else if value >= 0.70 {
        "HIGH"
    } else if value >= 0.40 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

fn line_size_band(lines: u64) -> TuiSizeBand {
    match lines {
        0..=999 => TuiSizeBand::Small,
        1_000..=3_999 => TuiSizeBand::Medium,
        4_000..=15_999 => TuiSizeBand::Large,
        _ => TuiSizeBand::VeryLarge,
    }
}

fn format_compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
fn format_bytes(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "unknown".to_owned();
    };

    if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes == 1 {
        "1 byte".to_owned()
    } else {
        format!("{bytes} bytes")
    }
}

fn ownership_risk(hotspot: &ReportHotspot) -> f64 {
    hotspot
        .normalized_metrics
        .ownership
        .unwrap_or(0.0)
        .clamp(0.0, 1.0)
}

fn ownership_risk_label(hotspot: &ReportHotspot) -> &'static str {
    owner_risk_label(ownership_risk(hotspot))
}

fn owner_risk_label(value: f64) -> &'static str {
    if value >= 0.85 {
        "EXTREME"
    } else if value >= 0.60 {
        "HIGH"
    } else if value >= 0.30 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

fn ownership_shape_label(hotspot: &ReportHotspot) -> &'static str {
    let Some(concentration) = hotspot.raw_metrics.dominant_owner_share else {
        return "UNOBSERVED";
    };

    ownership_shape_label_for_share(concentration)
}

fn ownership_shape_label_for_share(concentration: f64) -> &'static str {
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

fn risk_driver_lines(
    snapshot: &TuiSnapshot,
    hotspot: &ReportHotspot,
    options: TuiOptions,
) -> Vec<Line<'static>> {
    let mut lines = qualified_driver_tags(snapshot, hotspot)
        .into_iter()
        .filter(|signal| signal.strength >= 0.60 || signal.percentile >= 0.75)
        .filter_map(|signal| {
            risk_driver_sentence_from_signal(signal.label)
                .map(|message| (message.to_owned(), signal.strength, signal.priority))
        })
        .collect::<Vec<_>>();
    if ownership_risk(hotspot) >= 0.60 {
        lines.push((
            "Concentrated ownership / low maintainer redundancy".to_owned(),
            ownership_risk(hotspot),
            80,
        ));
    }
    if fan_for_path(snapshot, &hotspot.path).is_some_and(|fan| fan.fan_out >= 10) {
        lines.push(("Central architectural dependency point".to_owned(), 1.0, 75));
    }
    if fan_for_path(snapshot, &hotspot.path).is_some_and(|fan| fan.fan_in >= 10) {
        lines.push(("Central architectural dependency point".to_owned(), 1.0, 95));
    }
    if complexity_pressure_for_path(snapshot, &hotspot.path).is_some_and(|value| value >= 0.50) {
        lines.push(("High structural or logical complexity".to_owned(), 1.0, 70));
    }
    if file_for_path(snapshot, &hotspot.path)
        .and_then(|file| file.byte_size)
        .is_some_and(|bytes| normalized_context_tokens(bytes.div_ceil(4), 100_000) >= 0.50)
    {
        lines.push((
            "Expensive to review or reason about due to scale".to_owned(),
            1.0,
            75,
        ));
    }
    lines.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| {
                right
                    .1
                    .partial_cmp(&left.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.0.cmp(&right.0))
    });
    lines.dedup_by(|left, right| left.0 == right.0);

    if lines.is_empty() {
        return Vec::new();
    }

    let mut rendered = vec![
        Line::raw(""),
        Line::styled(
            "Why This File Matters",
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
    ];
    rendered.extend(
        lines
            .into_iter()
            .take(4)
            .map(|(message, _, _)| Line::raw(format!("  - {message}"))),
    );

    rendered
}

fn risk_driver_sentence_from_signal(label: &str) -> Option<&'static str> {
    match label {
        "CORE" => Some("Central architectural dependency point"),
        "CHURN" => Some("Changes significantly more often than typical files"),
        "COUPLING" => Some("Frequently changes alongside related modules"),
        "FRAGILITY" => Some("Concentrated ownership / low maintainer redundancy"),
        "SIZE" => Some("Expensive to review or reason about due to scale"),
        "COMPLEXITY" => Some("High structural or logical complexity"),
        "VOLATILITY" => Some("Unstable recent modification activity"),
        _ => None,
    }
}

fn inspector_lines(
    snapshot: &TuiSnapshot,
    state: &TuiAppState,
    rows: &[DisplayRow],
    width: u16,
    options: TuiOptions,
) -> Vec<Line<'static>> {
    let selected_text = rows
        .get(state.selected_index())
        .map(|row| row.text.as_str());
    let selected_path = selected_text
        .and_then(|row| match state.current_view {
            TuiView::Hotspots => hotspot_path_from_row(snapshot, row),
            TuiView::RepoTree => repo_tree_path_from_row(snapshot, row),
            TuiView::CouplingGraph => {
                coupling_graph_path_from_row(snapshot, state.current_path.as_deref(), row)
            }
            TuiView::FileDetail
            | TuiView::SymbolDetail
            | TuiView::GitDetail
            | TuiView::ExplainScore => state.current_path.clone(),
            TuiView::ContextBudgeting => None,
        })
        .or_else(|| state.current_path.clone());

    match selected_path {
        Some(path) => file_inspector_lines(snapshot, &path, width, options),
        None => repo_inspector_lines(snapshot, options),
    }
}

fn file_inspector_lines(
    snapshot: &TuiSnapshot,
    path: &str,
    width: u16,
    options: TuiOptions,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        truncate_middle(path, width.saturating_sub(4) as usize),
        style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
    )];
    if let Some(hotspot) = hotspot_for_path(snapshot, path) {
        lines.push(Line::raw(""));
        lines.push(metric_bar_line(
            "RISK",
            hotspot.score,
            severity_label(hotspot.score).to_owned(),
            severity_for_score(hotspot.score),
            options,
        ));
        lines.push(metric_bar_line(
            "FRAGILITY",
            ownership_risk(hotspot),
            ownership_risk_label(hotspot).to_owned(),
            severity_for_score(ownership_risk(hotspot)),
            options,
        ));

        if let Some(coupling) = coupling_pressure_for_path(snapshot, path) {
            lines.push(metric_bar_line(
                "COUPLING",
                coupling,
                coupling_severity_label(coupling).to_owned(),
                severity_for_score(coupling),
                options,
            ));
        }
        if let Some(complexity) = complexity_score_for_path(snapshot, path) {
            lines.push(metric_bar_line(
                "COMPLEXITY",
                (complexity / 35.0).clamp(0.0, 1.0),
                complexity_severity_label(complexity).to_owned(),
                severity_for_complexity_score(complexity),
                options,
            ));
        }
        if let Some(line_count) = file_for_path(snapshot, path).and_then(|file| file.line_count) {
            let band = line_size_band(line_count);
            let size_text = context_pressure_for_path(snapshot, path).map_or_else(
                || {
                    format!(
                        "{} {} lines",
                        band.label(),
                        format_compact_count(line_count)
                    )
                },
                |(_context, tokens)| {
                    format!(
                        "{} {} lines · {} tokens",
                        band.label(),
                        format_compact_count(line_count),
                        format_compact_count(tokens)
                    )
                },
            );
            lines.push(metric_bar_line(
                "SIZE",
                band.bar_value(),
                size_text,
                band.severity(),
                options,
            ));
        }

        let tags = hotspot_tag_text(&inspector_driver_tags(snapshot, hotspot));
        if !tags.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(tags, style(options, TuiSeverity::Muted)));
        }

        lines.push(Line::raw(""));
        lines.push(section_divider(width, options));
        lines.extend(risk_driver_lines(snapshot, hotspot, options));
    } else {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "No hotspot score for this file",
            style(options, TuiSeverity::Muted),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(section_divider(width, options));
    lines.push(Line::raw(""));
    let ownership_title = hotspot_for_path(snapshot, path).map_or_else(
        || "Ownership".to_owned(),
        |hotspot| format!("Ownership ({})", ownership_shape_label(hotspot)),
    );
    lines.push(Line::styled(
        ownership_title,
        style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
    ));
    lines.extend(ownership_distribution_lines(snapshot, path, options));

    lines
}

fn ownership_distribution_lines(
    snapshot: &TuiSnapshot,
    path: &str,
    options: TuiOptions,
) -> Vec<Line<'static>> {
    let Some(ownership) = ownership_for_path(snapshot, path) else {
        return vec![Line::styled(
            "  unavailable",
            style(options, TuiSeverity::Muted),
        )];
    };
    if ownership.owners.is_empty() {
        return vec![Line::styled(
            "  unavailable",
            style(options, TuiSeverity::Muted),
        )];
    }

    let mut visible = Vec::new();
    let mut others_share = 0.0;
    let mut others_touches = 0;
    for owner in &ownership.owners {
        if visible.len() < 3 && owner.author != "others" {
            visible.push(owner);
        } else {
            others_share += owner.share;
            others_touches += owner.touch_count;
        }
    }

    let mut lines = visible
        .into_iter()
        .map(|owner| owner_share_line(&display_author_name(&owner.author), owner.share, options))
        .collect::<Vec<_>>();
    if others_touches > 0 && rounded_percent(others_share) >= 1.0 {
        lines.push(owner_share_line("others", others_share, options));
    }

    lines
}

fn rounded_percent(share: f64) -> f64 {
    (share * 100.0).round()
}

fn owner_share_line(author: &str, share: f64, options: TuiOptions) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(
                "  {:<OWNER_NAME_WIDTH$}",
                truncate_end(author, OWNER_NAME_WIDTH)
            ),
            style(options, TuiSeverity::Muted),
        ),
        Span::raw(format!(" {:>3.0}%", share * 100.0)),
    ])
}

fn display_author_name(author: &str) -> String {
    author
        .split_once(" <")
        .map(|(name, _email)| name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(author)
        .to_owned()
}

fn repo_inspector_lines(snapshot: &TuiSnapshot, options: TuiOptions) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            "Repository",
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(format!("Files: {}", snapshot.scan.summary.total_files)),
        Line::raw(format!(
            "Hotspots: {}",
            snapshot.report.summary.hotspot_count
        )),
        Line::raw(format!(
            "Context tokens: {}",
            snapshot.report.summary.context_estimated_tokens
        )),
        Line::raw(format!(
            "Dependency edges: {}",
            snapshot.coupling.edges.len()
        )),
        Line::raw(format!(
            "Symbols: {}",
            snapshot.symbols.summary.symbol_count
        )),
        Line::raw(""),
        Line::styled(
            "Scores are advisory and local.",
            style(options, TuiSeverity::Muted),
        ),
    ]
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect, options: TuiOptions) {
    let popup = centered_rect(72, 60, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::styled(
            "Hotpath keys",
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("j/k or arrows  move selection"),
        Line::raw("Enter or l      drill into selected row"),
        Line::raw("h or Esc        go back, then exit at root"),
        Line::raw("/               search current view"),
        Line::raw("1..4            Hotspots, Tree, Coupling, Context"),
        Line::raw("t/g/c/x         tree, graph, context, explain score"),
        Line::raw("Tab             cycle focus"),
        Line::raw("Ctrl-P          command palette"),
        Line::raw("e               resolve editor action"),
        Line::raw("q               quit"),
        Line::raw(""),
        Line::styled(
            "All analysis stays local and offline.",
            style(options, TuiSeverity::Muted),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Help", true, options)),
        popup,
    );
}

fn render_command_palette(frame: &mut Frame<'_>, area: Rect, options: TuiOptions) {
    let popup = centered_rect(64, 42, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::styled(
            "Command palette",
            style(options, TuiSeverity::Medium).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("1  Open hotspots"),
        Line::raw("2  Open repository tree"),
        Line::raw("3  Open coupling graph"),
        Line::raw("4  Open context budgeting"),
        Line::raw("/  Search current view"),
        Line::raw("Esc close"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Ctrl-P", true, options)),
        popup,
    );
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
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);

    horizontal[1]
}

fn panel_block<'a>(title: &'a str, focused: bool, options: TuiOptions) -> Block<'a> {
    let border_style = if focused {
        style(options, TuiSeverity::Medium)
    } else {
        style(options, TuiSeverity::Muted)
    };
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(if options.ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        })
        .border_style(border_style)
}

fn plain_panel_block(focused: bool, options: TuiOptions) -> Block<'static> {
    let border_style = if focused {
        style(options, TuiSeverity::Medium)
    } else {
        style(options, TuiSeverity::Muted)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(if options.ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        })
        .border_style(border_style)
}

fn style(options: TuiOptions, severity: TuiSeverity) -> Style {
    if options.no_color {
        return Style::default();
    }

    let color = match severity {
        TuiSeverity::High => Color::Red,
        TuiSeverity::Medium => Color::Yellow,
        TuiSeverity::Low => Color::Green,
        TuiSeverity::Neutral => Color::White,
        TuiSeverity::Muted => Color::DarkGray,
    };

    Style::default().fg(color)
}

fn selected_row_style(options: TuiOptions, severity: TuiSeverity) -> Style {
    let base = style(options, severity).add_modifier(Modifier::BOLD);
    if options.no_color {
        return base.add_modifier(Modifier::REVERSED);
    }

    base.bg(Color::Rgb(32, 32, 32))
}

fn selected_gap_style(options: TuiOptions) -> Style {
    if options.no_color {
        return Style::default().add_modifier(Modifier::REVERSED);
    }

    Style::default().bg(Color::Rgb(32, 32, 32))
}

fn marker_style(selected: bool, options: TuiOptions) -> Style {
    if !selected {
        return Style::default();
    }

    selected_row_style(options, TuiSeverity::Medium)
}

fn score_bar_parts(score: f64, width: usize, options: TuiOptions) -> (String, String) {
    let filled = ((score.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    let fill = if options.ascii { "=" } else { "■" };
    let empty = if options.ascii { "." } else { "□" };

    (
        fill.repeat(filled),
        empty.repeat(width.saturating_sub(filled)),
    )
}

fn score_bar_spans(
    score: f64,
    width: usize,
    active_style: Style,
    options: TuiOptions,
) -> Vec<Span<'static>> {
    score_bar_spans_with_inactive(
        score,
        width,
        active_style,
        inactive_bar_style(options),
        options,
    )
}

fn selected_score_bar_spans(
    score: f64,
    width: usize,
    active_style: Style,
    options: TuiOptions,
) -> Vec<Span<'static>> {
    score_bar_spans_with_inactive(
        score,
        width,
        active_style,
        selected_inactive_bar_style(options),
        options,
    )
}

fn score_bar_spans_with_inactive(
    score: f64,
    width: usize,
    active_style: Style,
    inactive_style: Style,
    options: TuiOptions,
) -> Vec<Span<'static>> {
    let (filled, empty) = score_bar_parts(score, width, options);
    vec![
        Span::styled(filled, active_style),
        Span::styled(empty, inactive_style),
    ]
}

fn inactive_bar_style(options: TuiOptions) -> Style {
    if options.no_color {
        return Style::default();
    }
    if options.ascii {
        return style(options, TuiSeverity::Muted);
    }

    Style::default().fg(Color::Rgb(82, 82, 82)).bg(Color::Black)
}

fn selected_inactive_bar_style(options: TuiOptions) -> Style {
    if options.no_color {
        return Style::default().add_modifier(Modifier::REVERSED);
    }
    if options.ascii {
        return selected_row_style(options, TuiSeverity::Muted);
    }

    Style::default()
        .fg(Color::Rgb(118, 118, 118))
        .bg(Color::Rgb(32, 32, 32))
}

fn short_commit(value: &str) -> String {
    value.chars().take(7).collect()
}

fn truncate_middle(value: &str, max: usize) -> String {
    let length = value.chars().count();
    if length <= max {
        return value.to_owned();
    }
    if max <= 1 {
        return "…".to_owned();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let left_len = (max - 1) / 2;
    let right_len = max - 1 - left_len;
    let left = value.chars().take(left_len).collect::<String>();
    let right = value
        .chars()
        .rev()
        .take(right_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();

    format!("{left}…{right}")
}

fn truncate_end(value: &str, max: usize) -> String {
    let length = value.chars().count();
    if length <= max {
        return value.to_owned();
    }
    if max <= 1 {
        return "…".to_owned();
    }

    let prefix = value.chars().take(max - 1).collect::<String>();

    format!("{prefix}…")
}

fn pad_truncated_end(value: &str, width: usize) -> String {
    let truncated = truncate_end(value, width);
    let padding = width.saturating_sub(truncated.chars().count());

    format!("{truncated}{}", " ".repeat(padding))
}

fn truncate_path_start(value: &str, max: usize) -> String {
    let length = value.chars().count();
    if length <= max {
        return value.to_owned();
    }
    if max <= 1 {
        return "…".to_owned();
    }

    let suffix = value
        .chars()
        .rev()
        .take(max - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();

    format!("…{suffix}")
}

fn pad_truncated_path(value: &str, width: usize) -> String {
    let truncated = truncate_path_start(value, width);
    let padding = width.saturating_sub(truncated.chars().count());

    format!("{truncated}{}", " ".repeat(padding))
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
    use crate::report::{
        ReportContext, ReportFindingLevel, ReportGitSummary, ReportSummary, REPORT_SCHEMA_VERSION,
    };
    use crate::scoring::{
        FormulaVersion, NormalizedMetric, NormalizedScoreMetrics, RawScoreMetrics, ScoreLimitation,
        WeightedTerm,
    };
    use crate::test_support::parse_import as import;
    use crate::{ContentKind, FileRecord, ParseFileRecord, ParseSymbolRecord, ScanReport};
    use ratatui::backend::TestBackend;

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

        assert_eq!(state.selected_index(), 0);
    }

    #[test]
    fn reducer_supports_modern_navigation_keys_and_overlays() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('?'), None);
        assert!(state.show_help());
        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);
        assert!(!state.show_help());

        reduce_test_key(&mut state, &snapshot, KeyCode::Tab, None);
        assert_eq!(state.pane_focus(), TuiPaneFocus::Inspector);
        reduce_test_key(&mut state, &snapshot, KeyCode::BackTab, None);
        assert_eq!(state.pane_focus(), TuiPaneFocus::Main);

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('2'), None);
        assert_eq!(state.current_view(), TuiView::RepoTree);
        reduce_test_key(&mut state, &snapshot, KeyCode::Char('1'), None);
        assert_eq!(state.current_view(), TuiView::Hotspots);
    }

    #[test]
    fn reducer_drills_down_and_escape_navigates_back() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::FileDetail);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
        assert_eq!(
            state.last_action(),
            Some(&TuiAction::DrillDown {
                from: TuiView::Hotspots,
                to: TuiView::FileDetail,
                path: "src/lib.rs".to_owned(),
            })
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert_eq!(state.current_view(), TuiView::Hotspots);
        assert_eq!(state.current_path(), None);
        assert!(!state.should_exit());
    }

    #[test]
    fn reducer_search_filters_current_view_and_escape_clears_search_first() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('/'), None);
        assert!(state.is_search_editing());
        for character in "lib".chars() {
            reduce_test_key(&mut state, &snapshot, KeyCode::Char(character), None);
        }

        assert_eq!(state.search_query(), Some("lib"));
        assert_eq!(
            filtered_visible_rows(&snapshot, &state),
            vec!["#1 src/lib.rs score 0.640"]
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);

        assert_eq!(state.search_query(), None);
        assert!(!state.is_search_editing());
        assert_eq!(state.current_view(), TuiView::Hotspots);
        assert!(!state.should_exit());
    }

    #[test]
    fn reducer_enter_confirms_search_and_keeps_filtered_rows_active() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('/'), None);
        for character in "lib".chars() {
            reduce_test_key(&mut state, &snapshot, KeyCode::Char(character), None);
        }
        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.search_query(), Some("lib"));
        assert!(!state.is_search_editing());
        assert_eq!(
            filtered_visible_rows(&snapshot, &state),
            vec!["#1 src/lib.rs score 0.640"]
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::FileDetail);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
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

        assert_eq!(state.current_view(), TuiView::Hotspots);
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
    fn responsive_layout_modes_match_terminal_widths() {
        assert_eq!(layout_mode(Rect::new(0, 0, 130, 36)), TuiLayoutMode::Wide);
        assert_eq!(layout_mode(Rect::new(0, 0, 100, 26)), TuiLayoutMode::Medium);
        assert_eq!(layout_mode(Rect::new(0, 0, 80, 24)), TuiLayoutMode::Narrow);
    }

    #[test]
    fn render_dashboard_contains_top_navigation_and_inspector() {
        let snapshot = test_snapshot();
        let state = TuiAppState::default();
        let backend = TestBackend::new(130, 36);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| render(frame, &snapshot, &state))
            .expect("render should succeed");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Hotpath"));
        assert!(rendered.contains("[1 Hotspots]  2 Repo Tree"));
        assert!(!rendered.contains("Navigate"));
        assert!(rendered.contains("Inspector"));
        assert!(rendered.contains("src/lib.rs"));
    }

    #[test]
    fn header_uses_human_readable_token_count() {
        let mut snapshot = test_snapshot();
        snapshot.report.summary.context_estimated_tokens = 32_400;
        let state = TuiAppState::default();
        let backend = TestBackend::new(130, 4);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| {
                render_header(
                    frame,
                    Rect::new(0, 0, 130, 4),
                    &snapshot,
                    &state,
                    TuiOptions::default(),
                )
            })
            .expect("render should succeed");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.starts_with("[1 Hotspots]"));
        assert!(rendered.contains("32.4k tokens"));
        assert!(!rendered.contains("32400 tokens"));
    }

    #[test]
    fn footer_hints_use_middle_dot_separators() {
        let state = TuiAppState::default();
        let backend = TestBackend::new(140, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| {
                render_footer(
                    frame,
                    Rect::new(0, 0, 140, 3),
                    &state,
                    TuiOptions::default(),
                )
            })
            .expect("render should succeed");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(&format!("move{HOTSPOT_TAG_SEPARATOR}Enter")));
        assert!(rendered.contains(&format!("help{HOTSPOT_TAG_SEPARATOR}Ctrl-P")));
        assert!(!rendered.contains("move  Enter"));
    }

    #[test]
    fn footer_shows_background_progress_while_analysis_runs() {
        let state = TuiAppState {
            status: Some("Repo tree".to_owned()),
            background_status: Some({
                let mut update = TuiProgressUpdate::measured(
                    "Git history",
                    "diffing reachable commits",
                    500,
                    1_000,
                    "commits",
                );
                update.rate = Some(TuiProgressRate {
                    completed_at_start: 0,
                    started_at: Instant::now() - Duration::from_secs(4),
                });
                update
            }),
            analysis_running: true,
            ..TuiAppState::default()
        };
        let now = state
            .background_status
            .as_ref()
            .and_then(|update| update.rate)
            .expect("progress rate exists")
            .started_at
            + Duration::from_secs(4);
        let text = progress_status_text_at(
            state.background_status.as_ref().expect("progress exists"),
            120,
            TuiOptions {
                ascii: true,
                ..TuiOptions::default()
            },
            now,
        );

        assert!(text.contains("Git history"));
        assert!(text.contains("[=====.....]"));
        assert!(text.contains("50%"));
        assert!(text.contains("500/1000 commits"));
        assert!(text.contains("125 commits/s"));
        assert!(text.contains(&format!(
            "commits{HOTSPOT_TAG_SEPARATOR}125 commits/s{HOTSPOT_TAG_SEPARATOR}diffing reachable commits"
        )));
        assert!(!text.contains("Repo tree"));
    }

    #[test]
    fn progress_emitter_throttles_small_repeated_updates() {
        let (sender, receiver) = mpsc::channel();
        let mut emitter = TuiProgressEmitter::new(sender);

        emitter.emit(TuiProgressUpdate::measured(
            "Git history",
            "a",
            1,
            100,
            "commits",
        ));
        emitter.emit(TuiProgressUpdate::measured(
            "Git history",
            "b",
            2,
            100,
            "commits",
        ));
        emitter.emit(TuiProgressUpdate::measured(
            "Git history",
            "c",
            3,
            100,
            "commits",
        ));
        emitter.emit(TuiProgressUpdate::measured(
            "Git history",
            "done",
            100,
            100,
            "commits",
        ));

        let messages = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[0],
            TuiWorkerMessage::Progress(TuiProgressUpdate { detail, .. }) if detail == "a"
        ));
        assert!(matches!(
            &messages[1],
            TuiWorkerMessage::Progress(TuiProgressUpdate { detail, .. }) if detail == "c"
        ));
        assert!(matches!(
            &messages[2],
            TuiWorkerMessage::Progress(TuiProgressUpdate { detail, .. }) if detail == "done"
        ));
    }

    #[test]
    fn hotspot_rows_render_fixed_path_and_score_bar() {
        let snapshot = test_snapshot();
        let state = TuiAppState::default();
        let row = display_rows(&snapshot, &state)
            .into_iter()
            .next()
            .expect("fixture should have a hotspot row");

        let line = render_display_row(&row, true, 80, TuiOptions::default());
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.starts_with("\u{258C} src/lib.rs"));
        assert!(!rendered.contains("#1"));
        assert!(rendered.contains("src/lib.rs"));
        assert!(!rendered.contains("hotspot"));
        assert!(rendered.contains("6.4"));
        assert!(!rendered.contains("CHURN"));
    }

    #[test]
    fn selected_hotspot_row_uses_continuous_background() {
        let row = DisplayHotspotRow {
            path: "src/lib.rs".to_owned(),
            score: 0.8,
            tags: vec!["CHURN".to_owned()],
        };

        let line = render_hotspot_display_row(&row, true, 80, TuiOptions::default());

        assert!(line
            .spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .all(|span| span.style.bg == Some(Color::Rgb(32, 32, 32))));
        assert_eq!(line.spans[0].content.as_ref(), "\u{258C} ");
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn hotspot_driver_tags_are_deterministic_and_suppress_weak_terms() {
        let snapshot = test_snapshot();
        let tags = hotspot_driver_tags(&snapshot, &snapshot.report.hotspots[0]);

        assert!(tags.is_empty());
    }

    #[test]
    fn hotspot_score_bars_start_in_the_same_column() {
        let short = DisplayHotspotRow {
            path: "a.rs".to_owned(),
            score: 0.6,
            tags: Vec::new(),
        };
        let long = DisplayHotspotRow {
            path: "src/deeply/nested/repository/file.rs".to_owned(),
            score: 0.7,
            tags: Vec::new(),
        };
        let docs = DisplayHotspotRow {
            path: "docs/json-schema.md".to_owned(),
            score: 0.25,
            tags: Vec::new(),
        };
        let short_line = render_hotspot_display_row(&short, true, 80, TuiOptions::default());
        let long_line = render_hotspot_display_row(&long, false, 80, TuiOptions::default());
        let docs_line = render_hotspot_display_row(&docs, false, 80, TuiOptions::default());
        let short_rendered = line_text(&short_line);
        let long_rendered = line_text(&long_line);
        let docs_rendered = line_text(&docs_line);
        let active_bar = score_bar_parts(1.0, 1, TuiOptions::default()).0;
        let active_bar = active_bar.chars().next().expect("bar glyph");

        assert_eq!(
            char_position(&short_rendered, active_bar),
            char_position(&long_rendered, active_bar),
            "score bars should share a stable column"
        );
        assert_eq!(
            char_position(&short_rendered, active_bar),
            char_position(&docs_rendered, active_bar),
            "docs paths should not shift the score bar"
        );
        assert!(short_rendered.starts_with("\u{258C} a.rs"));
        assert!(long_rendered.contains("file.rs"));
        assert!(docs_rendered.starts_with("  docs/json-schema.md"));
        assert!(!short_rendered.contains("#1"));
        assert!(!docs_rendered.contains("GROWTH"));
    }

    #[test]
    fn hotspot_rows_clip_tags_and_paths_without_moving_bars() {
        let high = DisplayHotspotRow {
            path: "src/very/long/storage/integration/module/file.rs".to_owned(),
            score: 0.82,
            tags: vec!["CHURN".to_owned(), "CORE".to_owned(), "SIZE".to_owned()],
        };
        let low = DisplayHotspotRow {
            path: "src/main.rs".to_owned(),
            score: 0.39,
            tags: Vec::new(),
        };
        let high_rendered = line_text(&render_hotspot_display_row(
            &high,
            true,
            54,
            TuiOptions::default(),
        ));
        let low_rendered = line_text(&render_hotspot_display_row(
            &low,
            false,
            54,
            TuiOptions::default(),
        ));
        let active_bar = score_bar_parts(1.0, 1, TuiOptions::default()).0;
        let active_bar = active_bar.chars().next().expect("bar glyph");

        assert_eq!(
            char_position(&high_rendered, active_bar),
            char_position(&low_rendered, active_bar)
        );
        assert!(high_rendered.contains("…"));
        assert!(!low_rendered.contains("CHURN"));
        assert!(high_rendered.chars().count() <= 54);
    }

    #[test]
    fn hotspot_tags_render_with_centered_separators_and_clip_in_place() {
        let row = DisplayHotspotRow {
            path: "src/lib.rs".to_owned(),
            score: 0.8,
            tags: vec![
                "CHURN".to_owned(),
                "SIZE".to_owned(),
                "VOLATILITY".to_owned(),
                "COUPLING".to_owned(),
            ],
        };

        let wide = line_text(&render_hotspot_display_row(
            &row,
            true,
            96,
            TuiOptions::default(),
        ));
        let narrow = line_text(&render_hotspot_display_row(
            &row,
            true,
            58,
            TuiOptions::default(),
        ));

        assert_eq!(
            hotspot_tag_text(&[
                "CHURN".to_owned(),
                "SIZE".to_owned(),
                "VOLATILITY".to_owned()
            ]),
            format!("CHURN{HOTSPOT_TAG_SEPARATOR}SIZE{HOTSPOT_TAG_SEPARATOR}VOLATILITY")
        );
        assert!(wide.contains(&format!("CHURN{HOTSPOT_TAG_SEPARATOR}SIZE")));
        assert!(!wide.contains("CHURN SIZE"));
        assert!(!wide.contains("COUPLING ·"));
        assert!(narrow.chars().count() <= 58);
        assert!(!narrow.contains("COUPLING"));
    }

    #[test]
    fn dense_hotspot_list_renders_without_empty_spacer_rows() {
        let mut snapshot = test_snapshot();
        let mut second = snapshot.report.hotspots[0].clone();
        second.rank = 2;
        second.path = "src/very/long/storage/integration/module/file.rs".to_owned();
        second.score = 0.72;
        second.raw_metrics.path = second.path.clone();
        let mut third = snapshot.report.hotspots[0].clone();
        third.rank = 3;
        third.path = "docs/json-schema.md".to_owned();
        third.score = 0.25;
        third.raw_metrics.path = third.path.clone();
        snapshot.report.hotspots = vec![snapshot.report.hotspots[0].clone(), second, third];
        snapshot.report.summary.hotspot_count = 3;
        snapshot.scan.files.push(file_record(
            "src/very/long/storage/integration/module/file.rs",
        ));
        snapshot.scan.files.push(file_record("docs/json-schema.md"));

        let state = TuiAppState::default();
        let rows = display_rows(&snapshot, &state);
        let backend = TestBackend::new(82, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| {
                render_main_panel(
                    frame,
                    Rect::new(0, 0, 82, 12),
                    &snapshot,
                    &state,
                    &rows,
                    TuiOptions::default(),
                )
            })
            .expect("render should succeed");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let header = buffer_line_text(&terminal, 4, 82);
        let first = buffer_line_text(&terminal, 5, 82);
        let second = buffer_line_text(&terminal, 6, 82);
        let third = buffer_line_text(&terminal, 7, 82);

        assert!(header.contains("Path"));
        assert!(header.contains("Risk"));
        assert!(header.contains("Top Factor"));
        assert!(!rendered.contains("Coupling"));
        assert!(first.contains("src/lib.rs"));
        assert!(second.contains("file.rs"));
        assert!(third.contains("docs/json-schema.md"));
        assert!(!first.contains("#1"));
        assert!(!second.contains("#2"));
        assert!(!third.contains("#3"));
        assert!(!first.trim().is_empty());
        assert!(!second.trim().is_empty());
        assert!(!third.trim().is_empty());
    }

    #[test]
    fn generated_and_lockfile_hotspots_are_suppressed_by_default_policy() {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("src/lib.rs"), file_record("Cargo.lock")],
        );
        let mut report = report_with_hotspots(&scan);
        let mut lockfile = report.hotspots[0].clone();
        lockfile.rank = 2;
        lockfile.path = "Cargo.lock".to_owned();
        lockfile.raw_metrics.path = "Cargo.lock".to_owned();
        report.hotspots.push(lockfile);
        report.summary.hotspot_count = 2;

        suppress_generated_hotspots(&scan.files, &mut report);

        assert_eq!(
            report
                .hotspots
                .iter()
                .map(|hotspot| hotspot.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
        assert_eq!(report.summary.hotspot_count, 1);
    }

    #[test]
    fn ownership_bar_represents_operational_risk_not_health() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots[0].raw_metrics.dominant_owner_share = Some(1.0);
        snapshot.report.hotspots[0].normalized_metrics.ownership = Some(0.0);

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("FRAGILITY"));
        assert!(text.contains("Ownership (single owner)"));
        assert!(!text.contains("Ownership    SINGLE OWNER"));
        assert!(!text.contains("Owner Risk"));
        assert!(text.contains("LOW"));
        let ownership_line = text
            .lines()
            .find(|line| line.starts_with("Ownership"))
            .expect("ownership section title should render");
        assert!(!ownership_line.contains("■"));
    }

    #[test]
    fn inspector_includes_concise_risk_driver_section() {
        let snapshot = test_snapshot();
        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("Why This File Matters"));
        assert!(text.contains("  - Changes significantly more often than typical files"));
        assert!(!text.contains("Key Signals"));
        assert!(!text.contains("Structure"));
        assert!(!text.contains("  Formula"));
        assert!(!text.contains("  Rank"));
    }

    #[test]
    fn low_owner_risk_does_not_emit_strong_ownership_driver() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots[0].raw_metrics.dominant_owner_share = Some(1.0);
        snapshot.report.hotspots[0].normalized_metrics.ownership = Some(0.10);

        let text = lines_text(&risk_driver_lines(
            &snapshot,
            &snapshot.report.hotspots[0],
            TuiOptions::default(),
        ));

        assert!(!text.contains("Concentrated ownership / low maintainer redundancy"));
    }

    #[test]
    fn high_owner_risk_emits_contextual_ownership_driver() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots[0].raw_metrics.dominant_owner_share = Some(1.0);
        snapshot.report.hotspots[0].normalized_metrics.ownership = Some(0.85);

        let text = lines_text(&risk_driver_lines(
            &snapshot,
            &snapshot.report.hotspots[0],
            TuiOptions::default(),
        ));

        assert!(text.contains("Concentrated ownership / low maintainer redundancy"));
    }

    #[test]
    fn inspector_uses_operational_metric_labels() {
        let snapshot = test_snapshot();
        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("Ownership"));
        assert!(text.contains("Ownership (shared)"));
        assert!(text.contains("SIZE"));
        assert!(!text.lines().any(|line| line.starts_with("Context")));
        assert!(!text.contains("Key Signals"));
        assert!(!text.contains("Structure"));
        assert!(!text.contains("Lifetime Churn"));
        assert!(!text.contains("90 days Churn"));
        assert!(!text.contains("Bytes"));
        assert!(!text.contains("90d Churn"));
        assert!(!text.contains("Recent"));
        assert!(!text.contains("Fan-in"));
        assert!(!text.contains("Fan-out"));
    }

    #[test]
    fn inspector_renders_ownership_distribution_in_descending_order() {
        let mut snapshot = test_snapshot();
        snapshot.ownership = TuiOwnershipSnapshot {
            by_file: vec![TuiFileOwnership {
                path: "src/lib.rs".to_owned(),
                owners: vec![
                    owner_share("alice <alice@example.invalid>", 91, 0.91),
                    owner_share("bob <bob@example.invalid>", 7, 0.07),
                    owner_share("carol <carol@example.invalid>", 2, 0.02),
                ],
            }],
        };

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        let alice = text.find("alice").expect("alice should render");
        let bob = text.find("bob").expect("bob should render");
        let carol = text.find("carol").expect("carol should render");
        assert!(alice < bob && bob < carol);
        assert!(text.contains("alice                     91%"));
        assert!(text.contains("bob                        7%"));
        assert!(text.contains("carol                      2%"));
    }

    #[test]
    fn inspector_collapses_long_author_lists_into_others() {
        let mut snapshot = test_snapshot();
        snapshot.ownership = TuiOwnershipSnapshot {
            by_file: vec![TuiFileOwnership {
                path: "src/lib.rs".to_owned(),
                owners: vec![
                    owner_share("alice <alice@example.invalid>", 40, 0.40),
                    owner_share("bob <bob@example.invalid>", 25, 0.25),
                    owner_share("carol <carol@example.invalid>", 20, 0.20),
                    owner_share("dana <dana@example.invalid>", 10, 0.10),
                    owner_share("erin <erin@example.invalid>", 1, 0.01),
                    owner_share("frank <frank@example.invalid>", 1, 0.01),
                    owner_share("grace <grace@example.invalid>", 1, 0.01),
                ],
            }],
        };

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("others                    13%"));
        assert!(!text.contains("erin"));
        assert!(!text.contains("frank"));
        assert!(!text.contains("grace"));
    }

    #[test]
    fn hotspot_tags_are_selective_by_risk_band() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots = vec![
            hotspot_fixture(1, "src/high.rs", 0.80, 0.90, 0.89, 0.88),
            hotspot_fixture(2, "src/medium.rs", 0.55, 0.86, 0.80, 0.20),
            hotspot_fixture(3, "src/low.rs", 0.30, 0.95, 0.95, 0.95),
        ];

        assert_eq!(
            hotspot_driver_tags(&snapshot, &snapshot.report.hotspots[0]),
            vec!["CHURN"]
        );
        assert!(hotspot_driver_tags(&snapshot, &snapshot.report.hotspots[1]).is_empty());
        assert!(hotspot_driver_tags(&snapshot, &snapshot.report.hotspots[2]).is_empty());
    }

    #[test]
    fn exceptional_hotspot_rows_show_only_one_tag() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots = vec![hotspot_fixture(
            1,
            "src/critical.rs",
            0.90,
            0.95,
            0.92,
            0.91,
        )];

        assert_eq!(
            hotspot_driver_tags(&snapshot, &snapshot.report.hotspots[0]),
            vec!["CHURN"]
        );
    }

    #[test]
    fn inspector_headline_renders_coupling_and_complexity_severity() {
        let mut snapshot = coupling_snapshot();
        snapshot.report.hotspots[0].score = 0.77;
        snapshot.report.hotspots[0].path = "src/lib.rs".to_owned();
        snapshot.report.hotspots[0].raw_metrics.path = "src/lib.rs".to_owned();
        snapshot.complexity.symbols = vec![ComplexitySymbolRecord {
            path: "src/lib.rs".to_owned(),
            name: "hard".to_owned(),
            kind: "function".to_owned(),
            start_line: 1,
            end_line: 10,
            length_lines: 10,
            function_length_lines: Some(10),
            nesting_depth: 0,
            cyclomatic_complexity: Some(18),
            max_control_flow_nesting: Some(2),
            is_large_symbol: false,
        }];

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("COUPLING"));
        assert!(text.contains("COMPLEXITY"));
        assert!(text.contains("MEDIUM"));
        assert!(!text.contains("18.0 MEDIUM"));
    }

    #[test]
    fn coupling_pressure_uses_percentile_pressure_not_raw_count_linear_mapping() {
        let mut snapshot = test_snapshot();
        snapshot.coupling.fan_by_file = vec![
            TuiFileFan {
                path: "src/lib.rs".to_owned(),
                fan_in: 31,
                fan_out: 12,
            },
            TuiFileFan {
                path: "src/main.rs".to_owned(),
                fan_in: 1,
                fan_out: 0,
            },
            TuiFileFan {
                path: "src/leaf.rs".to_owned(),
                fan_in: 0,
                fan_out: 1,
            },
        ];
        snapshot.report.hotspots[0]
            .raw_metrics
            .co_changed_file_count = Some(20);

        let hub = coupling_pressure_for_path(&snapshot, "src/lib.rs")
            .expect("hub should have coupling pressure");
        let leaf = coupling_pressure_for_path(&snapshot, "src/leaf.rs")
            .expect("leaf should have coupling pressure");

        assert!(hub >= 0.70, "expected hub pressure to be high, got {hub}");
        assert!(
            leaf <= 0.25,
            "expected leaf pressure to stay low, got {leaf}"
        );
    }

    #[test]
    fn narrow_width_inspector_keeps_headline_bars_aligned() {
        let mut snapshot = test_snapshot();
        if let Some(file) = snapshot
            .scan
            .files
            .iter_mut()
            .find(|file| file.path == "src/lib.rs")
        {
            file.byte_size = Some(200_000);
        }
        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            42,
            TuiOptions::default(),
        ));
        let filled = score_bar_parts(1.0, 1, TuiOptions::default()).0;
        let glyph = filled.chars().next().expect("bar glyph");
        let columns = text
            .lines()
            .filter(|line| {
                line.starts_with("RISK")
                    || line.starts_with("FRAGILITY")
                    || line.starts_with("SIZE")
            })
            .filter_map(|line| char_position(line, glyph))
            .collect::<Vec<_>>();

        assert!(columns.len() >= 3);
        assert!(columns.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn inspector_tags_use_unicode_middle_dot_separator() {
        let mut snapshot = test_snapshot();
        snapshot.report.hotspots = vec![hotspot_fixture(1, "src/lib.rs", 0.90, 0.95, 0.92, 0.91)];

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains(&format!(
            "CHURN{HOTSPOT_TAG_SEPARATOR}SIZE{HOTSPOT_TAG_SEPARATOR}VOLATILITY"
        )));
    }

    #[test]
    fn inspector_tags_show_top_three_by_severity() {
        let mut snapshot = test_snapshot();
        let mut hotspot = hotspot_fixture(1, "src/lib.rs", 0.90, 0.95, 0.95, 0.95);
        hotspot.weighted_terms.push(weighted_term(
            "coupling_score",
            NormalizedMetric::Coupling,
            0.95,
            0.20,
        ));
        snapshot.report.hotspots = vec![hotspot];

        assert_eq!(
            inspector_driver_tags(&snapshot, &snapshot.report.hotspots[0]),
            vec!["CHURN", "COUPLING", "SIZE"]
        );
    }

    #[test]
    fn context_pressure_normalization_preserves_relative_scale() {
        assert!(normalized_context_tokens(32_000, 100_000) < 0.50);
        assert!(normalized_context_tokens(282_000, 500_000) < 0.70);
    }

    #[test]
    fn structure_size_uses_human_readable_units() {
        assert_eq!(format_bytes(Some(1)), "1 byte");
        assert_eq!(format_bytes(Some(166_687)), "162.8 KiB");
        assert_eq!(format_bytes(Some(2_097_152)), "2.0 MiB");
        assert_eq!(format_bytes(None), "unknown");
    }

    #[test]
    fn inspector_context_and_size_header_labels_use_operational_bands() {
        assert_eq!(line_size_band(999).label(), "SMALL");
        assert_eq!(line_size_band(3_700).label(), "MEDIUM");
        assert_eq!(line_size_band(4_000).label(), "LARGE");
        assert_eq!(line_size_band(16_000).label(), "VERY LARGE");
        assert_eq!(format_compact_count(32_200), "32.2k");
    }

    #[test]
    fn context_and_size_header_bars_use_their_size_band_severity() {
        let size = line_size_band(3_700);
        assert_eq!(size.label(), "MEDIUM");
        assert_eq!(size.bar_value(), 0.40);
        assert_eq!(size.severity(), TuiSeverity::Medium);
    }

    #[test]
    fn inspector_size_row_includes_context_tokens_when_available() {
        let mut snapshot = test_snapshot();
        if let Some(file) = snapshot
            .scan
            .files
            .iter_mut()
            .find(|file| file.path == "src/lib.rs")
        {
            file.line_count = Some(3_800);
            file.byte_size = Some(129_600);
        }

        let text = lines_text(&file_inspector_lines(
            &snapshot,
            "src/lib.rs",
            80,
            TuiOptions::default(),
        ));

        assert!(text.contains("SIZE"));
        assert!(text.contains("MEDIUM 3.8k lines · 32.4k tokens"));
        assert!(!text.lines().any(|line| line.starts_with("Context")));
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
                row_text: "#1 src/lib.rs score 0.640".to_owned(),
            })
        );
    }

    #[test]
    fn reducer_explain_score_records_selected_row() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('x'), None);

        assert_eq!(
            state.last_action(),
            Some(&TuiAction::ExplainScore {
                view: TuiView::Hotspots,
                path: "src/lib.rs".to_owned(),
            })
        );
        assert_eq!(state.current_view(), TuiView::ExplainScore);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
    }

    #[test]
    fn hotspots_view_is_root_and_uses_ranked_hotspot_rows() {
        let snapshot = test_snapshot();
        let state = TuiAppState::default();

        assert_eq!(state.current_view(), TuiView::Hotspots);
        assert_eq!(
            filtered_visible_rows(&snapshot, &state),
            vec!["#1 src/lib.rs score 0.640"]
        );
    }

    #[test]
    fn file_detail_rows_include_scan_score_metrics_limitations_and_symbols() {
        let snapshot = test_snapshot();
        let rows = file_detail_rows(&snapshot, "src/lib.rs");

        assert!(rows.contains(&"File: src/lib.rs".to_owned()));
        assert!(rows.contains(&"Language: Rust".to_owned()));
        assert!(rows.contains(&"Hotspot score: 0.640".to_owned()));
        assert!(rows.contains(&"Metric: commits 7".to_owned()));
        assert!(rows.contains(&"Metric: dominant ownership 57.0%".to_owned()));
        assert!(rows.contains(&"Limitation: test.limit - fixture limitation".to_owned()));
        assert!(rows.contains(&"Symbol: function run lines 1-1".to_owned()));
    }

    #[test]
    fn file_detail_enter_on_git_row_opens_git_detail_without_self_history() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);
        while filtered_visible_rows(&snapshot, &state)[state.selected_index()]
            != "Git detail: press Enter"
        {
            reduce_test_key(&mut state, &snapshot, KeyCode::Char('j'), None);
        }
        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::GitDetail);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
        assert_eq!(
            state.last_action(),
            Some(&TuiAction::DrillDown {
                from: TuiView::FileDetail,
                to: TuiView::GitDetail,
                path: "src/lib.rs".to_owned(),
            })
        );

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);
        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);
        assert_eq!(state.current_view(), TuiView::FileDetail);
    }

    #[test]
    fn git_detail_rows_use_existing_raw_score_metrics() {
        let snapshot = test_snapshot();

        assert_eq!(
            git_detail_rows(&snapshot, "src/lib.rs"),
            vec![
                "File: src/lib.rs",
                "Commits: 7",
                "Total churn lines: 120",
                "Contributors: 3",
                "Owners: 3",
                "Dominant ownership: 57.0%",
                "Co-changed file count: 4",
            ]
        );
    }

    #[test]
    fn explain_score_rows_include_formula_and_weighted_terms() {
        let snapshot = test_snapshot();
        let rows = explain_score_rows(&snapshot, "src/lib.rs");

        assert!(rows.contains(&"Formula: hotpath.score.v3".to_owned()));
        assert!(rows.contains(&"Formula version: 3.0".to_owned()));
        assert!(rows
            .contains(&"Term: churn_score weight 0.35 input 0.600 contribution 0.210".to_owned()));
        assert!(rows.contains(&"Limitation: test.limit - fixture limitation".to_owned()));
    }

    #[test]
    fn coupling_graph_file_rows_include_fan_edges_and_empty_states() {
        let snapshot = coupling_snapshot();

        assert_eq!(
            coupling_graph_rows(&snapshot, Some("src/lib.rs")),
            vec![
                "File: src/lib.rs",
                "Matched current file: true",
                "Coupling: 1 dependencies, 0 dependents",
                "Incoming edges:",
                "Incoming: none",
                "Outgoing edges:",
                "Outgoing: src/lib.rs -> src/child.rs (mod)",
            ]
        );
        assert_eq!(
            coupling_graph_rows(&snapshot, Some("src/child.rs")),
            vec![
                "File: src/child.rs",
                "Matched current file: true",
                "Coupling: 0 dependencies, 1 dependents",
                "Incoming edges:",
                "Incoming: src/lib.rs -> src/child.rs (mod)",
                "Outgoing edges:",
                "Outgoing: none",
            ]
        );
    }

    #[test]
    fn coupling_graph_overview_rows_use_deterministic_current_file_fan_rows() {
        let snapshot = coupling_snapshot();

        assert_eq!(
            coupling_graph_rows(&snapshot, None),
            vec![
                "Coupling graph: 1 resolved dependency edges",
                "Files by coupling:",
                "File: src/child.rs dependencies 0 dependents 1",
                "File: src/lib.rs dependencies 1 dependents 0",
            ]
        );
    }

    #[test]
    fn coupling_graph_edge_rows_resolve_the_neighbor_endpoint_exactly() {
        let snapshot = coupling_snapshot();

        assert_eq!(
            coupling_graph_path_from_row(
                &snapshot,
                Some("src/lib.rs"),
                "Outgoing: src/lib.rs -> src/child.rs (mod)",
            ),
            Some("src/child.rs".to_owned())
        );
        assert_eq!(
            coupling_graph_path_from_row(
                &snapshot,
                Some("src/child.rs"),
                "Incoming: src/lib.rs -> src/child.rs (mod)",
            ),
            Some("src/lib.rs".to_owned())
        );
        assert_eq!(
            coupling_graph_path_from_row(
                &snapshot,
                None,
                "Outgoing: src/lib.rs -> src/child.rsx (mod)",
            ),
            None
        );
    }

    #[test]
    fn context_budgeting_rows_include_summary_budget_groups_skips_and_notes() {
        let mut snapshot = test_snapshot();
        snapshot.report.context.summary = ContextSummary {
            total_files: 3,
            included_files: 2,
            skipped_files: 1,
            estimated_tokens: 9,
            included_bytes: 36,
            filtered_generated_files: 0,
            filtered_vendor_files: 0,
        };
        snapshot.report.context.groups = vec![crate::ContextGroupRow {
            path: "src".to_owned(),
            file_count: 2,
            byte_size: 36,
            estimated_tokens: 9,
        }];
        snapshot.report.context.skipped = vec![ContextSkippedRow {
            path: "target/cache.bin".to_owned(),
            reason: ContextSkippedReason::Binary,
        }];
        snapshot.report.context.budget = Some(ContextBudgetStatus {
            budget_tokens: 8,
            estimated_tokens: 9,
            remaining_tokens: None,
            over_budget_tokens: Some(1),
        });

        let rows = context_budgeting_rows(&snapshot);

        assert!(rows.contains(&"Total estimated tokens: 9".to_owned()));
        assert!(rows.contains(&"Included files: 2".to_owned()));
        assert!(rows.contains(&"Skipped files: 1".to_owned()));
        assert!(rows.contains(&"Included bytes: 36".to_owned()));
        assert!(
            rows.contains(&"Budget: over budget by 1 tokens (budget 8, estimated 9)".to_owned())
        );
        assert!(rows.contains(&"Group: src tokens 9 bytes 36 files 2".to_owned()));
        assert!(rows.contains(&"Skipped: target/cache.bin (binary)".to_owned()));
        assert!(rows.contains(
            &"Approximation: estimated tokens = ceil(byte_size / 4) for UTF-8 text files"
                .to_owned()
        ));
    }

    #[test]
    fn context_budgeting_rows_include_all_groups_and_skips() {
        let mut snapshot = test_snapshot();
        snapshot.report.context.groups = (0..6)
            .map(|index| crate::ContextGroupRow {
                path: format!("group{index}"),
                file_count: 1,
                byte_size: 4,
                estimated_tokens: 1,
            })
            .collect();
        snapshot.report.context.skipped = (0..6)
            .map(|index| ContextSkippedRow {
                path: format!("skip{index}.bin"),
                reason: ContextSkippedReason::Binary,
            })
            .collect();

        let rows = context_budgeting_rows(&snapshot);

        assert!(rows.contains(&"Group: group5 tokens 1 bytes 4 files 1".to_owned()));
        assert!(rows.contains(&"Skipped: skip5.bin (binary)".to_owned()));
    }

    #[test]
    fn context_budgeting_rows_report_empty_groups_and_skips() {
        let snapshot = test_snapshot();
        let rows = context_budgeting_rows(&snapshot);

        assert!(rows.contains(&"Budget: none configured".to_owned()));
        assert!(rows.contains(&"Group: none".to_owned()));
        assert!(rows.contains(&"Skipped: src/z.rs (unreadable)".to_owned()));
    }

    #[test]
    fn reducer_routes_to_coupling_graph_and_context_budgeting() {
        let snapshot = coupling_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('g'), None);

        assert_eq!(state.current_view(), TuiView::CouplingGraph);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
        assert!(filtered_visible_rows(&snapshot, &state)
            .contains(&"Outgoing: src/lib.rs -> src/child.rs (mod)".to_owned()));

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('c'), None);

        assert_eq!(state.current_view(), TuiView::ContextBudgeting);
        assert_eq!(state.current_path(), None);
        assert!(filtered_visible_rows(&snapshot, &state)
            .contains(&"Total estimated tokens: 0".to_owned()));
    }

    #[test]
    fn all_milestone_views_have_selection_state_and_titles() {
        let state = TuiAppState::default();
        let milestone_views = [
            TuiView::Hotspots,
            TuiView::RepoTree,
            TuiView::FileDetail,
            TuiView::SymbolDetail,
            TuiView::GitDetail,
            TuiView::CouplingGraph,
            TuiView::ContextBudgeting,
            TuiView::ExplainScore,
        ];

        assert_eq!(
            milestone_views.map(TuiView::title),
            [
                "Hotspots",
                "Repo Tree",
                "File Detail",
                "Symbol Detail",
                "Git Detail",
                "Coupling Graph",
                "Context Budgeting",
                "Explain Score",
            ]
        );
        assert!(milestone_views
            .iter()
            .all(|view| state.selections.contains_key(view)));
    }

    #[test]
    fn repo_tree_rows_use_deterministic_directory_then_file_ordering() {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![
                file_record("zeta.rs"),
                file_record("src/z.rs"),
                file_record("README.md"),
                file_record("src/bin/main.rs"),
                file_record("src/a.rs"),
                file_record("docs/guide.md"),
            ],
        );
        let parse = parse::scaffold_report_from_scan(&scan);
        let complexity = complexity::report_from_parse(&parse);
        let snapshot = TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity);

        let rows = repo_tree_rows(&snapshot)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            vec![
                "[dir] docs/",
                "  [file] docs/guide.md",
                "[dir] src/",
                "  [dir] bin/",
                "    [file] src/bin/main.rs",
                "  [file] src/a.rs",
                "  [file] src/z.rs",
                "[file] README.md",
                "[file] zeta.rs",
            ]
        );
    }

    #[test]
    fn repo_tree_rows_handle_deep_paths_iteratively() {
        let depth = 2_000;
        let path = (0..depth)
            .map(|index| format!("d{index}"))
            .chain(std::iter::once("leaf.rs".to_owned()))
            .collect::<Vec<_>>()
            .join("/");
        let scan = ScanReport::from_parts(Vec::new(), vec![file_record(&path)]);
        let parse = parse::scaffold_report_from_scan(&scan);
        let complexity = complexity::report_from_parse(&parse);
        let snapshot = TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity);

        let rows = repo_tree_rows(&snapshot);

        assert_eq!(rows.len(), depth + 1);
        assert_eq!(
            rows.last().map(|row| row.path.as_str()),
            Some(path.as_str())
        );
        assert!(rows.last().is_some_and(|row| row
            .text
            .starts_with(&format!("{}[file]", "  ".repeat(depth)))));
    }

    #[test]
    fn reducer_opens_repo_tree_and_drills_file_rows_to_file_detail() {
        let snapshot = test_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Char('t'), None);
        select_visible_row(&mut state, &snapshot, "  [file] src/main.rs");
        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::FileDetail);
        assert_eq!(state.current_path(), Some("src/main.rs"));
        assert_eq!(
            state.last_action(),
            Some(&TuiAction::DrillDown {
                from: TuiView::RepoTree,
                to: TuiView::FileDetail,
                path: "src/main.rs".to_owned(),
            })
        );
    }

    #[test]
    fn repo_tree_file_rows_include_relative_paths_to_avoid_ambiguous_basenames() {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("app/main.rs"), file_record("src/main.rs")],
        );
        let parse = parse::scaffold_report_from_scan(&scan);
        let complexity = complexity::report_from_parse(&parse);
        let snapshot = TuiSnapshot::from_parts(empty_report(&scan), scan, parse, complexity);

        let rows = repo_tree_rows(&snapshot)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();

        assert!(rows.contains(&"  [file] app/main.rs".to_owned()));
        assert!(rows.contains(&"  [file] src/main.rs".to_owned()));
        assert_eq!(
            repo_tree_path_from_row(&snapshot, "  [file] src/main.rs"),
            Some("src/main.rs".to_owned())
        );
    }

    #[test]
    fn symbol_detail_rows_include_parser_and_complexity_facts() {
        let snapshot = symbol_detail_snapshot();
        let key = SymbolKey::from_parse(&snapshot.symbols.symbols[0]);
        let rows = symbol_detail_rows(&snapshot, &key);

        assert!(rows.contains(&"Symbol: function render".to_owned()));
        assert!(rows.contains(&"Kind: function".to_owned()));
        assert!(rows.contains(&"Range: lines 10-92".to_owned()));
        assert!(rows.contains(&"Parent: impl Widget".to_owned()));
        assert!(rows.contains(&"Nesting depth: 1".to_owned()));
        assert!(rows.contains(&"Length lines: 83".to_owned()));
        assert!(rows.contains(&"Function length lines: 83".to_owned()));
        assert!(rows.contains(&"Cyclomatic complexity: 8".to_owned()));
        assert!(rows.contains(&"Max control flow nesting: 3".to_owned()));
        assert!(rows.contains(&"Large symbol: true".to_owned()));
    }

    #[test]
    fn file_detail_symbol_row_drills_to_symbol_detail_and_back() {
        let snapshot = symbol_detail_snapshot();
        let mut state = TuiAppState::default();

        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);
        select_visible_row(&mut state, &snapshot, "Symbol: function render lines 10-92");
        reduce_test_key(&mut state, &snapshot, KeyCode::Enter, None);

        assert_eq!(state.current_view(), TuiView::SymbolDetail);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
        assert_eq!(
            state.last_action(),
            Some(&TuiAction::DrillDown {
                from: TuiView::FileDetail,
                to: TuiView::SymbolDetail,
                path: "src/lib.rs".to_owned(),
            })
        );
        assert!(filtered_visible_rows(&snapshot, &state).contains(&"Large symbol: true".to_owned()));

        reduce_test_key(&mut state, &snapshot, KeyCode::Esc, None);
        assert_eq!(state.current_view(), TuiView::FileDetail);
        assert_eq!(state.current_path(), Some("src/lib.rs"));
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn lines_text(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn buffer_line_text(terminal: &Terminal<TestBackend>, y: usize, width: usize) -> String {
        let content = terminal.backend().buffer().content();
        content[y * width..(y + 1) * width]
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn select_visible_row(state: &mut TuiAppState, snapshot: &TuiSnapshot, row_text: &str) {
        let rows = filtered_visible_rows(snapshot, state);
        state.selection_for_current_view_mut().selected = rows
            .iter()
            .position(|row| row == row_text)
            .unwrap_or_else(|| panic!("expected visible row {row_text:?}, got {rows:?}"));
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
        let report = report_with_hotspots(&scan);

        TuiSnapshot::from_parts(report, scan, parse, complexity)
    }

    fn symbol_detail_snapshot() -> TuiSnapshot {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("src/lib.rs"), file_record("src/main.rs")],
        );
        let parse = ParseReport {
            warnings: Vec::new(),
            files: vec![parse_file("src/lib.rs"), parse_file("src/main.rs")],
            symbols: vec![detailed_symbol()],
            imports: Vec::new(),
        };
        let complexity = complexity::report_from_parse(&parse);
        let report = report_with_hotspots(&scan);

        TuiSnapshot::from_parts(report, scan, parse, complexity)
    }

    fn coupling_snapshot() -> TuiSnapshot {
        let scan = ScanReport::from_parts(
            Vec::new(),
            vec![file_record("src/child.rs"), file_record("src/lib.rs")],
        );
        let parse = ParseReport {
            warnings: Vec::new(),
            files: vec![parse_file("src/child.rs"), parse_file("src/lib.rs")],
            symbols: Vec::new(),
            imports: vec![import("src/lib.rs", "child", "mod")],
        };
        let complexity = complexity::report_from_parse(&parse);
        let report = report_with_hotspots(&scan);

        TuiSnapshot::from_parts(report, scan, parse, complexity)
    }

    fn report_with_hotspots(scan: &ScanReport) -> Report {
        let mut report = empty_report(scan);
        report.summary.hotspot_count = 1;
        report.summary.git.file_metric_count = 1;
        report.summary.git.co_change_count = 3;
        report.hotspots = vec![ReportHotspot {
            rank: 1,
            path: "src/lib.rs".to_owned(),
            score: 0.64,
            formula_version: FormulaVersion::current(),
            raw_metrics: RawScoreMetrics {
                path: "src/lib.rs".to_owned(),
                byte_size: Some(10),
                line_count: Some(1),
                commits_per_file: Some(7),
                total_churn_lines: Some(120),
                recent_churn_lines: Some(30),
                author_count: Some(3),
                owner_count: Some(3),
                dominant_owner_share: Some(0.57),
                co_changed_file_count: Some(4),
                file_age_days: Some(365),
                repository_age_days: Some(730),
                repository_author_count: Some(10),
                repository_file_count: Some(200),
            },
            normalized_metrics: NormalizedScoreMetrics {
                size: Some(0.1),
                churn: Some(0.6),
                recent_churn: Some(0.3),
                ownership: Some(0.5),
                coupling: Some(0.4),
            },
            weighted_terms: vec![
                WeightedTerm {
                    name: "churn_score".to_owned(),
                    metric: NormalizedMetric::Churn,
                    formula_version: FormulaVersion::current(),
                    weight: 0.35,
                    normalized_input: Some(0.6),
                    weighted_contribution: 0.21,
                },
                WeightedTerm {
                    name: "size_score".to_owned(),
                    metric: NormalizedMetric::Size,
                    formula_version: FormulaVersion::current(),
                    weight: 0.20,
                    normalized_input: Some(0.1),
                    weighted_contribution: 0.02,
                },
            ],
            limitations: vec![ScoreLimitation {
                code: "test.limit".to_owned(),
                message: "fixture limitation".to_owned(),
            }],
        }];
        report
    }

    fn hotspot_fixture(
        rank: u64,
        path: &str,
        score: f64,
        churn: f64,
        size: f64,
        recent_churn: f64,
    ) -> ReportHotspot {
        ReportHotspot {
            rank,
            path: path.to_owned(),
            score,
            formula_version: FormulaVersion::current(),
            raw_metrics: RawScoreMetrics {
                path: path.to_owned(),
                byte_size: Some((size * 400_000.0) as u64),
                line_count: Some(100),
                commits_per_file: Some(10),
                total_churn_lines: Some((churn * 2_000.0) as u64),
                recent_churn_lines: Some((recent_churn * 100.0) as u64),
                author_count: Some(2),
                owner_count: Some(2),
                dominant_owner_share: Some(0.5),
                co_changed_file_count: Some(1),
                file_age_days: Some(365),
                repository_age_days: Some(730),
                repository_author_count: Some(10),
                repository_file_count: Some(200),
            },
            normalized_metrics: NormalizedScoreMetrics {
                size: Some(size),
                churn: Some(churn),
                recent_churn: Some(recent_churn),
                ownership: Some(0.2),
                coupling: Some(0.1),
            },
            weighted_terms: vec![
                weighted_term("churn_score", NormalizedMetric::Churn, churn, 0.35),
                weighted_term("size_score", NormalizedMetric::Size, size, 0.20),
                weighted_term(
                    "recent_growth",
                    NormalizedMetric::RecentChurn,
                    recent_churn,
                    0.15,
                ),
            ],
            limitations: Vec::new(),
        }
    }

    fn weighted_term(
        name: &str,
        metric: NormalizedMetric,
        normalized_input: f64,
        weight: f64,
    ) -> WeightedTerm {
        WeightedTerm {
            name: name.to_owned(),
            metric,
            formula_version: FormulaVersion::current(),
            weight,
            normalized_input: Some(normalized_input),
            weighted_contribution: normalized_input * weight,
        }
    }

    fn owner_share(author: &str, touch_count: u64, share: f64) -> TuiOwnerShare {
        TuiOwnerShare {
            author: author.to_owned(),
            touch_count,
            share,
        }
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

    fn detailed_symbol() -> ParseSymbolRecord {
        ParseSymbolRecord {
            path: "src/lib.rs".to_owned(),
            name: "render".to_owned(),
            kind: "function".to_owned(),
            start_line: 10,
            end_line: 92,
            signature: Some("fn render_lines()".to_owned()),
            nesting_depth: 1,
            parent: Some("impl Widget".to_owned()),
            cyclomatic_complexity: Some(8),
            max_control_flow_nesting: Some(3),
        }
    }

    fn char_position(value: &str, needle: char) -> Option<usize> {
        value.chars().position(|candidate| candidate == needle)
    }
}
