// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanStub {
    pub status: &'static str,
    pub file_walking: &'static str,
    pub classification: &'static str,
}

impl ScanStub {
    pub fn pending() -> Self {
        Self {
            status: "stub",
            file_walking: "not_implemented",
            classification: "not_implemented",
        }
    }
}

pub fn scan_summary() -> Result<String, serde_json::Error> {
    let scan = ScanStub::pending();

    Ok(format!(
        "Hotpath scan summary\nstatus: {}\nfile walking: {}\nclassification: {}",
        scan.status, scan.file_walking, scan.classification
    ))
}

pub fn scan_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&ScanStub::pending())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reports_stub_boundaries() {
        let summary = scan_summary().expect("summary should render");

        assert!(summary.contains("status: stub"));
        assert!(summary.contains("file walking: not_implemented"));
        assert!(summary.contains("classification: not_implemented"));
    }

    #[test]
    fn json_reports_stub_boundaries() {
        let json = scan_json().expect("json should render");

        assert_eq!(
            json,
            "{\n  \"status\": \"stub\",\n  \"file_walking\": \"not_implemented\",\n  \"classification\": \"not_implemented\"\n}"
        );
    }
}
