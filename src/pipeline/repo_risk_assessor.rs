// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Default)]
/// Derives repository-level risk from file scores and repository-wide signals.
pub struct RepoRiskAssessor;

impl RepoRiskAssessor {
    pub fn new() -> Self {
        Self
    }
}
