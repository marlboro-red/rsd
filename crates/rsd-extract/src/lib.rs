//! rsd-extract: native content extractors (P3.2) — plain text with encoding
//! detection, and source code with tree-sitter symbol extraction.
//!
//! Everything runs under the extraction limit contract (DESIGN.md §10.1):
//! input/output/symbol budgets are enforced, overruns produce *labeled partial
//! results* (`ResourceBudgetExceeded`, `Partial`), and unparseable input is a
//! typed status, never a panic. This library is pure — it parses bytes it is
//! handed; the sandboxed worker (rsd-worker) is the only production caller.

use rsd_caes::{ExtractStatus, ExtractionRecord};
use serde::{Deserialize, Serialize};

mod source;

pub use source::Lang;

/// The extractor identity that keys CAES records.
pub const EXTRACTOR_ID: &str = "rsd.native";
pub const EXTRACTOR_VERSION: u32 = 1;

/// The extraction limit contract. Every budget is enforced; every overrun is
/// labeled in the result, never silent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Budgets {
    pub max_input_bytes: u64,
    pub max_output_bytes: usize,
    pub max_symbols: usize,
    pub parse_timeout_ms: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            max_input_bytes: 32 * 1024 * 1024,
            max_output_bytes: 2 * 1024 * 1024,
            max_symbols: 5_000,
            parse_timeout_ms: 2_000,
        }
    }
}

/// Instance hints that legitimately affect extraction (and therefore join the
/// CAES key via `hints_hash`): the name (extension drives sniffing) and
/// whether the input was truncated at the read boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractHints {
    pub name: String,
    pub full_size: u64,
}

impl ExtractHints {
    pub fn extension(&self) -> String {
        std::path::Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }

    /// Canonical hash of extraction-relevant hints for CAES keying.
    pub fn hints_hash(&self, truncated: bool) -> [u8; 32] {
        let mut h = blake3_hasher();
        h.update(self.extension().as_bytes());
        h.update(&[truncated as u8]);
        *h.finalize().as_bytes()
    }
}

fn blake3_hasher() -> blake3::Hasher {
    blake3::Hasher::new()
}

/// What the sniffer decided the bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Source(Lang),
    PlainText,
    Pdf,
    Binary,
}

/// Image formats OCR can read. The extension already varies the CAES
/// hints_hash, so an image's OCR record never collides with a text record.
pub fn is_image(name: &str) -> bool {
    let ext = std::path::Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "tif" | "heic" | "webp"
    )
}

/// Sniff by extension first, then content shape.
pub fn sniff(hints: &ExtractHints, bytes: &[u8]) -> Format {
    if bytes.starts_with(b"%PDF-") || hints.extension() == "pdf" {
        return Format::Pdf;
    }
    if let Some(lang) = Lang::from_extension(&hints.extension()) {
        return Format::Source(lang);
    }
    match hints.extension().as_str() {
        "txt" | "md" | "markdown" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "htm"
        | "css" | "csv" | "log" | "sh" | "zsh" | "bash" | "ini" | "cfg" | "conf" | "sql" => {
            Format::PlainText
        }
        _ => {
            if looks_binary(bytes) {
                Format::Binary
            } else {
                Format::PlainText
            }
        }
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    head.contains(&0)
}

/// Decode bytes to text: BOM-driven (UTF-8/16), else UTF-8 with lossy repair.
/// Returns None when the content is not plausibly text.
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _, _) = encoding.decode(&bytes[bom_len..]);
        return Some(text.into_owned());
    }
    if looks_binary(bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// The extraction entry point: bytes (possibly truncated at the input budget)
/// + hints + budgets → a typed, budget-labeled record.
pub fn extract_bytes(hints: &ExtractHints, budgets: &Budgets, bytes: &[u8]) -> ExtractionRecord {
    let truncated_input = (bytes.len() as u64) < hints.full_size;
    let mut attrs: Vec<(String, String)> = vec![(
        "rsd.extractor".into(),
        format!("{EXTRACTOR_ID}/{EXTRACTOR_VERSION}"),
    )];

    let format = sniff(hints, bytes);
    let (mut text, mut symbols, mut status) = match format {
        Format::Pdf => {
            // v1: pure-Rust extraction (pdfium upgrade tracked for quality).
            // Panics are contained twice: locally here, and by the sealed
            // worker process boundary if a parser bug slips past.
            let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
            match result {
                Ok(Ok(text)) => {
                    attrs.push(("rsd.format".into(), "pdf".into()));
                    (text, vec![], ExtractStatus::Complete)
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    let status = if msg.to_lowercase().contains("encrypt") {
                        ExtractStatus::EncryptedContent
                    } else {
                        ExtractStatus::Corrupt
                    };
                    attrs.push(("rsd.error".into(), msg));
                    return ExtractionRecord {
                        status,
                        text: String::new(),
                        attrs,
                        symbols: vec![],
                    };
                }
                Err(_) => {
                    attrs.push(("rsd.error".into(), "pdf parser panic".into()));
                    return ExtractionRecord {
                        status: ExtractStatus::Corrupt,
                        text: String::new(),
                        attrs,
                        symbols: vec![],
                    };
                }
            }
        }
        Format::Binary => {
            return ExtractionRecord {
                status: ExtractStatus::Unsupported,
                text: String::new(),
                attrs,
                symbols: vec![],
            };
        }
        Format::PlainText => match decode_text(bytes) {
            Some(t) => (t, vec![], ExtractStatus::Complete),
            None => {
                return ExtractionRecord {
                    status: ExtractStatus::Unsupported,
                    text: String::new(),
                    attrs,
                    symbols: vec![],
                };
            }
        },
        Format::Source(lang) => match decode_text(bytes) {
            Some(t) => {
                attrs.push(("rsd.lang".into(), lang.name().to_string()));
                let (symbols, sym_status) = source::symbols(lang, &t, budgets);
                (t, symbols, sym_status)
            }
            None => {
                return ExtractionRecord {
                    status: ExtractStatus::Unsupported,
                    text: String::new(),
                    attrs,
                    symbols: vec![],
                };
            }
        },
    };

    // Output budget: truncate on a char boundary, label as partial.
    if text.len() > budgets.max_output_bytes {
        let mut cut = budgets.max_output_bytes;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        attrs.push(("rsd.partial_reason".into(), "output_budget".into()));
        status = ExtractStatus::Partial;
    }
    if symbols.len() > budgets.max_symbols {
        symbols.truncate(budgets.max_symbols);
        attrs.push(("rsd.partial_reason".into(), "symbol_budget".into()));
        status = ExtractStatus::Partial;
    }
    // Input budget overrun outranks other statuses: the caller could not even
    // give us the whole file.
    if truncated_input {
        attrs.push(("rsd.partial_reason".into(), "input_budget".into()));
        status = ExtractStatus::ResourceBudgetExceeded;
    }

    ExtractionRecord {
        status,
        text,
        attrs,
        symbols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(name: &str, full: u64) -> ExtractHints {
        ExtractHints {
            name: name.into(),
            full_size: full,
        }
    }

    fn b() -> Budgets {
        Budgets::default()
    }

    fn sym_names(rec: &ExtractionRecord) -> Vec<(&str, &str)> {
        rec.symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.name.as_str()))
            .collect()
    }

    #[test]
    fn rust_symbols_golden() {
        let src = r#"
pub struct Catalog { db: u32 }
enum Kind { A, B }
trait Store { fn get(&self); }
pub fn resolve_work(x: u32) -> u32 { x }
impl Catalog { fn open() {} }
"#;
        let rec = extract_bytes(&hints("lib.rs", src.len() as u64), &b(), src.as_bytes());
        assert_eq!(rec.status, ExtractStatus::Complete);
        let syms = sym_names(&rec);
        assert!(syms.contains(&("type", "Catalog")), "{syms:?}");
        assert!(syms.contains(&("type", "Kind")));
        assert!(syms.contains(&("type", "Store")));
        assert!(syms.contains(&("function", "resolve_work")));
        assert!(syms.contains(&("function", "open")));
        assert!(rec.text.contains("resolve_work"));
        // Line numbers are 1-based and plausible.
        let f = rec
            .symbols
            .iter()
            .find(|s| s.name == "resolve_work")
            .unwrap();
        assert_eq!(f.line, 5);
    }

    #[test]
    fn python_symbols_golden() {
        let src = "class Indexer:\n    def extract(self):\n        pass\n\ndef main():\n    pass\n";
        let rec = extract_bytes(&hints("m.py", src.len() as u64), &b(), src.as_bytes());
        let syms = sym_names(&rec);
        assert!(syms.contains(&("type", "Indexer")), "{syms:?}");
        assert!(syms.contains(&("function", "extract")));
        assert!(syms.contains(&("function", "main")));
    }

    #[test]
    fn javascript_symbols_golden() {
        let src = "class Query {\n  compile() {}\n}\nfunction parse(s) { return s }\n";
        let rec = extract_bytes(&hints("q.js", src.len() as u64), &b(), src.as_bytes());
        let syms = sym_names(&rec);
        assert!(syms.contains(&("type", "Query")), "{syms:?}");
        assert!(syms.contains(&("function", "compile")));
        assert!(syms.contains(&("function", "parse")));
    }

    #[test]
    fn go_symbols_golden() {
        let src =
            "package main\n\ntype Journal struct{}\n\nfunc (j *Journal) Append() {}\n\nfunc main() {}\n";
        let rec = extract_bytes(&hints("j.go", src.len() as u64), &b(), src.as_bytes());
        let syms = sym_names(&rec);
        assert!(syms.contains(&("type", "Journal")), "{syms:?}");
        assert!(syms.contains(&("function", "Append")));
        assert!(syms.contains(&("function", "main")));
    }

    #[test]
    fn c_symbols_golden() {
        let src = "struct catalog { int db; };\n\nint resolve(int x) { return x; }\n";
        let rec = extract_bytes(&hints("c.c", src.len() as u64), &b(), src.as_bytes());
        let syms = sym_names(&rec);
        assert!(syms.contains(&("function", "resolve")), "{syms:?}");
        assert!(syms.contains(&("type", "catalog")));
    }

    #[test]
    fn utf16_bom_decodes() {
        let text = "héllo wörld";
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let rec = extract_bytes(&hints("u.txt", bytes.len() as u64), &b(), &bytes);
        assert_eq!(rec.status, ExtractStatus::Complete);
        assert_eq!(rec.text, text);
    }

    #[test]
    fn binary_is_typed_unsupported_not_garbage() {
        let bytes = [0u8, 159, 146, 150, 0, 1, 2, 3];
        let rec = extract_bytes(&hints("blob.bin", 8), &b(), &bytes);
        assert_eq!(rec.status, ExtractStatus::Unsupported);
        assert!(rec.text.is_empty());
    }

    #[test]
    fn oversize_input_is_labeled_resource_budget_exceeded() {
        // Caller could only hand us the first N bytes of a larger file.
        let part = "x".repeat(1000);
        let rec = extract_bytes(&hints("big.txt", 10_000_000), &b(), part.as_bytes());
        assert_eq!(rec.status, ExtractStatus::ResourceBudgetExceeded);
        assert_eq!(rec.text.len(), 1000, "partial text still extracted");
        assert!(rec
            .attrs
            .iter()
            .any(|(k, v)| k == "rsd.partial_reason" && v == "input_budget"));
    }

    #[test]
    fn output_budget_truncates_on_char_boundary() {
        let src = "é".repeat(10_000); // 2 bytes each
        let budgets = Budgets {
            max_output_bytes: 1001, // mid-char
            ..Budgets::default()
        };
        let rec = extract_bytes(&hints("t.txt", src.len() as u64), &budgets, src.as_bytes());
        assert_eq!(rec.status, ExtractStatus::Partial);
        assert_eq!(rec.text.len(), 1000);
        assert!(rec.text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn symbol_budget_truncates_and_labels() {
        let src: String = (0..100).map(|i| format!("fn f{i}() {{}}\n")).collect();
        let budgets = Budgets {
            max_symbols: 10,
            ..Budgets::default()
        };
        let rec = extract_bytes(
            &hints("many.rs", src.len() as u64),
            &budgets,
            src.as_bytes(),
        );
        assert_eq!(rec.status, ExtractStatus::Partial);
        assert_eq!(rec.symbols.len(), 10);
    }

    #[test]
    fn broken_source_never_panics_and_still_texts() {
        let src = "fn broken( {{{{ 中文 \u{0007} unclosed";
        let rec = extract_bytes(&hints("bad.rs", src.len() as u64), &b(), src.as_bytes());
        assert!(rec.text.contains("unclosed"));
    }
}

#[cfg(test)]
mod pdf_tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};

    fn make_pdf(text: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn pdf_text_is_extracted() {
        let bytes = make_pdf("Hello rsd PDF extraction net-60 terms");
        let rec = extract_bytes(
            &ExtractHints {
                name: "doc.pdf".into(),
                full_size: bytes.len() as u64,
            },
            &Budgets::default(),
            &bytes,
        );
        assert_eq!(rec.status, ExtractStatus::Complete, "{:?}", rec.attrs);
        assert!(rec.text.contains("net-60"), "{:?}", rec.text);
    }

    #[test]
    fn garbage_pdf_is_typed_never_panics() {
        let mut bytes = b"%PDF-1.7 utterly broken".to_vec();
        bytes.extend_from_slice(&[0, 1, 2, 3, 255, 254]);
        let rec = extract_bytes(
            &ExtractHints {
                name: "bad.pdf".into(),
                full_size: bytes.len() as u64,
            },
            &Budgets::default(),
            &bytes,
        );
        assert!(
            matches!(
                rec.status,
                ExtractStatus::Corrupt | ExtractStatus::Unsupported
            ),
            "{:?}",
            rec.status
        );
    }
}
