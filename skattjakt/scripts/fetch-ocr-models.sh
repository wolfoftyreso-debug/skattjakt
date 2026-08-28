#!/usr/bin/env bash
# The two OCR models, fetched by checksum.
#
# They are 11.5 MB and do not belong in git, but everything else in this
# repository's supply chain is checksummed and these are read by a process
# that handles customers' accounts. So the digests are pinned here: a model
# that does not match is not used, and the failure says which one.
#
# Usage: scripts/fetch-ocr-models.sh [directory]     (default: models/)

set -euo pipefail

DEST="${1:-models}"
BASE="https://ocrs-models.s3-accelerate.amazonaws.com"

# ocrs 0.12's models, retrieved 2026-08-28.
DETECTION_SHA=f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca
RECOGNITION_SHA=e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e

mkdir -p "$DEST"

fetch() { # name expected-sha
    local name="$1" want="$2" path="$DEST/$1.rten"

    if [[ -f "$path" ]] && [[ "$(sha256sum "$path" | cut -d' ' -f1)" == "$want" ]]; then
        echo "  $name.rten already present and matches"
        return
    fi

    echo "  fetching $name.rten"
    curl -sSfL --retry 3 --max-time 300 -o "$path.part" "$BASE/$name.rten"

    local got
    got="$(sha256sum "$path.part" | cut -d' ' -f1)"
    if [[ "$got" != "$want" ]]; then
        rm -f "$path.part"
        echo "checksum mismatch for $name.rten" >&2
        echo "  expected $want" >&2
        echo "  got      $got" >&2
        exit 1
    fi
    mv "$path.part" "$path"
    echo "  $name.rten ok"
}

fetch text-detection "$DETECTION_SHA"
fetch text-recognition "$RECOGNITION_SHA"

echo "OCR models in $DEST"
