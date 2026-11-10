#!/bin/sh
# Runtime wrapper for the installed binaries.
#
# The real executable is installed next to this wrapper with a "-bin" suffix.
# The LD_LIBRARY_PATH entries let a binary built with Nix's glibc find the
# system libwayland-client at runtime; they are harmless for system-linked
# binaries.
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
export LD_LIBRARY_PATH="/lib64:/usr/lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
bin="$dir/$(basename -- "$0")-bin"

daemon=0
for arg in "$@"; do
    if [ "$arg" = "--daemon" ]; then
        daemon=1
    fi
done

# The autostart daemon is supervised: a lost compositor connection or a crash
# exits nonzero, and we start a fresh process so the next Alt+Tab is fast.
# A clean --quit (0) or a termination signal (>=128) ends the supervisor.
if [ "$daemon" -eq 1 ]; then
    while :; do
        "$bin" "$@"
        status=$?
        if [ "$status" -eq 0 ] || [ "$status" -ge 128 ]; then
            exit "$status"
        fi
        echo "cosmic-ext-app-switcher: daemon exited with status $status; restarting" >&2
        sleep 2
    done
fi

exec "$bin" "$@"
