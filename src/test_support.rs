// SPDX-License-Identifier: Apache-2.0

use crate::ParseImportRecord;

pub fn parse_import(path: &str, target: &str, kind: &str) -> ParseImportRecord {
    ParseImportRecord {
        path: path.to_owned(),
        target: target.to_owned(),
        kind: kind.to_owned(),
        start_line: 1,
        end_line: 1,
    }
}
