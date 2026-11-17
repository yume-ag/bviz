// The file server: holds the files a visitor hands bviz, and answers each
// request bviz's threads make in bviz's memory for a slice of one, writing the
// slice there. The window thread starts it on that memory, hands it each file
// by an id with the layout of the file's request, tells it the id of each file
// bviz lets go of, and frees the request once the server sends that id back.

const sources = new Map();
const reader = new FileReaderSync();

let memory;
let answers;
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
    ({ memory, answers } = data);
  } else if (data.file) {
    const source = {
      file: data.file,
      layout: data.layout,
      // A view of the memory as it stands, which holds the request however
      // the memory grows after.
      words: new Int32Array(memory.buffer),
      // Whether bviz has let go of the file, after which the server touches
      // its request no more.
      released: false,
    };
    sources.set(data.id, source);
    serve(source).catch(fail);
  } else {
    const source = sources.get(data.id);
    sources.delete(data.id);
    source.released = true;
    Atomics.notify(source.words, source.layout.state);
    postMessage(data.id);
  }
};

// Answers each request for `source` as it is asked, until the server stops or
// bviz lets go of the file.
async function serve(source) {
  const { words, layout } = source;
  while (!stopped && !source.released) {
    const state = Atomics.load(words, layout.state);
    if (state === answers.states.asked) {
      answer(source);
      continue;
    }
    const { async, value } = Atomics.waitAsync(words, layout.state, state);
    if (async) {
      await value;
    }
  }
}

function answer({ file, words, layout }) {
  const offset = Atomics.load(words, layout.offsetLow) >>> 0;
  const start = offset + (Atomics.load(words, layout.offsetHigh) >>> 0) * 2 ** 32;
  const end = Math.min(file.size, start + (Atomics.load(words, layout.length) >>> 0));
  let told;
  try {
    const slice = start < end ? new Uint8Array(reader.readAsArrayBuffer(file.slice(start, end))) : new Uint8Array();
    if (slice.length < end - start) {
      // The file ended short of its size as handed over.
      told = answers.changed;
    } else {
      // A view made afresh, as the memory's buffer is replaced as it grows.
      new Uint8Array(memory.buffer, Atomics.load(words, layout.into) >>> 0, slice.length).set(slice);
      told = slice.length;
    }
  } catch (error) {
    told = answers.errors[error?.name] ?? answers.failed;
  }
  Atomics.store(words, layout.answer, told);
  Atomics.store(words, layout.state, answers.states.answered);
  Atomics.notify(words, layout.state);
}
