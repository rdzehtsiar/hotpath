// SPDX-License-Identifier: Apache-2.0

use crate::languages::{
    UniversalCodeMetricsInput, UniversalControlFlowKind, UniversalControlFlowNode,
};

#[derive(Debug, Default)]
/// Computes language-neutral complexity metrics from universal source facts.
pub struct CodeMetricsAnalyzer;

impl CodeMetricsAnalyzer {
    pub fn new() -> Self {
        Self
    }

    pub fn analyze(&self, input: &UniversalCodeMetricsInput) -> CodeMetricsResult {
        let mut cognitive_complexity = 0;
        let mut max_function_complexity = 0;

        for function in &input.functions {
            let function_complexity = complexity_for_nodes(&function.control_flow, 0);
            cognitive_complexity += function_complexity;
            max_function_complexity = max_function_complexity.max(function_complexity);
        }

        CodeMetricsResult {
            cognitive_complexity,
            max_function_complexity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeMetricsResult {
    pub cognitive_complexity: u64,
    pub max_function_complexity: u64,
}

fn complexity_for_nodes(nodes: &[UniversalControlFlowNode], nesting: u64) -> u64 {
    nodes
        .iter()
        .map(|node| complexity_for_node(node, nesting))
        .sum()
}

fn complexity_for_node(node: &UniversalControlFlowNode, nesting: u64) -> u64 {
    let own_score = match node.kind {
        UniversalControlFlowKind::Branch
        | UniversalControlFlowKind::ElseIf
        | UniversalControlFlowKind::Loop
        | UniversalControlFlowKind::Switch
        | UniversalControlFlowKind::Case => 1 + nesting,
        UniversalControlFlowKind::BooleanChain | UniversalControlFlowKind::Jump => 1,
    };

    let child_nesting = match node.kind {
        UniversalControlFlowKind::BooleanChain | UniversalControlFlowKind::Jump => nesting,
        _ => nesting + 1,
    };

    own_score + complexity_for_nodes(&node.children, child_nesting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::{
        UniversalCodeMetricsInput, UniversalControlFlowKind, UniversalControlFlowNode,
        UniversalFunction, UniversalFunctionKind,
    };

    #[test]
    fn empty_input_has_zero_complexity() {
        let result = CodeMetricsAnalyzer::new().analyze(&UniversalCodeMetricsInput::default());

        assert_eq!(result.cognitive_complexity, 0);
        assert_eq!(result.max_function_complexity, 0);
    }

    #[test]
    fn nested_control_flow_adds_nesting_penalty() {
        let input = UniversalCodeMetricsInput {
            functions: vec![UniversalFunction {
                name: "main".to_owned(),
                kind: UniversalFunctionKind::Function,
                control_flow: vec![node(
                    UniversalControlFlowKind::Branch,
                    vec![node(UniversalControlFlowKind::Loop, Vec::new())],
                )],
            }],
        };

        let result = CodeMetricsAnalyzer::new().analyze(&input);

        assert_eq!(result.cognitive_complexity, 3);
        assert_eq!(result.max_function_complexity, 3);
    }

    #[test]
    fn file_complexity_is_sum_of_function_complexity() {
        let input = UniversalCodeMetricsInput {
            functions: vec![
                UniversalFunction {
                    name: "a".to_owned(),
                    kind: UniversalFunctionKind::Function,
                    control_flow: vec![node(UniversalControlFlowKind::BooleanChain, Vec::new())],
                },
                UniversalFunction {
                    name: "b".to_owned(),
                    kind: UniversalFunctionKind::Function,
                    control_flow: vec![node(UniversalControlFlowKind::Jump, Vec::new())],
                },
            ],
        };

        let result = CodeMetricsAnalyzer::new().analyze(&input);

        assert_eq!(result.cognitive_complexity, 2);
        assert_eq!(result.max_function_complexity, 1);
    }

    fn node(
        kind: UniversalControlFlowKind,
        children: Vec<UniversalControlFlowNode>,
    ) -> UniversalControlFlowNode {
        UniversalControlFlowNode { kind, children }
    }
}
