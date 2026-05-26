// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Default)]
/// Extracts file-local facts and delegates source parsing to language parsers.
pub struct FileAnalyzer;

impl FileAnalyzer {
    pub fn new() -> Self {
        Self
    }
}
