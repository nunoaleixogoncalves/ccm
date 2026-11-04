#!/bin/sh
# Builds ccm from this repository and installs it, for a first install or an update.
# Set CCM_INSTALL_DIR to install somewhere other than ~/.local/bin.
set -eu

install_dir="${CCM_INSTALL_DIR:-$HOME/.local/bin}"
case "$install_dir" in
    /*) ;;
    *) install_dir="$PWD/$install_dir" ;;
esac
target="$install_dir/ccm"
cd "$(dirname "$0")"

fail() {
    printf 'ccm: %s\n' "$1" >&2
    exit 1
}

case "$(uname -s)" in
    Linux | Darwin) ;;
    *) fail "only Linux and macOS are supported." ;;
esac

# A mise shim can be on PATH and still fail, so run it rather than only looking for it.
if cargo --version >/dev/null 2>&1; then
    set -- cargo
elif command -v mise >/dev/null 2>&1 && mise exec -- cargo --version >/dev/null 2>&1; then
    set -- mise exec -- cargo
else
    fail "Rust isn't installed. Get it from https://rustup.rs, then run this again."
fi

if ! cc --version >/dev/null 2>&1; then
    case "$(uname -s)" in
        Darwin) hint="xcode-select --install" ;;
        *) hint="the build-essential or base-devel package" ;;
    esac
    fail "a C compiler is needed to build SQLite. Install one with: $hint"
fi

# The copy below expects the build in ./target.
unset CARGO_TARGET_DIR
"$@" build --release --locked -p ccm

# Moved into place, so a running ccm or hook keeps its old copy instead of being overwritten.
mkdir -p "$install_dir"
cp target/release/ccm "$target.new"
mv -f "$target.new" "$target"
printf 'Installed %s\n\n' "$target"

if [ -t 0 ]; then
    "$target" setup
else
    printf 'Run "%s setup" to install the hooks crash recovery needs.\n' "$target"
fi

# An earlier "cargo install" leaves a copy that can shadow this one, and mise makes a shim for it
# that fails outside this repository. Only remove it once no hook still runs it.
config="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
old="$HOME/.cargo/bin/ccm"
if [ -x "$old" ] && [ ! "$old" -ef "$target" ] && "$old" --help 2>/dev/null | grep -q 'browse your conversations'; then
    if grep -qE '(/|^|[^[:alnum:]_])\.cargo/bin/ccm' "$config/settings.json" "$config/settings.local.json" "$config/statusline.sh" 2>/dev/null; then
        printf '\nKept %s because Claude Code hooks still use it. Run "%s setup" to move them.\n' "$old" "$target"
    else
        rm -f "$old"
        printf '\nRemoved the old copy at %s\n' "$old"
    fi
fi
if command -v mise >/dev/null 2>&1; then
    mise reshim >/dev/null 2>&1 || true
fi

case ":$PATH:" in
    *":$install_dir:"*)
        found="$(command -v ccm || true)"
        if [ "$found" != "$target" ]; then
            printf '\nWarning: "ccm" runs %s, not this copy. Remove that one or put %s first in PATH.\n' "$found" "$install_dir"
        fi
        ;;
    *)
        printf '\n%s is not on your PATH. Add this line to your shell startup file:\n  export PATH="%s:$PATH"\n' "$install_dir" "$install_dir"
        ;;
esac
