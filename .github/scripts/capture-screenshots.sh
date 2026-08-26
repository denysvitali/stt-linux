#!/usr/bin/env bash
# Render the overlay demo under a headless wlroots compositor and capture
# frames of it with grim. Writes into the repo:
#
#   assets/overlay-meter.png  — listening, no words yet: the live waveform
#   assets/overlay-live.png   — a transcript arriving word by word
#
# The runner has no GPU, no seat and no input devices, so sway runs on the
# headless backend with the software (pixman) renderer; screencopy and the
# shm buffers the overlay uses work there exactly as on a real session.
set -euo pipefail

cd "$(dirname "$0")/../.."

export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman
export WLR_LIBINPUT_NO_DEVICES=1

# A plain colour behind the overlay: dark glass on black would be invisible.
cat > "$XDG_RUNTIME_DIR/sway-config" <<'EOF'
output * bg #2e3440 solid_color
EOF

sway --headless --config "$XDG_RUNTIME_DIR/sway-config" \
  > "$XDG_RUNTIME_DIR/sway.log" 2>&1 &
SWAY_PID=$!
cleanup() {
  kill "$SWAY_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Wait for the compositor's Wayland socket to appear.
sock=""
for _ in $(seq 1 50); do
  sock="$(ls "$XDG_RUNTIME_DIR" | grep -m1 '^wayland-[0-9]*$' || true)"
  [ -n "$sock" ] && break
  sleep 0.2
done
if [ -z "$sock" ]; then
  echo "sway never opened a Wayland socket; its log follows" >&2
  cat "$XDG_RUNTIME_DIR/sway.log" >&2
  exit 1
fi
export WAYLAND_DISPLAY="$sock"
echo "compositor up on $WAYLAND_DISPLAY"

./target/release/examples/demo &
DEMO_PID=$!
kill_demo() {
  kill "$DEMO_PID" 2>/dev/null || true
}
trap 'kill_demo; cleanup' EXIT

# The demo sweeps the meter for ~1.5 s, grows a ten-word transcript over
# ~4.8 s, then holds a dimmed "transcribing" frame for ~1.2 s before hiding.
sleep 0.85
grim assets/overlay-meter.png
sleep 3.55
grim assets/overlay-live.png

wait "$DEMO_PID"
echo "captured:"
ls -la assets/overlay-*.png
