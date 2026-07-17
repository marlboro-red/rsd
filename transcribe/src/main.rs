//! rsd-transcribe — the audio/video transcription helper (P7.1 media). A
//! separate process (its own boundary, heavy model isolated from the daemon)
//! that decodes an audio/video file to 16 kHz mono PCM (symphonia) and
//! transcribes it with whisper (whisper-rs / whisper.cpp, Metal-accelerated).
//! Prints the transcript to stdout. No authorization prompts, works headless —
//! the reason we use whisper instead of Apple Speech.
//!
//!   rsd-transcribe <audio-or-video> [--model <ggml.bin>]
//!
//! Model path: --model, else $RSD_WHISPER_MODEL, else
//! ~/.cache/rsd/models/whisper/ggml-base.en.bin (fetched by fetch-model.sh).

use std::path::{Path, PathBuf};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const TARGET_HZ: u32 = 16_000;

fn model_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    if let Ok(p) = std::env::var("RSD_WHISPER_MODEL") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".cache/rsd/models/whisper/ggml-base.en.bin")
}

/// Decode any symphonia-supported container to mono f32 at 16 kHz (naive
/// linear resample; whisper is robust to it).
fn decode_to_pcm(path: &Path) -> Result<Vec<f32>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or("no audio track")?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;
    let src_hz = track.codec_params.sample_rate.unwrap_or(TARGET_HZ);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1);

    let mut mono: Vec<f32> = Vec::new();
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    while let Ok(packet) = format.next_packet() {
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if sbuf.is_none() {
            let spec = *decoded.spec();
            sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }
        let sb = sbuf.as_mut().unwrap();
        sb.copy_interleaved_ref(decoded);
        for frame in sb.samples().chunks(channels.max(1)) {
            mono.push(frame.iter().copied().sum::<f32>() / channels.max(1) as f32);
        }
    }
    if mono.is_empty() {
        return Err("no samples decoded".into());
    }
    // Resample to 16 kHz (linear).
    if src_hz == TARGET_HZ {
        return Ok(mono);
    }
    let ratio = TARGET_HZ as f64 / src_hz as f64;
    let out_len = (mono.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let j = src.floor() as usize;
        let frac = (src - j as f64) as f32;
        let a = mono.get(j).copied().unwrap_or(0.0);
        let b = mono.get(j + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    Ok(out)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut input: Option<PathBuf> = None;
    let mut model: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--model" => model = args.next().map(PathBuf::from),
            _ => input = Some(PathBuf::from(a)),
        }
    }
    let Some(input) = input else {
        eprintln!("usage: rsd-transcribe <audio-or-video> [--model <ggml.bin>]");
        std::process::exit(2);
    };
    let model = model_path(model);
    if !model.exists() {
        eprintln!("rsd-transcribe: model not found at {model:?} (run fetch-model.sh)");
        std::process::exit(3);
    }

    let pcm = match decode_to_pcm(&input) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rsd-transcribe: decode failed: {e}");
            std::process::exit(1);
        }
    };

    let ctx = match WhisperContext::new_with_params(
        &model.to_string_lossy(),
        WhisperContextParameters::default(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rsd-transcribe: model load failed: {e}");
            std::process::exit(1);
        }
    };
    let mut state = ctx.create_state().expect("whisper state");
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_language(Some("en"));
    if let Err(e) = state.full(params, &pcm) {
        eprintln!("rsd-transcribe: transcription failed: {e}");
        std::process::exit(1);
    }
    let n = state.full_n_segments().unwrap_or(0);
    let mut out = String::new();
    for i in 0..n {
        if let Ok(seg) = state.full_get_segment_text(i) {
            out.push_str(seg.trim());
            out.push(' ');
        }
    }
    print!("{}", out.trim());
}
