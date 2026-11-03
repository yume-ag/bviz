// The script each of bviz's threads starts in, as a Web Worker, given the
// module, its shared memory, the work to run and the stack to run it on by the
// thread that spawns it.
import { crashOf } from "../crash.js";

const crashes = new BroadcastChannel(location.hash.slice(1));

self.onmessage = async (event) => {
  try {
    const { default: init, wasm_thread_entry_point } = await import("./bviz.js");
    const [module_or_path, memory, work, thread_stack_size] = event.data;
    const { __wbindgen_thread_destroy } = await init({ module_or_path, memory, thread_stack_size });
    wasm_thread_entry_point(work);
    // The stack and thread-locals live in the shared memory, which outlives
    // the worker.
    __wbindgen_thread_destroy();
    close();
  } catch (error) {
    crashes.postMessage(crashOf(error));
    // Thrown from a task of its own, the error reaches the worker's error
    // event, which a rejected promise does not.
    setTimeout(() => {
      throw error;
    });
  }
};
