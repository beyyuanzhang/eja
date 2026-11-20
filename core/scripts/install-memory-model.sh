#!/bin/sh
# Install only the pinned public weights. Journal contents stay local.
set -eu
dir=${1:-"$(dirname "$0")/../.models/multilingual-e5-small"}
base=https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78
mkdir -p "$dir"
check() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi | cut -d ' ' -f 1
}
while read -r expected asset; do
    file="$dir/${asset##*/}"
    if [ -f "$file" ] && [ "$(check "$file")" = "$expected" ]; then continue; fi
    part=$(mktemp "$dir/.download.XXXXXX")
    trap 'rm -f "$part"' EXIT
    curl -fLsS --retry 2 "$base/$asset" -o "$part"
    [ "$(check "$part")" = "$expected" ] || { echo "Checksum mismatch for $asset" >&2; exit 1; }
    mv "$part" "$file"
done <<'ASSETS'
cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1 config.json
f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193 onnx/model_quantized.onnx
d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7 special_tokens_map.json
0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39 tokenizer.json
a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b tokenizer_config.json
ASSETS
cat >"$dir/encoder.json" <<'PROFILE'
{"dimensions":384,"pooling":"mean","passage_prefix":"passage: ","query_prefix":"query: ","max_tokens":512}
PROFILE
echo "Memory model ready in $dir"
