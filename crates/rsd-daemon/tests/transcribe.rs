//! P7.1 media: the whisper helper round-trips speech → text. Runs when
//! RSD_TRANSCRIBE_BIN + a whisper model are present; skips cleanly otherwise
//! (the model is a 148MB fetch, not shipped or downloaded in CI).

use rsd_daemon::dispatch::ContentSource;
use rsd_daemon::transcribe::TranscribeExtractor;
use rsd_extract::{Budgets, ExtractHints};

#[test]
fn speech_is_transcribed_into_an_extraction_record() {
    let Some(bin) = std::env::var_os("RSD_TRANSCRIBE_BIN").map(std::path::PathBuf::from) else {
        eprintln!("RSD_TRANSCRIBE_BIN unset — skipping");
        return;
    };
    let model = TranscribeExtractor::default_model();
    if !bin.exists() || !model.exists() {
        eprintln!("helper or model missing — skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("clip.wav");
    // Synthesize known speech with `say`, then transcribe it back.
    let ok = std::process::Command::new("say")
        .args(["the dilithium matrix is destabilizing", "-o"])
        .arg(&wav)
        .args(["--file-format=WAVE", "--data-format=LEI16@16000"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("`say` unavailable — skipping");
        return;
    }
    let mut t = TranscribeExtractor::at(bin, model);
    assert!(t.handles("clip.wav"));
    assert!(!t.handles("notes.txt"));
    let rec = t
        .extract_file(
            &wav,
            &ExtractHints {
                name: "clip.wav".into(),
                full_size: std::fs::metadata(&wav).unwrap().len(),
            },
            &Budgets::default(),
        )
        .unwrap();
    assert_eq!(rec.status, rsd_caes::ExtractStatus::Complete);
    assert!(
        rec.text.to_lowercase().contains("dilithium"),
        "transcript: {:?}",
        rec.text
    );
    assert!(rec.attrs.iter().any(|(k, _)| k == "rsd.transcribed"));
}
