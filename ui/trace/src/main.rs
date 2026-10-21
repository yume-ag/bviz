#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    bviz_ui_trace::run();
}
