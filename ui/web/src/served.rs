//! Files the page's visitor hands bviz, read where they are by bviz's threads.
//! A thread can take no browser `File`, as it handles no message while it runs
//! bviz, so a worker of the page's own, the file server, holds the files and
//! writes each slice a thread asks for through [`REQUEST`] into bviz's memory.

use crate::files::Unread;
use bviz_input::{Named, ReadAt};
use js_sys::{Atomics, Int32Array, Object, Reflect, WebAssembly};
use std::io;
use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{BroadcastChannel, File, Worker, WorkerOptions, WorkerType};

/// The global by which the page names the address of the file server's
/// script.
const SERVER_SCRIPT: &str = "bvizFileServerScript";

/// The most bytes one request asks for, so a slice the server reads passes
/// through the browser's memory a bounded piece at a time.
const SLICE_BYTES: usize = 8 << 20;

/// How long, in milliseconds, a thread waits on an answer between looks at
/// whether the server has gone.
const GONE_CHECK_MS: f64 = 1000.0;

/// What the request is at, in its `state`.
const IDLE: i32 = 0;
const ASKED: i32 = 1;
const ANSWERED: i32 = 2;

/// The answers that tell of no slice read: the DOM exception the server met
/// by its name, any other as [`FAILED`].
const NOT_FOUND: i32 = -1;
const NOT_READABLE: i32 = -2;
const FAILED: i32 = -3;

/// A request for a slice of a file, written by one thread at a time, which
/// holds [`ASKING`] while it waits for the answer. The server finds each
/// field by the word index [`layout`] tells it.
#[repr(C)]
struct Request {
    state: AtomicI32,
    file: AtomicU32,
    offset_low: AtomicU32,
    offset_high: AtomicU32,
    /// Where in bviz's memory the slice goes.
    into: AtomicU32,
    length: AtomicU32,
    /// The bytes read, or one of the answers that tell of none.
    answer: AtomicI32,
}

static REQUEST: Request = Request {
    state: AtomicI32::new(IDLE),
    file: AtomicU32::new(0),
    offset_low: AtomicU32::new(0),
    offset_high: AtomicU32::new(0),
    into: AtomicU32::new(0),
    length: AtomicU32::new(0),
    answer: AtomicI32::new(0),
};

static ASKING: Mutex<()> = Mutex::new(());

/// Whether the file server has failed, so that no request is answered.
static GONE: AtomicBool = AtomicBool::new(false);

/// The name of the channel a thread tells the server on of each file it
/// lets go of: the window thread hands the server its files, but any thread
/// may drop one.
static RELEASES: OnceLock<String> = OnceLock::new();

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// The file server, started by the window thread on the first file it
    /// hands over.
    static SERVER: Result<Worker, String> = start_server();

    /// This thread's view of bviz's memory by words, which holds
    /// [`REQUEST`] however the memory grows after it is made.
    static WORDS: Int32Array = Int32Array::new(&memory().buffer());
}

/// Each of `files` as a file bviz's threads read where it is, under its name.
pub(crate) fn serve(files: Vec<File>) -> Vec<Result<Named, Unread>> {
    SERVER.with(|server| {
        (files.into_iter())
            .map(|file| {
                let name = file.name();
                match server
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|server| hand_over(server, &file))
                {
                    Ok(source) => Ok(Named::Seekable {
                        name,
                        source: Box::new(source),
                        size: file.size() as u64,
                    }),
                    Err(why) => Err(Unread {
                        name,
                        error: io::Error::other(why),
                    }),
                }
            })
            .collect()
    })
}

/// Hands `file` to `server`, returning what reads it there.
fn hand_over(server: &Worker, file: &File) -> Result<Served, String> {
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let message = object(&[("id", id.into()), ("file", file.into())]);
    server.post_message(&message).map_err(js_error)?;
    Ok(Served { id })
}

/// Starts the file server on the memory bviz shares between its threads.
fn start_server() -> Result<Worker, String> {
    let global = js_sys::global();
    let script = Reflect::get(&global, &SERVER_SCRIPT.into())
        .ok()
        .and_then(|script| script.as_string())
        .ok_or_else(|| format!("the page names no file server script as {SERVER_SCRIPT}"))?;
    let options = WorkerOptions::new();
    options.set_type(WorkerType::Module);
    let server = Worker::new_with_options(&script, &options).map_err(js_error)?;
    let failed = Closure::<dyn Fn(web_sys::Event)>::new(|_| {
        GONE.store(true, Ordering::SeqCst);
        if let Ok(words) = WORDS.try_with(Int32Array::clone) {
            Atomics::notify(&words, word(offset_of!(Request, state))).ok();
        }
    });
    server.set_onerror(Some(failed.as_ref().unchecked_ref()));
    failed.forget();
    // A name no other page's server takes, so the releases of this page's
    // files reach this page's server alone.
    let crypto = web_sys::window().map(|window| window.crypto());
    let unique = crypto
        .ok_or("no window starts the file server")?
        .map_err(js_error)?;
    let releases = format!("bviz:released:{}", unique.random_uuid());
    let releases = RELEASES.get_or_init(|| releases);
    let start = object(&[
        ("memory", memory().into()),
        ("releases", releases.into()),
        ("layout", layout().into()),
    ]);
    server.post_message(&start).map_err(js_error)?;
    Ok(server)
}

/// What the server reads [`REQUEST`] by: each field's word in bviz's memory,
/// each state, and the answer each DOM exception it meets gives by its name.
fn layout() -> Object {
    let states = object(&[
        ("idle", IDLE.into()),
        ("asked", ASKED.into()),
        ("answered", ANSWERED.into()),
    ]);
    let errors = object(&[
        ("NotFoundError", NOT_FOUND.into()),
        ("NotReadableError", NOT_READABLE.into()),
    ]);
    object(&[
        ("state", word(offset_of!(Request, state)).into()),
        ("file", word(offset_of!(Request, file)).into()),
        ("offsetLow", word(offset_of!(Request, offset_low)).into()),
        ("offsetHigh", word(offset_of!(Request, offset_high)).into()),
        ("into", word(offset_of!(Request, into)).into()),
        ("length", word(offset_of!(Request, length)).into()),
        ("answer", word(offset_of!(Request, answer)).into()),
        ("states", states.into()),
        ("errors", errors.into()),
        ("failed", FAILED.into()),
    ])
}

/// A file the server holds, read through it; the server lets go of the file
/// when this drops.
struct Served {
    id: u32,
}

impl ReadAt for Served {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        bviz_parallel::assert_can_block();
        if buf.is_empty() {
            return Ok(0);
        }
        let length = buf.len().min(SLICE_BYTES);
        let _asking = ASKING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if GONE.load(Ordering::SeqCst) {
            return Err(gone());
        }
        REQUEST.file.store(self.id, Ordering::Relaxed);
        REQUEST.offset_low.store(offset as u32, Ordering::Relaxed);
        REQUEST
            .offset_high
            .store((offset >> u32::BITS) as u32, Ordering::Relaxed);
        REQUEST.into.store(
            buf.as_mut_ptr().expose_provenance() as u32,
            Ordering::Relaxed,
        );
        REQUEST.length.store(length as u32, Ordering::Relaxed);
        REQUEST.state.store(ASKED, Ordering::SeqCst);
        let state = word(offset_of!(Request, state));
        WORDS.with(|words| {
            Atomics::notify(words, state).ok();
            while REQUEST.state.load(Ordering::SeqCst) == ASKED {
                if GONE.load(Ordering::SeqCst) {
                    REQUEST.state.store(IDLE, Ordering::SeqCst);
                    return Err(gone());
                }
                Atomics::wait_with_timeout(words, state, ASKED, GONE_CHECK_MS).ok();
            }
            Ok(())
        })?;
        let answer = REQUEST.answer.load(Ordering::SeqCst);
        REQUEST.state.store(IDLE, Ordering::SeqCst);
        match answer {
            NOT_FOUND => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the file is no longer where it was handed over from",
            )),
            NOT_READABLE => Err(io::Error::other(
                "the browser can no longer read the file, which may have changed since it was handed over",
            )),
            read => usize::try_from(read)
                .map_err(|_| io::Error::other("the browser could not read the file")),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let Some(releases) = RELEASES.get() else {
            return;
        };
        // The channel's messages are already on their way as it closes.
        if let Ok(channel) = BroadcastChannel::new(releases) {
            channel.post_message(&self.id.into()).ok();
            channel.close();
        }
    }
}

fn gone() -> io::Error {
    io::Error::other("the browser's reader of the files handed over has failed")
}

fn memory() -> WebAssembly::Memory {
    wasm_bindgen::memory().unchecked_into()
}

/// The index in bviz's memory of the word at `offset` bytes into [`REQUEST`].
fn word(offset: usize) -> u32 {
    let address = (&raw const REQUEST).expose_provenance() + offset;
    (address / size_of::<i32>()) as u32
}

fn object(entries: &[(&str, JsValue)]) -> Object {
    let object = Object::new();
    for (key, value) in entries {
        Reflect::set(&object, &(*key).into(), value).ok();
    }
    object
}

fn js_error(error: JsValue) -> String {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => format!("{error:?}"),
    }
}
