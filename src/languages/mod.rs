// SPDX-License-Identifier: Apache-2.0

pub mod go;

use crate::pipeline::file_analyzer::AnalyzedFile;

pub trait LanguageParser: Send + Sync {
    fn language_id(&self) -> &'static str;

    fn recognize(&self, file: &AnalyzedFile) -> ParserRecognition;

    fn parse(&self, file: &AnalyzedFile) -> ParserOutput;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParserRecognition {
    Recognized,
    NotRecognized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserOutput {
    pub language_id: String,
    pub symbols: Vec<UniversalSymbol>,
    pub references: Vec<UniversalReference>,
    pub diagnostics: Vec<ParserDiagnostic>,
    pub limitations: Vec<ParserLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniversalSymbol {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniversalReference {
    pub target: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserDiagnostic {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserLimitation {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        LanguageParser, ParserOutput, ParserRecognition, UniversalReference, UniversalSymbol,
    };
    use crate::pipeline::file_analyzer::{AnalyzedFile, FileAnalyzerOptions};

    struct MockParser;

    impl LanguageParser for MockParser {
        fn language_id(&self) -> &'static str {
            "mock"
        }

        fn recognize(&self, file: &AnalyzedFile) -> ParserRecognition {
            if file.path().ends_with("main.mock") {
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
                    target: "fmt".to_owned(),
                    kind: "import".to_owned(),
                }],
                diagnostics: Vec::new(),
                limitations: Vec::new(),
            }
        }
    }

    #[test]
    fn parser_abstraction_receives_file_object_and_returns_universal_output() {
        let parser = MockParser;
        let file = AnalyzedFile::new(PathBuf::from("main.mock"), FileAnalyzerOptions::default());

        assert_eq!(parser.recognize(&file), ParserRecognition::Recognized);
        let output = parser.parse(&file);

        assert_eq!(output.language_id, "mock");
        assert_eq!(output.symbols[0].name, "main");
        assert_eq!(output.references[0].kind, "import");
    }
}
