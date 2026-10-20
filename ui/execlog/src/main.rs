#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

fn main() {
    bviz_ui_execlog::run();
}
