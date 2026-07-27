#!/bin/bash
#
# Cargo runner for development on macOS.
#
# A raw Rust executable receives an ad-hoc signature whose identity changes
# after every link. macOS privacy permissions are keyed to application
# identity, so Accessibility and screen-capture grants become unreliable.
# Signing the main executable with the same development certificate and bundle
# identifier gives it a stable designated requirement instead.
#
# Set AIBO_CODESIGN_IDENTITY to a certificate hash or full identity name to
# override automatic selection.

set -euo pipefail

executable="${1:?cargo runner did not provide an executable}"
shift

if [[ "$(/usr/bin/basename "$executable")" == "aibo" ]]; then
    identity="${AIBO_CODESIGN_IDENTITY:-}"
    if [[ -z "$identity" ]]; then
        identity="$(
            /usr/bin/security find-identity -v -p codesigning |
                /usr/bin/awk '/"Apple Development:/ { print $2; exit }'
        )"
    fi

    if [[ -z "$identity" ]]; then
        echo "Aibo needs a stable macOS development signature for privacy permissions." >&2
        echo "Install an Apple Development certificate or set AIBO_CODESIGN_IDENTITY." >&2
        exit 1
    fi

    /usr/bin/codesign \
        --force \
        --sign "$identity" \
        --identifier com.aibo.aibo \
        --timestamp=none \
        "$executable"
fi

exec "$executable" "$@"
