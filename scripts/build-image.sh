#!/usr/bin/env bash
# build-image: build the openrusty gateway image from
# build/image/Dockerfile (multi-stage cargo build -> debian:bookworm-slim
# runtime) and sanity-check the result before anyone deploys it: non-root
# UID 511, iptables present, `openrusty --help` exits 0. Docker is the
# only external tool; the build context is pinned small by .dockerignore.
#
# --save [PATH] additionally writes a `docker save` tar for air-gapped
# `docker load` (default build/image/openrusty-<tag>.tar; PATH '-' streams
# the tar to stdout).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

usage() {
    cat <<'EOF'
usage: scripts/build-image.sh [--ref REF] [--save [PATH]]

  --ref REF      image ref to build (default: $IMAGE_REF or openrusty:local)
  --save [PATH]  docker-save the image after the checks
                 (default: build/image/openrusty-<tag>.tar; '-' = stdout)
  -h, --help     this text
EOF
}

REF="${IMAGE_REF:-openrusty:local}"
WANT_SAVE=0
SAVE_PATH=""

while [ $# -gt 0 ]; do
    case "$1" in
        --ref)
            [ $# -ge 2 ] || { echo "build-image: --ref needs a value" >&2; exit 2; }
            REF="$2"; shift 2 ;;
        --ref=*) REF="${1#*=}"; shift ;;
        --save)
            # Optional value: consume the next word only when it is a path
            # ('-' counts), never a flag.
            if [ $# -ge 2 ] && { [ "$2" = "-" ] || [[ "$2" != -* ]]; }; then
                SAVE_PATH="$2"; shift 2
            else
                WANT_SAVE=1; shift
            fi ;;
        --save=*) WANT_SAVE=1; SAVE_PATH="${1#*=}"; shift ;;
        -h | --help) usage; exit 0 ;;
        *)
            echo "build-image: unknown flag '$1'" >&2; usage >&2; exit 2 ;;
    esac
done

# Tar filename from the ref's tag (docker save writes plain files, so
# registry-path slashes must go); an untagged ref means docker's :latest.
tag_of() {
    local t="${1##*:}"
    [ "$t" = "$1" ] && t="latest"
    printf '%s' "$t"
}
DEFAULT_TAR="build/image/openrusty-$(tag_of "$REF").tar"

# Keep the tree clean: build/ is already gitignored; a custom save path
# outside it gets named in .gitignore (only when one already exists).
ensure_ignored() {
    local path="$1"
    [ -f .gitignore ] || return 0
    case "$path" in /*) return 0 ;; esac
    git check-ignore -q "$path" 2>/dev/null && return 0
    printf '/%s\n' "$path" >> .gitignore
    echo ">> appended /$path to .gitignore"
}

echo ">> docker build -f build/image/Dockerfile -t $REF ."
docker build -f build/image/Dockerfile -t "$REF" .

PASS=0
FAIL=0
report() { # ok name
    if [ "$1" = 0 ]; then PASS=$((PASS + 1)); echo "PASS: $2"
    else FAIL=$((FAIL + 1)); echo "FAIL: $2"; fi
}

uid="$(docker run --rm --entrypoint id "$REF" -u 2>/dev/null || true)"
[ "$uid" = "511" ]; report $? "runs as non-root UID 511 (id -u: '$uid')"

docker run --rm --entrypoint iptables "$REF" --version >/dev/null 2>&1
report $? "iptables binary present in the image"

docker run --rm --entrypoint /usr/local/bin/openrusty "$REF" --help >/dev/null 2>&1
report $? "openrusty --help exits 0"

if [ "$WANT_SAVE" = 1 ] || [ -n "$SAVE_PATH" ]; then
    SAVE_PATH="${SAVE_PATH:-$DEFAULT_TAR}"
    if [ "$SAVE_PATH" = "-" ]; then
        echo ">> docker save $REF -> stdout" >&2
        docker save "$REF"
    else
        echo ">> docker save $REF -> $SAVE_PATH"
        mkdir -p "$(dirname "$SAVE_PATH")"
        docker save "$REF" -o "$SAVE_PATH"
        ensure_ignored "$SAVE_PATH"
    fi
fi

echo "build-image: $PASS passed, $FAIL failed ($REF)"
[ "$FAIL" -eq 0 ]
