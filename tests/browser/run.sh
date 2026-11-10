#!/bin/bash
# Checks ccm web in headless Chrome, with the terminal app open on the same data.
#
#   cargo build --locked && tests/browser/run.sh [list] [actions] [delete] [focus] [worktrees]
#
# With no names, every suite runs. It needs Node 22 or later, Chrome, Python 3, Git, curl and ss.
# CCM_BIN picks another program and CHROME another browser.
#
# Each suite gets an invented HOME in a new temporary folder, and a fake terminal that only logs
# what it was asked to open, so nothing here reads or changes real conversations. A suite that
# fails leaves its folder behind, with the screenshots it took.

set -u

here=$(cd "$(dirname "$0")" && pwd)
bin=${CCM_BIN:-$here/../../target/debug/ccm}
[ -x "$bin" ] || { echo "$bin not found. Run cargo build --locked first."; exit 2; }
for tool in node python3 git curl ss "${CHROME:-google-chrome}"; do
    command -v "$tool" >/dev/null || { echo "$tool is needed to run these checks."; exit 2; }
done
[ "$(node -p 'parseInt(process.versions.node)')" -ge 22 ] || { echo "Node 22 or later is needed."; exit 2; }

server=
terminal_app=
stop() {
    [ -n "$server" ] && kill "$server" 2>/dev/null
    [ -n "$terminal_app" ] && kill "$terminal_app" 2>/dev/null
    server=
    terminal_app=
}
trap stop EXIT

failed=0
passed=0

check() { # name, expected, got
    if [ "$2" = "$3" ]; then
        echo "ok   $1"
        passed=$((passed + 1))
    else
        echo "FAIL $1  got: $3"
        failed=$((failed + 1))
    fi
}

code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

suite() { # name, the script that makes its data
    local name=$1 data=$2 root home failed_before=$failed link port i
    echo "=== $name"
    root=$(mktemp -d -t ccm-browser-XXXXXX)
    home=$root/home
    python3 "$here/$data" "$root" || { echo "FAIL could not make the data"; failed=$((failed + 1)); return; }

    cat > "$root/fake-term" <<EOT
#!/bin/sh
[ -f "$root/fail" ] && exit 3
echo "\$(pwd) :: \$*" >> "$root/term.log"
EOT
    chmod +x "$root/fake-term"
    mkdir -p "$home/.config/ccm" "$home/.local/state/ccm"
    echo "terminal = [\"$root/fake-term\"]" > "$home/.config/ccm/config.toml"
    python3 "$here/mark_open.py" "$home"

    # Only this suite thinks it's on Hyprland. The fake shows window 0xa with the focus, then a new
    # 0xb until the focus is given back.
    local on_hyprland=()
    if [ "$name" = focus ]; then
        mkdir "$root/bin"
        cat > "$root/bin/hyprctl" <<EOT
#!/bin/sh
echo "\$*" >> "$root/hyprctl.log"
case "\$1" in
    activewindow)
        if [ -f "$root/new-window" ]; then echo '{"address":"0xb"}'; else touch "$root/new-window"; echo '{"address":"0xa"}'; fi ;;
    clients) echo '[{"address":"0xa"}]' ;;
    dispatch) rm -f "$root/new-window" ;;
esac
EOT
        chmod +x "$root/bin/hyprctl"
        on_hyprland=(HYPRLAND_INSTANCE_SIGNATURE=invented)
    fi

    env -i PATH="$root/bin:$PATH" HOME="$home" GIT_OPTIONAL_LOCKS=0 "${on_hyprland[@]}" "$bin" web --no-browser > "$root/server.txt" 2>&1 &
    server=$!
    for i in $(seq 40); do
        grep -q token= "$root/server.txt" && break
        sleep 0.25
    done
    link=$(grep -o 'http://127[^ ]*' "$root/server.txt")
    port=$(echo "$link" | sed -E 's/.*:([0-9]+)\/.*/\1/')
    if [ -z "$link" ]; then
        echo "FAIL ccm web gave no link: $(cat "$root/server.txt")"
        failed=$((failed + 1))
        stop
        return
    fi

    check "a request with no cookie is refused" 401 "$(code "http://127.0.0.1:$port/api/list")"
    check "another Host is refused" 403 "$(code -H "Host: evil.example:$port" "http://127.0.0.1:$port/")"
    check "a wrong token is refused" 401 "$(code "http://127.0.0.1:$port/?token=abc")"
    check "it only listens on this computer" "127.0.0.1:$port" "$(ss -ltnH "sport = :$port" | awk '{print $4}')"

    python3 "$here/terminal_app.py" "$bin" "$home" 30 > "$root/terminal-app.txt" 2>&1 &
    terminal_app=$!
    node "$here/$name.mjs" "$link" "$root" "$home" "$home/.claude/projects/$(echo "$home/notes" | tr / -)" "$root/term.log" | tee "$root/out.txt"
    [ "${PIPESTATUS[0]}" = 0 ] || failed=$((failed + 1))
    passed=$((passed + $(grep -c '^ok ' "$root/out.txt")))
    wait "$terminal_app"
    terminal_app=
    check "the terminal app ran alongside it" "exit 0 showed list: True failed refresh: False" "$(cat "$root/terminal-app.txt")"
    check "the link only works once" 401 "$(code "$link")"

    stop
    if [ "$failed" = "$failed_before" ]; then
        rm -rf "$root"
    else
        echo "Kept for a look: $root"
    fi
}

all=(list:data_list.py actions:data_list.py delete:data_delete.py focus:data_list.py worktrees:data_worktrees.py)
[ $# -gt 0 ] || set -- list actions delete focus worktrees
for wanted in "$@"; do
    found=
    for entry in "${all[@]}"; do
        if [ "${entry%%:*}" = "$wanted" ]; then
            suite "$wanted" "${entry#*:}"
            found=1
        fi
    done
    [ -n "$found" ] || { echo "No suite called $wanted."; exit 2; }
done

echo "$passed passed, $failed failed"
[ "$failed" = 0 ]
