"""CF-only guest transaction probes; every adb command names loopback serial."""
from __future__ import annotations

import subprocess
import time
from pathlib import Path

ESP = '/debug_ramdisk/esp'
STAGE = '/dev/efivars/Stage-rom2-7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b'
BASES = ('boot', 'init_boot', 'vendor_boot')


def shell(session, command: str) -> str:
    if not session.serial.startswith('127.0.0.1:'):
        raise ValueError('CF guest probes require a loopback adb serial')
    return session.shell('ota-proof-guest', 'set -eu\n' + command)


def stage(session) -> int:
    from .ota_proof import decode_stage
    command = ['adb', '-s', session.serial, 'exec-out', 'cat', STAGE]
    if not session.serial.startswith('127.0.0.1:'):
        raise ValueError('Stage probes require the CF loopback serial')
    wire = subprocess.check_output(command, timeout=15)
    try:
        name = decode_stage(wire)
    except ValueError as error:
        raise ValueError(f'{error}: wire={wire.hex()} text={wire!r}') from error
    return ('none', 'staging', 'sealed', 'promote').index(name)

def wait_stage(session, expected: int, timeout: int = 120) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        current = stage(session)
        print(f'Stage={current}', flush=True)
        if current == expected:
            return
        time.sleep(0.2)
    raise ValueError(f'Stage did not reach {expected}')


def idle(session) -> None:
    print(shell(session, '''cat /proc/cmdline
case "$(cat /proc/cmdline)" in *rdinit=/esuinit*) ;; *) exit 1;; esac
getenforce
test "$(getenforce)" = Permissive
readlink /proc/1/exe
test "$(readlink /proc/1/exe)" = /system/bin/init
tr '\\0' '\\n' < /proc/1/environ
ls -l /dev/block/esd/pv/a /dev/block/esd/lv /dev/block/esd/by-name
test -b /dev/block/esd/pv/a
test -b /dev/block/esd/lv/userdata_2
/debug_ramdisk/esu/bin/lvm version --config "$(cat /debug_ramdisk/esu/bin/lvm.conf)"
service list | grep 'android.hardware.boot.IBootControl/default'
getprop init.svc.esu.bootctl
'''))
    if stage(session) != 0:
        raise ValueError('idle boot has nonzero Stage')
    for base in BASES:
        print(shell(session, f'''dmctl table rom2-ota-{base}
readlink -f /dev/block/by-name/{base}_b
node=$(basename "$(readlink -f /dev/block/by-name/{base}_b)")
disk=$(readlink -f /sys/class/block/$node/..)
case "$disk" in *esu-gpt*) ;; *) echo "not an esu projection: $disk"; exit 1;; esac
if dd if=/dev/block/by-name/{base}_b of=/dev/null bs=4096 count=1; then
    echo "inactive switch unexpectedly readable"; exit 1
fi
cat {ESP}/esu/roms/rom2.toml
'''))
        table = shell(session, f'dmctl table rom2-ota-{base}')
        if 'error' not in table:
            raise ValueError(f'{base} idle switch is not error')


def apply(session, fixture: Path, *, cancel: bool = False, denied: bool = False) -> None:
    from lab.android.command_log import Exec
    for name in ('payload.bin', 'payload_properties.txt'):
        session.run(Exec('ota-push', ('adb', '-s', session.serial, 'push',
                        str(fixture / 'self-ota' / name), f'/data/ota_package/{name}')))
    command = ['adb', '-s', session.serial, 'shell',
               ('update_engine_client --update --follow --payload=file:///data/ota_package/payload.bin '
                '--headers="$(cat /data/ota_package/payload_properties.txt)"')]
    print('$', ' '.join(command), flush=True)
    output = session.evidence / 'update-follow.log'
    observed = {stage(session)}
    with output.open('wb') as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 600
        try:
            while process.poll() is None:
                current = stage(session)
                observed.add(current)
                print(f'Stage={current}', flush=True)
                if cancel and current == 1:
                    print(shell(session, 'update_engine_client --cancel; update_engine_client --reset_status'))
                    break
                if time.monotonic() >= deadline:
                    print(output.read_text(errors='replace')[-4000:], flush=True)
                    print(shell(session, 'logcat -d -b all -s update_engine esu-bootctl')[-12000:], flush=True)
                    print(shell(session, 'cat /dev/esu-bootctl.log'))
                    raise TimeoutError('self OTA did not finish in 600 seconds')
                time.sleep(0.1)
            process.wait(timeout=30)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    text = output.read_text(errors='replace')
    print(text[-4000:], flush=True)
    logs = shell(session, 'logcat -d -b all -s update_engine esu-bootctl')
    print(logs[-12000:], flush=True)
    print(shell(session, 'cat /dev/esu-bootctl.log'))
    if cancel:
        wait_stage(session, 0)
        absent_volumes(session)
    elif denied:
        if 'kPostinstallRunnerError' not in text + logs:
            raise ValueError('missing updater postinstall denial')
        print(shell(session, f'cat {ESP}/esu/receipts/ota-denied.txt'))
        if stage(session) != 1:
            raise ValueError('denial changed transaction state')
        print(shell(session, 'update_engine_client --reset_status; bootctl set-active-boot-slot 0'))
        wait_stage(session, 0)
        absent_volumes(session)
    else:
        observed.add(stage(session))
        if process.returncode != 0 or not {0, 1, 2}.issubset(observed):
            raise ValueError(f'OTA failed or Stage sequence incomplete: exit={process.returncode}, states={observed}')
        if 'Hash of the target partition' not in logs and 'All post-install commands succeeded' not in logs:
            raise ValueError('updater target verification evidence missing')
        print(shell(session, f'ls -l {ESP}/rom/rom2/esu.stage.cpio; '
                    'for p in /sys/block/dm-*/dm/name; do cat "$p"; done'))
        for base in BASES:
            print(shell(session, f'dmctl table rom-rom2--stage--{base}; '
                        f'sha256sum /dev/block/esd/lv/rom2-stage-{base}'))


def absent_volumes(session) -> None:
    names = shell(session, 'for p in /sys/block/dm-*/dm/name; do cat "$p"; done')
    print(names, flush=True)
    if 'rom-rom2--stage--' in names:
        print(shell(session, 'cat /dev/esu-bootctl.log'))
        print(shell(session, 'for p in /sys/block/dm-*/dm; do echo "$p"; cat "$p/name" "$p/uuid"; done'))
        print(shell(session, f'{ESP}/esu/bin/lvm lvs --config "$(cat {ESP}/esu/bin/lvm.conf)" --noheadings -o lv_name rom'))
        raise ValueError('staging LVs remain')


def missing_kmi(session, fixture: Path) -> None:
    root = f'{ESP}/esu/kmi'
    print(shell(session, f'mount -o remount,rw {ESP}; mv {root}/android16-6.12-6 {root}/denied-set; mount -o remount,ro {ESP}'))
    try:
        apply(session, fixture, denied=True)
    finally:
        print(shell(session, f'mount -o remount,rw {ESP}; mv {root}/denied-set {root}/android16-6.12-6; mount -o remount,ro {ESP}'))


def promote(session, hashes: dict[str, str], *, kill: bool) -> None:
    print(shell(session, 'update_engine_client --merge'))
    deadline = time.monotonic() + 300
    observed = set()
    killed = False
    while time.monotonic() < deadline:
        current = stage(session)
        observed.add(current)
        print(f'Stage={current}', flush=True)
        if current == 3 and kill and not killed:
            print(shell(session, 'kill -9 $(pidof esu-bootctl)'))
            killed = True
        if current == 0:
            break
        time.sleep(0.05)
    if 0 not in observed:
        print(shell(session, 'cat /dev/esu-bootctl.log'))
        print(shell(session, 'logcat -d -b all -s update_engine esu-bootctl')[-12000:], flush=True)
        raise ValueError(f'promote did not converge: states={observed}')
    if kill and not killed:
        raise ValueError('promotion finished before SIGKILL; resume was not exercised')
    if 3 not in observed:
        raise ValueError('Promote state was not observed')
    absent_volumes(session)
    for filename, expected in hashes.items():
        actual = shell(session, f'sha256sum {ESP}/rom/rom2/{filename}').split()[0]
        if actual != expected:
            raise ValueError(f'promoted {filename} hash differs: {actual} != {expected}')
        print(f'{filename} sha256={actual}', flush=True)
    print(shell(session, f'test ! -e {ESP}/rom/rom2/esu.stage.cpio'))
