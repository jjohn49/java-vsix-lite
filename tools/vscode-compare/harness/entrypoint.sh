#!/bin/sh
# Container entrypoint for ONE flavor ($JVL_COMPARE_FLAVOR). run.sh starts
# this image once per flavor so the two never share a container.
#
# Xvfb is started by hand rather than via `xvfb-run`: that wrapper's
# readiness handshake (a SIGUSR1 the backgrounded Xvfb sends its parent
# shell once ready) does not reliably propagate through this image's process
# tree, leaving it hung forever waiting for a signal that never arrives.
# Polling for the X11 socket file is a portable readiness check that works
# natively and under emulation alike.
set -e
Xvfb :99 -screen 0 1280x1024x24 -nolisten tcp &
for i in $(seq 1 30); do
  [ -e /tmp/.X11-unix/X99 ] && break
  sleep 1
done
export DISPLAY=:99

# No sampling starts here on purpose: runFlavor.js launches
# sampleStats.sh only after the extension is installed and the settle
# period has elapsed, so container creation, Xvfb boot, and the extension
# install stay outside the measured window.
exec node /work/tools/vscode-compare/harness/runFlavor.js
