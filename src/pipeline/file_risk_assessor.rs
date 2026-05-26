// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Default)]
/// Scores file-level risk from merged intelligence facts and limitations.
pub struct FileRiskAssessor;

impl FileRiskAssessor {
    pub fn new() -> Self {
        Self
    }
}
