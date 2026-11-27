#!/bin/bash
# Replaces the installed ClawTank build, captures unified logs, then launches that exact app.

set -euo pipefail
set -m
umask 077

app_path="${1:?usage: run-local.sh /path/to/ClawTank.app}"
install_path="${2:-/Applications/ClawTank.app}"
if [[ ! -d "$app_path" ]]; then
    printf 'App bundle not found: %s\n' "$app_path" >&2
    exit 1
fi
app_path="$(cd "$(/usr/bin/dirname "$app_path")" && /bin/pwd -P)/$(/usr/bin/basename "$app_path")"
install_parent="$(/usr/bin/dirname "$install_path")"
stage_path="$install_path.stage.$$"
backup_path="$install_path.previous.$$"

if /usr/bin/pgrep -x ClawTank >/dev/null; then
    /usr/bin/osascript -e 'tell application id "dev.clawtank.ClawTank" to quit' >/dev/null 2>&1 || true
    for _ in $(/usr/bin/seq 1 30); do
        if ! /usr/bin/pgrep -x ClawTank >/dev/null; then break; fi
        /bin/sleep 1
    done
    if /usr/bin/pgrep -x ClawTank >/dev/null; then
        printf 'ClawTank did not quit cleanly; refusing to replace the running app.\n' >&2
        exit 1
    fi
fi

if [[ "$app_path" != "$install_path" ]]; then
    if [[ ! -d "$install_parent" || ! -w "$install_parent" ]]; then
        printf 'Cannot replace %s without write access; no privilege escalation is attempted.\n' "$install_path" >&2
        exit 1
    fi
    if [[ -e "$stage_path" || -e "$backup_path" ]]; then
        printf 'A previous deployment left a staging path; inspect %s and %s before retrying.\n' \
            "$stage_path" "$backup_path" >&2
        exit 1
    fi
    /usr/bin/ditto "$app_path" "$stage_path"
    /usr/bin/codesign --verify --deep --strict "$stage_path"
    if [[ -e "$install_path" ]]; then /bin/mv "$install_path" "$backup_path"; fi
    if ! /bin/mv "$stage_path" "$install_path"; then
        if [[ -e "$backup_path" ]]; then /bin/mv "$backup_path" "$install_path"; fi
        exit 1
    fi
    /bin/rm -rf "$backup_path"
fi
app_path="$install_path"

support_dir="$HOME/Library/Application Support/ClawTank"
log_file="$support_dir/runtime.log"
error_file="$support_dir/runtime-errors.log"
session_file="$support_dir/runtime-session.txt"
mkdir -p "$support_dir"

bitmap_diagnostics="${CLAWTANK_RDP_BITMAP_DIAGNOSTICS:-0}"
if [[ "$bitmap_diagnostics" != "0" && "$bitmap_diagnostics" != "1" ]]; then
    printf 'CLAWTANK_RDP_BITMAP_DIAGNOSTICS must be 0 or 1.\n' >&2
    exit 1
fi

rm -f "$log_file" "$error_file" "$session_file"
printf 'runID=%s\nstartedAtUTC=%s\napp=%s\narchitecture=%s\nbitmapDiagnostics=%s\n' \
    "$(/usr/bin/uuidgen)" \
    "$(/bin/date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    "$app_path" \
    "$(/usr/bin/uname -m)" \
    "$bitmap_diagnostics" > "$session_file"

predicate='subsystem == "dev.clawtank.ClawTank"'
/usr/bin/log stream --style ndjson --level debug --predicate "$predicate" \
    2>"$error_file" \
    | /usr/bin/awk '/"eventType"[[:space:]]*:/ { print; fflush() }' > "$log_file" &
stream_pid=$!
stream_pgid="$(/bin/ps -o pgid= -p "$stream_pid" | /usr/bin/tr -d ' ')"

cleanup() {
    if [[ -n "${stream_pgid:-}" ]]; then
        kill -TERM -- "-$stream_pgid" 2>/dev/null || true
    else
        kill "$stream_pid" 2>/dev/null || true
    fi
    wait "$stream_pid" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

/bin/sleep 1
if ! kill -0 "$stream_pid" 2>/dev/null; then
    printf 'macOS unified log capture did not start. See %s\n' "$error_file" >&2
    exit 1
fi

if [[ "$bitmap_diagnostics" == "1" ]]; then
    /usr/bin/open -na "$app_path" --env CLAWTANK_RDP_BITMAP_DIAGNOSTICS=1
else
    /usr/bin/open -na "$app_path"
fi
app_pid=""
for _ in $(/usr/bin/seq 1 30); do
    app_pid="$(/usr/bin/pgrep -x ClawTank | /usr/bin/head -1 || true)"
    if [[ -n "$app_pid" ]]; then break; fi
    /bin/sleep 1
done
if [[ -z "$app_pid" ]]; then
    printf 'Launch Services did not start ClawTank from %s.\n' "$app_path" >&2
    exit 1
fi
app_command="$(/bin/ps -p "$app_pid" -o command= | /usr/bin/sed 's/^[[:space:]]*//')"
expected_executable="$app_path/Contents/MacOS/ClawTank"
if [[ "$app_command" != "$expected_executable" ]]; then
    printf 'Unexpected ClawTank process: pid=%s executable=%s\n' "$app_pid" "$app_command" >&2
    exit 1
fi
printf 'processID=%s\nexecutable=%s\n' "$app_pid" "$expected_executable" >> "$session_file"
printf 'Capturing ClawTank unified logs to: %s\nErrors: %s\nPress Ctrl-C to stop capture.\n' \
    "$log_file" "$error_file"
wait "$stream_pid"
