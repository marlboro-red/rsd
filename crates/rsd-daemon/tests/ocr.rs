//! P7.1 OCR: the Vision helper round-trips image → text through OcrExtractor.
//! Runs when RSD_OCR_BIN points at the built helper (ci.sh sets it); skips
//! cleanly otherwise (Vision is macOS-only and helper-dependent).

use rsd_daemon::dispatch::ContentSource;
use rsd_daemon::ocr::OcrExtractor;
use rsd_extract::{Budgets, ExtractHints};

fn helper() -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(std::env::var("RSD_OCR_BIN").ok()?);
    p.exists().then_some(p)
}

#[test]
fn image_text_is_recognized_and_becomes_an_extraction_record() {
    let Some(bin) = helper() else {
        eprintln!("RSD_OCR_BIN unset/missing — skipping OCR test");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("shot.png");
    // Render a known phrase, then OCR it back.
    let status = std::process::Command::new(&bin)
        .arg("--render")
        .arg("meeting notes about the flux capacitor")
        .arg(&img)
        .status()
        .unwrap();
    assert!(status.success());

    let mut ocr = OcrExtractor::at(bin);
    let rec = ocr
        .extract_file(
            &img,
            &ExtractHints {
                name: "shot.png".into(),
                full_size: std::fs::metadata(&img).unwrap().len(),
            },
            &Budgets::default(),
        )
        .unwrap();
    assert_eq!(rec.status, rsd_caes::ExtractStatus::Complete);
    assert!(
        rec.text.to_lowercase().contains("capacitor"),
        "got: {:?}",
        rec.text
    );
    assert!(rec.attrs.iter().any(|(k, _)| k == "rsd.ocr"));
}
