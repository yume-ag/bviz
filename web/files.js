// The file server: holds the files a visitor hands bviz, and answers each
// request bviz's threads make in bviz's memory for a slice of one, writing the
// slice there. The window thread starts it on that memory, with the layout of
// the request, and hands it each file by an id; any thread tells it on the
// channel of releases when it lets go of one.

const files = new Map();
// The ids of files let go of before the server was handed them.
const released = new Set();
const reader = new FileReaderSync();

let memory;
let words;
let layout;
// Resolves the wait of a request for a file the server is yet to be handed.
let handed = () => {};
// Whether the server has stopped answering, for good.
let stopped = false;

// Stops the server and raises `error` on the worker's error event.
function fail(error) {
  stopped = true;
  setTimeout(() => {
    throw error;
  });
}

self.addEventListener("error", () => {
  stopped = true;
});
self.onmessageerror = () => fail(new Error("a file handed to the file server did not arrive"));

self.onmessage = ({ data }) => {
  if (data.memory) {
    ({ memory, layout } = data);
    // A view of the memory as it stands, which holds the request however the
    // memory grows after.
    words = new Int32Array(memory.buffer);
    new BroadcastChannel(data.releases).onmessage = ({ data: id }) => {
      if (!files.delete(id)) {
        released.add(id);
      }
    };
    serve().catch(fail);
  } else if (!released.delete(data.id)) {
    files.set(data.id, data.file);
    handed();
  }
};

// Answers each request as it is asked, until the server stops.
async function serve() {
  const { asked } = layout.states;
  while (!stopped) {
    const state = Atomics.load(words, layout.state);
    if (state === asked) {
      if (!answer()) {
        await new Promise((resolve) => { handed = resolve; });
      }
      continue;
    }
    const { async, value } = Atomics.waitAsync(words, layout.state, state);
    if (async) {
      await value;
    }
  }
}

// Answers the request asked, unless it asks for a file the server is yet to be
// handed.
function answer() {
  const file = files.get(Atomics.load(words, layout.file));
  if (!file) {
    return false;
  }
  const offset = Atomics.load(words, layout.offsetLow) >>> 0;
  const start = offset + (Atomics.load(words, layout.offsetHigh) >>> 0) * 2 ** 32;
  const end = Math.min(file.size, start + (Atomics.load(words, layout.length) >>> 0));
  let told;
  try {
    const slice = start < end ? new Uint8Array(reader.readAsArrayBuffer(file.slice(start, end))) : new Uint8Array();
    if (slice.length < end - start) {
      // The file ended short of its size as handed over.
      told = layout.changed;
    } else {
      // A view made afresh, as the memory's buffer is replaced as it grows.
      new Uint8Array(memory.buffer, Atomics.load(words, layout.into) >>> 0, slice.length).set(slice);
      told = slice.length;
    }
  } catch (error) {
    told = layout.errors[error?.name] ?? layout.failed;
  }
  Atomics.store(words, layout.answer, told);
  Atomics.store(words, layout.state, layout.states.answered);
  Atomics.notify(words, layout.state);
  return true;
}
