// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

/// Extracts file-local facts and delegates source parsing to language parsers.
#[derive(Debug, Default)]
pub struct FileAnalyzer;

impl FileAnalyzer {
    pub fn new() -> Self {
        Self
    }

    pub fn analyze(&self, input: FileAnalysisInput) -> FileAnalysisResult {
        FileAnalysisResult { path: input.path }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysisInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysisResult {
    pub path: PathBuf,
}
