// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Default)]
/// Reduces local Git history into file-level and repository-level Git facts.
pub struct GitHistoryAnalyzer;

impl GitHistoryAnalyzer {
    pub fn new() -> Self {
        Self
    }
}
