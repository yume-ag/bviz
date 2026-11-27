// The source server: holds the files a visitor hands bviz and fetches the
// documents bviz opens by URL, and answers each request bviz's threads make in
// bviz's memory for a slice of one, writing the slice there. The window thread
// starts it on that memory, hands it each source by an id with the layout of
// the source's request, tells it the id of each source bviz lets go of, and
// frees the request once the server sends that id back.

const sources = new Map();
const reader = new FileReaderSync();

// The most bytes of a document held ahead of bviz's reading of it.
const READ_AHEAD_BYTES = 16 << 20;
// The most bytes of every document together held ahead of bviz's reading. A
// document holding none may read ahead past it, so that each is read however
// much the others hold.
const READ_AHEAD_BUDGET_BYTES = 4 * READ_AHEAD_BYTES;
// The longest read off a document's stream, ahead of it, that is answered by a
// request of its own, such as one of a gzip trailer.
const SHORT_READ_BYTES = 64 << 10;

const PARTIAL_CONTENT = 206;
const RANGE_NOT_SATISFIABLE = 416;

let memory;
let answers;
// The bytes every document holds ahead of bviz's reading together, and what
// waits for some of them to be read.
let heldAhead = 0;
let budgetWaiters = [];
// Whether the server has stopped answering, for good.
let stopped = false;

// Stops the server and raises `error` on the worker's error event.
function fail(error) {
  stop();
  setTimeout(() => {
    throw error;
  });
}

// Stops the server for good, abandoning every fetch under way: the window
// thread then lets go of the memory each request would write into.
function stop() {
  stopped = true;
  for (const source of sources.values()) {
    source.cancelling?.abort();
  }
}

self.addEventListener("error", stop);
self.onmessageerror = () => fail(new Error("a source handed to the source server did not arrive"));

self.onmessage = ({ data }) => {
  if (data.memory) {
    ({ memory, answers } = data);
  } else if (data.file) {
    const source = { ...slot(data), file: data.file };
    sources.set(data.id, source);
    serve(source, answerFile).catch(fail);
  } else if (data.url) {
    const document = new Document(data);
    sources.set(data.id, document);
    open(document).catch(fail);
  } else if ("proceed" in data) {
    sources.get(data.proceed)?.decide({ seekable: data.seekable, size: data.size });
  } else if ("cancel" in data) {
    sources.get(data.cancel)?.cancel();
  } else if ("refuse" in data) {
    release(data.refuse);
  } else if ("release" in data) {
    release(data.release);
  }
};

// What every source holds of its request.
function slot({ id, layout }) {
  return {
    id,
    layout,
    // A view of the memory as it stands, which holds the request however the
    // memory grows after.
    words: new Int32Array(memory.buffer),
    // Whether bviz has let go of the source, after which the server touches
    // its request no more.
    released: false,
  };
}

function release(id) {
  const source = sources.get(id);
  if (!source) {
    return;
  }
  sources.delete(id);
  source.released = true;
  source.close?.();
  Atomics.notify(source.words, source.layout.state);
  postMessage({ released: id });
}

// Answers each request for `source` by `answer` as it is asked, until the
// server stops or bviz lets go of the source.
async function serve(source, answer) {
  const { words, layout } = source;
  while (!stopped && !source.released) {
    const state = Atomics.load(words, layout.state);
    if (state === answers.states.asked) {
      tell(source, await answer(source));
      continue;
    }
    const { async, value } = Atomics.waitAsync(words, layout.state, state);
    if (async) {
      await value;
    }
  }
}

function tell(source, told) {
  if (stopped || source.released) {
    return;
  }
  const { words, layout } = source;
  Atomics.store(words, layout.answer, told);
  Atomics.store(words, layout.state, answers.states.answered);
  Atomics.notify(words, layout.state);
}

// What the request asks for: where in the source it starts, how many bytes,
// and where in bviz's memory they go.
function asked({ words, layout }) {
  const low = Atomics.load(words, layout.offsetLow) >>> 0;
  return {
    start: low + (Atomics.load(words, layout.offsetHigh) >>> 0) * 2 ** 32,
    length: Atomics.load(words, layout.length) >>> 0,
    into: Atomics.load(words, layout.into) >>> 0,
  };
}

// Writes `bytes` into bviz's memory at `into`, through a view made afresh, as
// the memory's buffer is replaced as it grows. `room` is how many bytes the
// request leaves to fill from `into`, past which lies memory not the reader's.
// Nothing is written for a request of `source` no reader waits on any more.
function write(source, into, bytes, room) {
  if (stopped || source.released) {
    throw answers.failed;
  }
  if (bytes.length > room) {
    throw new Error(`a slice of ${bytes.length} bytes does not fit the ${room} asked for`);
  }
  new Uint8Array(memory.buffer, into, bytes.length).set(bytes);
}

function answerFile(source) {
  const { file } = source;
  const { start, length, into } = asked(source);
  const end = Math.min(file.size, start + length);
  try {
    const slice = start < end ? new Uint8Array(reader.readAsArrayBuffer(file.slice(start, end))) : new Uint8Array();
    if (slice.length < end - start) {
      // The file ended short of its size as handed over.
      return answers.changed;
    }
    write(source, into, slice, length);
    return slice.length;
  } catch (error) {
    return answers.errors[error?.name] ?? answers.failed;
  }
}

// A document fetched by URL. Its body is read into a queue ahead of bviz from
// `position`, the offset of the queue's first byte.
class Document {
  constructor(data) {
    Object.assign(this, slot(data));
    this.url = data.url;
    this.init = data.init;
    this.maxBytes = data.maxBytes;
    // Aborted to cancel every request for the document.
    this.cancelling = new AbortController();
    // The answer every request gets once one has failed.
    this.error = null;
    this.decision = new Promise((resolve) => {
      this.decide = resolve;
    });
    // Set from the first response, once the window thread lets it be read.
    this.seekable = false;
    this.size = null;
    // What every later response must repeat of the first, which tells the
    // document has not changed since.
    this.lastModified = null;
    this.tag = null;
    // The body being read, which a restart replaces.
    this.stream = null;
    this.queue = [];
    this.queued = 0;
    this.position = 0;
    this.waiters = [];
  }

  // Waits for the next change to what the document holds or why it fails.
  changed() {
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  wake() {
    const waiters = this.waiters;
    this.waiters = [];
    for (const resolve of waiters) {
      resolve();
    }
  }

  failWith(answer) {
    this.error ??= answer;
    this.wake();
  }

  cancel() {
    this.cancelling.abort();
    this.failWith(answers.fetch.cancelled);
  }

  close() {
    this.stream?.controller.abort();
    this.stream = null;
    this.empty();
    this.decide(null);
    this.wake();
  }

  // Fetches the document's bytes from `start`, to `end` where it is given,
  // or, where `start` is null, the document without asking for a range,
  // failing with the answer that tells why.
  async request(start, end) {
    const controller = new AbortController();
    const headers = start === null ? {} : { Range: `bytes=${start}-${end ?? ""}` };
    const signal = AbortSignal.any([this.cancelling.signal, controller.signal]);
    try {
      const response = await stalling(controller, fetch(this.url, { ...this.init, headers, signal }));
      return { response, controller };
    } catch (error) {
      throw new Failure(this.answerFor(controller), String(error?.message ?? error));
    }
  }

  // The answer a request made with `controller` gets where it fails.
  answerFor(controller) {
    if (this.cancelling.signal.aborted) {
      return answers.fetch.cancelled;
    }
    if (controller.stalled) {
      return answers.fetch.stalled;
    }
    return answers.fetch.notServed;
  }

  // Fetches the document's bytes from `start`, to `end` where it is given,
  // from where its first response came, with nothing changed since.
  async range(start, end) {
    const { response, controller } = await this.request(start, end);
    const expected = (end === null ? this.size - 1 : end) - start + 1;
    let answer = null;
    if (response.url !== this.url) {
      answer = answers.fetch.refused;
    } else if (response.status !== PARTIAL_CONTENT) {
      answer = answers.fetch.notServed;
    } else if (
      response.headers.get("Last-Modified") !== this.lastModified ||
      strongTag(response) !== this.tag ||
      lengthOf(response) !== expected
    ) {
      answer = answers.changed;
    }
    if (answer !== null) {
      controller.abort();
      throw new Failure(answer);
    }
    return { response, controller };
  }

  // The `length` bytes of the document from `start`, fetched by a request of
  // their own. A body sent encoded is decoded, so it may hold other than the
  // length its response declares; one that does fails as changed.
  async slice(start, length) {
    const { response, controller } = await this.range(start, start + length - 1);
    const bytes = new Uint8Array(length);
    let at = 0;
    const reader = (response.body ?? new Blob().stream()).getReader();
    for (;;) {
      let read;
      try {
        read = await stalling(controller, reader.read());
      } catch {
        throw this.answerFor(controller);
      }
      if (read.done) {
        break;
      }
      if (read.value.length > length - at) {
        controller.abort();
        throw answers.changed;
      }
      bytes.set(read.value, at);
      at += read.value.length;
    }
    if (at !== length) {
      throw answers.changed;
    }
    return bytes;
  }

  // Reads the document afresh from `start`, failing it where that fails.
  async restart(start) {
    this.stream.controller.abort();
    this.stream = null;
    try {
      const { response, controller } = await this.range(start, null);
      this.read(response, controller, start);
    } catch (failure) {
      this.failWith(answerOf(failure));
      throw failure;
    }
  }

  // Reads the body of `response`, from `start`, into the queue.
  read(response, controller, start) {
    this.stream?.controller.abort();
    // A response of a status without a body, such as 204, reads as empty.
    const body = response.body ?? new Blob().stream();
    const stream = { reader: body.getReader(), controller };
    this.stream = stream;
    this.empty();
    this.position = start;
    this.pump(stream, start).catch(fail);
  }

  async pump(stream, start) {
    let received = start;
    while (this.stream === stream) {
      if (this.queued >= READ_AHEAD_BYTES) {
        await this.changed();
        continue;
      }
      if (this.queued > 0 && heldAhead >= READ_AHEAD_BUDGET_BYTES) {
        await Promise.race([this.changed(), budgetFreed()]);
        continue;
      }
      let read;
      try {
        read = await stalling(stream.controller, stream.reader.read());
      } catch {
        if (this.stream === stream) {
          this.failWith(this.answerFor(stream.controller));
        }
        return;
      }
      if (this.stream !== stream) {
        return;
      }
      if (read.done) {
        if (this.size !== null && received !== this.size) {
          this.failWith(answers.changed);
        }
        stream.done = true;
        this.wake();
        return;
      }
      received += read.value.length;
      if (received > this.maxBytes) {
        stream.controller.abort();
        this.failWith(answers.fetch.tooLarge);
        return;
      }
      if (this.size !== null && received > this.size) {
        stream.controller.abort();
        this.failWith(answers.changed);
        return;
      }
      this.queue.push(read.value);
      this.hold(read.value.length);
      this.wake();
    }
  }

  // Counts `bytes` more queued, of the document and of every document.
  hold(bytes) {
    this.queued += bytes;
    heldAhead += bytes;
    if (bytes < 0) {
      const waiters = budgetWaiters;
      budgetWaiters = [];
      for (const resolve of waiters) {
        resolve();
      }
    }
  }

  empty() {
    this.queue = [];
    this.hold(-this.queued);
  }

  // Writes up to `length` queued bytes from `skip` bytes into the queue into
  // bviz's memory at `into`, leaving the queue as it is, and returns how many
  // it wrote.
  copy(skip, length, into) {
    let at = 0;
    let written = 0;
    for (const chunk of this.queue) {
      if (written === length) {
        break;
      }
      const from = Math.max(0, skip - at);
      at += chunk.length;
      if (from < chunk.length) {
        const bytes = chunk.subarray(from, from + length - written);
        write(this, into + written, bytes, length - written);
        written += bytes.length;
      }
    }
    return written;
  }

  // Writes `count` bytes from the front of the queue into bviz's memory at
  // `into`, dropping them from the queue.
  take(count, into) {
    let taken = 0;
    while (taken < count) {
      const chunk = this.queue[0];
      const n = Math.min(chunk.length, count - taken);
      write(this, into + taken, chunk.subarray(0, n), count - taken);
      taken += n;
      if (n === chunk.length) {
        this.queue.shift();
      } else {
        this.queue[0] = chunk.subarray(n);
      }
    }
    this.hold(-taken);
    this.position += taken;
    this.wake();
  }
}

// Waits for a document to hold fewer bytes ahead.
function budgetFreed() {
  return new Promise((resolve) => budgetWaiters.push(resolve));
}

// Why a request failed: the answer it gets, and what the browser said of it.
class Failure {
  constructor(answer, detail) {
    this.answer = answer;
    this.detail = detail;
  }
}

// The answer a request that threw `thrown` gets.
function answerOf(thrown) {
  if (thrown instanceof Failure) {
    return thrown.answer;
  }
  return typeof thrown === "number" ? thrown : answers.failed;
}

// Gives up on `promise`, aborting `controller`, where it waits for the server
// for longer than a stall.
async function stalling(controller, promise) {
  const timer = setTimeout(() => {
    controller.stalled = true;
    controller.abort();
  }, answers.fetch.stallMillis);
  try {
    return await promise;
  } finally {
    clearTimeout(timer);
  }
}

// The length `response` declares, read as bviz reads it: digits alone, of a
// number held exactly.
function lengthOf(response) {
  const length = response.headers.get("Content-Length");
  if (length === null || !/^\d+$/.test(length)) {
    return null;
  }
  const bytes = Number(length);
  return Number.isSafeInteger(bytes) ? bytes : null;
}

// The strong entity tag of `response`, which a cross-origin read sees only
// where the server exposes it.
function strongTag(response) {
  const tag = response.headers.get("ETag");
  return tag === null || tag.startsWith("W/") ? null : tag;
}

// Fetches `document`, tells the window thread what its response says of
// itself, and serves it once the window thread lets its body be read.
async function open(document) {
  let first;
  try {
    first = await document.request(0, null);
    if (first.response.status === PARTIAL_CONTENT) {
      first = await whole(document, first);
    }
  } catch (failure) {
    if (!document.released) {
      postMessage({ failed: document.id, answer: answerOf(failure), detail: failure.detail });
      release(document.id);
    }
    return;
  }
  const { response, controller } = first;
  if (document.released) {
    controller.abort();
    return;
  }
  const { url, status } = response;
  const lastModified = response.headers.get("Last-Modified");
  const tag = strongTag(response);
  postMessage({
    answered: document.id,
    url,
    status,
    length: lengthOf(response),
    whole: first.whole === true,
    validated: lastModified !== null || tag !== null,
  });
  const decided = await document.decision;
  if (decided === null || document.released) {
    controller.abort();
    return;
  }
  Object.assign(document, {
    url,
    seekable: decided.seekable,
    size: decided.size,
    lastModified,
    tag,
  });
  document.read(response, controller, 0);
  await serve(document, answerDocument);
}

// `first`, a partial response to the request for every byte of `document`,
// where the refusal of the bytes past its length as unsatisfiable confirms it
// holds them all, and otherwise a response to a request asking for no range.
async function whole(document, first) {
  const length = lengthOf(first.response);
  if (length !== null) {
    let past;
    try {
      past = await document.request(length, null);
    } catch (failure) {
      first.controller.abort();
      throw failure;
    }
    past.controller.abort();
    if (past.response.status === RANGE_NOT_SATISFIABLE && past.response.url === first.response.url) {
      return { ...first, whole: true };
    }
  }
  first.controller.abort();
  return { ...(await document.request(null, null)), whole: false };
}

async function answerDocument(document) {
  const { start, length, into } = asked(document);
  try {
    return await deliver(document, start, length, into);
  } catch (thrown) {
    return answerOf(thrown);
  }
}

// Writes up to `length` of the document's bytes from `start` into bviz's
// memory at `into`, returning how many it wrote.
async function deliver(document, start, wanted, into) {
  let length = wanted;
  if (document.size !== null) {
    if (start >= document.size) {
      return 0;
    }
    length = Math.min(length, document.size - start);
  }
  for (;;) {
    if (document.error !== null) {
      throw document.error;
    }
    const { position, queued } = document;
    if (start === position) {
      if (queued > 0) {
        break;
      }
      if (document.stream.done) {
        return 0;
      }
      await document.changed();
    } else if (!document.seekable) {
      throw answers.fetch.unordered;
    } else if (start > position && start < position + queued) {
      return document.copy(start - position, length, into);
    } else if (start > position && length <= SHORT_READ_BYTES) {
      const bytes = await document.slice(start, length);
      write(document, into, bytes, length);
      return bytes.length;
    } else {
      await document.restart(start);
    }
  }
  const count = Math.min(length, document.queued);
  document.take(count, into);
  return count;
}
