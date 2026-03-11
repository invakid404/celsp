//! Semantic tokens for CEL syntax highlighting.

use cel_core::{
    types::{BinaryOp, Expr, UnaryOp},
    SpannedExpr,
};
use lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend};

use crate::document::LineIndex;
#[cfg(not(target_arch = "wasm32"))]
use crate::document::ProtoDocumentState;
use crate::types::is_builtin;

/// Token type indices (must match LEGEND order).
pub mod token_types {
    pub const KEYWORD: u32 = 0;
    pub const NUMBER: u32 = 1;
    pub const STRING: u32 = 2;
    pub const OPERATOR: u32 = 3;
    pub const VARIABLE: u32 = 4;
    pub const FUNCTION: u32 = 5;
    pub const METHOD: u32 = 6;
    pub const PUNCTUATION: u32 = 7;
}

/// Token modifier bit flags.
pub mod token_modifiers {
    pub const DEFAULT_LIBRARY: u32 = 1 << 0;
}

/// Get the semantic tokens legend for capability declaration.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,
            SemanticTokenType::NUMBER,
            SemanticTokenType::STRING,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::new("punctuation"),
        ],
        token_modifiers: vec![SemanticTokenModifier::DEFAULT_LIBRARY],
    }
}

/// A raw token before delta encoding.
#[derive(Debug, Clone)]
struct RawToken {
    start: usize,
    length: usize,
    token_type: u32,
    token_modifiers: u32,
}

/// Collector for semantic tokens.
struct TokenCollector<'a> {
    source: &'a str,
    tokens: Vec<RawToken>,
    /// (start_offset, iter_range_length) for each comprehension.
    /// Used to filter synthetic tokens that overlap the real iter target.
    comp_iter_ranges: Vec<(usize, usize)>,
}

impl<'a> TokenCollector<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            tokens: Vec::new(),
            comp_iter_ranges: Vec::new(),
        }
    }

    fn push(&mut self, start: usize, end: usize, token_type: u32, token_modifiers: u32) {
        if start < end && end <= self.source.len() {
            self.tokens.push(RawToken {
                start,
                length: end - start,
                token_type,
                token_modifiers,
            });
        }
    }

    fn push_punctuation(&mut self, start: usize, len: usize) {
        self.push(start, start + len, token_types::PUNCTUATION, 0);
    }

    /// Find a single character in the source between start and end.
    /// Returns None for out-of-bounds ranges (e.g. synthetic macro spans).
    fn find_char(&self, start: usize, end: usize, c: char) -> Option<usize> {
        if start >= end || end > self.source.len() {
            return None;
        }
        self.source[start..end].find(c).map(|i| start + i)
    }

    /// Advance past ASCII whitespace.
    fn skip_whitespace(&self, start: usize, end: usize) -> usize {
        let mut cursor = start;
        while cursor < end
            && self
                .source
                .as_bytes()
                .get(cursor)
                .map_or(false, |b| b.is_ascii_whitespace())
        {
            cursor += 1;
        }
        cursor
    }

    fn visit_expr(&mut self, expr: &SpannedExpr) {
        // Skip expressions with spans outside the source — these are
        // synthetic nodes from macro expansion (exists, all, filter, map).
        if expr.span.start > expr.span.end || expr.span.end > self.source.len() {
            return;
        }
        match &expr.node {
            Expr::Null => {
                self.push(expr.span.start, expr.span.end, token_types::KEYWORD, 0);
            }
            Expr::Bool(_) => {
                self.push(expr.span.start, expr.span.end, token_types::KEYWORD, 0);
            }
            Expr::Int(_) | Expr::UInt(_) | Expr::Float(_) => {
                self.push(expr.span.start, expr.span.end, token_types::NUMBER, 0);
            }
            Expr::String(_) | Expr::Bytes(_) => {
                self.push(expr.span.start, expr.span.end, token_types::STRING, 0);
            }
            Expr::Ident(name) | Expr::RootIdent(name) => {
                let modifiers = if is_builtin(name) {
                    token_modifiers::DEFAULT_LIBRARY
                } else {
                    0
                };
                self.push(
                    expr.span.start,
                    expr.span.end,
                    token_types::VARIABLE,
                    modifiers,
                );
            }
            Expr::List(items) => {
                // Opening bracket
                self.push_punctuation(expr.span.start, 1);

                for item in items {
                    self.visit_expr(&item.expr);
                }

                // Commas between items
                for window in items.windows(2) {
                    let gap_start = window[0].expr.span.end;
                    let gap_end = window[1].expr.span.start;
                    if let Some(pos) = self.find_char(gap_start, gap_end, ',') {
                        self.push_punctuation(pos, 1);
                    }
                }

                // Closing bracket
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Map(entries) => {
                // Opening brace
                self.push_punctuation(expr.span.start, 1);

                for entry in entries {
                    self.visit_expr(&entry.key);

                    // Colon between key and value
                    let gap_start = entry.key.span.end;
                    let gap_end = entry.value.span.start;
                    if let Some(pos) = self.find_char(gap_start, gap_end, ':') {
                        self.push_punctuation(pos, 1);
                    }

                    self.visit_expr(&entry.value);
                }

                // Commas between entries
                for window in entries.windows(2) {
                    let gap_start = window[0].value.span.end;
                    let gap_end = window[1].key.span.start;
                    if let Some(pos) = self.find_char(gap_start, gap_end, ',') {
                        self.push_punctuation(pos, 1);
                    }
                }

                // Closing brace
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Unary { op, expr: inner } => {
                let op_len = match op {
                    UnaryOp::Neg | UnaryOp::Not => 1,
                };
                self.push(
                    expr.span.start,
                    expr.span.start + op_len,
                    token_types::OPERATOR,
                    0,
                );
                self.visit_expr(inner);
            }
            Expr::Binary { op, left, right } => {
                self.visit_expr(left);

                let op_start = left.span.end;
                let op_end = right.span.start;
                if let Some((op_text, op_offset)) = self.find_operator(op_start, op_end, *op) {
                    self.push(
                        op_start + op_offset,
                        op_start + op_offset + op_text.len(),
                        token_types::OPERATOR,
                        0,
                    );
                }

                self.visit_expr(right);
            }
            Expr::Ternary {
                cond,
                then_expr,
                else_expr,
            } => {
                self.visit_expr(cond);

                // Question mark
                let gap1_start = cond.span.end;
                let gap1_end = then_expr.span.start;
                if let Some(pos) = self.find_char(gap1_start, gap1_end, '?') {
                    self.push_punctuation(pos, 1);
                }

                self.visit_expr(then_expr);

                // Colon
                let gap2_start = then_expr.span.end;
                let gap2_end = else_expr.span.start;
                if let Some(pos) = self.find_char(gap2_start, gap2_end, ':') {
                    self.push_punctuation(pos, 1);
                }

                self.visit_expr(else_expr);
            }
            Expr::Member {
                expr: inner, field, ..
            } => {
                self.visit_expr(inner);

                // Dot
                let dot_pos = inner.span.end;
                if dot_pos < expr.span.end {
                    self.push_punctuation(dot_pos, 1);
                }

                // Field
                let field_start = expr.span.end - field.len();
                self.push(field_start, expr.span.end, token_types::VARIABLE, 0);
            }
            Expr::Index {
                expr: inner, index, ..
            } => {
                self.visit_expr(inner);

                // Opening bracket - find it after the inner expression
                if let Some(pos) = self.find_char(inner.span.end, index.span.start, '[') {
                    self.push_punctuation(pos, 1);
                }

                self.visit_expr(index);

                // Closing bracket
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Call { expr: callee, args } => {
                match &callee.node {
                    Expr::Ident(name) => {
                        let modifiers = if is_builtin(name) {
                            token_modifiers::DEFAULT_LIBRARY
                        } else {
                            0
                        };
                        self.push(
                            callee.span.start,
                            callee.span.end,
                            token_types::FUNCTION,
                            modifiers,
                        );
                    }
                    Expr::Member {
                        expr: obj, field, ..
                    } => {
                        self.visit_expr(obj);

                        // Dot
                        let dot_pos = obj.span.end;
                        if dot_pos < callee.span.end {
                            self.push_punctuation(dot_pos, 1);
                        }

                        let field_start = callee.span.end - field.len();
                        let modifiers = if is_builtin(field) {
                            token_modifiers::DEFAULT_LIBRARY
                        } else {
                            0
                        };
                        self.push(field_start, callee.span.end, token_types::METHOD, modifiers);
                    }
                    _ => {
                        self.visit_expr(callee);
                    }
                }

                // Opening parenthesis
                if let Some(pos) = self.find_char(callee.span.end, expr.span.end, '(') {
                    self.push_punctuation(pos, 1);
                }

                for arg in args {
                    self.visit_expr(arg);
                }

                // Commas between arguments
                for window in args.windows(2) {
                    let gap_start = window[0].span.end;
                    let gap_end = window[1].span.start;
                    if let Some(pos) = self.find_char(gap_start, gap_end, ',') {
                        self.push_punctuation(pos, 1);
                    }
                }

                // Closing parenthesis
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Struct { type_name, fields } => {
                // Type name
                self.visit_expr(type_name);

                // Opening brace
                if let Some(pos) = self.find_char(type_name.span.end, expr.span.end, '{') {
                    self.push_punctuation(pos, 1);
                }

                for field in fields {
                    // Field name - find it before the colon
                    // We need to locate the field name in the source
                    let field_end = field.value.span.start;
                    if let Some(colon_pos) = self.find_char(type_name.span.end, field_end, ':') {
                        // Field name is just before the colon (with possible whitespace)
                        let field_name_end = colon_pos;
                        let field_name_start = field_name_end.saturating_sub(field.name.len());
                        self.push(field_name_start, field_name_end, token_types::VARIABLE, 0);
                        // Colon
                        self.push_punctuation(colon_pos, 1);
                    }

                    self.visit_expr(&field.value);
                }

                // Commas between fields
                for window in fields.windows(2) {
                    let gap_start = window[0].value.span.end;
                    let gap_end = window[1].value.span.start;
                    if let Some(pos) = self.find_char(gap_start, gap_end, ',') {
                        self.push_punctuation(pos, 1);
                    }
                }

                // Closing brace
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Comprehension(comp) => {
                // Comprehensions are macro expansions (exists, all, filter, map).
                // The AST loses the original call structure, so we reconstruct
                // tokens for the macro name, punctuation, and iteration variables
                // from the source text and ComprehensionData fields.
                //
                // Record the comprehension start + iter_range length so we can
                // filter out synthetic tokens that overlap the real iter target.
                let iter_len = comp
                    .iter_range
                    .span
                    .end
                    .saturating_sub(comp.iter_range.span.start);
                self.comp_iter_ranges.push((expr.span.start, iter_len));

                // Receiver / iter_range (e.g., "labels")
                self.visit_expr(&comp.iter_range);

                let after_iter = comp.iter_range.span.end;
                let comp_end = expr.span.end;

                // Dot between receiver and macro name
                if let Some(dot_pos) = self.find_char(after_iter, comp_end, '.') {
                    self.push_punctuation(dot_pos, 1);

                    // Opening paren — macro name is between dot+1 and paren
                    if let Some(paren_pos) = self.find_char(dot_pos + 1, comp_end, '(') {
                        let macro_start = dot_pos + 1;
                        if macro_start < paren_pos {
                            self.push(
                                macro_start,
                                paren_pos,
                                token_types::FUNCTION,
                                token_modifiers::DEFAULT_LIBRARY,
                            );
                        }
                        self.push_punctuation(paren_pos, 1);

                        // Iteration variable(s) and commas
                        let mut cursor = paren_pos + 1;

                        // First iter var
                        if !comp.iter_var.is_empty() {
                            cursor = self.skip_whitespace(cursor, comp_end);
                            let var_end = cursor + comp.iter_var.len();
                            if var_end <= comp_end && &self.source[cursor..var_end] == comp.iter_var
                            {
                                self.push(cursor, var_end, token_types::VARIABLE, 0);
                                cursor = var_end;
                            }
                        }

                        // Second iter var (3-arg macros like all(k, v, cond))
                        if !comp.iter_var2.is_empty() {
                            if let Some(comma_pos) = self.find_char(cursor, comp_end, ',') {
                                self.push_punctuation(comma_pos, 1);
                                cursor = self.skip_whitespace(comma_pos + 1, comp_end);
                                let var_end = cursor + comp.iter_var2.len();
                                if var_end <= comp_end
                                    && &self.source[cursor..var_end] == comp.iter_var2
                                {
                                    self.push(cursor, var_end, token_types::VARIABLE, 0);
                                    cursor = var_end;
                                }
                            }
                        }

                        // Comma before the predicate/body expression
                        if let Some(comma_pos) = self.find_char(cursor, comp_end, ',') {
                            self.push_punctuation(comma_pos, 1);
                        }
                    }
                }

                // The user's predicate/transform lives inside loop_step
                self.visit_expr(&comp.loop_step);

                // Closing paren
                if comp_end > 0 {
                    self.push_punctuation(comp_end - 1, 1);
                }
            }
            Expr::MemberTestOnly { expr: inner, field } => {
                // MemberTestOnly is the expansion of has(expr.field)
                // The outer span covers the full `has(expr.field)` source text

                // "has" keyword
                let has_end = expr.span.start + 3;
                self.push(
                    expr.span.start,
                    has_end,
                    token_types::FUNCTION,
                    token_modifiers::DEFAULT_LIBRARY,
                );

                // Opening parenthesis
                if let Some(pos) = self.find_char(has_end, inner.span.start, '(') {
                    self.push_punctuation(pos, 1);
                }

                // Inner expression (e.g., `this` or `msg`)
                self.visit_expr(inner);

                // Dot between inner expression and field
                if let Some(pos) = self.find_char(inner.span.end, expr.span.end, '.') {
                    self.push_punctuation(pos, 1);
                }

                // Field name
                let field_start = expr.span.end - 1 - field.len();
                self.push(
                    field_start,
                    field_start + field.len(),
                    token_types::VARIABLE,
                    0,
                );

                // Closing parenthesis
                self.push_punctuation(expr.span.end - 1, 1);
            }
            Expr::Bind { init, body, .. } => {
                // Bind is synthetic from cel.bind() - visit sub-expressions
                self.visit_expr(init);
                self.visit_expr(body);
            }
            Expr::Error => {
                // Skip error nodes
            }
        }
    }

    fn find_operator(
        &self,
        start: usize,
        end: usize,
        op: BinaryOp,
    ) -> Option<(&'static str, usize)> {
        let op_str = match op {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::In => "in",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        };

        if start >= end || end > self.source.len() {
            return None;
        }
        self.source[start..end]
            .find(op_str)
            .map(|offset| (op_str, offset))
    }

    fn into_semantic_tokens(mut self, line_index: &LineIndex) -> Vec<SemanticToken> {
        // Sort by position.
        self.tokens.sort_by_key(|t| t.start);

        // Comprehension (macro) expansion produces synthetic tokens at the
        // comprehension's start offset that overlap the real iter_range
        // token. At each comprehension start, keep only the token whose
        // length matches the iter_range — the rest are synthetic junk.
        if !self.comp_iter_ranges.is_empty() {
            self.tokens.retain(|t| {
                if let Some(&(_, iter_len)) = self
                    .comp_iter_ranges
                    .iter()
                    .find(|(start, _)| *start == t.start)
                {
                    t.length == iter_len
                } else {
                    true
                }
            });
        }

        let mut result = Vec::with_capacity(self.tokens.len());
        let mut prev_line = 0u32;
        let mut prev_start = 0u32;

        for token in &self.tokens {
            let pos = line_index.offset_to_position(token.start);
            let delta_line = pos.line - prev_line;
            let delta_start = if delta_line == 0 {
                pos.character - prev_start
            } else {
                pos.character
            };

            result.push(SemanticToken {
                delta_line,
                delta_start,
                length: token.length as u32,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers,
            });

            prev_line = pos.line;
            prev_start = pos.character;
        }

        result
    }
}

/// Generate semantic tokens for a parsed expression.
pub fn tokens_for_ast(line_index: &LineIndex, ast: &SpannedExpr) -> Vec<SemanticToken> {
    let mut collector = TokenCollector::new(line_index.source());
    collector.visit_expr(ast);
    collector.into_semantic_tokens(line_index)
}

#[cfg(not(target_arch = "wasm32"))]
/// Generate semantic tokens for a proto document containing CEL regions.
///
/// This processes all CEL regions, generates tokens for each, and maps
/// them to host document coordinates.
pub fn tokens_for_proto(state: &ProtoDocumentState) -> Vec<SemanticToken> {
    let mut all_tokens: Vec<RawToken> = Vec::new();

    for region_state in &state.regions {
        if let Some(ast) = &region_state.ast {
            // Generate tokens with CEL-local offsets
            let mut collector = TokenCollector::new(&region_state.region.source);
            collector.visit_expr(ast);

            // Convert to host coordinates
            for token in collector.tokens {
                let host_start = region_state.mapper.to_host(token.start);
                all_tokens.push(RawToken {
                    start: host_start,
                    length: token.length,
                    token_type: token.token_type,
                    token_modifiers: token.token_modifiers,
                });
            }
        }
    }

    // Sort by position and convert to delta-encoded format
    all_tokens.sort_by_key(|t| t.start);
    encode_tokens(&all_tokens, &state.line_index)
}

/// Convert raw tokens to delta-encoded semantic tokens.
#[cfg(not(target_arch = "wasm32"))]
fn encode_tokens(tokens: &[RawToken], line_index: &LineIndex) -> Vec<SemanticToken> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for token in tokens {
        let pos = line_index.offset_to_position(token.start);
        let delta_line = pos.line - prev_line;
        let delta_start = if delta_line == 0 {
            pos.character - prev_start
        } else {
            pos.character
        };

        result.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.length as u32,
            token_type: token.token_type,
            token_modifiers_bitset: token.token_modifiers,
        });

        prev_line = pos.line;
        prev_start = pos.character;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel_core::parse;

    #[test]
    fn legend_has_expected_types() {
        let leg = legend();
        assert!(leg.token_types.contains(&SemanticTokenType::KEYWORD));
        assert!(leg.token_types.contains(&SemanticTokenType::NUMBER));
        assert!(leg.token_types.contains(&SemanticTokenType::FUNCTION));
        assert_eq!(leg.token_types.len(), 8); // Now includes punctuation
    }

    #[test]
    fn tokens_for_simple_expression() {
        let source = "1 + 2";
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);
        // Should have: number(1), operator(+), number(2)
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn tokens_for_function_call() {
        let source = "size(x)";
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);
        // Should have: function(size), (, variable(x), )
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].token_type, token_types::FUNCTION);
        assert_eq!(
            tokens[0].token_modifiers_bitset,
            token_modifiers::DEFAULT_LIBRARY
        );
    }

    #[test]
    fn tokens_for_list() {
        let source = "[1, 2]";
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);
        // Should have: [, number(1), comma, number(2), ]
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[0].token_type, token_types::PUNCTUATION); // [
        assert_eq!(tokens[4].token_type, token_types::PUNCTUATION); // ]
    }

    #[test]
    fn tokens_for_ternary() {
        let source = "a ? b : c";
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);
        // Should have: variable(a), ?, variable(b), :, variable(c)
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[1].token_type, token_types::PUNCTUATION); // ?
        assert_eq!(tokens[3].token_type, token_types::PUNCTUATION); // :
    }

    #[test]
    fn tokens_for_has_macro() {
        let source = "has(msg.field)";
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);
        // Should have: function(has), (, variable(msg), ., variable(field), )
        assert_eq!(tokens.len(), 6);
        assert_eq!(tokens[0].token_type, token_types::FUNCTION); // has
        assert_eq!(
            tokens[0].token_modifiers_bitset,
            token_modifiers::DEFAULT_LIBRARY
        );
        assert_eq!(tokens[1].token_type, token_types::PUNCTUATION); // (
        assert_eq!(tokens[2].token_type, token_types::VARIABLE); // msg
        assert_eq!(tokens[3].token_type, token_types::PUNCTUATION); // .
        assert_eq!(tokens[4].token_type, token_types::VARIABLE); // field
        assert_eq!(tokens[5].token_type, token_types::PUNCTUATION); // )
    }

    #[test]
    fn tokens_for_exists_macro_no_overlap() {
        // Regression: the comprehension expansion creates a synthetic Ident("l")
        // whose span overlaps with the start of "labels". Without dedup, this
        // produces a 1-char token at the same offset as the 6-char "labels"
        // token, causing the first letter to render in a different color.
        let source = r#"labels.exists(l, l.startsWith("prod"))"#;
        let result = parse(source);
        let ast = result.ast.unwrap();
        let line_index = LineIndex::new(source.to_string());

        let tokens = tokens_for_ast(&line_index, &ast);

        // labels  .  exists  (  l  ,  l  .  startsWith  (  "prod"  )  )
        let expected_types = vec![
            (token_types::VARIABLE, 6, "labels"),
            (token_types::PUNCTUATION, 1, "."),
            (token_types::FUNCTION, 6, "exists"),
            (token_types::PUNCTUATION, 1, "("),
            (token_types::VARIABLE, 1, "l"),
            (token_types::PUNCTUATION, 1, ","),
            (token_types::VARIABLE, 1, "l"),
            (token_types::PUNCTUATION, 1, "."),
            (token_types::METHOD, 10, "startsWith"),
            (token_types::PUNCTUATION, 1, "("),
            (token_types::STRING, 6, "\"prod\""),
            (token_types::PUNCTUATION, 1, ")"),
            (token_types::PUNCTUATION, 1, ")"),
        ];
        assert_eq!(
            tokens.len(),
            expected_types.len(),
            "expected {} tokens, got {}",
            expected_types.len(),
            tokens.len()
        );
        for (i, (tt, len, label)) in expected_types.iter().enumerate() {
            assert_eq!(
                tokens[i].token_type, *tt,
                "token {i} ({label}): expected type {tt}, got {}",
                tokens[i].token_type
            );
            assert_eq!(
                tokens[i].length, *len as u32,
                "token {i} ({label}): expected length {len}, got {}",
                tokens[i].length
            );
        }
    }
}
