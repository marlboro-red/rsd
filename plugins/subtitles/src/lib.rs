//! Reference rsd extractor plugin: subtitles (.srt/.vtt) → searchable dialogue.
//! Strips cue numbers and timecodes, leaving the spoken text. ~40 lines, no
//! deps, no ambient capability — the whole point of the WASM ABI.
//!
//! Build: cargo build --release --target wasm32-unknown-unknown

const ABI_VERSION: i32 = 1;

/// pack ptr into the high 32 bits, len into the low 32.
fn pack(bytes: Vec<u8>) -> i64 {
    let ptr = bytes.as_ptr() as u64;
    let len = bytes.len() as u64;
    core::mem::forget(bytes);
    ((ptr << 32) | len) as i64
}

#[no_mangle]
pub extern "C" fn rsd_abi_version() -> i32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn rsd_alloc(len: i32) -> i32 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn rsd_extensions() -> i64 {
    pack(b"srt,vtt".to_vec())
}

#[no_mangle]
pub extern "C" fn rsd_extract(ptr: i32, len: i32) -> i64 {
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let text = String::from_utf8_lossy(input);
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains("-->") {
            continue; // timecode line
        }
        if t.bytes().all(|b| b.is_ascii_digit()) {
            continue; // cue number
        }
        if t.starts_with("WEBVTT") || t.starts_with("NOTE") {
            continue; // vtt header
        }
        out.push_str(t);
        out.push('\n');
    }
    let mut result = Vec::with_capacity(out.len() + 1);
    result.push(0u8); // status: complete
    result.extend_from_slice(out.as_bytes());
    pack(result)
}
