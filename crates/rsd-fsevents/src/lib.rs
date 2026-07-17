//! rsd-fsevents: a safe wrapper over the FSEvents C API (P1.5).
//!
//! Design rules (DESIGN.md §7.1): file-level events requested; the callback
//! thread hands off through a *bounded* channel; overflow sets a flag instead
//! of blocking or growing — the consumer degrades to a scoped rescan. Events
//! are hints; nothing here is trusted beyond "look at this path".
//!
//! This is the one crate in the workspace permitted to contain `unsafe` (FFI).

use std::ffi::{c_void, CStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

// ---------- minimal CoreFoundation / FSEvents FFI ----------

type CFIndex = isize;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type Boolean = u8;
type FSEventStreamRef = *mut c_void;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external: Boolean,
    ) -> CFStringRef;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        num: CFIndex,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRelease(cf: *const c_void);
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        alloc: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths: CFArrayRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        rl: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> Boolean;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
}

// Stream-creation flags.
const CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
const CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
const CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

/// `kFSEventStreamEventIdSinceNow`.
pub const SINCE_NOW: u64 = u64::MAX;

/// Per-event flag bits (kFSEventStreamEventFlag*).
pub mod flags {
    pub const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    pub const USER_DROPPED: u32 = 0x0000_0002;
    pub const KERNEL_DROPPED: u32 = 0x0000_0004;
    pub const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
    pub const HISTORY_DONE: u32 = 0x0000_0010;
    pub const ROOT_CHANGED: u32 = 0x0000_0020;
    pub const ITEM_CREATED: u32 = 0x0000_0100;
    pub const ITEM_REMOVED: u32 = 0x0000_0200;
    pub const ITEM_INODE_META_MOD: u32 = 0x0000_0400;
    pub const ITEM_RENAMED: u32 = 0x0000_0800;
    pub const ITEM_MODIFIED: u32 = 0x0000_1000;
    pub const ITEM_IS_FILE: u32 = 0x0001_0000;
    pub const ITEM_IS_DIR: u32 = 0x0002_0000;
    pub const ITEM_IS_SYMLINK: u32 = 0x0004_0000;
}

/// Decoded per-event flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventFlags(pub u32);

impl EventFlags {
    pub fn must_scan_subdirs(&self) -> bool {
        self.0 & flags::MUST_SCAN_SUBDIRS != 0
    }
    /// The OS itself dropped events; the affected scope needs reconciliation.
    pub fn dropped(&self) -> bool {
        self.0 & (flags::USER_DROPPED | flags::KERNEL_DROPPED) != 0
    }
    pub fn ids_wrapped(&self) -> bool {
        self.0 & flags::EVENT_IDS_WRAPPED != 0
    }
    pub fn history_done(&self) -> bool {
        self.0 & flags::HISTORY_DONE != 0
    }
    pub fn root_changed(&self) -> bool {
        self.0 & flags::ROOT_CHANGED != 0
    }
    pub fn renamed(&self) -> bool {
        self.0 & flags::ITEM_RENAMED != 0
    }
    pub fn created(&self) -> bool {
        self.0 & flags::ITEM_CREATED != 0
    }
    pub fn is_dir(&self) -> bool {
        self.0 & flags::ITEM_IS_DIR != 0
    }
    pub fn is_file(&self) -> bool {
        self.0 & flags::ITEM_IS_FILE != 0
    }
    pub fn is_symlink(&self) -> bool {
        self.0 & flags::ITEM_IS_SYMLINK != 0
    }
}

/// One decoded FSEvents event.
#[derive(Debug, Clone)]
pub struct FsEvent {
    pub path: PathBuf,
    pub flags: EventFlags,
    pub event_id: u64,
}

/// The current end of the volume's event stream — persist this as the resume
/// cursor (`sinceWhen`) once derived work is durable (P2.2).
pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

pub struct WatchConfig {
    /// Resume cursor; `None` means "from now".
    pub since: Option<u64>,
    pub latency: Duration,
    /// Bounded channel capacity between the callback thread and the consumer.
    pub capacity: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            since: None,
            latency: Duration::from_millis(100),
            capacity: 8_192,
        }
    }
}

struct CallbackState {
    tx: mpsc::SyncSender<FsEvent>,
    overflow: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
}

extern "C" fn stream_callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    let state = unsafe { &*(info as *const CallbackState) };
    let paths = event_paths as *const *const libc::c_char;
    for i in 0..num_events {
        let (path, fl, id) = unsafe {
            let c = CStr::from_ptr(*paths.add(i));
            (
                PathBuf::from(std::ffi::OsStr::from_bytes(c.to_bytes())),
                EventFlags(*event_flags.add(i)),
                *event_ids.add(i),
            )
        };
        state.delivered.fetch_add(1, Ordering::Relaxed);
        let ev = FsEvent {
            path,
            flags: fl,
            event_id: id,
        };
        // Never block the FSEvents thread: full channel => overflow flag; the
        // consumer must schedule a scoped rescan (structural backpressure).
        if state.tx.try_send(ev).is_err() {
            state.overflow.store(true, Ordering::Relaxed);
        }
    }
}

/// A running FSEvents stream on its own CFRunLoop thread.
pub struct Watcher {
    runloop: usize,
    thread: Option<JoinHandle<()>>,
    overflow: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
}

impl Watcher {
    /// Watch `paths`. Returns the watcher, the bounded event receiver, and the
    /// overflow flag (set when the channel was full and events were shed).
    pub fn start(
        paths: &[&Path],
        cfg: WatchConfig,
    ) -> io::Result<(Watcher, mpsc::Receiver<FsEvent>)> {
        // FSEventStream setup (create/schedule/start) races when performed
        // concurrently from multiple threads in one process (crashes with
        // SIGTRAP inside CoreServices). Serialize the setup critical section;
        // steady-state event delivery is unaffected.
        static SETUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _setup_guard = SETUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = mpsc::sync_channel(cfg.capacity);
        let overflow = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(AtomicU64::new(0));
        let state = Box::new(CallbackState {
            tx,
            overflow: overflow.clone(),
            delivered: delivered.clone(),
        });
        let path_bufs: Vec<Vec<u8>> = paths
            .iter()
            .map(|p| p.as_os_str().as_bytes().to_vec())
            .collect();
        let since = cfg.since.unwrap_or(SINCE_NOW);
        let latency = cfg.latency.as_secs_f64();

        let (ready_tx, ready_rx) = mpsc::channel::<Result<usize, String>>();
        let thread = std::thread::Builder::new()
            .name("rsd-fsevents".into())
            .spawn(move || unsafe {
                let state_ptr = Box::into_raw(state);
                // Build CFArray<CFString> of watch paths.
                let cf_strings: Vec<CFStringRef> = path_bufs
                    .iter()
                    .map(|b| {
                        CFStringCreateWithBytes(
                            std::ptr::null(),
                            b.as_ptr(),
                            b.len() as CFIndex,
                            K_CF_STRING_ENCODING_UTF8,
                            0,
                        )
                    })
                    .collect();
                let cf_array = CFArrayCreate(
                    std::ptr::null(),
                    cf_strings.as_ptr(),
                    cf_strings.len() as CFIndex,
                    &kCFTypeArrayCallBacks as *const c_void,
                );
                let context = FSEventStreamContext {
                    version: 0,
                    info: state_ptr as *mut c_void,
                    retain: std::ptr::null(),
                    release: std::ptr::null(),
                    copy_description: std::ptr::null(),
                };
                let stream = FSEventStreamCreate(
                    std::ptr::null(),
                    stream_callback,
                    &context,
                    cf_array,
                    since,
                    latency,
                    CREATE_FLAG_NO_DEFER | CREATE_FLAG_WATCH_ROOT | CREATE_FLAG_FILE_EVENTS,
                );
                CFRelease(cf_array);
                for s in cf_strings {
                    CFRelease(s);
                }
                if stream.is_null() {
                    let _ = ready_tx.send(Err("FSEventStreamCreate returned null".into()));
                    drop(Box::from_raw(state_ptr));
                    return;
                }
                // Retain: the runloop object dies with this thread, but the
                // Watcher on another thread holds a pointer to it for stop().
                let rl = CFRetain(CFRunLoopGetCurrent() as *const c_void) as CFRunLoopRef;
                FSEventStreamScheduleWithRunLoop(stream, rl, kCFRunLoopDefaultMode);
                if FSEventStreamStart(stream) == 0 {
                    let _ = ready_tx.send(Err("FSEventStreamStart failed".into()));
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    drop(Box::from_raw(state_ptr));
                    return;
                }
                let _ = ready_tx.send(Ok(rl as usize));
                CFRunLoopRun(); // parked until CFRunLoopStop from Watcher::stop
                FSEventStreamStop(stream);
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                drop(Box::from_raw(state_ptr));
            })?;

        match ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| io::Error::other(format!("fsevents thread not ready: {e}")))?
        {
            Ok(runloop) => Ok((
                Watcher {
                    runloop,
                    thread: Some(thread),
                    overflow,
                    delivered,
                },
                rx,
            )),
            Err(msg) => {
                let _ = thread.join();
                Err(io::Error::other(msg))
            }
        }
    }

    /// True when the callback shed events into the overflow path; the caller
    /// owns clearing it after scheduling reconciliation.
    pub fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::Relaxed)
    }

    pub fn clear_overflow(&self) {
        self.overflow.store(false, Ordering::Relaxed);
    }

    /// Shared handle to the overflow flag, for consumers that recover on their
    /// own thread (swap-and-rescan).
    pub fn overflow_handle(&self) -> Arc<AtomicBool> {
        self.overflow.clone()
    }

    /// Total events the OS delivered to the callback (observability/tests).
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    /// Stop the stream and join the runloop thread.
    pub fn stop(mut self) -> std::thread::Result<()> {
        self.teardown()
    }

    /// Idempotent: stops the runloop, joins the thread, releases the retained
    /// runloop. Guarded by `thread` so stop-then-drop never double-stops or
    /// touches the runloop after release.
    fn teardown(&mut self) -> std::thread::Result<()> {
        let Some(t) = self.thread.take() else {
            return Ok(());
        };
        unsafe { CFRunLoopStop(self.runloop as CFRunLoopRef) };
        let r = t.join();
        unsafe { CFRelease(self.runloop as *const c_void) };
        r
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}
