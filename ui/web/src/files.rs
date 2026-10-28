//! Files the page's visitor hands bviz, which the browser gives as bytes
//! alone, read whole into memory.

use bviz_input::Named;
use js_sys::{Promise, Uint8Array};
use std::io;
use std::ops::ControlFlow;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{DragEvent, File, FileList, HtmlInputElement};

/// Bytes of a file the browser hands over at a time, so a large file passes
/// through the browser's memory a slice at a time on its way into bviz's.
const CHUNK_BYTES: u32 = 16 << 20;

/// Why a file that does not fit bviz's memory does not open.
const TOO_LARGE: &str = "too large to hold in the browser's memory";

/// A file handed over that bviz could not read.
pub struct Unread {
    pub name: String,
    pub error: io::Error,
}

/// Asks for files through the browser's file chooser, which opens before
/// this returns: a browser lets a page open it only while it handles the
/// user's input. Resolves to the files chosen, none where the chooser is
/// dismissed.
pub fn choose_files() -> impl Future<Output = Vec<File>> {
    let chooser = chooser().map(|input| {
        let mut answered = |answer, _| {
            for event in ["change", "cancel"] {
                input.add_event_listener_with_callback(event, &answer).ok();
            }
        };
        let answered = JsFuture::from(Promise::new(&mut answered));
        input.click();
        (input, answered)
    });
    async move {
        let Some((input, answered)) = chooser else {
            return Vec::new();
        };
        answered.await.ok();
        input.remove();
        input.files().map(files_of).unwrap_or_default()
    }
}

/// A file chooser of its own for one question, so no answer to an earlier
/// one reaches it, standing hidden in the page until it is answered: a
/// browser may neither open nor answer one outside the page.
fn chooser() -> Option<HtmlInputElement> {
    let document = web_sys::window()?.document()?;
    let input: HtmlInputElement = document.create_element("input").ok()?.dyn_into().ok()?;
    input.set_type("file");
    input.set_multiple(true);
    input.set_hidden(true);
    document.body()?.append_child(&input).ok()?;
    Some(input)
}

/// Hands `dropped` the files of each drop of files on the page, for as long
/// as the page stands, in place of the browser opening them itself.
pub fn on_files_dropped(mut dropped: impl FnMut(Vec<File>) + 'static) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let over = Closure::<dyn Fn(DragEvent)>::new(|event: DragEvent| {
        if carries_files(&event) {
            event.prevent_default();
        }
    });
    // A panic escaping `dropped` ends bviz in its crashed state, so nothing
    // observes what the unwind leaves behind.
    let drop = Closure::<dyn FnMut(DragEvent)>::own_assert_unwind_safe(move |event: DragEvent| {
        event.prevent_default();
        let files = (event.data_transfer())
            .and_then(|carried| carried.files())
            .map(files_of)
            .unwrap_or_default();
        if !files.is_empty() {
            dropped(files);
        }
    });
    for (name, listener) in [("dragover", over.as_ref()), ("drop", drop.as_ref())] {
        (document.add_event_listener_with_callback(name, listener.unchecked_ref())).ok();
    }
    over.forget();
    drop.forget();
}

/// Whether `event` drags files, which a drop on the page may take: anything
/// else dragged is left to the browser.
fn carries_files(event: &DragEvent) -> bool {
    (event.data_transfer()).is_some_and(|carried| carried.types().includes(&"Files".into(), 0))
}

fn files_of(list: FileList) -> Vec<File> {
    let mut files = Vec::with_capacity(list.length() as usize);
    files.extend((0..list.length()).filter_map(|at| list.get(at)));
    files
}

/// Each of `files` read whole, under its name, telling `progress` the place
/// among `files` of the file being read and how many of its bytes are read
/// as each slice of it arrives; `None` where `progress` breaks off the
/// reading.
pub async fn read_files(
    files: Vec<File>,
    mut progress: impl FnMut(usize, u64) -> ControlFlow<()>,
) -> Option<Vec<Result<Named, Unread>>> {
    let mut read = Vec::with_capacity(files.len());
    for (at, file) in files.into_iter().enumerate() {
        let name = file.name();
        read.push(match read_file(&file, |bytes| progress(at, bytes)).await {
            Ok(ControlFlow::Continue(bytes)) => Ok(Named::Memory { name, bytes }),
            Ok(ControlFlow::Break(())) => return None,
            Err(error) => Err(Unread { name, error }),
        });
    }
    Some(read)
}

/// `file`'s bytes, in a buffer sized for them before the first is read,
/// telling `progress` how many are read as each slice arrives, unless it
/// breaks off the reading.
async fn read_file(
    file: &File,
    mut progress: impl FnMut(u64) -> ControlFlow<()>,
) -> io::Result<ControlFlow<(), Vec<u8>>> {
    let size = file.size();
    let mut bytes = Vec::new();
    (bytes.try_reserve_exact(size as usize))
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, TOO_LARGE))?;
    let mut at = 0.0;
    while at < size {
        let end = size.min(at + f64::from(CHUNK_BYTES));
        let slice = file.slice_with_f64_and_f64(at, end).map_err(js_error)?;
        let chunk = JsFuture::from(slice.array_buffer())
            .await
            .map_err(js_error)?;
        let chunk = Uint8Array::new(&chunk);
        let start = bytes.len();
        bytes.resize(start + chunk.length() as usize, 0);
        chunk.copy_to(&mut bytes[start..]);
        if progress(bytes.len() as u64).is_break() {
            return Ok(ControlFlow::Break(()));
        }
        at = end;
    }
    Ok(ControlFlow::Continue(bytes))
}

fn js_error(error: JsValue) -> io::Error {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => io::Error::other(String::from(error.message())),
        None => io::Error::other(format!("{error:?}")),
    }
}
