#!/bin/sh
# Recovery boots the same switch devices: recovery has no staging transaction,
# so every declared base resolves to an error target of its image's size and a
# ROM 1 exits 0 immediately.
set -eu
exec ota-stage
