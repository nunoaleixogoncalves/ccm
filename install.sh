#!/bin/sh
# Installs or updates ccm. Piped from curl, or run on its own, it downloads the ready-made program
# for this system. Run from a clone of the repository, it builds from source.
# Set CCM_INSTALL_DIR to install somewhere other than ~/.local/bin.
# Set CCM_RELEASE_URL to download from somewhere other than the latest GitHub release.
set -eu

fail() {
    printf 'ccm: %s\n' "$1" >&2
    exit 1
}

download_target() {
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64 | Linux-amd64) echo x86_64-unknown-linux-musl ;;
        Linux-aarch64 | Linux-arm64) echo aarch64-unknown-linux-musl ;;
        Darwin-x86_64) echo x86_64-apple-darwin ;;
        Darwin-arm64) echo aarch64-apple-darwin ;;
        *) fail "there's no ready-made program for $(uname -s) on $(uname -m). Build from source: see the README." ;;
    esac
}

check_sum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1" >/dev/null 2>&1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$1" >/dev/null 2>&1
    else
        fail "sha256sum or shasum is needed to check the download."
    fi
}

install_download() {
    command -v curl >/dev/null 2>&1 || fail "curl is needed to download ccm."
    base="${CCM_RELEASE_URL:-https://github.com/nunoaleixogoncalves/ccm/releases/latest/download}"
    archive="ccm-$(download_target).tar.gz"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    for file in "$archive" "$archive.sha256"; do
        curl -fsSL --proto-redir '=https' -o "$work/$file" "$base/$file" || fail "couldn't download $base/$file"
    done
    (cd "$work" && check_sum "$archive.sha256") || fail "the download doesn't match its checksum, so nothing was installed."
    tar -xzf "$work/$archive" -C "$work" ccm
    place "$work/ccm"
}

build_from_source() {
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
    place target/release/ccm
}

# Moved into place, so a running ccm or hook keeps its old copy instead of being overwritten.
place() {
    mkdir -p "$install_dir"
    cp "$1" "$target.new"
    chmod 755 "$target.new"
    mv -f "$target.new" "$target"
    printf 'Installed %s\n\n' "$target"
}

main() {
    install_dir="${CCM_INSTALL_DIR:-$HOME/.local/bin}"
    case "$install_dir" in
        /*) ;;
        *) install_dir="$PWD/$install_dir" ;;
    esac
    target="$install_dir/ccm"

    case "$(uname -s)" in
        Linux | Darwin) ;;
        *) fail "only Linux and macOS are supported." ;;
    esac

    from_source=false
    if [ "$(basename "$0")" = install.sh ] && [ -f "$(dirname "$0")/Cargo.toml" ]; then
        from_source=true
    fi
    if "$from_source"; then
        cd "$(dirname "$0")"
        build_from_source
    else
        install_download
    fi

    # Piped from curl, the script itself is on stdin, so the answer has to come from the terminal.
    if [ -t 0 ]; then
        "$target" setup
    elif ! "$from_source" && (: </dev/tty) 2>/dev/null; then
        "$target" setup </dev/tty
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
}

# Called only on the last line, so a download cut short can't run half of this script.
main "$@"
