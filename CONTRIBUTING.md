# Contributing to bviz

Thanks for taking the time. Small fixes are welcome as a pull request straight
away. For anything larger — a new view, a change to how the graph is laid out,
a new dependency — open an issue first, so we agree on the design before you
spend time on the code.

## Setup

All you install is [bazelisk](https://github.com/bazelbuild/bazelisk), which
picks up the Bazel version `.bazelversion` pins. Bazel fetches and builds
everything else, the Rust and C++ toolchains included, so there is no
`rustup`, no `cargo build`, and no library taken from the host. Development and
CI run on x86_64 Linux, and cross-compile from there the release binaries for
Linux and macOS on x86_64 and arm64.

Machine-local Bazel settings, such as a remote cache, go in `user.bazelrc`,
which is untracked. A fresh clone builds without credentials.

For an editor environment, with `cargo`, `rustc`, `rustfmt` and `rust-analyzer`
on `PATH` through [direnv](https://direnv.net):

```sh
bazel run //tools:bazel_env
direnv allow
bazel run //tools/rust_analyzer:gen_rust_project
```

## Running from source

`bazel run` starts the binary in its runfiles directory, so give input paths in
full. Build with `-c opt` for usable interactive performance.

```sh
bazel run -c opt //bviz -- --profile /tmp/build.profile.gz
```

Each viewer also builds as a binary of its own, taking the same flags as
`bviz` for its kind of document, for work on that viewer alone:

| Target | Shows |
| --- | --- |
| `//ui/graph:graph` | the build and dependency graph |
| `//ui/trace:trace` | the action timeline: thread lanes, counters, flame graph, stats |
| `//ui/console:console` | the console output, replayed against its clock |
| `//ui/execlog:execlog` | the spawn time by phase, from the execution log |

### Running in a browser

`//web:serve` builds bviz for the browser and serves it locally, on
`localhost` only. Open the address it prints.

```sh
bazel run -c opt //web:serve
```

What it serves is `//web:site`, the directory a static host would serve. It
sends the headers the page needs: the wasm media type, cross-origin isolation,
which lets the page share memory between threads, and a content security policy
that admits no inline script or style. `--port` picks another port, and
`--no-isolation` leaves out the cross-origin isolation headers, and the page
then shows a banner saying bviz cannot use more than one thread.

### Measuring performance

Set these environment variables when running bviz or a viewer binary to measure
it; each reports on stderr. The values the `BVIZ_PAN_BENCH*` variables take are
documented in [`ui/graph/src/pan_bench.rs`](ui/graph/src/pan_bench.rs).

| Variable | Purpose |
| --- | --- |
| `BVIZ_PAN_BENCH` | runs a self-driving graph pan benchmark and reports its frame times |
| `BVIZ_PAN_BENCH_FRAMES` | how many frames the benchmark records |
| `BVIZ_PAN_BENCH_MOTION` | how the benchmark moves the view |
| `BVIZ_PAN_BENCH_PRESS` | where the benchmark's motion starts |
| `BVIZ_TRACE_TIMING` | times each stage of loading a profile and of laying out its lanes |

## Before you open a pull request

CI runs the full check: the build, the tests (Starlark style among them), and
the rustfmt and clippy checks. Run it locally first. The tests need neither a
display nor a GPU.

```sh
bazel test --config=check //...
```

While iterating, check only the targets you are changing; it is faster. The full
check before you commit then catches anything you broke elsewhere.

```sh
bazel test //ui/graph/...
bazel build --config=rustfmt //ui/graph/...
bazel build --config=clippy //ui/graph/...
bazel test //:buildifier_test
```

Rust formatting and Starlark style have fixers; clippy findings are yours to
resolve.

```sh
bazel run @rules_rs_rules_rust//:rustfmt
bazel run //:buildifier
```

If your change affects what is drawn, attach a screenshot of the result on real
input. A passing build says nothing about overlapping nodes, detached edges, or
clipped text.

## Commits

Keep each commit to one concern, with an imperative subject and a body that
explains why. Write the message so it makes sense to someone who never saw the
pull request discussion: that is where the history gets read from later.

## Dependencies

Pin new dependencies in `MODULE.bazel` or `Cargo.lock`, and commit the updated
lockfiles. The build checks the licence of every third-party work linked into
the binaries; a dependency with no declared licence, or under a GPL, LGPL or
AGPL licence, fails that check and needs a maintainer's decision before it can
go in.

## Releases

To release version `X.Y.Z`, set the `version` of `//cli` to it, and once that
change is on `main`, push the tag `vX.Y.Z` on it. The tag may carry a
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) pre-release part,
as in `v1.0.0-rc.1`, which publishes a pre-release. The
[release workflow](.github/workflows/release.yaml) builds the archives and
publishes them only if `bviz --version` reports the tag's version.

## Licensing

bviz is licensed under the Apache License 2.0. Per section 5 of
[the licence](LICENSE), anything you submit for inclusion is contributed under
those same terms. There is no separate agreement to sign and no sign-off line
to add.

## Further reading

[CLAUDE.md](CLAUDE.md) spells out the project's conventions in more detail —
code comments, commit granularity, hermeticity, licence declarations, Bazel
style — and can be a useful reference if you want to match the existing code
closely. Its Workflow section is written for AI coding agents; you can skip it.
