#!/bin/sh

set -eu

# cargo-make (available here as `makers`) inherits the caller's PATH.  On
# macOS machines managed by Nix that can select Nix's `cc` instead of Apple's
# clang, which makes aws-lc-sys fail to link against the macOS SDK.  Keep this
# workaround in the task wrapper so every makers cargo task uses the same
# toolchain.
if [ "$(uname -s)" = "Darwin" ]; then
    command -v xcrun >/dev/null 2>&1 || {
        echo "lait: xcrun is required on macOS to locate the macOS SDK" >&2
        exit 1
    }

    export CC="${LAIT_CC:-/usr/bin/clang}"
    export CXX="${LAIT_CXX:-/usr/bin/clang++}"
    export SDKROOT="${LAIT_SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"

    case "$(uname -m)" in
        arm64|aarch64)
            export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="${LAIT_LINKER:-/usr/bin/clang}"
            ;;
        x86_64)
            export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="${LAIT_LINKER:-/usr/bin/clang}"
            ;;
    esac
fi

exec cargo "$@"
