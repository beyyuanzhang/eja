#!/usr/bin/env bash
set -euo pipefail

FIXTURE="$(mktemp -d)"
trap 'rm -r "$FIXTURE"' EXIT
DATA="$FIXTURE/data"
UI="$DATA/ui"
mkdir -p "$UI"
cp "${1:?Provide the path to the built launcher}" "$FIXTURE/ejaApp"

build_ui() {
    printf '%s\n' '#include <stdio.h>' '#include <unistd.h>' "void eja_ui_main(void) { $2 }" |
        xcrun clang -x c -dynamiclib -o "$1" -
}

launch() {
    "$FIXTURE/ejaApp" --data-dir "$DATA"
}

build_ui "$FIXTURE/libejaUI.dylib" 'puts("factory");'
build_ui "$UI/libejaUI.dylib" 'puts("custom");'
shasum -a 256 "$FIXTURE/libejaUI.dylib" | cut -d' ' -f1 > "$UI/.factory-digest"
shasum -a 256 "$UI/libejaUI.dylib" | cut -d' ' -f1 > "$UI/.booting"

test "$(launch)" = custom
test ! -e "$UI/.booting"
test "$(launch)" = custom
test "$(eja_SAFE_MODE=1 launch)" = factory
test -f "$UI/libejaUI.dylib"

build_ui "$UI/libejaUI.dylib" '_exit(9);'
EXIT_CODE=0
launch || EXIT_CODE=$?
test "$EXIT_CODE" = 9
test -f "$UI/.booting"
test "$(launch)" = factory
test ! -e "$UI/.booting"
test -f "$UI/libejaUI.dylib"
