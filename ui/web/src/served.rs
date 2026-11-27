//! Sources bviz's threads read where they are: the files the page's visitor
//! hands bviz, and the documents it fetches by URL. A thread can take no
//! browser `File` or fetch, as it handles no message while it runs bviz, so a
//! worker of the page's own, the source server, holds each source and writes
//! each slice a thread asks for through the source's [`Slot`] into bviz's
//! memory.

use crate::fetched::{STALL_MILLIS, options};
use crate::files::Unread;
use crate::js_message;
use bviz_input::{Named, ReadAt};
use bviz_links::Origin;
use bviz_links::fetch::{self, Answered, FetchError, Probed, Reading};
use js_sys::{Array, Atomics, Function, Int32Array, Object, Promise, Reflect, WebAssembly};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::marker::PhantomData;
use std::mem::offset_of;
use std::ops::ControlFlow;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{BroadcastChannel, File, MessageEvent, Worker, WorkerOptions, WorkerType};

/// The global by which the page names the address of the source server's
/// script.
const SERVER_SCRIPT: &str = "bvizSourceServerScript";

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

/// The answers that tell of no slice read: the exception the server met
/// reading a file by its name, any other as [`FAILED`], or a source that no
/// longer holds what it held when bviz began reading it as [`CHANGED`].
const NOT_FOUND: i32 = -1;
const NOT_READABLE: i32 = -2;
const FAILED: i32 = -3;
const CHANGED: i32 = -4;
/// The answers only a fetched document gets, each telling why its fetch
/// failed: [`REFUSED`] where a later response comes from elsewhere,
/// [`NOT_SERVED`] where the server fails or refuses a request, and
/// [`UNORDERED`] where a document that is read once, in order, is asked for
/// out of order.
const CANCELLED: i32 = -5;
const TOO_LARGE: i32 = -6;
const STALLED: i32 = -7;
const REFUSED: i32 = -8;
const NOT_SERVED: i32 = -9;
const UNORDERED: i32 = -10;

/// A request for a slice of a source. The server finds each field by the word
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

/// What the readers of one source reach the server by, so that a reader waits
/// only on the source it reads. A slot lives as long as the page: a worker
/// that has failed or been terminated may not have stopped touching it, so it
/// is taken again only once a server has acknowledged the release of its
/// source.
struct Slot {
    request: Request,
    /// Held by the one thread at a time that writes the request, while it
    /// waits for the answer.
    asking: Mutex<()>,
}

/// A source server running, which ends when this drops.
struct Server {
    /// What tells this server's events from those of one that ran before.
    number: u32,
    worker: Worker,
    /// The slot of each source handed over whose release the server is yet
    /// to acknowledge. The server ends once none is left, when no reader can
    /// be waiting on it.
    live: HashMap<u32, &'static Slot>,
    /// What resolves the wait of each document handed over for its first
    /// response, until the server tells of it.
    awaiting: HashMap<u32, Function>,
    /// The window thread's end of the channel a thread tells it on of each
    /// source it lets go of: the window thread hands the server its sources,
    /// but any thread may drop one. It forwards each release to the server on
    /// the port it handed the source over on, so the release follows the
    /// source.
    releases: BroadcastChannel,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.worker.terminate();
        self.releases.close();
    }
}

static NEXT_SOURCE: AtomicU32 = AtomicU32::new(0);
static NEXT_SERVER: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// The source server the window thread hands sources to, started on the
    /// first it hands over while none runs.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };

    /// The slots of sources whose release a server has acknowledged, for the
    /// sources handed over next.
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
                let served = running(server, count).and_then(|running| {
                    let (id, slot) = hand_over(running, &[("file", file.clone().into())])?;
                    Ok(running.served(id, slot, Kind::File))
                });
                if served.is_err() {
                    end_if_idle(server);
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

/// What cancels the fetch of a document, which then fails as
/// [`FetchError::Cancelled`], as does every read of it after.
pub struct Cancel {
    server: u32,
    id: u32,
    /// The server is reached from the window thread alone.
    _window_thread: PhantomData<*const ()>,
}

impl Cancel {
    pub fn cancel(&self) {
        on_server(self.server, |running| {
            if running.live.contains_key(&self.id) {
                running.post(&[("cancel", self.id.into())]);
            }
            ControlFlow::Continue(())
        });
    }
}

/// Fetches the document `request` asks for, to be read where it is by bviz's
/// threads once its first response, coming from an origin other than the one
/// asked, comes from one `accepts`, as [`Answered::reading`] decides.
pub fn fetch(
    request: fetch::Request,
    accepts: impl FnOnce(&Origin) -> bool,
) -> Result<(Cancel, impl Future<Output = Result<Named, FetchError>>), FetchError> {
    let (deciding, answered) = SERVER.with_borrow_mut(|server| {
        let handed = running(server, 1).and_then(|running| {
            let (id, slot) = hand_over(
                running,
                &[
                    ("url", request.url.url().as_str().into()),
                    ("init", options(request.credentials).into()),
                    ("maxBytes", (request.max_bytes as f64).into()),
                ],
            )?;
            // Handed out of the executor in a JavaScript array, which a
            // panic leaves as consistent as the executor found it.
            let resolvers = Array::new();
            let answered = JsFuture::from(Promise::new(&mut |resolve, _| {
                resolvers.push(&resolve);
            }));
            running
                .awaiting
                .insert(id, resolvers.get(0).unchecked_into());
            let deciding = Deciding {
                number: running.number,
                id,
                undecided: Some(slot),
            };
            Ok((deciding, answered))
        });
        if handed.is_err() {
            end_if_idle(server);
        }
        handed.map_err(FetchError::Failed)
    })?;
    let cancel = Cancel {
        server: deciding.number,
        id: deciding.id,
        _window_thread: PhantomData,
    };
    let fetched = async move {
        // Resolved with nothing where the server fails.
        let told = answered.await.unwrap_or(JsValue::UNDEFINED);
        if let Some(answer) = number_at(&told, "answer") {
            return Err(fetch_failed(answer as i32, &told));
        }
        let url = (Reflect::get(&told, &"url".into()).ok())
            .and_then(|url| url.as_string())
            .ok_or_else(|| FetchError::Failed(GONE_MESSAGE.into()))?;
        let length = number_at(&told, "length").and_then(whole_length);
        let status = number_at(&told, "status").map_or(0, |status| status as u16);
        let answered = Answered {
            url: &url,
            status,
            length,
        };
        request.check(&answered, accepts)?;
        let probed = Probed {
            whole: flag_at(&told, "whole"),
            validated: flag_at(&told, "validated"),
        };
        let reading = answered.reading(probed)?;
        let seekable = matches!(reading, Reading::Seekable { .. });
        let served = deciding.proceed(seekable, length, request.max_bytes)?;
        let name = request.url.name();
        Ok(match reading {
            Reading::Seekable { size } => Named::Seekable {
                name,
                source: Box::new(served),
                size,
            },
            Reading::Stream => Named::Stream {
                name,
                reader: Box::new(Streamed { served, at: 0 }),
                size: length,
            },
        })
    };
    Ok((cancel, fetched))
}

/// A document handed over whose first response the window thread is yet to
/// let be read; the server lets go of it where this drops undecided.
struct Deciding {
    number: u32,
    id: u32,
    /// The document's slot, until the decision hands it to its `Served`.
    undecided: Option<&'static Slot>,
}

impl Deciding {
    /// What reads the document, once the server is told to serve it as
    /// `seekable` or not, of `size` where that is known, holding at most
    /// `max_bytes`.
    fn proceed(
        mut self,
        seekable: bool,
        size: Option<u64>,
        max_bytes: u64,
    ) -> Result<Served, FetchError> {
        let mut served = None;
        on_server(self.number, |running| {
            if let Some(slot) =
                (self.undecided.take()).filter(|_| running.live.contains_key(&self.id))
            {
                let size = size.map_or(JsValue::NULL, |size| (size as f64).into());
                running.post(&[
                    ("proceed", self.id.into()),
                    ("seekable", seekable.into()),
                    ("size", size),
                ]);
                served = Some(running.served(self.id, slot, Kind::Document { max_bytes }));
            }
            ControlFlow::Continue(())
        });
        served.ok_or_else(|| FetchError::Failed(GONE_MESSAGE.into()))
    }
}

impl Drop for Deciding {
    fn drop(&mut self) {
        if self.undecided.is_some() {
            on_server(self.number, |running| {
                running.awaiting.remove(&self.id);
                if running.live.contains_key(&self.id) {
                    running.post(&[("refuse", self.id.into())]);
                }
                ControlFlow::Continue(())
            });
        }
    }
}

/// Why the server failed the fetch of a document before its first response,
/// as its `answer` and what it `told` of it say.
fn fetch_failed(answer: i32, told: &JsValue) -> FetchError {
    match answer {
        CANCELLED => FetchError::Cancelled,
        STALLED => FetchError::Stalled,
        _ => FetchError::Failed(
            (Reflect::get(told, &"detail".into()).ok())
                .and_then(|detail| detail.as_string())
                .unwrap_or_else(|| "the server could not be reached".into()),
        ),
    }
}

fn flag_at(object: &JsValue, key: &str) -> bool {
    Reflect::get(object, &key.into()).is_ok_and(|flag| flag.is_truthy())
}

/// `length`, told by the server, where it is a length bviz reads.
fn whole_length(length: f64) -> Option<u64> {
    let whole =
        length.fract() == 0.0 && (0.0..=fetch::MAX_DECLARED_LENGTH as f64).contains(&length);
    whole.then_some(length as u64)
}

fn number_at(object: &JsValue, key: &str) -> Option<f64> {
    Reflect::get(object, &key.into()).ok()?.as_f64()
}

/// The server running in `server`, started with room for `sources` where none
/// runs.
fn running(server: &mut Option<Server>, sources: usize) -> Result<&mut Server, String> {
    match server {
        Some(running) => Ok(running),
        None => start_server(sources).map(|started| server.insert(started)),
    }
}

/// Ends the server in `server` where it holds no sources, which no release
/// would end.
fn end_if_idle(server: &mut Option<Server>) {
    if server
        .as_ref()
        .is_some_and(|running| running.live.is_empty())
    {
        *server = None;
    }
}

/// Hands `server` the source `entries` describe, with a slot of its own,
/// returning the source's id and its slot.
fn hand_over(
    server: &mut Server,
    entries: &[(&str, JsValue)],
) -> Result<(u32, &'static Slot), String> {
    let id = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
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
    let message = object(entries);
    Reflect::set(&message, &"id".into(), &id.into()).ok();
    Reflect::set(&message, &"layout".into(), &slot.layout()).ok();
    if let Err(error) = server.worker.post_message(&message) {
        FREE_SLOTS.with_borrow_mut(|free| free.push(slot));
        return Err(js_message(error));
    }
    server.live.insert(id, slot);
    Ok((id, slot))
}

impl Server {
    fn served(&self, id: u32, slot: &'static Slot, kind: Kind) -> Served {
        Served {
            id,
            slot,
            releases: self.releases.name(),
            kind,
        }
    }

    fn post(&self, entries: &[(&str, JsValue)]) {
        self.worker.post_message(&object(entries)).ok();
    }

    /// Takes in what the server tells, ending the server where it breaks.
    fn heard(&mut self, told: &JsValue) -> ControlFlow<()> {
        if let Some(id) = number_at(told, "released") {
            if let Some(slot) = self.live.remove(&(id as u32)) {
                FREE_SLOTS.with_borrow_mut(|free| free.push(slot));
            }
            if self.live.is_empty() {
                return ControlFlow::Break(());
            }
        }
        let first = number_at(told, "answered").or_else(|| number_at(told, "failed"));
        if let Some(resolve) = first.and_then(|id| self.awaiting.remove(&(id as u32))) {
            resolve.call1(&JsValue::NULL, told).ok();
        }
        ControlFlow::Continue(())
    }
}

/// Starts a source server on the memory bviz shares between its threads, with
/// room for `sources` sources.
fn start_server(sources: usize) -> Result<Server, String> {
    let global = js_sys::global();
    let script = Reflect::get(&global, &SERVER_SCRIPT.into())
        .ok()
        .and_then(|script| script.as_string())
        .ok_or_else(|| format!("the page names no source server script as {SERVER_SCRIPT}"))?;
    // A name no other server takes, so the releases of its sources reach it
    // alone.
    let crypto = web_sys::window().map(|window| window.crypto());
    let unique = crypto
        .ok_or("no window starts the source server")?
        .map_err(js_message)?;
    let name = format!("bviz:released:{}", unique.random_uuid());
    let options = WorkerOptions::new();
    options.set_type(WorkerType::Module);
    let worker = Worker::new_with_options(&script, &options).map_err(js_message)?;
    let releases = BroadcastChannel::new(&name)
        .inspect_err(|_| worker.terminate())
        .map_err(js_message)?;
    let number = NEXT_SERVER.fetch_add(1, Ordering::Relaxed);
    let server = Server {
        number,
        worker,
        live: HashMap::with_capacity(sources),
        awaiting: HashMap::new(),
        releases,
    };
    let released = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(id) = event.data().as_f64() else {
            return;
        };
        on_server(number, |running| {
            if running.live.contains_key(&(id as u32)) {
                running.post(&[("release", id.into())]);
            }
            ControlFlow::Continue(())
        });
    });
    server
        .releases
        .set_onmessage(Some(released.into_js_value().unchecked_ref()));
    let told = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        on_server(number, |running| running.heard(&event.data()));
    });
    server
        .worker
        .set_onmessage(Some(told.into_js_value().unchecked_ref()));
    let failed = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        on_server(number, |failed| {
            for slot in failed.live.values() {
                slot.request.state.store(GONE, Ordering::SeqCst);
                Atomics::notify(&words_to(slot.state()), slot.state()).ok();
            }
            for resolve in failed.awaiting.values() {
                resolve.call0(&JsValue::NULL).ok();
            }
            ControlFlow::Break(())
        });
    });
    server
        .worker
        .set_onerror(Some(failed.into_js_value().unchecked_ref()));
    let start = object(&[("memory", memory().into()), ("answers", answers().into())]);
    server.worker.post_message(&start).map_err(js_message)?;
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

/// What the server answers a request by: each state, the answer each
/// exception it meets reading a file gives by its name, and those a fetch
/// gives.
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
    let fetch = object(&[
        ("cancelled", CANCELLED.into()),
        ("tooLarge", TOO_LARGE.into()),
        ("stalled", STALLED.into()),
        ("refused", REFUSED.into()),
        ("notServed", NOT_SERVED.into()),
        ("unordered", UNORDERED.into()),
        ("stallMillis", STALL_MILLIS.into()),
    ]);
    object(&[
        ("states", states.into()),
        ("errors", errors.into()),
        ("failed", FAILED.into()),
        ("changed", CHANGED.into()),
        ("fetch", fetch.into()),
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

/// What a source is, which its readers' errors tell of.
#[derive(Clone, Copy)]
enum Kind {
    File,
    /// A document fetched by URL, of at most `max_bytes`.
    Document {
        max_bytes: u64,
    },
}

/// A source a server holds, read through it; the server lets go of the
/// source when this drops.
struct Served {
    id: u32,
    slot: &'static Slot,
    /// The name of its server's channel of releases.
    releases: String,
    kind: Kind,
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
                panic!("cannot wait for the source server: {}", js_message(error))
            });
        }
        let answer = request.answer.load(Ordering::SeqCst);
        (request
            .state
            .compare_exchange(ANSWERED, IDLE, Ordering::SeqCst, Ordering::SeqCst))
        .map_err(|_| gone())?;
        match usize::try_from(answer) {
            Ok(read) if read <= length => Ok(read),
            Ok(_) => Err(io::Error::other(
                "the browser's reader of the sources answered with more than was asked",
            )),
            Err(_) => Err(self.kind.error(answer)),
        }
    }
}

impl Kind {
    /// What a reader is told by the server's `answer` telling of no slice
    /// read.
    fn error(self, answer: i32) -> io::Error {
        match (self, answer) {
            (Kind::File, NOT_FOUND) => io::Error::new(
                io::ErrorKind::NotFound,
                "the file is no longer where it was handed over from",
            ),
            (Kind::File, NOT_READABLE) => io::Error::other(
                "the browser can no longer read the file, which may have changed since it was handed over",
            ),
            (Kind::File, CHANGED) => {
                io::Error::other("the file has changed since it was handed over")
            }
            (Kind::File, _) => io::Error::other("the browser could not read the file"),
            (Kind::Document { .. }, CHANGED) => io::Error::other(
                "the document has changed on its server since bviz began reading it",
            ),
            (Kind::Document { .. }, CANCELLED) => io::Error::other(FetchError::Cancelled),
            (Kind::Document { max_bytes }, TOO_LARGE) => io::Error::other(FetchError::TooLarge {
                declared: None,
                max: max_bytes,
            }),
            (Kind::Document { .. }, STALLED) => io::Error::other(FetchError::Stalled),
            (Kind::Document { .. }, REFUSED) => {
                io::Error::other("the server moved the document while bviz read it")
            }
            (Kind::Document { .. }, NOT_SERVED) => {
                io::Error::other("the server stopped serving the document")
            }
            (Kind::Document { .. }, UNORDERED) => io::Error::other(
                "the document's server serves no byte ranges, so it is read once, in order",
            ),
            (Kind::Document { .. }, _) => {
                io::Error::other("the browser could not read the document")
            }
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

/// A document read once, in order, through the server.
struct Streamed {
    served: Served,
    at: u64,
}

impl Read for Streamed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.served.read_at(buf, self.at)?;
        self.at += n as u64;
        Ok(n)
    }
}

const GONE_MESSAGE: &str = "the browser's reader of the sources handed over has failed";

fn gone() -> io::Error {
    io::Error::other(GONE_MESSAGE)
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
