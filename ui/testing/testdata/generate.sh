#!/usr/bin/env bash
# Writes the fixtures to the directory given: one small build's profile, build
# event stream and execution log, and the query, cquery and aquery graphs of
# its targets. BAZEL names a Bazel 9.2.0 binary. The build runs in a scratch
# workspace under a clean environment, and the host name and login are
# overwritten with fillers of their length, so nothing names the machine or
# its user.
set -euo pipefail

out=$(realpath "${1:?usage: BAZEL=<bazel 9.2.0> generate.sh <output dir>}")
root=/tmp/bviz-fixture
if [[ -e $root ]]; then
    echo "$root is in the way: remove it to generate afresh" >&2
    exit 1
fi
mkdir -p "$root/home" "$root/ws" "$root/bin" "$out"
cp "${BAZEL:?}" "$root/bin/bazel"
for tool in bash cat sleep; do
    ln -s "$(command -v "$tool")" "$root/bin/$tool"
done
cd "$root/ws"

echo 'module(name = "fixture")' > MODULE.bazel
cat > BUILD.bazel <<'BUILD'
[genrule(
    name = name,
    srcs = srcs,
    outs = [name + ".txt"],
    cmd = "sleep %s; cat $(SRCS) /dev/null > $@; echo %s >> $@" % (seconds, name),
) for name, srcs, seconds in [
    ("base", [], "0.3"),
    ("left", [":base"], "0.4"),
    ("right", [":base"], "0.35"),
    ("side", [], "0.1"),
    ("join", [":left", ":right", ":side"], "0.2"),
    ("top", [":join"], "0.1"),
]]
BUILD

bazel() {
    env -i HOME="$root/home" USER=user LOGNAME=user PATH="$root/bin" TERM=dumb \
        "$root/bin/bazel" --nosystem_rc --nohome_rc "$@"
}
common=(--shell_executable="$root/bin/bash" --action_env=PATH="$root/bin")

bazel build //:top "${common[@]}" \
    --profile="$root/build.profile.gz" \
    --noslim_profile \
    --record_full_profiler_data \
    --experimental_profile_include_primary_output \
    --experimental_profile_include_target_label \
    --experimental_profile_include_target_configuration \
    --build_event_binary_file="$root/build.bep" \
    --show_timestamps \
    --execution_log_compact_file="$root/exec.log.zst"
graph=(--output=streamed_proto --consistent_labels)
bazel query //... "${graph[@]}" --proto:output_rule_attrs=all > "$root/query.binpb"
bazel cquery //... "${common[@]}" "${graph[@]}" --transitions=lite \
    --proto:include_configurations --proto:output_rule_attrs=all > "$root/cquery.binpb"
bazel aquery //... "${common[@]}" --output=streamed_proto --include_artifacts \
    > "$root/aquery.binpb"
bazel shutdown

# Bazel reads the host name and the user's login from the system, not the
# environment. A login as short as two letters is matched with the length byte
# protobuf writes before it, so the filler replaces no stray byte pair.
filler() { printf "$1%.0s" $(seq "$2") | head -c "$2"; }
literal() { printf '%s' "$1" | sed 's/[][\.*^$/]/\\&/g'; }
host=$(hostname)
login=$(id -un)
login_field=$(printf '\\x%02x%s' "${#login}" "$(literal "$login")")
scrub() {
    LC_ALL=C sed -e "s/$(literal "$host")/$(filler host ${#host})/g" \
        -e "s/$login_field/$(printf '\\x%02x' "${#login}")$(filler user ${#login})/g"
}
zcat "$root/build.profile.gz" | scrub | gzip -n9 > "$out/build.profile.gz"
scrub < "$root/build.bep" > "$out/build.bep"
zstd -dqc "$root/exec.log.zst" | scrub | zstd -qc19 > "$out/exec.log.zst"
for graph in query cquery aquery; do
    scrub < "$root/$graph.binpb" > "$out/$graph.binpb"
done
