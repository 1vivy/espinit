"""One disposable stock-U-Boot CF lane, no Surfacer/GBL claim or lab record."""
from __future__ import annotations

import dataclasses
import os
import re
import socket
import subprocess
from pathlib import Path
from types import SimpleNamespace

from . import ota_disk, ota_guest
from .ota_proof import boot_slot


def private_adb_environment(port: int) -> dict[str, str]:
    return {'ADB_SERVER_PORT': str(port), 'ANDROID_ADB_SERVER_PORT': str(port),
            'ADB_SERVER_SOCKET': f'tcp:{port}'}



def execute(proof, args) -> None:
    from lab.android.command_log import Exec
    from lab.android.cuttlefish import BuiltImage, Session
    from lab.android.cuttlefish_runtime import (
        require_resume_safe,
        runtime_path,
        single_runtime_file,
    )
    from lab.android.esu_payload import (
        adapt_assembly,
        custom_image,
        extract_failure_receipt,
        install_esp,
        load_payload,
    )
    from lab.android.paused_assembly import assemble_paused
    from lab.android.pins import PINS, load

    rows = ['VG-reserve-ROM2-install', 'rdinit-idle-boot', 'missing-KMI-denial',
            'cancel-staging', 'self-OTA-staging-sealed', 'target-staged-boot',
            'kill-promote-resume', 'ESP-hash-LV-removal', 'second-idle-boot']
    running = subprocess.check_output(['docker', 'ps', '--format', '{{.Names}} {{.Image}}'], text=True)
    conflicts = [line for line in running.splitlines() if line.startswith('gbl-shim-')]
    if conflicts:
        for row in rows:
            proof.blocked(row, 'existing CF container: ' + '; '.join(conflicts))
        return
    payload = load_payload(args.payload)
    pin = dataclasses.replace(load(PINS), base=args.base.resolve())
    evidence = proof.output / 'commands'
    evidence.mkdir()
    session = Session(args.lab, proof.output / 'state', evidence, pin,
                      BuiltImage(payload.init_boot, payload.init_boot_sha256), proof.output.name)
    live = subprocess.run(['pgrep', '-a', 'crosvm'], capture_output=True, text=True, check=False)
    occupied = {int(cid) for cid in re.findall(r'--vsock=(?:cid=)?(\d+)', live.stdout)}
    if session.vsock_cid in occupied:
        for row in rows:
            proof.blocked(row, f'vsock CID {session.vsock_cid} is already in use')
        return
    print(f'CF owned container={session.container} vsock_cid={session.vsock_cid}; occupied={sorted(occupied)}')
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        adb_port = reservation.getsockname()[1]
    private_env = private_adb_environment(adb_port)
    previous_env = {key: os.environ.get(key) for key in private_env}
    os.environ.update(private_env)
    print(f'CF private adb server port={adb_port}')
    bootconfig = 'androidboot.esu.rom=rom2'
    kernel = 'rdinit=/esuinit androidboot.selinux=permissive'
    adapted = None
    def disk() -> None:
        nonlocal adapted
        session.prepare()
        custom = custom_image(pin)
        install_esp(payload, session.state / 'images', custom)
        # The managed PV is initialized offline, never on the cached image.
        source = session.state / 'images/userdata.img'
        size = source.stat().st_size
        with source.open('wb') as stream:
            stream.truncate(size)
        assemble_paused(session, kernel, '')
        adapted = adapt_assembly(session, payload, custom, proof.output)
        ota_disk.prepare(session, payload, adapted, args.lab.parent, args.artifacts)
        session.run(Exec('touch-managed-composite', ('docker', 'exec', session.container,
                         'touch', runtime_path(session, adapted.composite))))
        require_resume_safe(session)
    def environment(expected: str) -> None:
        text = session.kernel_log().read_text(errors='replace')
        values = re.findall(r'CF-ESU_STAGE=([^\r\n]*)', text)
        if not values or values[-1].strip() != expected:
            raise ValueError(f'PID1 ESU_STAGE evidence differs: {values[-1:]}, expected {expected!r}')
        print(f'Observed PID1 ESU_STAGE={values[-1]!r}')
    def source_boot() -> None:
        session.stop()
        require_resume_safe(session)
        misc = session.kernel_log().parent.parent / 'misc'
        with misc.open('rb') as stream:
            print(f'BCB command before cvd start: {stream.read(32)!r}')
        command = session.start_command(kernel, bootconfig, resume=False)
        session.created = True
        session.run_guest(Exec(command.name, tuple('--guest_enforce_security=false'
                          if item == '--guest_enforce_security=true' else item for item in command.argv)),
                          scan_output=True)
        session.waits.start_vmm_watch()
        session.attach('device')
        session.wait_boot_completed()
        session.root_adb()
        print(ota_guest.shell(session, 'mkdir -p /data/ota_package; restorecon -RF /data/ota_package'))
        ota_guest.idle(session)
        environment('')
    def restart(slot: str) -> None:
        session.waits.stop_vmm_watch()
        session.stop()
        # CVD compares metadata/misc too: guest writes must not regenerate the
        # managed GPT. Refuse changed immutable inputs first; refresh this
        # composite, then its existing overlay last so CVD preserves its data.
        require_resume_safe(session)
        assert adapted is not None
        session.run(Exec('touch-managed-restart', ('docker', 'exec', session.container,
                         'touch', runtime_path(session, adapted.composite))))
        overlay = single_runtime_file(session.state / 'runtime', 'overlay.img')
        if overlay is None:
            raise ValueError('owned OS overlay is absent before restart')
        session.run(Exec('touch-owned-overlay', ('docker', 'exec', session.container,
                         'touch', runtime_path(session, overlay))))
        require_resume_safe(session)
        command = session.start_command(kernel, bootconfig, resume=False)
        print(f'CF U-Boot emulation: boot_slot={slot}; GBS1 is not read by U-Boot')
        arguments = boot_slot(command.argv, slot)
        arguments = tuple('--guest_enforce_security=false' if item == '--guest_enforce_security=true'
                          else item for item in arguments)
        session.created = True
        session.run_guest(Exec('cvd-emulated-slot', arguments), scan_output=True)
        session.waits.start_vmm_watch()
        session.attach('device')
        session.wait_boot_completed()
        session.root_adb()
    hashes = {}
    def apply() -> None:
        ota_guest.apply(session, args.fixture)
        for base in ota_guest.BASES:
            hashes[f'{base}.img'] = ota_guest.shell(session, f'sha256sum /dev/block/esd/lv/rom2-stage-{base}').split()[0]
        hashes['esu.cpio'] = ota_guest.shell(
            session, f'sha256sum {ota_guest.ESP}/rom/rom2/esu.stage.cpio').split()[0]
    def target() -> None:
        restart('b')
        print(ota_guest.shell(session, '''getprop ro.boot.slot_suffix
cat /proc/cmdline
tr '\\0' '\\n' < /proc/1/environ
'''))
        if ota_guest.shell(session, 'getprop ro.boot.slot_suffix').strip() != '_b':
            raise ValueError('CF target letter did not boot')
        environment('b:ro')
        for base in ota_guest.BASES:
            table = ota_guest.shell(session, f'dmctl table rom2-ota-{base}')
            print(table)
            if 'linear' not in table:
                raise ValueError(f'{base} target switch is not linear')
            print(ota_guest.shell(session, f'test "$(blockdev --getro /dev/block/mapper/rom2-ota-{base})" = 1'))
    def second() -> None:
        restart('b')
        if ota_guest.stage(session) != 0:
            raise ValueError('second boot is not idle')
        print(ota_guest.shell(session, "tr '\\0' '\\n' < /proc/1/environ"))
        ota_guest.absent_volumes(session)
        environment('')
        for base in ota_guest.BASES:
            table = ota_guest.shell(session, f'dmctl table rom2-ota-{base}')
            print(table)
            if ': error' not in table:
                raise ValueError(f'{base} second-idle switch is not an error target')
    actions = [disk, source_boot,
               lambda: ota_guest.missing_kmi(session, args.fixture),
               lambda: ota_guest.apply(session, args.fixture, cancel=True),
               apply, target, lambda: ota_guest.promote(session, hashes, kill=True),
               lambda: ota_guest.absent_volumes(session), second]
    try:
        subprocess.run(['adb', '-P', str(adb_port), 'start-server'], check=True, timeout=15)
        for index, (row, action) in enumerate(zip(rows, actions, strict=True)):
            if not proof.observe(row, action):
                if adapted is not None:
                    # Offline diagnostics must not rescan the already-observed
                    # fatal marker and abort before reading the overlay.
                    offline = SimpleNamespace(state=session.state, run=session.journal.run)
                    print(f'Failure receipt: {extract_failure_receipt(offline, adapted, proof.output)}')
                kernel_log = session.kernel_log()
                diagnosis = kernel_log.read_text(errors='replace')
                lines = diagnosis.splitlines()
                failures = []
                for offset, line in enumerate(lines):
                    if any(marker in line for marker in (
                            'esu early boot failed:', 'esud fatal:', 'boot-hal transaction:',
                            'boot-hal storage unavailable;', 'Failed to set encryption policy')):
                        # stderr formatting can split one error across kmsg records.
                        failures.append(' '.join(lines[offset:offset + 3])
                                        if 'boot-hal ' in line else line)
                row_log = Path(proof.rows[-1]['log'])
                row_lines = row_log.read_text(errors='replace').splitlines()
                row_errors = [line for line in row_lines
                              if line.startswith(('ValueError:', 'ControlError:', 'WaitFailure:', 'TimeoutError:'))]
                backend_errors = [line for line in row_lines
                                  if line.startswith('boot-hal transaction:')]
                detail = (failures[-1] if failures else backend_errors[-1] if backend_errors
                          else row_errors[-1] if row_errors else 'guest row failed')
                reason = f'{detail}; evidence: {row_log}; kernel: {kernel_log}'
                proof.rows[-1]['diagnosis'] = reason
                proof.rows[-1]['excerpt'] = reason + '\n' + str(proof.rows[-1]['excerpt'])
                for blocked in rows[index + 1:]:
                    proof.blocked(blocked, f'prerequisite {row} failed: {reason}')
                break
    finally:
        proof.observe('CF-cleanup', lambda: print(session.cleanup()))
        subprocess.run(['adb', '-P', str(adb_port), 'kill-server'], check=False, timeout=15)
        for key, value in previous_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
