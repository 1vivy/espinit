#!/system/bin/sh
# Equivalent manual init-root invocation; esud Early calls the binary directly.
# No arguments, commands, configuration paths, or environment-selected targets.
set -eu
[ "$#" -eq 0 ]
exec /metadata/esu/modules/tiny-espsu/tiny-espsu
