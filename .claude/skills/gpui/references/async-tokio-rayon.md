# Bridging tokio and rayon into gpui

gpui runs on **its own executor** — a foreground queue on the UI thread plus a
small background thread pool (built on `async-task`/smol primitives, sized by
gpui). It is *not* a tokio runtime and it is *not* rayon. Two families of
foreign work therefore need an explicit bridge:

- **tokio** — futures from libraries that require tokio's reactor/runtime
  (`reqwest`, `hyper`, `tonic`/gRPC, `tokio::net`, `tokio::time`, most cloud
  SDKs). Polled on gpui's executor they panic with *"there is no reactor
  running, must be called from the context of a Tokio runtime"*, or fail the
  moment they call `tokio::runtime::Handle::current()`.
- **rayon** — CPU-bound *data parallelism* (parallel iterators, `par_sort`) over
  a large in-memory collection. Rayon owns a work-stealing thread pool and needs
  no runtime, but a parallel iterator **blocks the calling thread** until the
  whole job finishes, so calling it on the UI thread freezes the frame.

The reference implementations live in Zed at **v1.24.3**, in two Apache-licensed
crates: `crates/gpui_tokio/src/gpui_tokio.rs` (the whole tokio bridge, ~100
lines) and the rayon call site in `crates/sum_tree/src/sum_tree.rs`
(`from_par_iter`). Read them at the tag this repo targets — the gpui method
names (`background_spawn`, `read_global`) shift between versions.

## Tokio

### The bridge

Own one tokio runtime, keep its `Handle` as a gpui `Global`, and turn each
tokio `JoinHandle` back into a gpui `Task`. This is the entirety of
`gpui_tokio`; adapt it into a `//ui` crate (it depends only on `gpui`, `tokio`
with the `rt`/`rt-multi-thread` features, `anyhow`, and a one-line `defer`
Drop-guard):

```rust
pub fn init(cx: &mut App) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(bviz_parallel::worker_count())
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let handle = runtime.handle().clone();
    cx.set_global(GlobalTokio { owned_runtime: Some(runtime), handle });
}

impl Tokio {
    pub fn spawn<C, Fut, R>(cx: &C, f: Fut) -> Task<Result<R, JoinError>>
    where
        C: AppContext,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        cx.read_global(|tokio: &GlobalTokio, cx| {
            let join = tokio.handle.spawn(f);
            let abort = join.abort_handle();
            let cancel = defer(move || abort.abort());   // drop-to-cancel
            cx.background_spawn(async move {
                let result = join.await;
                drop(cancel);
                result
            })
        })
    }
}
```

Key properties, all load-bearing:

- The future runs on tokio's threads (its reactor is live there), then
  `background_spawn` awaits the `JoinHandle` on gpui's background pool and hands
  the result to gpui code. The returned value is a gpui `Task<Result<R,
  JoinError>>` you await like any other.
- **Dropping the gpui `Task` aborts the tokio task** — the `defer` guard fires
  `abort_handle.abort()`. This is what makes a view's in-flight request cancel
  when the view goes away; store the `Task` in a view field (same rule as
  `load_in_background`) so it lives exactly as long as the view wants the work.
- `GlobalTokio`'s `Drop` calls `runtime.shutdown_background()`, so the runtime
  tears down with the app.
- Provide a `spawn_result` variant that flattens `Task<anyhow::Result<R>>` for
  the common fallible case.

### Synchronous entry: `Handle::enter()`

Some constructors grab the ambient runtime at call time via
`Handle::current()` (`reqwest::Client::new()`, many SDK builders) rather than
when a future is polled. For those, enter the runtime for the duration of the
synchronous call:

```rust
let _guard = Tokio::handle(cx).enter();
let client = SdkClient::new();   // now sees a current runtime
```

The guard must stay in scope across the call.

### Where init goes

Call `init(cx)` once inside `application().run(|cx| …)`, before any
`Tokio::spawn`. Nothing needs tokio until the first bridged call.

## Rayon

### The rule: only from a background task

Never call a rayon parallel iterator on the UI thread. Run it inside a
background task and return the result to the UI thread — the same shape as
`load_in_background`, whose `work` closure already runs on gpui's background
executor:

```rust
self.task = load_in_background(
    cx,
    move || {
        bviz_parallel::workers()
            .install(|| graph.into_par_iter().map(layout_node).collect::<Vec<_>>())
    },
    |view, laid_out, cx| { view.nodes = laid_out; /* helper calls cx.notify() */ },
);
```

Or explicitly:

```rust
cx.spawn(async move |this, cx| {
    let sorted = cx
        .background_executor()
        .spawn(async move {
            // blocks THIS background thread
            bviz_parallel::workers().install(|| entries.par_sort_by(cmp));
            entries
        })
        .await;
    this.update(cx, |view, cx| { view.rows = sorted; cx.notify(); }).ok();
})
```

The rayon call occupies one gpui background thread while the worker pool does
the parallel work; the UI thread stays free to render.

### Gate parallelism by input size

Rayon's fan-out overhead loses to a serial pass on small inputs. Branch on
length: take the `par_*` path above a threshold element count and run serially
below it.

### Run rayon work on `bviz_parallel::workers()`

Every rayon entry point in the repo runs inside
`bviz_parallel::workers().install(..)`: the process's one pool, `worker_count()`
(physical cores) wide. A parallel iterator left outside it runs on rayon's
implicit global pool — a second set of workers, sized by logical threads, whose
threads contend with the first pool's for the same cores.

Reach for rayon on genuinely CPU-bound fan-out (independent per-item compute),
not to parallelize a pass that is already bandwidth-limited.

### gpui's own `scoped` — often enough without rayon

For a handful of coarse chunks you do not need rayon at all.
`cx.background_executor().scoped(|scope| { scope.spawn(async { … }); … })` runs
N futures on gpui's background pool and joins them — no extra dependency, no
second thread pool. Reach for rayon specifically when you want work-stealing
over a parallel *iterator* or `par_sort` across many small items; use `scoped`
(or the repo's existing `std::thread::scope` pattern) for a few large chunks.

## Keeping the pools apart

tokio's runtime, rayon's pool, and gpui's background pool are three independent
sets of threads. Do async I/O on tokio (bridged) or on gpui tasks, do CPU
fan-out on rayon, and cross between them only at task boundaries by returning
values. Do not `block_on` a future from inside a rayon worker, and do not run a
long rayon job on gpui's small background pool in many concurrent copies — each
occupies a gpui background thread for its full duration and can starve other
background tasks.
