use rsd_wasm::{Plugin, PluginHost};

#[test]
fn real_subtitles_plugin_strips_timecodes() {
    let wasm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../plugins/subtitles/target/wasm32-unknown-unknown/release/rsd_plugin_subtitles.wasm",
    );
    if !wasm.exists() {
        assert!(
            std::env::var_os("RSD_CI_HELPERS_REQUIRED").is_none(),
            "CI built the plugin gate but its wasm artifact is missing"
        );
        eprintln!("plugin wasm not built — skipping outside helper CI");
        return;
    }
    let mut host = PluginHost::new().unwrap();
    let p = Plugin::load(host.engine(), &wasm).unwrap();
    eprintln!("declared extensions: {:?}", p.extensions);
    assert!(
        p.extensions.contains(&"srt".to_string()),
        "exts: {:?}",
        p.extensions
    );
    host.add(p);

    let srt = "1\n00:00:01,000 --> 00:00:04,000\nThe dilithium matrix is destabilizing.\n\n2\n00:00:05,000 --> 00:00:08,000\nReroute plasma conduits.\n";
    let r = host.extract("srt", srt.as_bytes()).unwrap().unwrap();
    eprintln!("plugin output: {:?}", r.text);
    assert!(r.text.contains("dilithium matrix"));
    assert!(!r.text.contains("-->"), "timecodes leaked: {:?}", r.text);
}
