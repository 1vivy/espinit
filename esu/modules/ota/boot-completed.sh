#!/bin/sh
# Post the boot HAL's refusal to activate an update as an Android notification,
# then clear it. The HAL writes the receipt when it denies a transaction and
# also tries the notification itself; when that exec was refused under
# enforcing, this is the delivery path that remains. Best effort: a missing
# receipt, a refused `cmd` or a read-only ESP must not fail the stage.
set -u

receipt=/debug_ramdisk/esp/esu/receipts/ota-denied.txt
[ -f "$receipt" ] || exit 0

reason=$(cat "$receipt")
if cmd notification post -S bigtext -t "esu OTA" esu.ota "$reason"; then
    rm -f "$receipt" 2>/dev/null || true
fi
exit 0
