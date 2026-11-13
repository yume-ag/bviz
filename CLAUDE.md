# bviz

Interactive visualization and inspection tool for Bazel builds.

@CONTRIBUTING.md

## Layout

- `bviz/` is the application; `ui/` holds its gpui views, one package per viewer
  plus the shared ones they are composed from.
- The other top-level packages are UI-free libraries: parsers and models of
  Bazel's outputs (e.g. `trace/profile`, `graph/layout`) and their support code.
- `tools/` holds development and build-time tools; `platforms/` and `dist/` the
  target platforms and release archives.
- `patches/<upstream>/` patches fetched dependencies, `3rd-party/` holds BUILD
  overlays for them, and `third_party/` vendors source in-tree.

## Workflow

- For a task of more than a few steps, delegate research, implementation,
  adversarial review, fixes, and validation to subagents, keeping your own
  context for coordinating them and weighing their results. Run Bazel in at most
  one of them at a time.
- In a jj checkout, if a jujutsu (jj) skill is available, load it before the
  first version-control command of a task.
- Inspect the working copy before the first edit of any task, and never edit
  into a change you did not open for this task. In a jj checkout (a `.jj`
  directory at the root) the working copy is itself a commit, so any edit lands
  in whatever change it holds: editing while it carries an unrelated change
  silently fuses the two, and splitting them back apart is error-prone. So
  before touching a file, run `jj st`; if the working copy is non-empty, or its
  description names work other than the task at hand, run `jj new` first. Only
  edit directly into it when it is empty or its description is the very change
  you are continuing — even for a one-line fix. In a git checkout, run
  `git status` and keep unrelated uncommitted changes out of your commits.
- In a jj checkout, `jj restore <path>` resets that path to the PARENT commit
  (`@-`), not to some pre-edit snapshot — and because the working copy IS a
  commit, that discards the CURRENT commit's own changes to the path, not just an
  uncommitted tweak. So never reach for it to strip a small unwanted edit (a
  debug tweak, a temporary flag flip) from a file the working commit is actively
  changing: it silently wipes that commit's work on the whole file. Revert the
  unwanted hunk with a targeted edit instead; if you already ran the restore,
  `jj undo` recovers it.
- Commit your work as part of the task, without waiting to be asked, following
  the jj skill's guidance in a jj checkout. Commit frequently, one concern per
  commit — neither one giant commit nor one per file. One concern means one
  problem addressed, not a shared file, function, feature area, session, or
  round of review feedback: a request that bundles two fixes and a tweak is
  three commits. If the subject needs an "and" or the body lists independent
  changes, split it. When unsure, split.
- Write commit messages for a human reader: clear, concise prose that says what
  changed and why, not an inventory of edits.
- Only rewrite history that has not been pushed to a remote, and avoid
  rewriting the main branch even then. Reshape history (reword, squash, drop,
  reorder) with the VCS's own operations — `git rebase`/`cherry-pick`, or
  `jj rebase`/`squash`/`describe` — and resolve conflicts through them rather
  than rebuilding commits by hand. Squash a commit that only undoes or replaces
  an earlier one instead of leaving the churn. Confirm the rewritten tree is
  unchanged, or changed only as intended: in git, rewrite on a scratch branch and
  `git diff` it against the original before moving the real branch; in jj,
  `jj diff --from <old> --to <new>` against the pre-rewrite commit (`jj evolog`
  lists it), and `jj undo` a rewrite that went wrong.
- Commit only verified work: run the checks under "Before you open a pull
  request", on the targets you are changing while iterating and in full before
  committing. For toolchain changes, confirm with `--toolchain_resolution_debug`.
  Report outcomes honestly; flag side effects.
- On a machine short of memory or cores, build with remote execution when it is
  configured for you (`--config=remote`, maintainers only, credentials in
  `user.bazelrc`), so builds do not overload it.
- Verify a change to visual output by viewing the rendered result on real
  input, captured with the screenshot tools the gpui skill provides; a
  structural check does not catch visual faults.

## Code

- Before hand-rolling a widget or other capability, check whether the framework
  or its component library already provides one, and reuse it unless it brings
  unacceptable dependencies; framework-specific facts (what the framework does and
  doesn't offer) belong in its skill, not rediscovered each time. An interactive
  control must work end to end — a scroll bar drags and its track clicks, not just
  a drawn indicator.
- Behavior must not be wired to the machine that happened to write it. Never
  hard-code a value that is really a property of one host — a core or thread
  count, a page or cache-line size, a memory ceiling, a screen resolution or DPI,
  a device path, an environment particular to your box: a constant that matched
  the current machine silently mis-sizes every other one. Query the property at
  runtime through the portable, cross-OS API for it (a path via the platform's
  dirs/temp API, not `/home/...`) and derive the value from that. When a bound is
  genuinely intrinsic to the workload rather than the host, express it as that
  structural property, not the number it happens to equal on one machine.
- Size thread pools and per-core structures (shard fan-out, per-worker buffers)
  by PHYSICAL cores via `worker_count()` (`num_cpus::get_physical()` clamped to
  `available_parallelism()`), not logical/SMT threads: the heavy passes here
  (parse, aggregation) are memory-bandwidth bound and saturate at the physical
  cores that own the memory channels, so SMT oversubscription burns CPU for no
  wall-time gain. Reach for the logical thread count only with a measured reason.
- Pre-allocate a collection's capacity whenever its final size is known or
  cheaply estimable; the regrowth of a hand-grown `Vec::new()`/`vec![]`/
  `String::new()` is an avoidable cost anywhere in the code. Size from an exact
  count, a small structural bound, or a byte-density estimate (mirror the
  `BYTES_PER_*` constants in `trace/profile`, measured on representative
  input and rounded to over-reserve slightly). Reserve where the count is
  known: if a type's `new` cannot, add a `with_capacity` constructor and size it
  at the call site (grep `fn with_capacity` for the pattern).
- Make invalid states unrepresentable rather than documenting or guarding them.
  When a combination of fields cannot legitimately occur, reshape the types so it
  cannot be written: move fields into the enum variant that guarantees them,
  pair them in one struct, or make a handle carry what it indexes. The tell is a
  guard for a case believed impossible — an `unwrap`, an `unreachable!`, a
  comment asserting a combination never occurs. Carry a single-value invariant
  (a range, non-empty, validated) in an opaque newtype: private field, checking
  constructor, no route back such as `From<Inner>`. A few extra lines are no
  reason to leave an invalid state representable; state the invariant once at
  the definition and keep the guard only when encoding it needs machinery out of
  all proportion to the gain — deep generics, a state machine of marker types, a
  conversion at every use.
- Avoid unnamed magic numbers: bind a non-obvious literal to a named constant
  (or a `let` with a descriptive name) at its definition, so its meaning is
  stated once where the value lives. A self-evident literal in context (an index
  `0`, a `* 2`, a percentage `0.5` whose role is clear from the expression) needs
  no name; a bare `86_400`, `4096`, or `0.85` does.

## Comments and commit messages

Code, comments, and commit messages must be self-contained: a reader who has no
access to the chat or task that produced them must fully understand them.

- Never reference the development process — no step/phase numbers, no "as
  discussed", no "next we will", no roadmap of future work ("X comes later"), no
  TODOs that only make sense mid-thread, and no justifying a change by alluding
  to other commits or the work in progress ("the X commit", "when we restructured
  Y"); state the change and its rationale on their own terms, and if you must
  cite a prior commit, identify it by hash or subject.
- Describe the change in absolute terms, not against transient state — not what
  another branch has, nor what the tree "already"/"now"/"yet" contains or was
  "missing" before; such claims hold only at the instant of writing and rot as
  the repo and other branches move on.
- Don't ground a claim in a specific external input the reader has no access to,
  or in one run's raw figures — "the ~1.4GB profile", "the 8M-event trace",
  "takes 4s here" name a data artifact and a machine that live outside the repo.
  Describe the input by its general character ("a large profile") and prefer a
  relative or qualitative result ("about half the time", "roughly 2x faster") or
  a property that holds regardless of the sample; if a concrete number genuinely
  earns its place, state the reproducible conditions behind it.
- Name any external reference in full — its title or identifier (a paper's name
  and arXiv id, a ticket number) — never a bare "the paper"/"the spec".
- Keep references portable: never hard-code a machine-local absolute path
  (`/home/<user>/...`, a personal checkout location) in a checked-in file. Point
  at a repo-relative path, or for external source pin a stable identifier (a
  release tag or permalink URL, not a moving branch). When a task names a local
  checkout of an upstream, prefer it over fetching the source for research and
  review, but never refer to it in what you write.
- Checked-in test fixtures and generated data must not embed machine- or
  user-identifying values (usernames, home paths, hostnames); generate them in a
  neutral location — e.g. a `/tmp` workspace with `HOME`/`USER` overridden — and
  normalize any residue with a length-preserving substitution before committing.

Comments are terse and only earn their place by adding what the code can't say —
a sentence, not a paragraph. State each fact once, at its definition.

- A comment describes the code as it stands — a value's role, an invariant it
  upholds, the non-obvious reason it is what it is — never the edit that
  produced it or an alternative the code no longer holds. Phrasings that only
  parse for a reader who knows the prior version are the tell: "leave/keep X
  set", "don't clear Y", "now uses", "instead of Z", "X is also Y so we don't
  bother". State the standing fact directly — say what X is and why it must be
  that, not that you refrained from changing it.
- Don't restate the code: a comment that names what the next line already
  names is noise, including one that restates an identifier the declaration
  already gives, or re-derives in one file what another already encodes. Don't
  re-explain what a rule's or tool's own documentation covers, and don't narrate
  what a framework or library call does: its behavior is the API's contract, so
  a comment that a UI helper "re-renders each frame", that a lock "blocks until
  acquired", or that a builder method "sets the title" is noise; comment only
  the non-obvious reason the call is used here, if any.
- Don't write comments that drift as nearby code changes: don't describe which
  of several things a wildcard or shared target covers, and don't restate the
  concrete value of something defined elsewhere — a color, dimension, or
  default; name it by its role or source of truth ("the file-leaf accent", not
  "(green)"). Annotating a literal at its own definition is fine.
- Don't narrate the design: a block comment restating a module's model, data
  flow, or algorithm only duplicates the code and is the first thing a later
  refactor deletes. Citing an external source (paper, spec, issue) is fine.
- BUILD, `.bzl`, and `MODULE.bazel` files follow the same rules: a declarative
  rule and its attributes already state what they do, so a comment that names a
  rule's effect (`# expose the binary` over a `gen_binaries` attribute) or
  restates an attribute is noise. When one rationale spans several files (a
  toolchain and the target that consumes it), state it once at the definition
  or in the commit message, and avoid cross-file pointers
  (`# registered in MODULE.bazel as X`) that drift when either side moves.
- Never document a struct by enumerating what it contains — neither a doc
  comment on the struct that lists its own fields, nor a comment on a field
  whose type names a struct that lists that struct's fields. The declaration
  already gives the inventory, and the copy goes stale the moment a field is
  added, renamed, or moved. Document instead what the declaration cannot state —
  the type's purpose, and on a handle field what this holder reads or writes
  through it and which way the coupling runs. Where the name and type already
  say it, write no comment at all.
- A doc comment binds to exactly one declaration, and everything that surfaces
  it — an editor hover, rustdoc, a reader scanning the declaration — shows it
  only there, so a fact stated on one field is invisible from its siblings.
  Never let one field's doc carry a property that in truth governs several:
  parallel pools indexed alike, a partition they all follow, an invariant tying
  two fields together. Neither copy the sentence onto each sibling nor leave it
  in several plausible homes: put it on the one declaration every reader of
  every governed field must pass through — the handle, span, or accessor they
  are all reached by where one exists, and only failing that the type's own doc,
  where it states what holds between the fields, never what they are. Leave
  each field a doc that stands alone about itself.
- A comment at a construction or call site must not repeat what the constructed
  or called item's own doc says. It earns its place only by adding what is true
  of *this* site and nowhere else — an ordering constraint between two calls, why
  this caller passes what it passes, a consequence local to this context.

## Hermeticity

Everything is pinned and Bazel-managed. No system toolchains, no system tools.

- Rust toolchain via rules_rs; CC via the hermetic llvm toolchain.
- Dev tools (cargo, rustc, rustfmt, rust-analyzer) via `//tools:bazel_env` +
  direnv; other binaries via multitool, sha256-pinned per platform.
- Pin versions in `MODULE.bazel`; commit all lockfiles (`MODULE.bazel.lock`,
  `Cargo.lock`, `multitool.lock.json`). Repin `Cargo.lock` with
  the Bazel-provided cargo, never system cargo. Repinning with cargo writes a
  `target/` tree (cargo's, not Bazel's); it is gitignored — never commit it.
- When a build needs a native/system C library (e.g. a Rust `*-sys` crate or a
  `#[link]`/pkg-config dependency on the host), do not link the host's copy or
  hand-vendor unpinned sources. The invariant is hermeticity: the library must be
  fetched at a pinned version and built by Bazel from source, never taken from the
  host. Any Bazel-managed fetch satisfies this — a Bazel Central Registry
  (https://registry.bazel.build) module added as a `bazel_dep` when one exists, or
  an `http_archive` (or similar) pinned by sha256 with a BUILD overlay otherwise.
  Prefer a BCR module when available for the maintained build files, but an
  `http_archive` is an equally hermetic path, not a fallback to avoid.

## Third-party licences

Declare them to Bazel with `package_metadata` + `license`; never copy a licence
text that Bazel already fetches.

- Fetched work: declare in its BUILD overlay, `text` pointing at the file in the
  archive (see `3rd-party/*.BUILD`).
- Work carried in-tree — source written from an upstream example, a constant
  taken verbatim, a patch shipping the lines it replaces — keeps its text in
  `LICENSES/`, declared there and attached to a `filegroup` of just those files
  so the claim does not cover the whole library.
- Patches are resolved when a crate repository is fetched, so their labels must
  be source files: declare per package, one package per upstream.
- `NOTICE` adds only what a declaration cannot state — which part of bviz a work
  reached, and how.

## Bazel conventions

- Prefer specific visibility (`//pkg:__pkg__`) over `//visibility:public`.
- Set `visibility` only on targets other packages depend on, including through
  an `alias`; omit it on leaf targets such as a `rust_binary`.
- Export a source file with `exports_files` only when a rule, attribute default
  or flag outside its package names it; a file named only from `MODULE.bazel`,
  such as a patch, needs no export.
- Give every crate a project-namespaced `crate_name` carrying the `bviz_` prefix,
  matching its Cargo package name (e.g. `bviz_graph_svg`, not a bare `graph_svg`).
- Prefer symbolic macros over legacy macros.
- Never compute runfiles paths by hand; use the language's `rlocation` helper and
  `$(rlocationpath LABEL)` in BUILD files.
- Never hardcode canonical repository names (`@@name+`); reference a repo by
  its apparent name, or, where that is not resolvable, alias the target in a
  package and reference the alias.
