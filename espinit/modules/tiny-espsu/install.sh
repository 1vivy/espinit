#!/system/bin/sh
# Equivalent manual init-root invocation; espinitd Early calls the binary directly.
# No arguments, commands, configuration paths, or environment-selected targets.
set -eu
[ "$#" -eq 0 ]
exec /metadata/espinit/modules/tiny-espsu/tiny-espsu
