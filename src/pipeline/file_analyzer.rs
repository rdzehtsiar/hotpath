// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::languages::{go::GoParser, LanguageParser, ParserOutput, ParserRecognition};
use crate::pipeline::code_metrics_analyzer::CodeMetricsAnalyzer;

pub const DEFAULT_CONTENT_WINDOW_BYTES: usize = 1024 * 1024;

/// Extracts file-local facts and delegates source parsing to language parsers.
#[derive(Debug, Clone)]
pub struct FileAnalyzer {
    options: FileAnalyzerOptions,
}

impl FileAnalyzer {
    pub fn new() -> Self {
        Self::with_options(FileAnalyzerOptions::default())
    }

    pub fn with_options(options: FileAnalyzerOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> &FileAnalyzerOptions {
        &self.options
    }

    pub fn analyze(&self, input: FileAnalysisInput) -> FileAnalysisResult {
        let file = AnalyzedFile::new(input.path.clone(), self.options.clone());
        let window = file.first_content_window();
        let mut diagnostics = window.diagnostics.clone();
        let parser = self.parse(&file);
        let line_count = line_count_from_window(&window, &mut diagnostics);
        let parser_metrics = parser.output.as_ref().map(parser_metrics);

        FileAnalysisResult {
            path: input.path,
            byte_size: file.metadata().map(|metadata| metadata.byte_size),
            extension: file_extension(file.path()),
            content_kind: window.content_kind,
            line_count,
            is_generated: is_generated_path(file.path()),
            is_vendor: is_vendor_path(file.path()),
            diagnostics,
            parser_status: parser.status,
            parser_output: parser.output,
            parser_recognition_attempts: parser.recognition_attempts,
            language_id: parser_metrics
                .as_ref()
                .map(|metrics| metrics.language_id.clone()),
            symbol_count: parser_metrics
                .as_ref()
                .map_or(0, |metrics| metrics.symbol_count),
            function_count: parser_metrics
                .as_ref()
                .map_or(0, |metrics| metrics.function_count),
            method_count: parser_metrics
                .as_ref()
                .map_or(0, |metrics| metrics.method_count),
            type_count: parser_metrics
                .as_ref()
                .map_or(0, |metrics| metrics.type_count),
            import_count: parser_metrics
                .as_ref()
                .map_or(0, |metrics| metrics.import_count),
            complexity_pressure: parser_metrics
                .as_ref()
                .map(|metrics| metrics.complexity_pressure),
            max_function_complexity_pressure: parser_metrics
                .as_ref()
                .map(|metrics| metrics.max_function_complexity_pressure),
        }
    }

    fn parse(&self, file: &AnalyzedFile) -> FileParserResult {
        let mut recognition_attempts = 0;

        for parser in &self.options.parsers {
            recognition_attempts += 1;
            if parser.recognize(file) == ParserRecognition::Recognized {
                return FileParserResult {
                    status: FileParserStatus::Parsed,
                    output: Some(parser.parse(file)),
                    recognition_attempts,
                };
            }
        }

        FileParserResult {
            status: FileParserStatus::Unsupported,
            output: None,
            recognition_attempts,
        }
    }
}

impl Default for FileAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct FileAnalyzerOptions {
    pub content_window_bytes: usize,
    pub parsers: Vec<Arc<dyn LanguageParser>>,
}

impl PartialEq for FileAnalyzerOptions {
    fn eq(&self, other: &Self) -> bool {
        self.content_window_bytes == other.content_window_bytes
            && self.parsers.len() == other.parsers.len()
    }
}

impl Eq for FileAnalyzerOptions {}

impl fmt::Debug for FileAnalyzerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileAnalyzerOptions")
            .field("content_window_bytes", &self.content_window_bytes)
            .field("parser_count", &self.parsers.len())
            .finish()
    }
}

impl Default for FileAnalyzerOptions {
    fn default() -> Self {
        Self {
            content_window_bytes: DEFAULT_CONTENT_WINDOW_BYTES,
            parsers: vec![Arc::new(GoParser::new())],
        }
    }
}

pub fn file_analyzer_options_signature(options: &FileAnalyzerOptions) -> String {
    format!(
        "file-local-v3-source-refs;content-window={};parsers={}",
        options.content_window_bytes,
        options
            .parsers
            .iter()
            .map(|parser| parser.language_id())
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysisInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysisResult {
    pub path: PathBuf,
    pub byte_size: Option<u64>,
    pub extension: Option<String>,
    pub content_kind: ContentKind,
    pub line_count: Option<u64>,
    pub is_generated: bool,
    pub is_vendor: bool,
    pub diagnostics: Vec<FileDiagnostic>,
    pub parser_status: FileParserStatus,
    pub parser_output: Option<ParserOutput>,
    pub parser_recognition_attempts: usize,
    pub language_id: Option<String>,
    pub symbol_count: u64,
    pub function_count: u64,
    pub method_count: u64,
    pub type_count: u64,
    pub import_count: u64,
    pub complexity_pressure: Option<u64>,
    pub max_function_complexity_pressure: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileParserStatus {
    Unsupported,
    Parsed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileParserResult {
    status: FileParserStatus,
    output: Option<ParserOutput>,
    recognition_attempts: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Binary,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostic {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct AnalyzedFile {
    path: PathBuf,
    options: FileAnalyzerOptions,
    metadata: Option<FileMetadata>,
    content_window: RefCell<Option<FileContentWindow>>,
}

impl AnalyzedFile {
    pub fn new(path: PathBuf, options: FileAnalyzerOptions) -> Self {
        let metadata = fs::metadata(&path).ok().map(FileMetadata::from);

        Self {
            path,
            options,
            metadata,
            content_window: RefCell::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> Option<&FileMetadata> {
        self.metadata.as_ref()
    }

    pub fn first_content_window(&self) -> FileContentWindow {
        if let Some(window) = self.content_window.borrow().as_ref() {
            return window.clone();
        }

        let window = self.read_first_content_window();
        *self.content_window.borrow_mut() = Some(window.clone());
        window
    }

    fn read_first_content_window(&self) -> FileContentWindow {
        let mut diagnostics = Vec::new();
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(source) => {
                diagnostics.push(FileDiagnostic {
                    code: "read_failed".to_owned(),
                    message: format!("failed to open file contents: {source}"),
                });
                return FileContentWindow {
                    bytes: Vec::new(),
                    content_kind: ContentKind::Unknown,
                    diagnostics,
                    truncated: false,
                };
            }
        };

        let window_bytes = self.options.content_window_bytes.max(1);
        let mut bytes = Vec::with_capacity(window_bytes);
        let mut limited = (&mut file).take(window_bytes as u64);
        if let Err(source) = limited.read_to_end(&mut bytes) {
            diagnostics.push(FileDiagnostic {
                code: "read_failed".to_owned(),
                message: format!("failed to read file contents: {source}"),
            });
            return FileContentWindow {
                bytes: Vec::new(),
                content_kind: ContentKind::Unknown,
                diagnostics,
                truncated: false,
            };
        }

        let truncated = self
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.byte_size as usize > bytes.len());
        let content_kind = detect_content_kind(&bytes, &mut diagnostics);

        FileContentWindow {
            bytes,
            content_kind,
            diagnostics,
            truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParserMetrics {
    language_id: String,
    symbol_count: u64,
    function_count: u64,
    method_count: u64,
    type_count: u64,
    import_count: u64,
    complexity_pressure: u64,
    max_function_complexity_pressure: u64,
}

fn parser_metrics(output: &ParserOutput) -> ParserMetrics {
    let complexity = CodeMetricsAnalyzer::new().analyze(&output.metrics_input);

    ParserMetrics {
        language_id: output.language_id.clone(),
        symbol_count: output.symbols.len() as u64,
        function_count: count_symbols(output, "function"),
        method_count: count_symbols(output, "method"),
        type_count: output
            .symbols
            .iter()
            .filter(|symbol| matches!(symbol.kind.as_str(), "type" | "struct" | "interface"))
            .count() as u64,
        import_count: output
            .references
            .iter()
            .filter(|reference| reference.kind == "import")
            .count() as u64,
        complexity_pressure: complexity.complexity_pressure,
        max_function_complexity_pressure: complexity.max_function_complexity_pressure,
    }
}

fn count_symbols(output: &ParserOutput, kind: &str) -> u64 {
    output
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .count() as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub byte_size: u64,
}

impl From<fs::Metadata> for FileMetadata {
    fn from(metadata: fs::Metadata) -> Self {
        Self {
            byte_size: metadata.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContentWindow {
    pub bytes: Vec<u8>,
    pub content_kind: ContentKind,
    pub diagnostics: Vec<FileDiagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadError {
    pub diagnostic: FileDiagnostic,
}

impl fmt::Display for FileReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.diagnostic.message)
    }
}

impl StdError for FileReadError {}

fn detect_content_kind(bytes: &[u8], diagnostics: &mut Vec<FileDiagnostic>) -> ContentKind {
    if bytes.contains(&0) {
        return ContentKind::Binary;
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => ContentKind::Text,
        Err(_) => {
            diagnostics.push(FileDiagnostic {
                code: "unsupported_encoding".to_owned(),
                message: "file contents are not valid UTF-8".to_owned(),
            });
            ContentKind::Unknown
        }
    }
}

fn line_count_from_window(
    window: &FileContentWindow,
    diagnostics: &mut Vec<FileDiagnostic>,
) -> Option<u64> {
    if window.content_kind != ContentKind::Text {
        return None;
    }

    if window.truncated {
        diagnostics.push(FileDiagnostic {
            code: "line_count_skipped".to_owned(),
            message: "file is larger than the active content window".to_owned(),
        });
        return None;
    }

    std::str::from_utf8(&window.bytes)
        .ok()
        .map(|text| text.lines().count() as u64)
}

fn file_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_vendor_path(path: &Path) -> bool {
    normalized_components(path).any(|component| {
        matches_case_insensitive(
            component,
            &[
                "node_modules",
                "vendor",
                "third_party",
                "third-party",
                "external",
            ],
        )
    })
}

fn is_generated_path(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    normalized_components(path).any(|component| {
        matches_case_insensitive(component, &["generated", "gen", "codegen", "dist", "build"])
    }) || file_name.contains(".generated.")
        || file_name.contains(".gen.")
        || file_name.ends_with(".pb.go")
        || file_name.ends_with(".pb.rs")
        || file_name.ends_with(".g.cs")
}

fn normalized_components(path: &Path) -> impl Iterator<Item = &str> {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::trim)
        .filter(|component| !component.is_empty())
}

fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::{
        AnalyzedFile, ContentKind, FileAnalysisInput, FileAnalyzer, FileAnalyzerOptions,
        FileParserStatus, DEFAULT_CONTENT_WINDOW_BYTES,
    };
    use crate::languages::{
        LanguageParser, ParserOutput, ParserRecognition, UniversalCodeMetricsInput,
        UniversalReference, UniversalSymbol,
    };

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::current_dir()
                .expect("test should have current directory")
                .join("target")
                .join("file-analyzer-fixtures")
                .join(format!("{name}-{}-{id}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("fixture root should be created");

            Self { path }
        }

        fn write(&self, relative_path: impl AsRef<Path>, contents: &[u8]) -> PathBuf {
            let path = self.path.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent should be created");
            }
            fs::write(&path, contents).expect("fixture file should be written");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn file_object_reads_existing_file_metadata() {
        let fixture = Fixture::new("metadata");
        let path = fixture.write("main.go", b"package main\n");
        let file = AnalyzedFile::new(path, FileAnalyzerOptions::default());

        assert_eq!(
            file.metadata().expect("metadata should exist").byte_size,
            "package main\n".len() as u64
        );
    }

    #[test]
    fn default_content_window_is_one_mebibyte() {
        assert_eq!(DEFAULT_CONTENT_WINDOW_BYTES, 1024 * 1024);
        assert_eq!(
            FileAnalyzerOptions::default().content_window_bytes,
            DEFAULT_CONTENT_WINDOW_BYTES
        );
    }

    #[test]
    fn first_content_window_is_limited_by_configured_size() {
        let fixture = Fixture::new("window");
        let path = fixture.write("large.go", b"abcdef");
        let file = AnalyzedFile::new(
            path,
            FileAnalyzerOptions {
                content_window_bytes: 3,
                parsers: Vec::new(),
            },
        );

        let window = file.first_content_window();

        assert_eq!(window.bytes, b"abc");
        assert!(window.truncated);
        assert_eq!(window.content_kind, ContentKind::Text);
    }

    #[test]
    fn content_kind_detects_text_and_binary() {
        let fixture = Fixture::new("content-kind");
        let text_path = fixture.write("text.go", b"package main\n");
        let binary_path = fixture.write("binary.dat", b"abc\0def");

        let text = AnalyzedFile::new(text_path, FileAnalyzerOptions::default());
        let binary = AnalyzedFile::new(binary_path, FileAnalyzerOptions::default());

        assert_eq!(text.first_content_window().content_kind, ContentKind::Text);
        assert_eq!(
            binary.first_content_window().content_kind,
            ContentKind::Binary
        );
    }

    #[test]
    fn missing_file_returns_unknown_content_without_panic() {
        let fixture = Fixture::new("missing");
        let path = fixture.path.join("missing.go");
        let file = AnalyzedFile::new(path, FileAnalyzerOptions::default());

        let window = file.first_content_window();

        assert!(file.metadata().is_none());
        assert_eq!(window.content_kind, ContentKind::Unknown);
        assert_eq!(window.diagnostics[0].code, "read_failed");
    }

    #[test]
    fn analyzer_creates_file_object_and_returns_path() {
        let fixture = Fixture::new("analyzer");
        let path = fixture.write("main.go", b"package main\n");
        let analyzer = FileAnalyzer::with_options(FileAnalyzerOptions {
            content_window_bytes: 64,
            parsers: Vec::new(),
        });

        let result = analyzer.analyze(FileAnalysisInput { path: path.clone() });

        assert_eq!(result.path, path);
        assert_eq!(result.byte_size, Some("package main\n".len() as u64));
        assert_eq!(result.extension.as_deref(), Some("go"));
        assert_eq!(result.content_kind, ContentKind::Text);
        assert_eq!(result.line_count, Some(1));
        assert_eq!(result.parser_status, FileParserStatus::Unsupported);
        assert_eq!(result.parser_recognition_attempts, 0);
        assert!(result.parser_output.is_none());
        assert_eq!(analyzer.options().content_window_bytes, 64);
    }

    #[test]
    fn analyzer_classifies_binary_and_invalid_utf8() {
        let fixture = Fixture::new("binary-invalid");
        let binary_path = fixture.write("blob.bin", b"abc\0def");
        let invalid_path = fixture.write("bad.go", &[b'a', 0xff]);
        let analyzer = FileAnalyzer::new();

        let binary = analyzer.analyze(FileAnalysisInput { path: binary_path });
        let invalid = analyzer.analyze(FileAnalysisInput { path: invalid_path });

        assert_eq!(binary.content_kind, ContentKind::Binary);
        assert_eq!(binary.line_count, None);
        assert_eq!(invalid.content_kind, ContentKind::Unknown);
        assert_eq!(invalid.line_count, None);
        assert_eq!(invalid.diagnostics[0].code, "unsupported_encoding");
    }

    #[test]
    fn analyzer_marks_truncated_text_and_omits_line_count() {
        let fixture = Fixture::new("truncated");
        let path = fixture.write("large.go", b"line 1\nline 2\n");
        let analyzer = FileAnalyzer::with_options(FileAnalyzerOptions {
            content_window_bytes: 4,
            parsers: Vec::new(),
        });

        let result = analyzer.analyze(FileAnalysisInput { path });

        assert_eq!(result.content_kind, ContentKind::Text);
        assert_eq!(result.line_count, None);
        assert!(result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "line_count_skipped"));
    }

    #[test]
    fn analyzer_classifies_generated_and_vendor_paths() {
        let fixture = Fixture::new("path-classification");
        let generated_path = fixture.write("generated/client.go", b"package generated\n");
        let vendor_path = fixture.write("node_modules/pkg/index.go", b"package pkg\n");
        let analyzer = FileAnalyzer::new();

        let generated = analyzer.analyze(FileAnalysisInput {
            path: generated_path,
        });
        let vendor = analyzer.analyze(FileAnalysisInput { path: vendor_path });

        assert!(generated.is_generated);
        assert!(!generated.is_vendor);
        assert!(vendor.is_vendor);
    }

    #[test]
    fn empty_parser_registry_reports_unsupported_without_attempts() {
        let fixture = Fixture::new("empty-parser-registry");
        let path = fixture.write("main.go", b"package main\n");
        let analyzer = FileAnalyzer::with_options(FileAnalyzerOptions {
            content_window_bytes: DEFAULT_CONTENT_WINDOW_BYTES,
            parsers: Vec::new(),
        });

        let result = analyzer.analyze(FileAnalysisInput { path });

        assert_eq!(result.parser_status, FileParserStatus::Unsupported);
        assert_eq!(result.parser_recognition_attempts, 0);
        assert!(result.parser_output.is_none());
    }

    #[test]
    fn default_analyzer_parses_go_and_computes_compact_metrics() {
        let fixture = Fixture::new("default-go-parser");
        let path = fixture.write(
            "main.go",
            b"package main\n\nimport \"fmt\"\n\ntype Service struct{}\n\nfunc main() {\n    if true {\n        fmt.Println(\"x\")\n    }\n}\n",
        );
        let analyzer = FileAnalyzer::new();

        let result = analyzer.analyze(FileAnalysisInput { path });

        assert_eq!(result.parser_status, FileParserStatus::Parsed);
        assert_eq!(result.language_id.as_deref(), Some("go"));
        assert_eq!(result.function_count, 1);
        assert_eq!(result.method_count, 0);
        assert_eq!(result.type_count, 1);
        assert_eq!(result.import_count, 1);
        assert!(result.symbol_count >= 2);
        assert_eq!(result.complexity_pressure, Some(1));
        assert_eq!(result.max_function_complexity_pressure, Some(1));
    }

    #[test]
    fn parser_loop_uses_first_recognized_parser() {
        let fixture = Fixture::new("parser-loop");
        let path = fixture.write("main.mock", b"mock\n");
        let analyzer = FileAnalyzer::with_options(FileAnalyzerOptions {
            content_window_bytes: DEFAULT_CONTENT_WINDOW_BYTES,
            parsers: vec![
                Arc::new(MockParser::new(false)),
                Arc::new(MockParser::new(true)),
            ],
        });

        let result = analyzer.analyze(FileAnalysisInput { path });

        assert_eq!(result.parser_status, FileParserStatus::Parsed);
        assert_eq!(result.parser_recognition_attempts, 2);
        assert_eq!(
            result
                .parser_output
                .expect("parser output should be present")
                .language_id,
            "mock"
        );
    }

    struct MockParser {
        recognized: bool,
    }

    impl MockParser {
        fn new(recognized: bool) -> Self {
            Self { recognized }
        }
    }

    impl LanguageParser for MockParser {
        fn language_id(&self) -> &'static str {
            "mock"
        }

        fn recognize(&self, _file: &AnalyzedFile) -> ParserRecognition {
            if self.recognized {
                ParserRecognition::Recognized
            } else {
                ParserRecognition::NotRecognized
            }
        }

        fn parse(&self, _file: &AnalyzedFile) -> ParserOutput {
            ParserOutput {
                language_id: self.language_id().to_owned(),
                symbols: vec![UniversalSymbol {
                    name: "main".to_owned(),
                    kind: "function".to_owned(),
                }],
                references: vec![UniversalReference {
                    target: "dep".to_owned(),
                    kind: "import".to_owned(),
                }],
                metrics_input: UniversalCodeMetricsInput::default(),
                diagnostics: Vec::new(),
                limitations: Vec::new(),
            }
        }
    }
}
