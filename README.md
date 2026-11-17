# bviz

A graphical Bazel build visualization and inspection tool.

![bviz with a dependency graph, a trace profile, its flame graph and a console replay open side by side, split diagonally between the dark and light themes](docs/screenshots/bviz.png)

Seeing what a Bazel build actually did is hard. bviz reads the outputs Bazel
generates, such as query results, the trace profile and the build event log,
and lets you explore them visually.

bviz is early in development. Expect rough edges, and expect the interface and
command line to change.

## Features

bviz reads these Bazel outputs, alone or together:

- **`query`, `cquery` and `aquery` results:** one merged target and action
  graph to explore, search and filter, with views of why one target depends on
  another and what each dependency brings in.
- **Trace profile:** a timeline of the build, flame graphs and statistics of
  where the time went, and its critical path.
- **Build event log:** the build's console output and the outcome of each
  target.
- **Execution log:** where the time of each spawn went.

## Install

Download bviz for arm64 or x86_64 Linux or macOS from the
[latest release](https://github.com/yume-ag/bviz/releases/latest), or the latest
build of `main` from the
[nightly release](https://github.com/yume-ag/bviz/releases/tag/nightly).

| | Linux | macOS |
| --- | --- | --- |
| Architecture | arm64, x86_64 | arm64, x86_64 |
| Operating system | glibc 2.28 or newer | macOS 14 or newer |
| Display | a Wayland session | |
| Graphics | a Vulkan driver | Metal |

On an X11 session, run bviz inside a nested Wayland compositor such as
[cage](https://github.com/cage-kiosk/cage): `cage -- bviz --profile build.profile.gz`.

The arm64 Linux and the macOS binaries are cross-compiled on x86_64 Linux and are
not tested in CI. The macOS binaries are not signed, and macOS blocks an unsigned
download until you clear its quarantine flag on the extracted binary:
`xattr -d com.apple.quarantine path/to/bviz`.
Alternatively, download with curl, which sets no quarantine flag:

```sh
curl -fsSL "https://github.com/yume-ag/bviz/releases/download/nightly/bviz-macos-$(uname -m).tar.gz" | tar -xz bviz
./bviz
```

bviz reads the outputs of Bazel 8.0 or newer.

To build from source, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Quick start

In a Bazel workspace, profile a build and open the profile:

```sh
bazel build --profile=/tmp/build.profile.gz //...
bviz --profile /tmp/build.profile.gz
```

## Usage

`bviz bazelrc` generates a Bazel config with every flag bviz needs. From the
workspace root:

```sh
mkdir -p .bviz && bviz bazelrc > .bviz/bazelrc
echo 'try-import %workspace%/.bviz/bazelrc' >> .bazelrc
```

Then follow the instructions at the top of `.bviz/bazelrc`.

Each input flag takes the file one Bazel invocation writes. The bazelrc adds
further flags that give the views more to show.

| bviz flag | Written by |
| --- | --- |
| `--profile FILE` | `bazel build --profile=FILE` |
| `--bep FILE` | `bazel build --build_event_binary_file=FILE` |
| `--exec_log FILE` | `bazel build --execution_log_compact_file=FILE` |
| `--query FILE` | `bazel query --output=streamed_proto ... > FILE` |
| `--cquery FILE` | `bazel cquery --output=streamed_proto --transitions=lite ... > FILE` |
| `--aquery FILE` | `bazel aquery --output=streamed_proto ... > FILE` |

## Getting help

Report bugs and ask questions in the
[issue tracker](https://github.com/yume-ag/bviz/issues).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

bviz draws with [gpui](https://github.com/zed-industries/zed), the UI framework
of the Zed editor, and lays out graphs with
[elk-rs](https://github.com/openedges/elk-rs), a Rust port of the Eclipse
Layout Kernel.

## License

bviz is licensed under the Apache License 2.0; see [LICENSE](LICENSE). The
third-party works it bundles or derives from, and their terms, are listed in
[NOTICE](NOTICE).

Bazel is a trademark of Google LLC. bviz is not affiliated with or endorsed by
Google.
