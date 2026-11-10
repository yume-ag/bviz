// What a crash of bviz tells, on the page and in each of its threads' workers.

// The panic bviz last recorded on this thread, kept until the task that
// recorded it ends, so a crash in that task is the panic's, and one in a later
// task is not a panic bviz caught before it.
let lastPanic;
Object.defineProperty(globalThis, "bvizLastPanic", {
  get: () => lastPanic,
  set(message) {
    lastPanic = message;
    setTimeout(() => {
      lastPanic = undefined;
    });
  },
});

// What crashed bviz on this thread, for the page to tell: the panic or fault
// `error` that escaped it, named by the panic it last recorded where there is
// one.
export function crashOf(error) {
  return {
    fault: error instanceof WebAssembly.RuntimeError,
    panicked: error?.name === "PanicError" || lastPanic !== undefined,
    what: lastPanic ?? error?.message,
  };
}
