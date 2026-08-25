load("@package_metadata//licenses/rules:license.bzl", "license")
load("@package_metadata//rules:package_metadata.bzl", "package_metadata")

package(default_package_metadata = [":package_metadata"])

package_metadata(
    name = "package_metadata",
    attributes = [":license"],
    purl = "pkg:github/phosphor-icons/core@v2.0.8",
)

license(
    name = "license",
    kind = "@package_metadata//licenses/spdx:MIT",
    text = "LICENSE",
)

filegroup(
    name = "icons",
    srcs = [
        "assets/regular/caret-double-down.svg",
        "assets/regular/caret-double-up.svg",
        "assets/regular/caret-down.svg",
        "assets/regular/caret-up.svg",
        "assets/regular/check.svg",
        "assets/regular/circle-notch.svg",
        "assets/regular/minus.svg",
        "assets/regular/pause.svg",
        "assets/regular/play.svg",
        "assets/regular/skip-back.svg",
        "assets/regular/skip-forward.svg",
        "assets/regular/square.svg",
        "assets/regular/x.svg",
    ],
    visibility = ["//visibility:public"],
)
