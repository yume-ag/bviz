//! bviz in the browser, where there is no command line to name documents: the
//! window opens empty, and tells the page once it stands.

pub fn main() {
    bviz_ui_web::report_crashes();
    bviz_switches::set_from_fragment(&bviz_ui_web::fragment());
    crate::open_window(|_, _, _| bviz_ui_web::ready());
}
