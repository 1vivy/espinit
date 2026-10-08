#!/bin/sh
# Publish the /dev/block/esd device-name tree and the LVM command names the boot
# HAL and esud address it with. Runs at esud's early stage, before any
# `early_hal` service, so the HAL finds `lvm`, `lvcreate`, ... and the esd nodes
# already in place when it prepares or promotes a staging set.
set -eu

bin=/debug_ramdisk/esu/bin
for command in lvcreate lvremove lvchange lvs vgs pvs; do
    ln -sf lvm "$bin/$command"
done

exec esud esd refresh
