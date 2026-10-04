#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Only the newly allocated sparse-file loop device is passed to LVM.
set -euo pipefail
out=$(realpath "${1:-$(dirname "$0")}")
tmp=$(mktemp -d)
loop=
vg=rom
vg_created=0
lvm() { sudo -n env LVM_SYSTEM_DIR="$tmp" LVM_SUPPRESS_FD_WARNINGS=1 "$@" --devices "$loop"; }
cleanup() {
    if [[ -n "$loop" ]]; then
        if [[ "$vg_created" == 1 ]]; then
            lvm vgchange -an "$vg" || { echo "Deactivation failed: retained $tmp and $loop" >&2; return 1; }
        fi
        sudo -n losetup -d "$loop" || return 1
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
# Refuse colliding names rather than touching an existing VG or mapping.
if sudo -n vgs --noheadings -o vg_name | tr -d ' ' | grep -qx "$vg"; then
    echo "VG $vg already exists" >&2; exit 1
fi
cat > "$tmp/lvm.conf" <<'EOF'
devices { use_devicesfile = 0 }
backup { backup = 0 archive = 0 }
activation { udev_sync = 0 udev_rules = 0 monitoring = 0 thin_pool_autoextend_threshold = 100 }
EOF
truncate -s 512M "$tmp/pv.img"
loop=$(sudo -n losetup --find --show "$tmp/pv.img")
lvm pvcreate --metadatasize 128k --dataalignment 1m -y "$loop"
lvm vgcreate -s 4m "$vg" "$loop"
vg_created=1
lvm lvcreate -L 16m -n winhost "$vg"
lvm lvcreate -L 8m -n separator "$vg"
lvm lvextend -L +8m "$vg/winhost"
lvm lvcreate --type thin-pool -L 128m --poolmetadatasize 4m --chunksize 64k --zero n --discards nopassdown -n pool "$vg"
lvm lvcreate -V 64m --thinpool "$vg/pool" -n userdata_1
lvm lvcreate -V 16m --thinpool "$vg/pool" -n metadata_1
lvm lvcreate -s "$vg/userdata_1" -n linux_snapshot
lvm vgchange -ay "$vg"
lvm lvchange -ay -K "$vg/linux_snapshot"
sudo -n env LVM_SUPPRESS_FD_WARNINGS=1 lvm version > "$out/versions.txt"
sudo -n dmsetup targets >> "$out/versions.txt"
capture() {
    local destination=$1 bytes
    mkdir -p "$destination"
    # Verbatim oracle outputs; runtime dev_t values are recorded separately.
    lvm lvs -a -o +seg_pe_ranges,devices "$vg" > "$destination/lvs.txt"
    lvm pvs --units b --nosuffix -o +mda_size "$loop" > "$destination/pvs.txt"
    sudo -n dmsetup table > "$tmp/all-tables"
    # Do not capture any unrelated host mapping.
    grep '^rom-' "$tmp/all-tables" | sort > "$destination/dmsetup.txt"
    printf 'pv0 %s\n' "$(lsblk -dn -o MAJ:MIN "$loop" | tr -d ' ')" > "$destination/devices.txt"
    for name in pool_tdata pool_tmeta pool-tpool; do
        printf '%s %s\n' "$name" "$(sudo -n dmsetup info -c --noheadings --separator : -o major,minor "rom-$name" | tr -d ' ')" >> "$destination/devices.txt"
    done
    # pvs pe_start bounds the metadata-bearing prefix, not the LV data.
    bytes=$(lvm pvs --noheadings --units b --nosuffix -o pe_start "$loop" | tr -d ' ')
    bytes=${bytes%.*}
    lvm vgchange -an "$vg"
    dd if="$tmp/pv.img" bs=1M count="$bytes" iflag=count_bytes status=none | gzip -n > "$destination/pv-prefix.img.gz"
}
capture "$out"
lvm lvchange --zero y "$vg/pool"
lvm lvchange --discards ignore "$vg/pool"
lvm lvchange --errorwhenfull y "$vg/pool"
lvm vgchange -ay "$vg"
lvm lvchange -ay -K "$vg/linux_snapshot"
capture "$out/zero-ignore"
lvm lvchange --discards passdown "$vg/pool"
lvm lvchange --errorwhenfull n "$vg/pool"
lvm vgchange -ay "$vg"
lvm lvchange -ay -K "$vg/linux_snapshot"
capture "$out/zero-passdown"
