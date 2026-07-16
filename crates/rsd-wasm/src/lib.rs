//! rsd-wasm: the WASM extractor plugin host (P7.2, DESIGN.md §8/§10.2) — the
//! mdimporter successor, reinvented with capability security.
//!
//! A plugin is a `.wasm` module with a tiny, stable ABI. It runs inside
//! wasmtime with **fuel metering** (bounded CPU), a **memory cap** (bounded
//! RAM), and **zero imports** — no WASI, no host functions. It therefore has
//! no ambient authority whatsoever: it cannot open a file, touch the network,
//! or allocate without bound. It sees exactly the input bytes the host writes
//! into its linear memory and nothing else. A hostile or buggy plugin's blast
//! radius is one instantiation: it runs out of fuel or memory and is dropped.
//!
//! ## ABI v1 (core wasm, no component model — deliberately minimal so writing
//! a plugin is a weekend project)
//!
//! Exports:
//!   memory
//!   rsd_abi_version() -> i32          // must equal ABI_VERSION
//!   rsd_alloc(len: i32) -> i32        // allocate len bytes, return ptr
//!   rsd_extensions() -> i64           // packed (ptr<<32|len): "srt,vtt"
//!   rsd_extract(ptr: i32, len: i32) -> i64   // packed result buffer
//!
//! The result buffer is `[status: u8][utf8 text...]`; status 0=complete,
//! 1=partial, 2=unsupported. `pack` puts ptr in the high 32 bits, len in the
//! low 32. Output caching, dedup, and plane indexing are the host's job — the
//! plugin only turns bytes into text.

use std::collections::HashMap;
use std::path::Path;
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder};

pub const ABI_VERSION: i32 = 1;
const FUEL: u64 = 2_000_000_000;
const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("wasm: {0}")]
    Wasm(String),
    #[error("plugin ABI mismatch: got {got}, host wants {ABI_VERSION}")]
    AbiMismatch { got: i32 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin exhausted its fuel or memory budget")]
    Exhausted,
}

pub type Result<T> = std::result::Result<T, WasmError>;

fn werr(e: impl std::fmt::Display) -> WasmError {
    WasmError::Wasm(e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmStatus {
    Complete,
    Partial,
    Unsupported,
}

#[derive(Debug)]
pub struct WasmExtraction {
    pub status: WasmStatus,
    pub text: String,
}

struct Host {
    limits: StoreLimits,
}

/// A compiled plugin plus the extensions it declared.
pub struct Plugin {
    engine: Engine,
    module: Module,
    pub name: String,
    pub extensions: Vec<String>,
}

fn unpack(v: i64) -> (u32, u32) {
    let u = v as u64;
    ((u >> 32) as u32, (u & 0xFFFF_FFFF) as u32)
}

fn read_mem(store: &mut Store<Host>, mem: &Memory, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let data = mem.data(&store);
    let (start, end) = (ptr as usize, ptr as usize + len as usize);
    if end > data.len() || len as usize > MAX_OUTPUT_BYTES {
        return Err(WasmError::Wasm(
            "plugin returned an out-of-bounds buffer".into(),
        ));
    }
    Ok(data[start..end].to_vec())
}

impl Plugin {
    /// Compile a plugin and read its declared extensions. Runs the plugin once
    /// (for `rsd_extensions`) under the full sandbox.
    pub fn load(engine: &Engine, path: &Path) -> Result<Plugin> {
        let module = Module::from_file(engine, path).map_err(werr)?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".into());

        // Instantiate once to check ABI + query extensions.
        let (mut store, instance) = Self::instantiate(engine, &module)?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, "rsd_abi_version")
            .map_err(werr)?
            .call(&mut store, ())
            .map_err(werr)?;
        if abi != ABI_VERSION {
            return Err(WasmError::AbiMismatch { got: abi });
        }
        let mem = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Wasm("plugin exports no memory".into()))?;
        let packed = instance
            .get_typed_func::<(), i64>(&mut store, "rsd_extensions")
            .map_err(werr)?
            .call(&mut store, ())
            .map_err(werr)?;
        let (p, l) = unpack(packed);
        let ext_bytes = read_mem(&mut store, &mem, p, l)?;
        let extensions = String::from_utf8_lossy(&ext_bytes)
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();

        Ok(Plugin {
            engine: engine.clone(),
            module,
            name,
            extensions,
        })
    }

    /// Build from an already-compiled module (tests, embedded plugins): still
    /// checks ABI + reads declared extensions through the sandbox.
    pub fn load_from_module(engine: &Engine, module: Module, name: &str) -> Result<Plugin> {
        let (mut store, instance) = Self::instantiate(engine, &module)?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, "rsd_abi_version")
            .map_err(werr)?
            .call(&mut store, ())
            .map_err(werr)?;
        if abi != ABI_VERSION {
            return Err(WasmError::AbiMismatch { got: abi });
        }
        let mem = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Wasm("plugin exports no memory".into()))?;
        let packed = instance
            .get_typed_func::<(), i64>(&mut store, "rsd_extensions")
            .map_err(werr)?
            .call(&mut store, ())
            .map_err(werr)?;
        let (p, l) = unpack(packed);
        let ext_bytes = read_mem(&mut store, &mem, p, l)?;
        let extensions = String::from_utf8_lossy(&ext_bytes)
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        Ok(Plugin {
            engine: engine.clone(),
            module,
            name: name.to_string(),
            extensions,
        })
    }

    fn instantiate(engine: &Engine, module: &Module) -> Result<(Store<Host>, Instance)> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY_BYTES)
            .instances(1)
            .build();
        let mut store = Store::new(engine, Host { limits });
        store.limiter(|h| &mut h.limits);
        store.set_fuel(FUEL).map_err(werr)?;
        // Empty linker: NO imports. The plugin cannot reach anything.
        let linker: Linker<Host> = Linker::new(engine);
        let instance = linker.instantiate(&mut store, module).map_err(werr)?;
        Ok((store, instance))
    }

    /// Extract text from bytes in a fresh sandboxed instance (per-request
    /// isolation: fresh fuel, fresh memory, no shared state).
    pub fn extract(&self, input: &[u8]) -> Result<WasmExtraction> {
        let (mut store, instance) = Self::instantiate(&self.engine, &self.module)?;
        let mem = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Wasm("plugin exports no memory".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "rsd_alloc")
            .map_err(werr)?;
        let extract = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "rsd_extract")
            .map_err(werr)?;

        let in_ptr = alloc.call(&mut store, input.len() as i32).map_err(werr)?;
        mem.write(&mut store, in_ptr as usize, input)
            .map_err(|_| WasmError::Wasm("input write out of bounds".into()))?;

        let packed = match extract.call(&mut store, (in_ptr, input.len() as i32)) {
            Ok(v) => v,
            Err(err) => {
                // Fuel/memory exhaustion (and any trap: a misbehaving plugin
                // is contained regardless) surfaces as Exhausted — the plugin
                // is dropped, the host is never harmed.
                if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
                    if matches!(
                        trap,
                        wasmtime::Trap::OutOfFuel | wasmtime::Trap::MemoryOutOfBounds
                    ) {
                        return Err(WasmError::Exhausted);
                    }
                }
                return Err(WasmError::Wasm(err.to_string()));
            }
        };
        let (p, l) = unpack(packed);
        let out = read_mem(&mut store, &mem, p, l)?;
        let (status, text) = match out.split_first() {
            Some((0, rest)) => (WasmStatus::Complete, rest),
            Some((1, rest)) => (WasmStatus::Partial, rest),
            _ => (WasmStatus::Unsupported, &[][..]),
        };
        Ok(WasmExtraction {
            status,
            text: String::from_utf8_lossy(text).into_owned(),
        })
    }
}

/// A registry of loaded plugins keyed by the extensions they declared. Later
/// plugins win a contested extension (documented, deterministic).
pub struct PluginHost {
    engine: Engine,
    by_ext: HashMap<String, usize>,
    plugins: Vec<Plugin>,
}

impl PluginHost {
    pub fn new() -> Result<PluginHost> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(werr)?;
        Ok(PluginHost {
            engine,
            by_ext: HashMap::new(),
            plugins: Vec::new(),
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Load every `*.wasm` in `dir` (missing dir = no plugins, not an error).
    pub fn load_dir(&mut self, dir: &Path) -> Result<usize> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut loaded = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                continue;
            }
            match Plugin::load(&self.engine, &path) {
                Ok(p) => {
                    tracing::info!("wasm plugin {:?} handles {:?}", p.name, p.extensions);
                    self.add(p);
                    loaded += 1;
                }
                Err(e) => tracing::warn!("skipping wasm plugin {path:?}: {e}"),
            }
        }
        Ok(loaded)
    }

    pub fn add(&mut self, plugin: Plugin) {
        let idx = self.plugins.len();
        for ext in &plugin.extensions {
            self.by_ext.insert(ext.clone(), idx);
        }
        self.plugins.push(plugin);
    }

    pub fn handles(&self, ext: &str) -> bool {
        self.by_ext.contains_key(&ext.to_lowercase())
    }

    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    pub fn extract(&self, ext: &str, input: &[u8]) -> Option<Result<WasmExtraction>> {
        let idx = *self.by_ext.get(&ext.to_lowercase())?;
        Some(self.plugins[idx].extract(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A hand-written WAT plugin — no build step, exercises the full ABI.
    const ECHO_WAT: &str = r#"
    (module
      (memory (export "memory") 2)
      (global $bump (mut i32) (i32.const 1024))
      (func (export "rsd_abi_version") (result i32) (i32.const 1))
      (func (export "rsd_alloc") (param $len i32) (result i32)
        (local $p i32)
        (local.set $p (global.get $bump))
        (global.set $bump (i32.add (global.get $bump) (local.get $len)))
        (local.get $p))
      ;; extensions string "foo" written at offset 16
      (data (i32.const 16) "foo")
      (func (export "rsd_extensions") (result i64)
        (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 3)))
      ;; extract: write [status=0] then echo input back after it. We place the
      ;; result at offset 512: status byte + copy of input.
      (func (export "rsd_extract") (param $ptr i32) (param $len i32) (result i64)
        (local $i i32)
        (i32.store8 (i32.const 512) (i32.const 0)) ;; status complete
        (local.set $i (i32.const 0))
        (block $done (loop $l
          (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
          (i32.store8
            (i32.add (i32.const 513) (local.get $i))
            (i32.load8_u (i32.add (local.get $ptr) (local.get $i))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $l)))
        (i64.or
          (i64.shl (i64.const 512) (i64.const 32))
          (i64.extend_i32_u (i32.add (local.get $len) (i32.const 1)))))
    )"#;

    fn compile(engine: &Engine, wat_src: &str) -> Module {
        let bytes = wat::parse_str(wat_src).unwrap();
        Module::new(engine, &bytes).unwrap()
    }

    fn echo_host() -> PluginHost {
        let mut host = PluginHost::new().unwrap();
        let module = compile(host.engine(), ECHO_WAT);
        let plugin = Plugin {
            engine: host.engine().clone(),
            module,
            name: "echo".into(),
            extensions: vec!["foo".into()],
        };
        host.add(plugin);
        host
    }

    #[test]
    fn abi_roundtrip_through_a_wat_plugin() {
        let host = echo_host();
        assert!(host.handles("foo"));
        assert!(host.handles("FOO")); // case-insensitive
        assert!(!host.handles("bar"));
        let r = host.extract("foo", b"hello wasm").unwrap().unwrap();
        assert_eq!(r.status, WasmStatus::Complete);
        assert_eq!(r.text, "hello wasm");
    }

    #[test]
    fn declared_extensions_are_read_from_the_plugin() {
        let host = PluginHost::new().unwrap();
        let module = compile(host.engine(), ECHO_WAT);
        let plugin = Plugin::load_from_module(host.engine(), module, "echo").unwrap();
        assert_eq!(plugin.extensions, vec!["foo".to_string()]);
    }

    #[test]
    fn a_fuel_bomb_is_contained_not_fatal() {
        // A plugin whose extract loops forever must exhaust fuel and error,
        // never hang the host.
        let wat = r#"
        (module
          (memory (export "memory") 1)
          (func (export "rsd_abi_version") (result i32) (i32.const 1))
          (func (export "rsd_alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "rsd_extensions") (result i64)
            (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const 0)))
          (func (export "rsd_extract") (param i32) (param i32) (result i64)
            (loop $l (br $l))  ;; infinite loop
            (i64.const 0)))"#;
        let mut host = PluginHost::new().unwrap();
        let module = compile(host.engine(), wat);
        host.add(Plugin {
            engine: host.engine().clone(),
            module,
            name: "bomb".into(),
            extensions: vec!["bomb".into()],
        });
        let r = host.extract("bomb", b"x").unwrap();
        assert!(matches!(r, Err(WasmError::Exhausted)), "got {r:?}");
    }
}
