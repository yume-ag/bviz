//! The threads bviz spawns in the browser, each a Web Worker.

/// The global by which the page names the address of the script each thread's
/// worker starts in. The site's content security policy admits a worker only
/// from a script the site serves.
#[cfg(target_feature = "atomics")]
const WORKER_SCRIPT: &str = "bvizWorkerScript";

/// Runs `start` once bviz's threads can start: every thread wasm_thread spawns
/// from then on starts in the script the page names by [`WORKER_SCRIPT`], and
/// rayon's global pool stands on such threads.
#[cfg(target_feature = "atomics")]
pub fn when_threads_start(start: impl FnOnce() + 'static) {
    let script = js_sys::Reflect::get(&js_sys::global(), &WORKER_SCRIPT.into())
        .ok()
        .and_then(|script| script.as_string())
        .unwrap_or_else(|| panic!("the page names no worker script as {WORKER_SCRIPT}"));
    wasm_thread::Builder::new()
        .worker_script_url(script)
        .set_default();
    bviz_parallel::build_global_pool();
    start();
}

/// Runs `start` at once, as a build without threads starts none.
#[cfg(not(target_feature = "atomics"))]
pub fn when_threads_start(start: impl FnOnce() + 'static) {
    start();
}
