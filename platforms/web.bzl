"""The web build: compatibility with it, Rust executables built for it, and their bindings."""

load("@rules_rs_rules_rust//rust:defs.bzl", "rust_clippy_aspect")

# The target_compatible_with of a target that runs on the build's host, or
# needs a library the browser does not have.
NOT_WEB = select({
    "@platforms//cpu:wasm32": ["@platforms//:incompatible"],
    "//conditions:default": [],
})

def _to_platform_impl(_settings, attr):
    return {"//command_line_option:platforms": [str(attr.platform)]}

_to_platform = transition(
    implementation = _to_platform_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

_LINTED_ATTRS = ["crate", "deps", "proc_macro_deps"]

_WebClippyInfo = provider(
    doc = "The clippy checks of a Rust target and the Rust targets it builds on.",
    fields = ["checks"],
)

def _web_clippy_impl(target, ctx):
    checks = []
    if OutputGroupInfo in target and hasattr(target[OutputGroupInfo], "clippy_checks"):
        checks.append(target[OutputGroupInfo].clippy_checks)
    for attr in _LINTED_ATTRS:
        value = getattr(ctx.rule.attr, attr, [])
        for dep in value if type(value) == "list" else [value]:
            if dep and _WebClippyInfo in dep:
                checks.append(dep[_WebClippyInfo].checks)
    return [_WebClippyInfo(checks = depset(transitive = checks))]

# The clippy aspect --config=check applies reaches only top-level targets, and
# the crates an executable built for the web depends on are built for it only
# below one, so this one lints them all as the web build compiles them.
_web_clippy = aspect(
    implementation = _web_clippy_impl,
    attr_aspects = _LINTED_ATTRS,
    requires = [rust_clippy_aspect],
)

def _web_executable_impl(ctx):
    executable = ctx.attr.executable[0]
    module = executable[DefaultInfo].files_to_run.executable
    return [
        DefaultInfo(files = depset([module]), runfiles = ctx.runfiles([module])),
        OutputGroupInfo(clippy_checks = executable[_WebClippyInfo].checks),
    ]

web_executable = rule(
    implementation = _web_executable_impl,
    attrs = {
        "executable": attr.label(
            mandatory = True,
            cfg = _to_platform,
            aspects = [_web_clippy],
            doc = "The rust_binary or rust_test to build.",
        ),
        "platform": attr.label(
            mandatory = True,
            providers = [platform_common.PlatformInfo],
            doc = "The web platform to build it for.",
        ),
    },
    doc = "The WebAssembly module of a Rust executable built for a web platform.",
)

def _web_bindgen_impl(ctx):
    module = ctx.file.module
    bindings = ctx.actions.declare_directory(ctx.label.name)
    args = ctx.actions.args()
    args.add("--target", "web")
    args.add("--no-typescript")
    args.add("--out-name", module.basename.removesuffix("." + module.extension))
    args.add("--out-dir", bindings.path)
    args.add(module)
    ctx.actions.run(
        executable = ctx.executable._wasm_bindgen,
        arguments = [args],
        inputs = [module],
        outputs = [bindings],
        mnemonic = "WasmBindgen",
        progress_message = "Generating JavaScript bindings for %{label}",
    )
    return [DefaultInfo(files = depset([bindings]))]

web_bindgen = rule(
    implementation = _web_bindgen_impl,
    attrs = {
        "module": attr.label(
            mandatory = True,
            allow_single_file = [".wasm"],
            doc = "The web_executable whose module to bind.",
        ),
        "_wasm_bindgen": attr.label(
            default = "@crates//:wasm-bindgen-cli__wasm-bindgen",
            executable = True,
            cfg = "exec",
        ),
    },
    doc = "A directory of a WebAssembly module as wasm-bindgen prepares it for a " +
          "page to load, with the ES module that loads it, both named after the module.",
)
