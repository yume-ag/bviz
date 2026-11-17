//! Files the page's visitor hands bviz, read where they are by bviz's threads.
//! A thread can take no browser `File`, as it handles no message while it runs
//! bviz, so a worker of the page's own, the file server, holds the files and
//! writes each slice a thread asks for through the file's [`Slot`] into bviz's
//! memory.

use crate::files::Unread;
use bviz_input::{Named, ReadAt};
use js_sys::{Atomics, Int32Array, Object, Reflect, WebAssembly};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::mem::offset_of;
use std::ops::ControlFlow;
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
    offset_low: AtomicU32,
    offset_high: AtomicU32,
    /// Where in bviz's memory the slice goes.
    into: AtomicU32,
    length: AtomicU32,
    /// The bytes read, or one of the answers that tell of none.
    answer: AtomicI32,
}

/// What the readers of one file reach the server by, so that a reader waits
/// only on the file it reads. A slot lives as long as the page: a worker that
/// has failed or been terminated may not have stopped touching it, so it is
/// taken again only once a server has acknowledged the release of its file.
struct Slot {
    request: Request,
    /// Held by the one thread at a time that writes the request, while it
    /// waits for the answer.
    asking: Mutex<()>,
}

/// A file server running, which ends when this drops.
struct Server {
    /// What tells this server's events from those of one that ran before.
    number: u32,
    worker: Worker,
    /// The slot of each file handed over whose release the server is yet to
    /// acknowledge. The server ends once none is left, when no reader can be
    /// waiting on it.
    live: HashMap<u32, &'static Slot>,
    /// The window thread's end of the channel a thread tells it on of each
    /// file it lets go of: the window thread hands the server its files, but
    /// any thread may drop one. It forwards each release to the server on the
    /// port it handed the file over on, so the release follows the file.
    releases: BroadcastChannel,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.worker.terminate();
        self.releases.close();
    }
}

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);
static NEXT_SERVER: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// The file server the window thread hands files to, started on the
    /// first it hands over while none runs.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };

    /// The slots of files whose release a server has acknowledged, for the
    /// files handed over next.
    static FREE_SLOTS: RefCell<Vec<&'static Slot>> = const { RefCell::new(Vec::new()) };

    /// This thread's view of bviz's memory by words.
    static WORDS: RefCell<Int32Array> = RefCell::new(Int32Array::new(&memory().buffer()));
}

/// Each of `files` as a file bviz's threads read where it is, under its name.
pub(crate) fn serve(files: Vec<File>) -> Vec<Result<Named, Unread>> {
    SERVER.with_borrow_mut(|server| {
        let count = files.len();
        if let Some(running) = server {
            running.live.reserve(count);
        }
        (files.into_iter())
            .map(|file| {
                let name = file.name();
                let running = match server {
                    Some(running) => Ok(running),
                    None => start_server(count).map(|started| server.insert(started)),
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

/// Hands `file` to `server` with a slot of its own, returning what reads it
/// there.
fn hand_over(server: &mut Server, file: &File) -> Result<Served, String> {
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let slot = FREE_SLOTS.with_borrow_mut(Vec::pop).unwrap_or_else(|| {
        Box::leak(Box::new(Slot {
            request: Request {
                state: AtomicI32::new(IDLE),
                offset_low: AtomicU32::new(0),
                offset_high: AtomicU32::new(0),
                into: AtomicU32::new(0),
                length: AtomicU32::new(0),
                answer: AtomicI32::new(0),
            },
            asking: Mutex::new(()),
        }))
    });
    let message = object(&[
        ("id", id.into()),
        ("file", file.into()),
        ("layout", slot.layout().into()),
    ]);
    if let Err(error) = server.worker.post_message(&message) {
        FREE_SLOTS.with_borrow_mut(|free| free.push(slot));
        return Err(js_error(error));
    }
    server.live.insert(id, slot);
    Ok(Served {
        id,
        slot,
        releases: server.releases.name(),
    })
}

/// Starts a file server on the memory bviz shares between its threads, with
/// room for `files` files.
fn start_server(files: usize) -> Result<Server, String> {
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
    let number = NEXT_SERVER.fetch_add(1, Ordering::Relaxed);
    let server = Server {
        number,
        worker,
        live: HashMap::with_capacity(files),
        releases,
    };
    let released = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(id) = event.data().as_f64() else {
            return;
        };
        on_server(number, |running| {
            if running.live.contains_key(&(id as u32)) {
                let release = object(&[("id", id.into())]);
                running.worker.post_message(&release).ok();
            }
            ControlFlow::Continue(())
        });
    });
    server
        .releases
        .set_onmessage(Some(released.into_js_value().unchecked_ref()));
    let acknowledged = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(id) = event.data().as_f64() else {
            return;
        };
        on_server(number, |running| {
            if let Some(slot) = running.live.remove(&(id as u32)) {
                FREE_SLOTS.with_borrow_mut(|free| free.push(slot));
            }
            if running.live.is_empty() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
    });
    server
        .worker
        .set_onmessage(Some(acknowledged.into_js_value().unchecked_ref()));
    let failed = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        on_server(number, |failed| {
            for slot in failed.live.values() {
                slot.request.state.store(GONE, Ordering::SeqCst);
                Atomics::notify(&words_to(slot.state()), slot.state()).ok();
            }
            ControlFlow::Break(())
        });
    });
    server
        .worker
        .set_onerror(Some(failed.into_js_value().unchecked_ref()));
    let start = object(&[("memory", memory().into()), ("answers", answers().into())]);
    server.worker.post_message(&start).map_err(js_error)?;
    Ok(server)
}

/// Runs `f` on the window thread's server where it is the one numbered
/// `number`, ending the server where `f` breaks.
fn on_server(number: u32, f: impl FnOnce(&mut Server) -> ControlFlow<()>) {
    SERVER.with_borrow_mut(|server| {
        if let Some(running) = server.as_mut().filter(|running| running.number == number)
            && f(running).is_break()
        {
            *server = None;
        }
    });
}

/// What the server answers a request by: each state, and the answer each
/// exception it meets gives by its name.
fn answers() -> Object {
    let states = object(&[
        ("idle", IDLE.into()),
        ("asked", ASKED.into()),
        ("answered", ANSWERED.into()),
    ]);
    let errors = object(&[
        ("NotFoundError", NOT_FOUND.into()),
        ("NotReadableError", NOT_READABLE.into()),
        // Gecko names its exceptions by XPCOM result; its FileReaderSync
        // throws NS_ERROR_FAILURE where a file ends short of the size it was
        // handed over at (dom/file/FileReaderSync.cpp).
        ("NS_ERROR_FILE_NOT_FOUND", NOT_FOUND.into()),
        ("NS_ERROR_FAILURE", CHANGED.into()),
    ]);
    object(&[
        ("states", states.into()),
        ("errors", errors.into()),
        ("failed", FAILED.into()),
        ("changed", CHANGED.into()),
    ])
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
    /// memory.
    fn layout(&self) -> Object {
        object(&[
            ("state", self.state().into()),
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
        ])
    }
}

/// A file a server holds, read through it; the server lets go of the file
/// when this drops.
struct Served {
    id: u32,
    slot: &'static Slot,
    /// The name of its server's channel of releases.
    releases: String,
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
        if let Ok(channel) = BroadcastChannel::new(&self.releases) {
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
