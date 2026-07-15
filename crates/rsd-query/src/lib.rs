//! rsd-query: RQL v1 (P4.2) — a versioned, documented subset of the Spotlight
//! predicate grammar plus rsd extensions, with an honest error for everything
//! outside it (`UnsupportedPredicate`, never silent misinterpretation).
//!
//! Grammar v1 (see DIVERGENCES.md for the compatibility posture):
//!   expr     := or
//!   or       := and ("||" and)*
//!   and      := unary ("&&" unary)*
//!   unary    := "!" unary | "(" expr ")" | pred
//!   pred     := attr op value | "InRange(" attr "," value "," value ")"
//!             | bareword-or-string              (full-text content search)
//!   op       := "==" | "!=" | "<" | "<=" | ">" | ">="
//!   value    := string-with-modifiers | number | $time.now | $time.now(±N)
//!   string   := '"' ... '"' [c][d]   (c: case-insensitive; d accepted,
//!                                     diacritic folding is a documented gap)
//!
//! Attributes v1: kMDItemFSName, kMDItemFSSize, kMDItemDisplayName,
//! kMDItemContentModificationDate / kMDItemFSContentChangeDate,
//! kMDItemTextContent, kRSDIndexState, kRSDSymbols (rsd extension).

use rsd_catalog::{Catalog, ObjectKind, ObjectRecord};
use rsd_lexical::LexicalReader;
use std::collections::HashSet;

pub const GRAMMAR_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("parse error at {pos}: {msg}")]
    Parse { pos: usize, msg: String },
    #[error("unsupported predicate: {0} (grammar v{GRAMMAR_VERSION})")]
    Unsupported(String),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
    #[error("lexical: {0}")]
    Lexical(#[from] rsd_lexical::LexicalError),
    #[error("query needs a lexical plane (text predicate) but none is open")]
    NoLexicalPlane,
}

pub type Result<T> = std::result::Result<T, QueryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attr {
    FsName,
    FsSize,
    ModificationDate,
    TextContent,
    IndexState,
    Symbols,
}

impl Attr {
    fn parse(name: &str) -> Option<Attr> {
        match name {
            "kMDItemFSName" | "kMDItemDisplayName" => Some(Attr::FsName),
            "kMDItemFSSize" => Some(Attr::FsSize),
            "kMDItemContentModificationDate" | "kMDItemFSContentChangeDate" => {
                Some(Attr::ModificationDate)
            }
            "kMDItemTextContent" => Some(Attr::TextContent),
            "kRSDIndexState" => Some(Attr::IndexState),
            "kRSDSymbols" => Some(Attr::Symbols),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str {
        text: String,
        ci: bool,
    },
    Num(f64),
    /// Nanoseconds since epoch.
    Time(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cmp {
        attr: Attr,
        op: Op,
        value: Value,
    },
    InRange {
        attr: Attr,
        lo: Value,
        hi: Value,
    },
    /// Bare full-text content search.
    Text {
        terms: String,
    },
}

// ------------------------------------------------------------------- parser

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    now_ns: i64,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(QueryError::Parse {
            pos: self.pos,
            msg: msg.into(),
        })
    }

    fn ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, s: &str) -> bool {
        self.ws();
        if self.src[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.bytes.get(self.pos).copied()
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        let mut terms = vec![self.parse_and()?];
        while self.eat("||") {
            terms.push(self.parse_and()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("nonempty")
        } else {
            Expr::Or(terms)
        })
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut terms = vec![self.parse_unary()?];
        while self.eat("&&") {
            terms.push(self.parse_unary()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("nonempty")
        } else {
            Expr::And(terms)
        })
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat("!") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat("(") {
            let e = self.parse_expr()?;
            if !self.eat(")") {
                return self.err("expected ')'");
            }
            return Ok(e);
        }
        self.parse_pred()
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.pos;
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_alphanumeric() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.pos > start {
            Some(self.src[start..self.pos].to_string())
        } else {
            None
        }
    }

    fn parse_pred(&mut self) -> Result<Expr> {
        self.ws();
        // Quoted bare string => content search.
        if self.peek() == Some(b'"') {
            if let Value::Str { text, .. } = self.parse_string()? {
                return Ok(Expr::Text { terms: text });
            }
            unreachable!();
        }
        let save = self.pos;
        let Some(word) = self.ident() else {
            return self.err("expected predicate");
        };
        if word == "InRange" {
            if !self.eat("(") {
                return self.err("expected '(' after InRange");
            }
            let attr_name = self.ident().ok_or_else(|| QueryError::Parse {
                pos: self.pos,
                msg: "expected attribute in InRange".into(),
            })?;
            let attr = Attr::parse(&attr_name)
                .ok_or_else(|| QueryError::Unsupported(format!("attribute {attr_name}")))?;
            if !self.eat(",") {
                return self.err("expected ','");
            }
            let lo = self.parse_value()?;
            if !self.eat(",") {
                return self.err("expected ','");
            }
            let hi = self.parse_value()?;
            if !self.eat(")") {
                return self.err("expected ')'");
            }
            return Ok(Expr::InRange { attr, lo, hi });
        }
        if let Some(attr) = Attr::parse(&word) {
            let op = self.parse_op()?;
            let value = self.parse_value()?;
            return Ok(Expr::Cmp { attr, op, value });
        }
        if word.starts_with("kMD") || word.starts_with("kRSD") {
            return Err(QueryError::Unsupported(format!("attribute {word}")));
        }
        // Bare word => content search.
        self.pos = save;
        let start = self.pos;
        while self.pos < self.bytes.len() && !self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        Ok(Expr::Text {
            terms: self.src[start..self.pos].to_string(),
        })
    }

    fn parse_op(&mut self) -> Result<Op> {
        for (s, op) in [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if self.eat(s) {
                return Ok(op);
            }
        }
        self.err("expected comparison operator")
    }

    fn parse_string(&mut self) -> Result<Value> {
        self.ws();
        if self.bytes.get(self.pos) != Some(&b'"') {
            return self.err("expected string");
        }
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'"' {
            self.pos += 1;
        }
        if self.pos >= self.bytes.len() {
            return self.err("unterminated string");
        }
        let text = self.src[start..self.pos].to_string();
        self.pos += 1;
        // Modifiers: c (case-insensitive), d (accepted; folding is a
        // documented divergence), w (unsupported).
        let mut ci = false;
        while let Some(b) = self.bytes.get(self.pos) {
            match b {
                b'c' => {
                    ci = true;
                    self.pos += 1;
                }
                b'd' => {
                    self.pos += 1;
                }
                b'w' => return Err(QueryError::Unsupported("modifier 'w'".into())),
                _ => break,
            }
        }
        Ok(Value::Str { text, ci })
    }

    fn parse_value(&mut self) -> Result<Value> {
        self.ws();
        match self.peek() {
            Some(b'"') => self.parse_string(),
            Some(b'$') => {
                if !self.eat("$time.now") {
                    return Err(QueryError::Unsupported(
                        "only $time.now[(±N)] is supported in v1".into(),
                    ));
                }
                let mut t = self.now_ns;
                if self.eat("(") {
                    let n = self.parse_number()?;
                    if !self.eat(")") {
                        return self.err("expected ')'");
                    }
                    t += (n * 1e9) as i64;
                }
                Ok(Value::Time(t))
            }
            Some(c) if c == b'-' || c == b'+' || c.is_ascii_digit() => {
                Ok(Value::Num(self.parse_number()?))
            }
            _ => self.err("expected value"),
        }
    }

    fn parse_number(&mut self) -> Result<f64> {
        self.ws();
        let start = self.pos;
        if matches!(self.bytes.get(self.pos), Some(b'-') | Some(b'+')) {
            self.pos += 1;
        }
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'.')
        {
            self.pos += 1;
        }
        self.src[start..self.pos]
            .parse()
            .map_err(|_| QueryError::Parse {
                pos: start,
                msg: "expected number".into(),
            })
    }
}

pub fn parse(src: &str) -> Result<Expr> {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    parse_at(src, now_ns)
}

pub fn parse_at(src: &str, now_ns: i64) -> Result<Expr> {
    let mut p = Parser {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        now_ns,
    };
    let e = p.parse_expr()?;
    p.ws();
    if p.pos != src.len() {
        return p.err("trailing input");
    }
    Ok(e)
}

// ----------------------------------------------------------------- executor

/// Simple `*` glob with optional case folding.
fn glob_match(pattern: &str, text: &str, ci: bool) -> bool {
    let (p, t) = if ci {
        (pattern.to_lowercase(), text.to_lowercase())
    } else {
        (pattern.to_string(), text.to_string())
    };
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
            (Some(pc), Some(tc)) if pc == tc => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    inner(p.as_bytes(), t.as_bytes())
}

pub struct QueryEngine<'a> {
    pub catalog: &'a Catalog,
    pub lexical: Option<&'a LexicalReader>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub oid: u64,
    pub path: String,
}

/// EXPLAIN output: which strategy ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Pure text query answered directly by the lexical plane.
    LexicalDirect,
    /// Catalog scan with text-set membership (mixed predicates).
    CatalogScan { text_sets: usize },
}

impl<'a> QueryEngine<'a> {
    pub fn plan(&self, expr: &Expr) -> Plan {
        if matches!(expr, Expr::Text { .. }) {
            Plan::LexicalDirect
        } else {
            Plan::CatalogScan {
                text_sets: count_text(expr),
            }
        }
    }

    pub fn run(&self, expr: &Expr, scope: Option<&str>) -> Result<Vec<Hit>> {
        // Fast path: pure text query, no scope => lexical plane directly.
        if let (Expr::Text { terms }, None) = (expr, scope) {
            let lex = self.lexical.ok_or(QueryError::NoLexicalPlane)?;
            let oids = lex.search_content(terms, false, self.limit)?;
            let mut hits = Vec::with_capacity(oids.len());
            for oid in oids {
                // Liveness + path resolution through the catalog: renamed
                // files answer with their CURRENT path; dead oids drop out.
                if let Some(rec) = self.catalog.get_object(oid)? {
                    if let Some(p) = rec.entry_paths.first() {
                        hits.push(Hit {
                            oid,
                            path: p.clone(),
                        });
                    }
                }
            }
            return Ok(hits);
        }

        // General path: pre-resolve every text predicate to an oid set, then
        // scan the (scoped) catalog evaluating the expression per entry.
        let mut sets: Vec<HashSet<u64>> = Vec::new();
        collect_text_sets(expr, self.lexical, self.limit, &mut sets)?;
        let mut sets_iter = sets.into_iter();
        let expr = bind_text_sets(expr, &mut sets_iter);

        let mut hits = Vec::new();
        for (path, _) in self.catalog.listing()? {
            if let Some(prefix) = scope {
                if !path.starts_with(&format!("{}/", prefix.trim_end_matches('/'))) {
                    continue;
                }
            }
            let Some((oid, rec)) = self.catalog.get_by_path(&path)? else {
                continue;
            };
            if rec.kind == ObjectKind::Dir {
                continue;
            }
            if eval(&expr, oid, &path, &rec) {
                hits.push(Hit { oid, path });
                if hits.len() >= self.limit {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

fn count_text(e: &Expr) -> usize {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().map(count_text).sum(),
        Expr::Not(b) => count_text(b),
        Expr::Text { .. } => 1,
        Expr::Cmp {
            attr: Attr::TextContent | Attr::Symbols,
            ..
        } => 1,
        _ => 0,
    }
}

/// Bound expression: text predicates replaced by membership sets.
#[derive(Debug, Clone)]
enum Bound {
    And(Vec<Bound>),
    Or(Vec<Bound>),
    Not(Box<Bound>),
    Cmp { attr: Attr, op: Op, value: Value },
    InRange { attr: Attr, lo: Value, hi: Value },
    TextSet(HashSet<u64>),
}

fn collect_text_sets(
    e: &Expr,
    lex: Option<&LexicalReader>,
    limit: usize,
    out: &mut Vec<HashSet<u64>>,
) -> Result<()> {
    match e {
        Expr::And(v) | Expr::Or(v) => {
            for x in v {
                collect_text_sets(x, lex, limit, out)?;
            }
        }
        Expr::Not(b) => collect_text_sets(b, lex, limit, out)?,
        Expr::Text { terms } => {
            let lex = lex.ok_or(QueryError::NoLexicalPlane)?;
            out.push(
                lex.search_content(terms, false, limit.max(65_536))?
                    .into_iter()
                    .collect(),
            );
        }
        Expr::Cmp {
            attr: Attr::TextContent,
            value: Value::Str { text, .. },
            ..
        } => {
            let lex = lex.ok_or(QueryError::NoLexicalPlane)?;
            out.push(
                lex.search_content(text, false, limit.max(65_536))?
                    .into_iter()
                    .collect(),
            );
        }
        Expr::Cmp {
            attr: Attr::Symbols,
            value: Value::Str { text, .. },
            ..
        } => {
            let lex = lex.ok_or(QueryError::NoLexicalPlane)?;
            out.push(
                lex.search_symbols(text, limit.max(65_536))?
                    .into_iter()
                    .collect(),
            );
        }
        _ => {}
    }
    Ok(())
}

fn bind_text_sets(e: &Expr, sets: &mut impl Iterator<Item = HashSet<u64>>) -> Bound {
    match e {
        Expr::And(v) => Bound::And(v.iter().map(|x| bind_text_sets(x, sets)).collect()),
        Expr::Or(v) => Bound::Or(v.iter().map(|x| bind_text_sets(x, sets)).collect()),
        Expr::Not(b) => Bound::Not(Box::new(bind_text_sets(b, sets))),
        Expr::Text { .. } => Bound::TextSet(sets.next().unwrap_or_default()),
        Expr::Cmp {
            attr: Attr::TextContent | Attr::Symbols,
            ..
        } => Bound::TextSet(sets.next().unwrap_or_default()),
        Expr::Cmp { attr, op, value } => Bound::Cmp {
            attr: *attr,
            op: *op,
            value: value.clone(),
        },
        Expr::InRange { attr, lo, hi } => Bound::InRange {
            attr: *attr,
            lo: lo.clone(),
            hi: hi.clone(),
        },
    }
}

fn num_attr(attr: Attr, rec: &ObjectRecord) -> Option<f64> {
    match attr {
        Attr::FsSize => Some(rec.size as f64),
        Attr::ModificationDate => Some(rec.mtime_ns as f64),
        _ => None,
    }
}

fn cmp_f(op: Op, a: f64, b: f64) -> bool {
    match op {
        Op::Eq => a == b,
        Op::Ne => a != b,
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
    }
}

fn eval(e: &Bound, oid: u64, path: &str, rec: &ObjectRecord) -> bool {
    match e {
        Bound::And(v) => v.iter().all(|x| eval(x, oid, path, rec)),
        Bound::Or(v) => v.iter().any(|x| eval(x, oid, path, rec)),
        Bound::Not(b) => !eval(b, oid, path, rec),
        Bound::TextSet(set) => set.contains(&oid),
        Bound::InRange { attr, lo, hi } => match num_attr(*attr, rec) {
            Some(v) => value_num(lo) <= v && v <= value_num(hi),
            None => false,
        },
        Bound::Cmp { attr, op, value } => match (attr, value) {
            (Attr::FsName, Value::Str { text, ci }) => {
                let name = path.rsplit('/').next().unwrap_or(path);
                let matched = glob_match(text, name, *ci);
                if *op == Op::Ne {
                    !matched
                } else {
                    matched
                }
            }
            (Attr::IndexState, Value::Str { text, ci }) => {
                let state = rec.index_state.as_deref().unwrap_or("");
                let matched = glob_match(text, state, *ci);
                if *op == Op::Ne {
                    !matched
                } else {
                    matched
                }
            }
            (Attr::FsSize, v) => cmp_f(*op, rec.size as f64, value_num(v)),
            (Attr::ModificationDate, v) => cmp_f(*op, rec.mtime_ns as f64, value_num(v)),
            _ => false,
        },
    }
}

fn value_num(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Time(t) => *t as f64,
        Value::Str { .. } => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_corpus_parses() {
        let corpus = [
            r#"kMDItemFSName == "*.rs""#,
            r#"kMDItemFSName == "README*"c"#,
            r#"kMDItemFSSize > 1000"#,
            r#"kMDItemFSSize <= 42 && kMDItemFSName != "x.txt""#,
            r#"kMDItemTextContent == "invoice"cd"#,
            r#"kMDItemContentModificationDate > $time.now(-86400)"#,
            r#"InRange(kMDItemFSSize, 10, 100)"#,
            r#"(kMDItemFSSize > 5 || kMDItemFSName == "a") && !(kMDItemFSSize > 100)"#,
            r#"kRSDIndexState == "quarantined""#,
            r#"kRSDSymbols == "resolve_work""#,
            r#""bare text search""#,
        ];
        for q in corpus {
            parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }

    #[test]
    fn unsupported_is_typed_not_silent() {
        for q in [
            r#"kMDItemPixelHeight > 100"#,
            r#"kMDItemFSName == "x"w"#,
            r#"kMDItemFSSize > $time.today"#,
        ] {
            match parse(q) {
                Err(QueryError::Unsupported(_)) => {}
                other => panic!("{q}: expected Unsupported, got {other:?}"),
            }
        }
    }

    #[test]
    fn time_arithmetic_is_anchored() {
        let e = parse_at(
            r#"kMDItemContentModificationDate > $time.now(-100)"#,
            1_000_000_000_000,
        )
        .unwrap();
        match e {
            Expr::Cmp {
                value: Value::Time(t),
                ..
            } => assert_eq!(t, 1_000_000_000_000 - 100_000_000_000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_match("*.rs", "lib.rs", false));
        assert!(!glob_match("*.rs", "lib.rss", false));
        assert!(glob_match("README*", "readme.md", true));
        assert!(!glob_match("README*", "readme.md", false));
        assert!(glob_match("*inv*ce*", "my-invoice.pdf", false));
    }
}
