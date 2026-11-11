//! Files the page's visitor hands bviz, read where they are by bviz's threads.
//! A thread can take no browser `File`, as it handles no message while it runs
//! bviz, so a worker of the page's own, the file server, holds the files and
//! writes each slice a thread asks for through its [`Slot`] into bviz's memory.

use crate::files::Unread;
use bviz_input::{Named, ReadAt};
use js_sys::{Atomics, Int32Array, Object, Reflect, WebAssembly};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io;
use std::mem::offset_of;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{BroadcastChannel, File, MessageEvent, Worker, WorkerOptions, WorkerType};

/// The global by which the page names the address of the file server's
/// script.
const SERVER_SCRIPT: &str = "bvizFileServerScript";

/// The most bytes one request asks for, so a slice the server reads passes
/// through the browser's memory a bounded piece at a time.
const SLICE_BYTES: usize = 8 << 20;

/// What the request is at, in its `state`.
const IDLE: i32 = 0;
const ASKED: i32 = 1;
const ANSWERED: i32 = 2;
/// The server has stopped answering for good, so that a reader waiting on it
/// gives up. The window thread sets it on the server's error event, which
/// reaches it only once the server has stopped or where it never ran, so no
/// slice is written after.
const GONE: i32 = 3;

/// The answers that tell of no slice read: the exception the server met by
/// its name, any other as [`FAILED`], or a slice shorter than the file
/// handed over holds there as [`CHANGED`].
const NOT_FOUND: i32 = -1;
const NOT_READABLE: i32 = -2;
const FAILED: i32 = -3;
const CHANGED: i32 = -4;

/// A request for a slice of a file. The server finds each field by the word
/// index [`Slot::layout`] tells it.
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

impl Request {
    fn idle() -> Self {
        Request {
            state: AtomicI32::new(IDLE),
            file: AtomicU32::new(0),
            offset_low: AtomicU32::new(0),
            offset_high: AtomicU32::new(0),
            into: AtomicU32::new(0),
            length: AtomicU32::new(0),
            answer: AtomicI32::new(0),
        }
    }
}

/// What the readers of one file server reach it by. Each server takes a slot
/// of its own that is never freed, so no request for one server reaches
/// another, and a worker still running once it is terminated finds only its
/// own.
struct Slot {
    request: Request,
    /// Held by the one thread at a time that writes the request, while it
    /// waits for the answer.
    asking: Mutex<()>,
    /// The name of the channel a thread tells the window thread on of each
    /// file it lets go of: the window thread hands the server its files, but
    /// any thread may drop one.
    releases: String,
}

/// A file server running, which ends when this drops.
struct Server {
    worker: Worker,
    slot: &'static Slot,
    /// The ids of the files handed over and not let go of. The server ends
    /// once none is left, when no reader can be waiting on it.
    live: HashSet<u32>,
    /// The window thread's end of the channel of releases. It forwards each
    /// release to the server on the port it handed the file over on, so the
    /// release follows the file.
    releases: BroadcastChannel,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.worker.terminate();
        self.releases.close();
    }
}

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// The file server the window thread hands files to, started on the
    /// first it hands over while none runs.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };

    /// This thread's view of bviz's memory by words.
    static WORDS: RefCell<Int32Array> = RefCell::new(Int32Array::new(&memory().buffer()));
}

/// Each of `files` as a file bviz's threads read where it is, under its name.
pub(crate) fn serve(files: Vec<File>) -> Vec<Result<Named, Unread>> {
    SERVER.with_borrow_mut(|server| {
        (files.into_iter())
            .map(|file| {
                let name = file.name();
                let running = match server {
                    Some(running) => Ok(running),
                    None => start_server().map(|started| server.insert(started)),
                };
                let served = running.and_then(|running| hand_over(running, &file));
                // No release ends a server that holds no files.
                if served.is_err()
                    && server
                        .as_ref()
                        .is_some_and(|running| running.live.is_empty())
                {
                    *server = None;
                }
                match served {
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
fn hand_over(server: &mut Server, file: &File) -> Result<Served, String> {
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let message = object(&[("id", id.into()), ("file", file.into())]);
    server.worker.post_message(&message).map_err(js_error)?;
    server.live.insert(id);
    Ok(Served {
        id,
        slot: server.slot,
    })
}

/// Starts a file server on the memory bviz shares between its threads.
fn start_server() -> Result<Server, String> {
    let global = js_sys::global();
    let script = Reflect::get(&global, &SERVER_SCRIPT.into())
        .ok()
        .and_then(|script| script.as_string())
        .ok_or_else(|| format!("the page names no file server script as {SERVER_SCRIPT}"))?;
    // A name no other server takes, so the releases of its files reach it
    // alone.
    let crypto = web_sys::window().map(|window| window.crypto());
    let unique = crypto
        .ok_or("no window starts the file server")?
        .map_err(js_error)?;
    let name = format!("bviz:released:{}", unique.random_uuid());
    let options = WorkerOptions::new();
    options.set_type(WorkerType::Module);
    let worker = Worker::new_with_options(&script, &options).map_err(js_error)?;
    let releases = BroadcastChannel::new(&name)
        .inspect_err(|_| worker.terminate())
        .map_err(js_error)?;
    let slot: &'static Slot = Box::leak(Box::new(Slot {
        request: Request::idle(),
        asking: Mutex::new(()),
        releases: name,
    }));
    let server = Server {
        worker,
        slot,
        live: HashSet::new(),
        releases,
    };
    let released = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(id) = event.data().as_f64() else {
            return;
        };
        SERVER.with_borrow_mut(|server| {
            let Some(running) = server
                .as_mut()
                .filter(|running| ptr::eq(running.slot, slot))
            else {
                return;
            };
            running.live.remove(&(id as u32));
            if running.live.is_empty() {
                *server = None;
            } else {
                let release = object(&[("id", id.into())]);
                running.worker.post_message(&release).ok();
            }
        });
    });
    server
        .releases
        .set_onmessage(Some(released.into_js_value().unchecked_ref()));
    let failed = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        slot.request.state.store(GONE, Ordering::SeqCst);
        Atomics::notify(&words_to(slot.state()), slot.state()).ok();
        SERVER.with_borrow_mut(|server| {
            if server
                .as_ref()
                .is_some_and(|server| ptr::eq(server.slot, slot))
            {
                *server = None;
            }
        });
    });
    server
        .worker
        .set_onerror(Some(failed.into_js_value().unchecked_ref()));
    let start = object(&[
        ("memory", memory().into()),
        ("layout", slot.layout().into()),
    ]);
    server.worker.post_message(&start).map_err(js_error)?;
    Ok(server)
}

impl Slot {
    /// The index in bviz's memory of the word at `offset` bytes into the
    /// request.
    fn word(&self, offset: usize) -> u32 {
        let address = (&raw const self.request).expose_provenance() + offset;
        (address / size_of::<i32>()) as u32
    }

    fn state(&self) -> u32 {
        self.word(offset_of!(Request, state))
    }

    /// What the server reads the request by: each field's word in bviz's
    /// memory, each state, and the answer each exception it meets gives by
    /// its name.
    fn layout(&self) -> Object {
        let states = object(&[
            ("idle", IDLE.into()),
            ("asked", ASKED.into()),
            ("answered", ANSWERED.into()),
        ]);
        let errors = object(&[
            ("NotFoundError", NOT_FOUND.into()),
            ("NotReadableError", NOT_READABLE.into()),
            // Gecko names its exceptions by XPCOM result; its FileReaderSync
            // throws NS_ERROR_FAILURE where a file ends short of the size it
            // was handed over at (dom/file/FileReaderSync.cpp).
            ("NS_ERROR_FILE_NOT_FOUND", NOT_FOUND.into()),
            ("NS_ERROR_FAILURE", CHANGED.into()),
        ]);
        object(&[
            ("state", self.state().into()),
            ("file", self.word(offset_of!(Request, file)).into()),
            (
                "offsetLow",
                self.word(offset_of!(Request, offset_low)).into(),
            ),
            (
                "offsetHigh",
                self.word(offset_of!(Request, offset_high)).into(),
            ),
            ("into", self.word(offset_of!(Request, into)).into()),
            ("length", self.word(offset_of!(Request, length)).into()),
            ("answer", self.word(offset_of!(Request, answer)).into()),
            ("states", states.into()),
            ("errors", errors.into()),
            ("failed", FAILED.into()),
            ("changed", CHANGED.into()),
        ])
    }
}

/// A file a server holds, read through it; the server lets go of the file
/// when this drops.
struct Served {
    id: u32,
    slot: &'static Slot,
}

impl ReadAt for Served {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        bviz_parallel::assert_can_block();
        if buf.is_empty() {
            return Ok(0);
        }
        let length = buf.len().min(SLICE_BYTES);
        let slot = self.slot;
        let request = &slot.request;
        let _asking = (slot.asking.lock()).unwrap_or_else(|poisoned| poisoned.into_inner());
        request.file.store(self.id, Ordering::Relaxed);
        request.offset_low.store(offset as u32, Ordering::Relaxed);
        request
            .offset_high
            .store((offset >> u32::BITS) as u32, Ordering::Relaxed);
        request.into.store(
            buf.as_mut_ptr().expose_provenance() as u32,
            Ordering::Relaxed,
        );
        request.length.store(length as u32, Ordering::Relaxed);
        (request
            .state
            .compare_exchange(IDLE, ASKED, Ordering::SeqCst, Ordering::SeqCst))
        .map_err(|_| gone())?;
        let state = slot.state();
        let words = words_to(state);
        Atomics::notify(&words, state).ok();
        while request.state.load(Ordering::SeqCst) == ASKED {
            // A read must not return while its slice is asked for: the server
            // may yet write into `buf`.
            Atomics::wait(&words, state, ASKED).unwrap_or_else(|error| {
                panic!("cannot wait for the file server: {}", js_error(error))
            });
        }
        let answer = request.answer.load(Ordering::SeqCst);
        (request
            .state
            .compare_exchange(ANSWERED, IDLE, Ordering::SeqCst, Ordering::SeqCst))
        .map_err(|_| gone())?;
        match answer {
            NOT_FOUND => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the file is no longer where it was handed over from",
            )),
            NOT_READABLE => Err(io::Error::other(
                "the browser can no longer read the file, which may have changed since it was handed over",
            )),
            CHANGED => Err(io::Error::other(
                "the file has changed since it was handed over",
            )),
            read => usize::try_from(read)
                .map_err(|_| io::Error::other("the browser could not read the file")),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        // The channel's messages are already on their way as it closes.
        if let Ok(channel) = BroadcastChannel::new(&self.slot.releases) {
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

/// This thread's view of bviz's memory by words, made afresh where the memory
/// has grown past it to the word at `index` since.
fn words_to(index: u32) -> Int32Array {
    WORDS.with_borrow_mut(|words| {
        if words.length() <= index {
            *words = Int32Array::new(&memory().buffer());
        }
        words.clone()
    })
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
