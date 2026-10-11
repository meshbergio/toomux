#!/bin/sh
# Render assets/handover.gif from assets/src/handover.html: one headless
# Chromium screenshot per frame (the page draws itself at #t=<seconds>),
# then one shared palette. Needs chromium, ffmpeg and python3.
# Usage: scripts/render-handover-gif.sh [out.gif] [out.mp4]
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
gif=${1:-"$root/assets/handover.gif"}
mp4=${2:-}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
page="file://$root/assets/src/handover.html"
mkdir "$work/frames"
python3 -c 'for i in range(191): print(i, f"{i*0.08:.2f}")' |
  xargs -P 8 -n 2 sh -c '
    chromium --headless=new --disable-gpu --hide-scrollbars \
      --user-data-dir="'"$work"'/profile-$0" --window-size=1280,800 \
      --screenshot="$(printf "'"$work"'/frames/f%04d.png" "$0")" "'"$page"'#t=$1" >/dev/null 2>&1
    rm -rf "'"$work"'/profile-$0"'
ffmpeg -loglevel error -y -framerate 12.5 -i "$work/frames/f%04d.png" \
  -vf "scale=1120:-1:flags=lanczos,palettegen=max_colors=128:stats_mode=diff" "$work/pal.png"
ffmpeg -loglevel error -y -framerate 12.5 -i "$work/frames/f%04d.png" -i "$work/pal.png" \
  -lavfi "scale=1120:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" \
  -loop 0 "$gif"
if [ -n "$mp4" ]; then
  ffmpeg -loglevel error -y -framerate 12.5 -i "$work/frames/f%04d.png" \
    -c:v libx264 -pix_fmt yuv420p -crf 20 -preset slow -movflags +faststart "$mp4"
fi
echo "$gif"
