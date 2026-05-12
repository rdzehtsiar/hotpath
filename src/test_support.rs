// SPDX-License-Identifier: Apache-2.0

use crate::{ContentKind, ParseFileRecord, ParseFileStatus, ParseImportRecord};

pub fn parsed_text_file(path: &str, language: &'static str) -> ParseFileRecord {
    ParseFileRecord {
        path: path.to_owned(),
        language: Some(language),
        content: ContentKind::Text,
        status: ParseFileStatus::Parsed,
        reason: None,
        symbol_count: 0,
        import_count: 0,
    }
}

pub fn parse_import(path: &str, target: &str, kind: &str) -> ParseImportRecord {
    ParseImportRecord {
        path: path.to_owned(),
        target: target.to_owned(),
        kind: kind.to_owned(),
        start_line: 1,
        end_line: 1,
    }
}
