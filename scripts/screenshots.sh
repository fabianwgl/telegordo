#!/usr/bin/env bash
# Regenerates the README screenshots in docs/screenshots/.
#
# The ignored test `screenshots::readme_frames` draws the real UI with made-up
# chats and saves the exact bytes the app writes to the terminal. This script
# runs that test inside a fresh Ghostty window, replays each frame there and
# captures the window. No Telegram account is involved.
#
# Needs macOS, Ghostty in /Applications, and Screen Recording permission for
# the terminal that runs this script (System Settings → Privacy & Security).
# A Ghostty window opens for a few seconds while it runs.
set -euo pipefail

cols=120
rows=34
theme="Catppuccin Mocha"
repo=$(cd "$(dirname "$0")/.." && pwd)
out=$repo/docs/screenshots

die() {
    echo "screenshots: $*" >&2
    exit 1
}

if [[ ${1-} == --play ]]; then
    # Inside the Ghostty window: render the frames at this window's size, then
    # show them one by one while the outer script captures the window.
    work=$2 bin=$3
    TELEGORDO_SHOTS_DIR=$work "$bin" --ignored --exact screenshots::readme_frames \
        >"$work/test.log" 2>&1 || {
        touch "$work/failed"
        exit 1
    }
    while read -r name; do
        printf '\e[H\e[2J'
        cat "$work/$name.ans"
        touch "$work/$name.shown"
        until [[ -e $work/$name.captured ]]; do sleep 0.1; done
    done <"$work/frames.txt"
    exit 0
fi

[[ $(uname) == Darwin ]] || die "needs macOS"
[[ -d /Applications/Ghostty.app ]] || die "needs Ghostty in /Applications"

cd "$repo"
bin=$(cargo test --no-run 2>&1 | sed -n 's/.*Executable unittests src\/main.rs (\(.*\))$/\1/p')
[[ -n $bin ]] || die "building the tests failed; run cargo test --no-run"
[[ $bin == /* ]] || bin=$repo/$bin

# 26 pixels per point of text gives 16×34-pixel cells on any display, so the
# images are as sharp as a Retina screenshot.
scale=$(osascript -l JavaScript -e 'ObjC.import("AppKit"); $.NSScreen.mainScreen.backingScaleFactor')
font=$(awk -v s="$scale" 'BEGIN { print 26 / s }')
pad=$(awk -v s="$scale" 'BEGIN { print 20 / s }')

work=$(mktemp -d "${TMPDIR:-/tmp}/telegordo-shots.XXXXXX")
pid=
cleanup() {
    if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT

before=$(pgrep -x ghostty || true)
open -na Ghostty.app --args \
    --config-default-files=false \
    --window-save-state=never \
    --theme="$theme" \
    --font-size="$font" \
    --font-feature=-calt,-liga \
    --window-width="$cols" \
    --window-height="$rows" \
    --window-padding-x="$pad" \
    --window-padding-y="$pad" \
    --window-padding-balance=true \
    --title=telegordo \
    --confirm-close-surface=false \
    --quit-after-last-window-closed=true \
    -e /bin/bash "$repo/scripts/screenshots.sh" --play "$work" "$bin"

for _ in {1..100}; do
    pid=$(comm -13 <(sort <<<"$before") <(pgrep -x ghostty | sort) | tail -n 1)
    [[ -n $pid ]] && break
    sleep 0.1
done
[[ -n $pid ]] || die "Ghostty did not start"

wait_for() {
    local i
    for ((i = 0; i < $2 * 10; i++)); do
        [[ -e $1 ]] && return 0
        if [[ -e $work/failed ]]; then
            cat "$work/test.log" >&2
            die "rendering the frames failed"
        fi
        sleep 0.1
    done
    die "timed out waiting for $1"
}

wait_for "$work/frames.txt" 60
wid=$(osascript -l JavaScript -e '
ObjC.import("CoreGraphics");
function run(argv) {
    const opts = $.kCGWindowListOptionOnScreenOnly | $.kCGWindowListExcludeDesktopElements;
    const wins = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(opts, $.kCGNullWindowID)));
    const win = wins.find((w) => w.kCGWindowOwnerPID === Number(argv[0]) && w.kCGWindowLayer === 0);
    return win ? String(win.kCGWindowNumber) : "";
}' "$pid")
[[ -n $wid ]] || die "could not find the Ghostty window"

mkdir -p "$out"
while read -r name; do
    wait_for "$work/$name.shown" 30
    sleep 1 # let Ghostty draw the frame and its image
    screencapture -x -o -l "$wid" "$work/$name.png"
    # Convert to sRGB: the display's own color profile would name the monitor.
    sips -m "/System/Library/ColorSync/Profiles/sRGB Profile.icc" "$work/$name.png" \
        --out "$out/$name.png" >/dev/null
    touch "$work/$name.captured"
    echo "wrote docs/screenshots/$name.png"
done <"$work/frames.txt"
