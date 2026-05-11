// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};
use serde::Serialize;

#[cfg(test)]
const BINARY_SAMPLE_BYTES: usize = 8 * 1024;
const MAX_TEXT_READ_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Binary,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileWarning {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRecord {
    pub path: String,
    pub byte_size: Option<u64>,
    pub extension: Option<String>,
    pub language: Option<&'static str>,
    pub line_count: Option<u64>,
    pub is_vendor: bool,
    pub is_generated: bool,
    pub content: ContentKind,
    pub is_symlink: bool,
    pub classification: &'static str,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub status: &'static str,
    pub file_walking: &'static str,
    pub classification: &'static str,
    pub files: Vec<FileRecord>,
}

impl ScanReport {
    fn from_files(files: Vec<FileRecord>) -> Self {
        Self {
            status: "ok",
            file_walking: "implemented",
            classification: "implemented",
            files,
        }
    }
}

#[derive(Debug)]
pub enum ScanError {
    CurrentDir(std::io::Error),
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
    Walk(ignore::Error),
    RelativePath {
        root: PathBuf,
        path: PathBuf,
    },
    Json(serde_json::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Root { path, source } => {
                write!(f, "failed to read scan root '{}': {source}", path.display())
            }
            Self::Walk(source) => write!(f, "failed while walking repository files: {source}"),
            Self::RelativePath { root, path } => write!(
                f,
                "failed to make '{}' relative to scan root '{}'",
                path.display(),
                root.display()
            ),
            Self::Json(source) => write!(f, "failed to render scan JSON: {source}"),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDir(source) | Self::Root { source, .. } => Some(source),
            Self::Walk(source) => Some(source),
            Self::RelativePath { .. } => None,
            Self::Json(source) => Some(source),
        }
    }
}

impl From<ignore::Error> for ScanError {
    fn from(source: ignore::Error) -> Self {
        Self::Walk(source)
    }
}

impl From<serde_json::Error> for ScanError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

pub fn scan_current_dir() -> Result<ScanReport, ScanError> {
    let root = env::current_dir().map_err(ScanError::CurrentDir)?;

    scan_repository(root)
}

pub fn scan_repository(root: impl AsRef<Path>) -> Result<ScanReport, ScanError> {
    let root = root.as_ref();
    let root = fs::canonicalize(root).map_err(|source| ScanError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let mut files = Vec::new();

    for entry in WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| !is_git_entry(entry))
        .build()
    {
        let entry = entry?;

        if !is_walked_file(&entry) {
            continue;
        }

        files.push(classify_file(&root, entry.path())?);
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(ScanReport::from_files(files))
}

pub fn scan_summary() -> Result<String, ScanError> {
    Ok(render_summary(&scan_current_dir()?))
}

pub fn scan_json() -> Result<String, ScanError> {
    Ok(serde_json::to_string_pretty(&scan_current_dir()?)?)
}

fn is_git_entry(entry: &DirEntry) -> bool {
    entry.file_name() == ".git"
}

fn is_walked_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|file_type| {
        file_type.is_file()
            || (file_type.is_symlink()
                && fs::metadata(entry.path())
                    .map(|metadata| metadata.is_file())
                    .unwrap_or(true))
    })
}

fn classify_file(root: &Path, path: &Path) -> Result<FileRecord, ScanError> {
    let relative_path = normalized_relative_path(root, path)?;
    let mut record = FileRecord {
        byte_size: None,
        extension: file_extension(&relative_path),
        language: language_guess(&relative_path),
        line_count: None,
        is_vendor: is_vendor_path(&relative_path),
        is_generated: is_generated_path(&relative_path),
        content: ContentKind::Unknown,
        is_symlink: fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false),
        classification: "implemented",
        warnings: Vec::new(),
        path: relative_path,
    };

    if record.is_symlink {
        let target = match fs::canonicalize(path) {
            Ok(target) => target,
            Err(source) => {
                record.warnings.push(file_warning(
                    "symlink_target_unreadable",
                    format!("failed to canonicalize symlink target: {source}"),
                ));
                return Ok(record);
            }
        };

        if !target.starts_with(root) {
            record.warnings.push(file_warning(
                "symlink_target_outside_root",
                "symlink target is outside the scan root".to_owned(),
            ));
            return Ok(record);
        }
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) => {
            record.warnings.push(file_warning(
                "metadata_failed",
                format!("failed to read file metadata: {source}"),
            ));
            return Ok(record);
        }
    };

    record.byte_size = Some(metadata.len());

    classify_content(path, &mut record);

    Ok(record)
}

fn classify_content(path: &Path, record: &mut FileRecord) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) => {
            record.warnings.push(file_warning(
                "read_failed",
                format!("failed to open file contents: {source}"),
            ));
            return;
        }
    };

    let mut bytes = Vec::new();
    let mut bounded = file.take(MAX_TEXT_READ_BYTES.saturating_add(1));
    if let Err(source) = bounded.read_to_end(&mut bytes) {
        record.warnings.push(file_warning(
            "read_failed",
            format!("failed to read file contents: {source}"),
        ));
        return;
    }

    if bytes.contains(&0) {
        record.content = ContentKind::Binary;
        return;
    }

    let exceeded_read_limit = bytes.len() as u64 > MAX_TEXT_READ_BYTES;
    if exceeded_read_limit {
        bytes.truncate(MAX_TEXT_READ_BYTES as usize);
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            record.warnings.push(file_warning(
                "unsupported_encoding",
                "file contents are not valid UTF-8".to_owned(),
            ));
            return;
        }
    };

    record.content = ContentKind::Text;

    if exceeded_read_limit {
        record.warnings.push(file_warning(
            "line_count_skipped",
            format!("file is larger than the safe text read limit of {MAX_TEXT_READ_BYTES} bytes"),
        ));
        return;
    }

    record.line_count = Some(count_lines(&text));
}

fn file_warning(code: &'static str, message: String) -> FileWarning {
    FileWarning { code, message }
}

fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
}

fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

fn language_guess(path: &str) -> Option<&'static str> {
    let extension = file_extension(path);
    let file_name = Path::new(path)
        .file_name()
        .and_then(|file_name| file_name.to_str())?;

    match file_name {
        "Dockerfile" | "Containerfile" => return Some("Dockerfile"),
        "Makefile" => return Some("Makefile"),
        _ => {}
    }

    match extension.as_deref()? {
        "bash" | "sh" | "zsh" => Some("Shell"),
        "c" => Some("C"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some("C++"),
        "cs" => Some("C#"),
        "css" => Some("CSS"),
        "go" => Some("Go"),
        "h" => Some("C/C++ Header"),
        "htm" | "html" => Some("HTML"),
        "java" => Some("Java"),
        "js" | "mjs" | "cjs" => Some("JavaScript"),
        "json" => Some("JSON"),
        "jsx" => Some("JavaScript JSX"),
        "kt" | "kts" => Some("Kotlin"),
        "md" | "markdown" => Some("Markdown"),
        "php" => Some("PHP"),
        "proto" => Some("Protocol Buffers"),
        "ps1" => Some("PowerShell"),
        "py" => Some("Python"),
        "rb" => Some("Ruby"),
        "rs" => Some("Rust"),
        "scala" => Some("Scala"),
        "scss" => Some("Sass"),
        "sql" => Some("SQL"),
        "swift" => Some("Swift"),
        "toml" => Some("TOML"),
        "ts" => Some("TypeScript"),
        "tsx" => Some("TypeScript JSX"),
        "xml" => Some("XML"),
        "yaml" | "yml" => Some("YAML"),
        _ => None,
    }
}

fn is_vendor_path(path: &str) -> bool {
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

fn is_generated_path(path: &str) -> bool {
    let file_name = Path::new(path)
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

fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn normalized_components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter(|component| !component.is_empty())
        .map(|component| component.trim())
}

fn render_summary(scan: &ScanReport) -> String {
    let mut total_bytes = 0;
    let mut text_files = 0;
    let mut binary_files = 0;
    let mut unknown_files = 0;
    let mut generated_files = 0;
    let mut vendor_files = 0;
    let mut warning_count = 0;
    let mut unreadable_count = 0;
    let mut skipped_count = 0;
    let mut languages = BTreeMap::new();

    for file in &scan.files {
        total_bytes += file.byte_size.unwrap_or(0);

        match file.content {
            ContentKind::Text => text_files += 1,
            ContentKind::Binary => binary_files += 1,
            ContentKind::Unknown => unknown_files += 1,
        }

        if file.is_generated {
            generated_files += 1;
        }

        if file.is_vendor {
            vendor_files += 1;
        }

        if let Some(language) = file.language {
            *languages.entry(language).or_insert(0) += 1;
        }

        for warning in &file.warnings {
            warning_count += 1;

            if is_unreadable_warning(warning.code) {
                unreadable_count += 1;
            }

            if is_skipped_warning(warning.code) {
                skipped_count += 1;
            }
        }
    }

    let mut summary = format!(
        "Hotpath scan summary\ntotal files: {}\ntotal bytes: {}\ncontent: text {}, binary {}, unknown {}\nflags: generated {}, vendor {}",
        scan.files.len(),
        total_bytes,
        text_files,
        binary_files,
        unknown_files,
        generated_files,
        vendor_files
    );

    if warning_count > 0 {
        summary.push_str(&format!(
            "\nwarnings: {} (unreadable {}, skipped {})",
            warning_count, unreadable_count, skipped_count
        ));
    }

    summary.push_str("\nlanguages:");

    if languages.is_empty() {
        summary.push_str("\n  none");
    } else {
        for (language, count) in languages {
            summary.push_str(&format!("\n  {language}: {count}"));
        }
    }

    summary
}

fn is_unreadable_warning(code: &str) -> bool {
    matches!(
        code,
        "metadata_failed" | "read_failed" | "symlink_target_unreadable"
    )
}

fn is_skipped_warning(code: &str) -> bool {
    matches!(code, "line_count_skipped" | "symlink_target_outside_root")
}

fn normalized_relative_path(root: &Path, path: &Path) -> Result<String, ScanError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ScanError::RelativePath {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
        })?;
    let parts = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            Component::CurDir => None,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                unreachable!("stripped repository-relative paths cannot contain root components")
            }
        })
        .collect::<Vec<_>>();

    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::current_dir()
                .expect("test should have a current directory")
                .join("target")
                .join("test-fixtures")
                .join(format!("{name}-{}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("fixture root should be created");

            Self { path }
        }

        fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
            let path = self.path.join(relative_path);

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent should be created");
            }

            fs::write(path, contents).expect("fixture file should be written");
        }

        fn write_bytes(&self, relative_path: impl AsRef<Path>, contents: &[u8]) {
            let path = self.path.join(relative_path);

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent should be created");
            }

            fs::write(path, contents).expect("fixture file should be written");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn scanned_paths(root: &Path) -> Vec<String> {
        scan_repository(root)
            .expect("fixture scan should succeed")
            .files
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    fn scanned_records(root: &Path) -> Vec<FileRecord> {
        scan_repository(root)
            .expect("fixture scan should succeed")
            .files
    }

    fn scanned_record(root: &Path, path: &str) -> FileRecord {
        scanned_records(root)
            .into_iter()
            .find(|record| record.path == path)
            .unwrap_or_else(|| panic!("expected scan record for {path}"))
    }

    fn record(
        path: &str,
        byte_size: Option<u64>,
        language: Option<&'static str>,
        content: ContentKind,
    ) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size,
            extension: file_extension(path),
            language,
            line_count: None,
            is_vendor: false,
            is_generated: false,
            content,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(original, link)
    }

    #[cfg(unix)]
    fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::windows::fs::symlink_file(original, link)
    }

    #[test]
    fn scan_records_are_sorted_by_normalized_relative_path() {
        let fixture = Fixture::new("deterministic-ordering");
        fixture.write("z.rs", "");
        fixture.write(Path::new("nested").join("m.rs"), "");
        fixture.write("a.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec!["a.rs", "nested/m.rs", "z.rs"]
        );
    }

    #[test]
    fn scan_respects_gitignore_patterns() {
        let fixture = Fixture::new("gitignore");
        fixture.write(".gitignore", "ignored/\n*.log\n");
        fixture.write("ignored/file.rs", "");
        fixture.write("keep.rs", "");
        fixture.write("notes.log", "");

        assert_eq!(scanned_paths(&fixture.path), vec![".gitignore", "keep.rs"]);
    }

    #[test]
    fn scan_ignores_non_repository_ignore_sources() {
        let fixture = Fixture::new("deterministic-ignore-sources");
        fixture.write(".ignore", "ignored-by-dot-ignore.rs\n");
        fixture.write(".git/info/exclude", "ignored-by-git-exclude.rs\n");
        fixture.write("ignored-by-dot-ignore.rs", "");
        fixture.write("ignored-by-git-exclude.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec![
                ".ignore",
                "ignored-by-dot-ignore.rs",
                "ignored-by-git-exclude.rs"
            ]
        );
    }

    #[test]
    fn scan_excludes_git_entries_that_are_files() {
        let fixture = Fixture::new("git-file");
        fixture.write(".git", "gitdir: ../linked-worktree.git\n");
        fixture.write("keep.rs", "");

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn scan_does_not_descend_into_symlinked_directories() {
        let fixture = Fixture::new("symlinked-directory");
        let linked = Fixture::new("symlink-target");
        linked.write("nested/secret.rs", "");
        fixture.write("keep.rs", "");

        if symlink_dir(&linked.path, fixture.path.join("linked")).is_err() {
            return;
        }

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn binary_files_are_classified_without_line_counts() {
        let fixture = Fixture::new("binary-file");
        fixture.write_bytes("image.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);

        let record = scanned_record(&fixture.path, "image.bin");

        assert_eq!(record.byte_size, Some(8));
        assert_eq!(record.extension, Some("bin".to_owned()));
        assert_eq!(record.content, ContentKind::Binary);
        assert_eq!(record.line_count, None);
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn nul_after_initial_sample_classifies_file_as_binary() {
        let fixture = Fixture::new("delayed-nul");
        let mut contents = vec![b'a'; BINARY_SAMPLE_BYTES + 1];
        contents.push(0);
        fixture.write_bytes("delayed.bin", &contents);

        let record = scanned_record(&fixture.path, "delayed.bin");

        assert_eq!(record.content, ContentKind::Binary);
        assert_eq!(record.line_count, None);
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn text_larger_than_read_limit_skips_line_count() {
        let fixture = Fixture::new("large-text");
        let contents = vec![b'a'; MAX_TEXT_READ_BYTES as usize + 1];
        fixture.write_bytes("large.txt", &contents);

        let record = scanned_record(&fixture.path, "large.txt");

        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "line_count_skipped");
    }

    #[test]
    fn utf8_text_files_record_line_counts_and_sizes() {
        let fixture = Fixture::new("utf8-line-count");
        fixture.write("src/lib.rs", "fn main() {}\n\n// done\n");

        let record = scanned_record(&fixture.path, "src/lib.rs");

        assert_eq!(record.byte_size, Some(22));
        assert_eq!(record.extension, Some("rs".to_owned()));
        assert_eq!(record.language, Some("Rust"));
        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, Some(3));
    }

    #[test]
    fn invalid_utf8_fallback_does_not_panic_or_count_lines() {
        let fixture = Fixture::new("invalid-utf8");
        fixture.write_bytes("bad.txt", &[b'a', 0xff, b'\n']);

        let record = scanned_record(&fixture.path, "bad.txt");

        assert_eq!(record.byte_size, Some(3));
        assert_eq!(record.extension, Some("txt".to_owned()));
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "unsupported_encoding");
    }

    #[test]
    fn vendor_and_generated_paths_are_flagged() {
        let fixture = Fixture::new("vendor-generated");
        fixture.write("node_modules/pkg/index.js", "");
        fixture.write("src/api.generated.ts", "");
        fixture.write("src/handwritten.ts", "");

        let vendor = scanned_record(&fixture.path, "node_modules/pkg/index.js");
        let generated = scanned_record(&fixture.path, "src/api.generated.ts");
        let handwritten = scanned_record(&fixture.path, "src/handwritten.ts");

        assert!(vendor.is_vendor);
        assert!(!vendor.is_generated);
        assert!(generated.is_generated);
        assert!(!generated.is_vendor);
        assert!(!handwritten.is_vendor);
        assert!(!handwritten.is_generated);
    }

    #[test]
    fn vendor_and_generated_component_matching_is_case_insensitive() {
        let fixture = Fixture::new("cased-vendor-generated");
        fixture.write("Node_Modules/pkg/index.js", "");
        fixture.write("Src/CodeGen/api.ts", "");

        let vendor = scanned_record(&fixture.path, "Node_Modules/pkg/index.js");
        let generated = scanned_record(&fixture.path, "Src/CodeGen/api.ts");

        assert!(vendor.is_vendor);
        assert!(generated.is_generated);
    }

    #[test]
    fn extension_and_language_guesses_are_conservative() {
        let fixture = Fixture::new("language-guesses");
        fixture.write("README.md", "");
        fixture.write("Dockerfile", "");
        fixture.write("src/view.tsx", "");
        fixture.write("unknown.hotpath", "");

        let markdown = scanned_record(&fixture.path, "README.md");
        let dockerfile = scanned_record(&fixture.path, "Dockerfile");
        let tsx = scanned_record(&fixture.path, "src/view.tsx");
        let unknown = scanned_record(&fixture.path, "unknown.hotpath");

        assert_eq!(markdown.extension, Some("md".to_owned()));
        assert_eq!(markdown.language, Some("Markdown"));
        assert_eq!(dockerfile.extension, None);
        assert_eq!(dockerfile.language, Some("Dockerfile"));
        assert_eq!(tsx.extension, Some("tsx".to_owned()));
        assert_eq!(tsx.language, Some("TypeScript JSX"));
        assert_eq!(unknown.extension, Some("hotpath".to_owned()));
        assert_eq!(unknown.language, None);
    }

    #[test]
    fn symlinked_files_inside_scan_root_are_classified() {
        let fixture = Fixture::new("symlinked-file");
        fixture.write("target.rs", "fn linked() {}\n");

        if symlink_file(
            fixture.path.join("target.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.language, Some("Rust"));
        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, Some(1));
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn symlinked_files_outside_scan_root_are_recorded_without_content() {
        let fixture = Fixture::new("outside-symlink");
        let target = Fixture::new("outside-symlink-target");
        target.write("target.rs", "fn linked() {}\n");

        if symlink_file(
            target.path.join("target.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.byte_size, None);
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "symlink_target_outside_root");
    }

    #[test]
    fn unreadable_symlink_targets_are_recorded_without_content() {
        let fixture = Fixture::new("broken-symlink");

        if symlink_file(
            fixture.path.join("missing.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.byte_size, None);
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "symlink_target_unreadable");
    }

    #[test]
    fn summary_reports_concise_totals() {
        let mut generated = record(
            "dist/app.generated.js",
            Some(30),
            Some("JavaScript"),
            ContentKind::Text,
        );
        generated.is_generated = true;
        let mut vendor = record("vendor/blob.bin", Some(5), None, ContentKind::Binary);
        vendor.is_vendor = true;
        let mut unknown = record("notes.txt", None, None, ContentKind::Unknown);
        unknown.warnings.push(file_warning(
            "read_failed",
            "failed to open file contents: denied".to_owned(),
        ));

        let scan = ScanReport::from_files(vec![
            record("src/lib.rs", Some(10), Some("Rust"), ContentKind::Text),
            generated,
            vendor,
            unknown,
        ]);
        let summary = render_summary(&scan);

        assert_eq!(
            summary,
            "Hotpath scan summary\ntotal files: 4\ntotal bytes: 45\ncontent: text 2, binary 1, unknown 1\nflags: generated 1, vendor 1\nwarnings: 1 (unreadable 1, skipped 0)\nlanguages:\n  JavaScript: 1\n  Rust: 1"
        );
    }

    #[test]
    fn summary_omits_warning_line_when_no_warnings_are_present() {
        let scan = ScanReport::from_files(vec![record(
            "src/lib.rs",
            Some(10),
            Some("Rust"),
            ContentKind::Text,
        )]);

        let summary = render_summary(&scan);

        assert!(!summary.contains("warnings:"));
    }

    #[test]
    fn summary_reports_empty_language_counts_explicitly() {
        let scan = ScanReport::from_files(vec![record(
            "blob.bin",
            Some(10),
            None,
            ContentKind::Binary,
        )]);

        let summary = render_summary(&scan);

        assert!(summary.ends_with("languages:\n  none"));
    }

    #[test]
    fn summary_reports_skipped_warning_counts() {
        let mut skipped = record("large.txt", Some(10), None, ContentKind::Text);
        skipped.warnings.push(file_warning(
            "line_count_skipped",
            "file is larger than the safe text read limit".to_owned(),
        ));

        let summary = render_summary(&ScanReport::from_files(vec![skipped]));

        assert!(summary.contains("warnings: 1 (unreadable 0, skipped 1)"));
    }

    #[test]
    fn json_reports_file_records() {
        let report = ScanReport::from_files(vec![FileRecord {
            path: "src/lib.rs".to_owned(),
            byte_size: Some(10),
            extension: Some("rs".to_owned()),
            language: Some("Rust"),
            line_count: Some(1),
            is_vendor: false,
            is_generated: false,
            content: ContentKind::Text,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }]);
        let json = serde_json::to_string_pretty(&report).expect("json should render");

        assert_eq!(
            json,
            "{\n  \"status\": \"ok\",\n  \"file_walking\": \"implemented\",\n  \"classification\": \"implemented\",\n  \"files\": [\n    {\n      \"path\": \"src/lib.rs\",\n      \"byte_size\": 10,\n      \"extension\": \"rs\",\n      \"language\": \"Rust\",\n      \"line_count\": 1,\n      \"is_vendor\": false,\n      \"is_generated\": false,\n      \"content\": \"text\",\n      \"is_symlink\": false,\n      \"classification\": \"implemented\",\n      \"warnings\": []\n    }\n  ]\n}"
        );
    }
}
