"""Runs a Rust test compiled for the web build under wasmtime."""

load("//platforms:web.bzl", "NOT_WEB", "web_executable")

_WASMTIME = Label("//tools/wasm_test:wasmtime")

def _rlocationpath(ctx, label, target):
    return ctx.expand_location("$(rlocationpath %s)" % label, [target])

def _wasm_test_run_impl(ctx):
    runner = ctx.attr._runner[DefaultInfo]
    executable = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = executable,
        target_file = runner.files_to_run.executable,
        is_executable = True,
    )
    runfiles = ctx.runfiles([ctx.file._wasmtime]).merge_all([
        runner.default_runfiles,
        ctx.attr.module[DefaultInfo].default_runfiles,
        ctx.attr._wasmtime[DefaultInfo].default_runfiles,
    ])
    return [
        DefaultInfo(executable = executable, runfiles = runfiles),
        RunEnvironmentInfo(environment = {
            "WASMTIME": _rlocationpath(ctx, _WASMTIME, ctx.attr._wasmtime),
            "WASM_TEST_MODULE": _rlocationpath(ctx, ctx.attr.module.label, ctx.attr.module),
        }),
    ]

_wasm_test_run_test = rule(
    implementation = _wasm_test_run_impl,
    attrs = {
        "module": attr.label(
            mandatory = True,
            allow_single_file = True,
        ),
        "_runner": attr.label(
            default = Label("//tools/wasm_test:run_wasm_test"),
            executable = True,
            cfg = "target",
        ),
        "_wasmtime": attr.label(
            default = _WASMTIME,
            allow_single_file = True,
        ),
    },
    test = True,
    doc = "Runs a WebAssembly module of a rust_test under wasmtime.",
)

def _wasm_test_impl(name, visibility, test, size):
    module = name + "_module"
    web_executable(
        name = module,
        executable = test,
        platform = Label("//platforms:wasm32_web_test"),
        testonly = True,
    )
    _wasm_test_run_test(
        name = name,
        size = size,
        module = ":" + module,
        target_compatible_with = NOT_WEB,
        visibility = visibility,
    )

wasm_test = macro(
    implementation = _wasm_test_impl,
    attrs = {
        "test": attr.label(
            mandatory = True,
            configurable = False,
            doc = "The rust_test to build for the web and run.",
        ),
        "size": attr.string(
            default = "small",
            configurable = False,
        ),
    },
    doc = "Runs a rust_test as the web build compiles it, under wasmtime.",
)
