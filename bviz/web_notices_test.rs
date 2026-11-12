//! The site ships the notices of the threaded web build for both web builds, so
//! they must name every component the build without threads links.

use std::collections::BTreeSet;

/// The components a notices file lists: the unindented lines between its
/// COMPONENTS heading and the rule that opens the next section.
fn components(notices: &str) -> BTreeSet<&str> {
    let mut lines = notices.lines().skip_while(|line| *line != "COMPONENTS");
    lines.next().expect("the notices have a COMPONENTS section");
    lines
        .skip(1)
        .take_while(|line| !line.starts_with('='))
        .filter(|line| !line.is_empty() && !line.starts_with(' '))
        .collect()
}

fn read(variable: &str) -> String {
    let runfiles = runfiles::Runfiles::create().unwrap();
    let path = runfiles::rlocation!(runfiles, std::env::var(variable).unwrap()).unwrap();
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn threaded_notices_name_every_component_of_the_build_without_threads() {
    let without_threads = read("WEB_NOTICES");
    let threaded = read("WEB_THREADS_NOTICES");
    let shipped = components(&threaded);
    let listed = components(&without_threads);
    assert!(!listed.is_empty());
    let missing: Vec<_> = listed.difference(&shipped).collect();
    assert!(
        missing.is_empty(),
        "missing from the shipped notices: {missing:?}"
    );
}
