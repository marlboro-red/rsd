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
use std::path::{Path, PathBuf};

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
    #[error("query needs a vector plane (semantic predicate) but none is open")]
    NoVectorPlane,
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
    /// Semantic similarity search (rsd extension, P6.3).
    Semantic {
        query: String,
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
        if word == "semantic" {
            if !self.eat("(") {
                return self.err("expected '(' after semantic");
            }
            let Value::Str { text, .. } = self.parse_string()? else {
                return self.err("expected string");
            };
            if !self.eat(")") {
                return self.err("expected ')'");
            }
            return Ok(Expr::Semantic { query: text });
        }
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
    pub vector: Option<&'a rsd_vector::VectorPlane>,
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
        self.run_with_scopes(expr, scope, None)
    }

    /// Apply grants inside lexical/vector candidate generation and catalog
    /// enumeration. The optional caller scope only narrows these grants.
    pub fn run_authorized(
        &self,
        expr: &Expr,
        scope: Option<&str>,
        grants: &[PathBuf],
    ) -> Result<Vec<Hit>> {
        self.run_with_scopes(expr, scope, Some(grants))
    }

    /// Count every matching result without inheriting the ranked-search limit.
    pub fn count(&self, expr: &Expr, scope: Option<&str>) -> Result<u64> {
        self.count_with_scopes(expr, scope, None)
    }

    pub fn count_authorized(
        &self,
        expr: &Expr,
        scope: Option<&str>,
        grants: &[PathBuf],
    ) -> Result<u64> {
        self.count_with_scopes(expr, scope, Some(grants))
    }

    fn count_with_scopes(
        &self,
        expr: &Expr,
        requested_scope: Option<&str>,
        grants: Option<&[PathBuf]>,
    ) -> Result<u64> {
        let effective = effective_scopes(requested_scope, grants);
        if effective.as_ref().is_some_and(Vec::is_empty) {
            return Ok(0);
        }
        if contains_semantic(expr) {
            return Err(QueryError::Unsupported(
                "exact Count is undefined for ranked semantic predicates".into(),
            ));
        }

        if let Expr::Text { terms } = expr {
            let lexical = self.lexical.ok_or(QueryError::NoLexicalPlane)?;
            return match &effective {
                Some(scopes) => {
                    let refs: Vec<&Path> = scopes.iter().map(PathBuf::as_path).collect();
                    Ok(lexical.count_content_scoped(terms, false, &refs)?)
                }
                None => Ok(lexical.count_content(terms, false)?),
            };
        }
        if contains_lexical(expr) && self.lexical.is_none() {
            return Err(QueryError::NoLexicalPlane);
        }

        let bound = bind_text_sets(expr, &mut std::iter::empty());
        let mut count = 0u64;
        for path in self.candidate_paths(effective.as_deref())? {
            let Some((oid, rec)) = self.catalog.get_by_path(&path)? else {
                continue;
            };
            if rec.kind != ObjectKind::Dir && eval(&bound, oid, &path, &rec, self.lexical)? {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }

    fn run_with_scopes(
        &self,
        expr: &Expr,
        requested_scope: Option<&str>,
        grants: Option<&[PathBuf]>,
    ) -> Result<Vec<Hit>> {
        let effective = effective_scopes(requested_scope, grants);
        if effective.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let allowed_path = |path: &str| {
            effective.as_ref().is_none_or(|scopes| {
                scopes
                    .iter()
                    .any(|scope| Path::new(path).starts_with(scope))
            })
        };

        if let Expr::Semantic { query } = expr {
            let vp = self.vector.ok_or(QueryError::NoVectorPlane)?;
            let allowed_oids = self.authorized_oids(effective.as_deref())?;
            let hits = vp
                .search_filtered(query, self.limit, |oid| allowed_oids.contains(&oid))
                .map_err(|e| QueryError::Unsupported(e.to_string()))?;
            let mut out = Vec::with_capacity(hits.len());
            for hit in hits {
                if let Some(record) = self.catalog.get_object(hit.oid)? {
                    if let Some(path) = record.entry_paths.iter().find(|p| allowed_path(p)) {
                        out.push(Hit {
                            oid: hit.oid,
                            path: path.clone(),
                        });
                    }
                }
            }
            return Ok(out);
        }

        if let Expr::Text { terms } = expr {
            let lexical = self.lexical.ok_or(QueryError::NoLexicalPlane)?;
            let oids = match &effective {
                Some(scopes) => {
                    let refs: Vec<&Path> = scopes.iter().map(PathBuf::as_path).collect();
                    lexical.search_content_scoped(terms, false, &refs, self.limit)?
                }
                None => lexical.search_content(terms, false, self.limit)?,
            };
            let mut hits = Vec::with_capacity(oids.len());
            for oid in oids {
                if let Some(record) = self.catalog.get_object(oid)? {
                    if let Some(path) = record.entry_paths.iter().find(|p| allowed_path(p)) {
                        hits.push(Hit {
                            oid,
                            path: path.clone(),
                        });
                    }
                }
            }
            return Ok(hits);
        }
        if contains_lexical(expr) && self.lexical.is_none() {
            return Err(QueryError::NoLexicalPlane);
        }

        let allowed_oids = self.authorized_oids(effective.as_deref())?;
        let mut sets: Vec<HashSet<u64>> = Vec::new();
        collect_text_sets(
            expr,
            self.vector,
            self.limit,
            &allowed_oids,
            &mut sets,
        )?;
        let mut sets_iter = sets.into_iter();
        let expr = bind_text_sets(expr, &mut sets_iter);

        let mut hits = Vec::new();
        for path in self.candidate_paths(effective.as_deref())? {
            let Some((oid, rec)) = self.catalog.get_by_path(&path)? else {
                continue;
            };
            if rec.kind == ObjectKind::Dir {
                continue;
            }
            if eval(&expr, oid, &path, &rec, self.lexical)? {
                hits.push(Hit { oid, path });
                if hits.len() >= self.limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    fn candidate_paths(&self, scopes: Option<&[PathBuf]>) -> Result<Vec<String>> {
        let Some(scopes) = scopes else {
            return Ok(self
                .catalog
                .listing()?
                .into_keys()
                .collect());
        };
        let mut paths = HashSet::new();
        for scope in scopes {
            for path in self.catalog.subtree_paths(&scope.to_string_lossy())? {
                paths.insert(path);
            }
        }
        let mut paths: Vec<String> = paths.into_iter().collect();
        paths.sort_unstable();
        Ok(paths)
    }

    fn authorized_oids(&self, scopes: Option<&[PathBuf]>) -> Result<HashSet<u64>> {
        let mut oids = HashSet::new();
        for path in self.candidate_paths(scopes)? {
            if let Some((oid, _)) = self.catalog.get_by_path(&path)? {
                oids.insert(oid);
            }
        }
        Ok(oids)
    }
}

fn effective_scopes(requested: Option<&str>, grants: Option<&[PathBuf]>) -> Option<Vec<PathBuf>> {
    let requested = requested.map(PathBuf::from);
    match (requested, grants) {
        (None, None) => None,
        (Some(scope), None) => Some(vec![scope]),
        (None, Some(grants)) => Some(grants.to_vec()),
        (Some(requested), Some(grants)) => {
            let mut scopes = HashSet::new();
            for grant in grants {
                if requested.starts_with(grant) {
                    scopes.insert(requested.clone());
                } else if grant.starts_with(&requested) {
                    scopes.insert(grant.clone());
                }
            }
            Some(scopes.into_iter().collect())
        }
    }
}

fn count_text(e: &Expr) -> usize {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().map(count_text).sum(),
        Expr::Not(b) => count_text(b),
        Expr::Text { .. } | Expr::Semantic { .. } => 1,
        Expr::Cmp {
            attr: Attr::TextContent | Attr::Symbols,
            ..
        } => 1,
        _ => 0,
    }
}

fn contains_semantic(e: &Expr) -> bool {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().any(contains_semantic),
        Expr::Not(b) => contains_semantic(b),
        Expr::Semantic { .. } => true,
        _ => false,
    }
}

fn contains_lexical(e: &Expr) -> bool {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().any(contains_lexical),
        Expr::Not(b) => contains_lexical(b),
        Expr::Text { .. }
        | Expr::Cmp {
            attr: Attr::TextContent | Attr::Symbols,
            ..
        } => true,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy)]
enum LexicalField {
    Content,
    Symbols,
}

/// Bound expression: text predicates replaced by membership sets.
#[derive(Debug, Clone)]
enum Bound {
    And(Vec<Bound>),
    Or(Vec<Bound>),
    Not(Box<Bound>),
    Cmp { attr: Attr, op: Op, value: Value },
    InRange { attr: Attr, lo: Value, hi: Value },
    Lexical { field: LexicalField, terms: String },
    TextSet(HashSet<u64>),
}

fn collect_text_sets(
    e: &Expr,
    vector: Option<&rsd_vector::VectorPlane>,
    limit: usize,
    allowed_oids: &HashSet<u64>,
    out: &mut Vec<HashSet<u64>>,
) -> Result<()> {
    match e {
        Expr::And(v) | Expr::Or(v) => {
            for x in v {
                collect_text_sets(x, vector, limit, allowed_oids, out)?;
            }
        }
        Expr::Not(b) => collect_text_sets(b, vector, limit, allowed_oids, out)?,
        Expr::Text { .. } => {}
        Expr::Semantic { query } => {
            let vp = vector.ok_or(QueryError::NoVectorPlane)?;
            out.push(
                vp.search_filtered(query, limit.max(256), |oid| allowed_oids.contains(&oid))
                    .map_err(|e| QueryError::Unsupported(e.to_string()))?
                    .into_iter()
                    .map(|h| h.oid)
                    .collect(),
            );
        }
        Expr::Cmp {
            attr: Attr::TextContent,
            ..
        } => {}
        Expr::Cmp {
            attr: Attr::Symbols,
            ..
        } => {}
        _ => {}
    }
    Ok(())
}

fn bind_text_sets(e: &Expr, sets: &mut impl Iterator<Item = HashSet<u64>>) -> Bound {
    match e {
        Expr::And(v) => Bound::And(v.iter().map(|x| bind_text_sets(x, sets)).collect()),
        Expr::Or(v) => Bound::Or(v.iter().map(|x| bind_text_sets(x, sets)).collect()),
        Expr::Not(b) => Bound::Not(Box::new(bind_text_sets(b, sets))),
        Expr::Text { terms } => Bound::Lexical {
            field: LexicalField::Content,
            terms: terms.clone(),
        },
        Expr::Semantic { .. } => Bound::TextSet(sets.next().unwrap_or_default()),
        Expr::Cmp {
            attr: Attr::TextContent,
            value: Value::Str { text, .. },
            ..
        } => Bound::Lexical {
            field: LexicalField::Content,
            terms: text.clone(),
        },
        Expr::Cmp {
            attr: Attr::Symbols,
            value: Value::Str { text, .. },
            ..
        } => Bound::Lexical {
            field: LexicalField::Symbols,
            terms: text.clone(),
        },
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

fn eval(
    e: &Bound,
    oid: u64,
    path: &str,
    rec: &ObjectRecord,
    lexical: Option<&LexicalReader>,
) -> Result<bool> {
    Ok(match e {
        Bound::And(v) => {
            let mut matched = true;
            for child in v {
                if !eval(child, oid, path, rec, lexical)? {
                    matched = false;
                    break;
                }
            }
            matched
        }
        Bound::Or(v) => {
            let mut matched = false;
            for child in v {
                if eval(child, oid, path, rec, lexical)? {
                    matched = true;
                    break;
                }
            }
            matched
        }
        Bound::Not(b) => !eval(b, oid, path, rec, lexical)?,
        Bound::Lexical { field, terms } => {
            let lexical = lexical.ok_or(QueryError::NoLexicalPlane)?;
            match field {
                LexicalField::Content => lexical.matches_content(oid, terms, false)?,
                LexicalField::Symbols => lexical.matches_symbols(oid, terms)?,
            }
        }
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
    })
}

fn value_num(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Time(t) => *t as f64,
        Value::Str { .. } => f64::NAN,
    }
}

// ------------------------------------------------------------- live support

/// Evaluate an expression against ONE entry, with text/symbol predicates
/// answered by the caller's single-doc matcher (P5.2). Used by the live-view
/// engine, where no index scan is available or wanted.
pub fn eval_live(
    e: &Expr,
    path: &str,
    rec: &ObjectRecord,
    text_match: &dyn Fn(&str, bool) -> bool,
) -> bool {
    match e {
        Expr::And(v) => v.iter().all(|x| eval_live(x, path, rec, text_match)),
        Expr::Or(v) => v.iter().any(|x| eval_live(x, path, rec, text_match)),
        Expr::Not(b) => !eval_live(b, path, rec, text_match),
        Expr::Text { terms } => text_match(terms, false),
        Expr::Semantic { .. } => false, // live semantic alerts route via LiveEngine's vector hook
        Expr::Cmp {
            attr: Attr::TextContent,
            value: Value::Str { text, .. },
            ..
        } => text_match(text, false),
        Expr::Cmp {
            attr: Attr::Symbols,
            value: Value::Str { text, .. },
            ..
        } => text_match(text, true),
        Expr::Cmp { attr, op, value } => match (attr, value) {
            (Attr::FsName, Value::Str { text, ci }) => {
                let name = path.rsplit('/').next().unwrap_or(path);
                let m = glob_match(text, name, *ci);
                if *op == Op::Ne {
                    !m
                } else {
                    m
                }
            }
            (Attr::IndexState, Value::Str { text, ci }) => {
                let state = rec.index_state.as_deref().unwrap_or("");
                let m = glob_match(text, state, *ci);
                if *op == Op::Ne {
                    !m
                } else {
                    m
                }
            }
            (Attr::FsSize, v) => cmp_f(*op, rec.size as f64, value_num(v)),
            (Attr::ModificationDate, v) => cmp_f(*op, rec.mtime_ns as f64, value_num(v)),
            _ => false,
        },
        Expr::InRange { attr, lo, hi } => match num_attr(*attr, rec) {
            Some(v) => value_num(lo) <= v && v <= value_num(hi),
            None => false,
        },
    }
}

impl<'a> QueryEngine<'a> {
    /// Hybrid retrieval (P6.3): RRF fusion of lexical and semantic top-k.
    pub fn hybrid(&self, text: &str, k: usize) -> Result<Vec<Hit>> {
        Ok(self
            .hybrid_tagged(text, k)?
            .into_iter()
            .map(|(h, _)| h)
            .collect())
    }

    /// Hybrid retrieval with per-hit provenance (which engine(s) matched).
    pub fn hybrid_tagged(
        &self,
        text: &str,
        k: usize,
    ) -> Result<Vec<(Hit, rsd_vector::MatchOrigin)>> {
        let lex = self.lexical.ok_or(QueryError::NoLexicalPlane)?;
        let vp = self.vector.ok_or(QueryError::NoVectorPlane)?;
        let lexical = lex.search_content(text, false, k.max(50))?;
        let semantic: Vec<u64> = vp
            .search(text, k.max(50))
            .map_err(|e| QueryError::Unsupported(e.to_string()))?
            .into_iter()
            .map(|h| h.oid)
            .collect();
        let fused = rsd_vector::rrf_tagged(&lexical, &semantic, k);
        let mut out = Vec::with_capacity(fused.len());
        for (oid, origin) in fused {
            if let Some(rec) = self.catalog.get_object(oid)? {
                if let Some(p) = rec.entry_paths.first() {
                    out.push((
                        Hit {
                            oid,
                            path: p.clone(),
                        },
                        origin,
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(ino: u64) -> rsd_catalog::StatInfo {
        rsd_catalog::StatInfo {
            kind: ObjectKind::File,
            file_id: rsd_catalog::FileId { dev: 1, ino },
            size: 1,
            mtime_ns: 1,
            birthtime_ns: ino as i64,
            nlink: 1,
        }
    }

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
    fn scoped_catalog_counts_enumerate_grant_subtrees_without_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&tmp.path().join("catalog.redb")).unwrap();
        catalog
            .apply_changes_direct(&[
                rsd_catalog::Change::Upsert {
                    path: "/grant/a.txt".into(),
                    stat: stat(1),
                },
                rsd_catalog::Change::Upsert {
                    path: "/grant/sub/b.txt".into(),
                    stat: stat(2),
                },
                rsd_catalog::Change::Upsert {
                    path: "/grant-two/private.txt".into(),
                    stat: stat(3),
                },
            ])
            .unwrap();
        let engine = QueryEngine {
            catalog: &catalog,
            lexical: None,
            vector: None,
            limit: 1,
        };
        let expr = parse("kMDItemFSSize > 0").unwrap();

        assert_eq!(
            engine
                .count_authorized(
                    &expr,
                    None,
                    &[PathBuf::from("/grant"), PathBuf::from("/grant/sub")],
                )
                .unwrap(),
            2
        );
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

    #[test]
    fn requested_scope_intersects_grants_by_components() {
        let grants = vec![PathBuf::from("/root/docs")];
        assert_eq!(
            effective_scopes(Some("/root/docs/reports"), Some(&grants)),
            Some(vec![PathBuf::from("/root/docs/reports")])
        );
        assert_eq!(
            effective_scopes(Some("/root"), Some(&grants)),
            Some(vec![PathBuf::from("/root/docs")])
        );
        assert_eq!(
            effective_scopes(Some("/root/docs-private"), Some(&grants)),
            Some(Vec::new())
        );
    }
}
