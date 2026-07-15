//! Source-code symbol extraction via tree-sitter (P3.2).
//!
//! Per-language queries capture definitions with capture names `function` /
//! `type` — the capture name becomes the symbol kind. Parse failures and
//! timeouts degrade to plain-text extraction with no symbols; they never fail
//! the record.

use crate::Budgets;
use rsd_caes::{ExtractStatus, SymbolRec};
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    Go,
    C,
}

impl Lang {
    pub fn from_extension(ext: &str) -> Option<Lang> {
        match ext {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
            "go" => Some(Lang::Go),
            "c" | "h" => Some(Lang::C),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
            Lang::C => "c",
        }
    }

    fn language(&self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
        }
    }

    fn query_src(&self) -> &'static str {
        match self {
            Lang::Rust => {
                r#"
                (function_item name: (identifier) @function)
                (struct_item name: (type_identifier) @type)
                (enum_item name: (type_identifier) @type)
                (trait_item name: (type_identifier) @type)
                (mod_item name: (identifier) @type)
                "#
            }
            Lang::Python => {
                r#"
                (function_definition name: (identifier) @function)
                (class_definition name: (identifier) @type)
                "#
            }
            Lang::JavaScript => {
                r#"
                (function_declaration name: (identifier) @function)
                (method_definition name: (property_identifier) @function)
                (class_declaration name: (identifier) @type)
                "#
            }
            Lang::Go => {
                r#"
                (function_declaration name: (identifier) @function)
                (method_declaration name: (field_identifier) @function)
                (type_declaration (type_spec name: (type_identifier) @type))
                "#
            }
            Lang::C => {
                r#"
                (function_definition
                  declarator: (function_declarator declarator: (identifier) @function))
                (struct_specifier name: (type_identifier) @type)
                (enum_specifier name: (type_identifier) @type)
                (type_definition declarator: (type_identifier) @type)
                "#
            }
        }
    }

    fn query(&self) -> &'static Query {
        macro_rules! cached {
            ($cell:ident, $lang:expr) => {{
                static $cell: OnceLock<Query> = OnceLock::new();
                $cell.get_or_init(|| {
                    Query::new(&$lang.language(), $lang.query_src())
                        .expect("static query must compile")
                })
            }};
        }
        match self {
            Lang::Rust => cached!(RUST_Q, Lang::Rust),
            Lang::Python => cached!(PY_Q, Lang::Python),
            Lang::JavaScript => cached!(JS_Q, Lang::JavaScript),
            Lang::Go => cached!(GO_Q, Lang::Go),
            Lang::C => cached!(C_Q, Lang::C),
        }
    }
}

/// Extract definition symbols. Failures degrade to `(no symbols, Complete)` —
/// the text is still fully indexed.
pub fn symbols(lang: Lang, text: &str, budgets: &Budgets) -> (Vec<SymbolRec>, ExtractStatus) {
    let mut parser = Parser::new();
    if parser.set_language(&lang.language()).is_err() {
        return (vec![], ExtractStatus::Complete);
    }
    // Deadline: a pathological file must not pin the worker. The host enforces
    // a hard kill-timeout above this; this is the cooperative layer.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(budgets.parse_timeout_ms);
    let Some(tree) = parser.parse_with_options(
        &mut |byte, _| {
            if byte < text.len() {
                &text.as_bytes()[byte..]
            } else {
                &[]
            }
        },
        None,
        Some(
            tree_sitter::ParseOptions::default()
                .progress_callback(&mut |_| std::time::Instant::now() > deadline),
        ),
    ) else {
        return (vec![], ExtractStatus::Complete);
    };

    let query = lang.query();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let kind = names[cap.index as usize];
            if let Ok(name) = cap.node.utf8_text(text.as_bytes()) {
                out.push(SymbolRec {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    line: cap.node.start_position().row as u32 + 1,
                });
            }
            // Hard cap with headroom so the caller's truncation labels it.
            if out.len() > budgets.max_symbols.saturating_add(1) {
                return (out, ExtractStatus::Complete);
            }
        }
    }
    (out, ExtractStatus::Complete)
}
