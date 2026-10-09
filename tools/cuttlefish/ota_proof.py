#!/usr/bin/env python3
"""Host-only CF OTA prerequisite instrument; no phone selection or lab records.

Each command has an independent log and deadline. Failed prerequisites block
rather than manufacture downstream device evidence. Exit 1 means the Android
proof did not complete, including when a product admission check blocks it.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import logging
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROJECTS = ROOT.parent
BASES = ('boot', 'init_boot', 'vendor_boot')


def decode_stage(data: bytes) -> str:
    if len(data) != 12 or data[:4] != b'\x07\0\0\0' or data[4:8] != b'GBT1':
        raise ValueError('invalid Stage efivar')
    if data[8] > 3 or data[9:] != b'\0\0\0':
        raise ValueError('invalid Stage state/reserved bytes')
    return ('none', 'staging', 'sealed', 'promote')[data[8]]


def boot_slot(arguments: tuple[str, ...], slot: str) -> tuple[str, ...]:
    if slot not in ('a', 'b'):
        raise ValueError('invalid boot slot')
    if sum(item.startswith('--boot_slot=') for item in arguments) != 1:
        raise ValueError('launcher must supply exactly one boot_slot flag')
    return tuple(f'--boot_slot={slot}' if item.startswith('--boot_slot=') else item
                 for item in arguments)


class Proof:
    def __init__(self, output: Path):
        output.mkdir(parents=True, exist_ok=False)
        self.output = output
        self.start = time.monotonic()
        self.rows: list[dict[str, object]] = []

    def command(self, name: str, argv: list[str], *, timeout: int = 1800,
                env: dict[str, str] | None = None) -> bool:
        stem = self.output / f'{len(self.rows) + 1:02d}-{name}'
        stem.with_suffix('.argv.json').write_text(json.dumps(argv) + '\n')
        start = time.monotonic()
        with stem.with_suffix('.log').open('wb') as log:
            try:
                result = subprocess.run(argv, cwd=ROOT, env=env, stdout=log,
                                        stderr=subprocess.STDOUT, timeout=timeout, check=False)
                code = result.returncode
            except (OSError, subprocess.TimeoutExpired) as error:
                log.write(f'{error}\n'.encode())
                code = -1
        text = stem.with_suffix('.log').read_text(errors='replace')
        self.rows.append({'row': name, 'status': 'PASS' if code == 0 else 'FAIL',
                          'exit': code, 'seconds': round(time.monotonic() - start, 2),
                          'command': argv, 'log': str(stem.with_suffix('.log')),
                          'excerpt': text[-2000:]})
        return code == 0

    def observe(self, name: str, action) -> bool:
        stem = self.output / f'{len(self.rows) + 1:02d}-{name}.log'
        start = time.monotonic()
        passed = True
        with stem.open('w') as stream, contextlib.redirect_stdout(stream), contextlib.redirect_stderr(stream):
            try:
                action()
            except Exception:
                # A row is an exception boundary: retain the complete traceback
                # and emit FAIL rather than dropping the rest of the table.
                logger = logging.getLogger("cuttlefish.ota-proof")
                handler = logging.StreamHandler(stream)
                logger.addHandler(handler)
                logger.exception("Proof row failed")
                logger.removeHandler(handler)
                passed = False
        self.rows.append({'row': name, 'status': 'PASS' if passed else 'FAIL',
                          'seconds': round(time.monotonic() - start, 2),
                          'log': str(stem), 'excerpt': stem.read_text()[-4000:]})
        return passed


    def blocked(self, name: str, reason: str) -> None:
        self.rows.append({'row': name, 'status': 'NOT RUN', 'reason': reason})

    def finish(self) -> int:
        elapsed = time.monotonic() - self.start
        print('ROW                             RESULT     SECONDS')
        for row in self.rows:
            print(f"{row['row']:<32} {row['status']:<10} {row.get('seconds', '-')}\n"
                  f"  {row.get('excerpt', row.get('reason', ''))}")
        print(f'Total wall time: {elapsed:.2f}s')
        (self.output / 'results.json').write_text(json.dumps(
            {'wall_seconds': elapsed, 'rows': self.rows}, indent=2) + '\n')
        return int(any(row['status'] != 'PASS' for row in self.rows))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--artifacts', type=Path, default=PROJECTS / '.work/cf-ota-proof/artifacts')
    parser.add_argument('--modules', type=Path, default=PROJECTS / '.work/cf-ota-proof/modules')
    parser.add_argument('--kmi-out', type=Path, default=PROJECTS / '.work/thinpool-proof/.work/cf-out19')
    parser.add_argument('--ndk', type=Path, default=PROJECTS / 'gobbl/.cache/toolchains/android-ndk-r29')
    parser.add_argument('--build', action='store_true')
    parser.add_argument('--payload', type=Path, default=PROJECTS / '.work/cf-ota-proof/payload')
    parser.add_argument('--fixture', type=Path, default=PROJECTS / '.work/thinpool-proof/.work/cf-vab/state/fixture')
    parser.add_argument('--lab', type=Path, default=PROJECTS / 'gobbl-lab')
    parser.add_argument('--base', type=Path, default=PROJECTS / 'gobbl-cf/base-16102939')
    parser.add_argument('--busybox', type=Path, default=PROJECTS / '.work/cf-payload-192ddc27/bin/busybox')
    parser.add_argument('--avb-key', type=Path, default=PROJECTS / '.work/cf-payload-192ddc27/avb-key.pem')
    args = parser.parse_args(argv)
    proof = Proof(args.output)
    env = os.environ.copy()
    env['PATH'] = str(Path.home() / '.cargo/bin') + ':' + env['PATH']
    env['ESU_NDK'] = str(args.ndk)
    built = True
    if args.build:
        built = proof.command('x86_64-userspace', ['bash', str(ROOT / 'tools/cuttlefish/build-ota-userspace.sh'),
                                          str(args.artifacts)], env=env)
    verified = proof.command('CF-KMI-admission', ['python3', str(ROOT / 'scripts/kmi_modules.py'),
        'verify', '--kmi-out', str(args.kmi_out),
        *(item for name in ('kernelesp', 'thin', 'gpt', 'efivarfs', 'efivar_store')
          for item in ('--module', str(args.modules / f'{name}.ko')))])
    missing = [name for name in ('esuinit', 'esud', 'host-esud', 'thin-activate', 'fw-views',
                                'ota-stage', 'esu-bootctl', 'lvm')
               if not (args.artifacts / name).is_file()]
    reason = ('missing built artifacts: ' + ', '.join(missing)) if missing else ''
    if not built:
        reason = 'artifact build failed; see x86_64-userspace log. ' + reason
    if not verified:
        reason = 'CF module compatibility admission failed; see CF-KMI-admission log. ' + reason
    if reason:
        for name in ('payload-boot-patch', 'VG-reserve-ROM2-install', 'rdinit-idle-boot',
                     'self-OTA-staging-sealed', 'target-boot-merge-promote', 'second-idle-boot',
                     'missing-KMI-denial', 'cancel-staging', 'kill-promote-resume'):
            proof.blocked(name, reason)
        return proof.finish()
    assembly = ['python3', str(ROOT / 'tools/cuttlefish/assemble.py'),
                '--stock-init-boot', str(args.base / 'init_boot.img'),
                '--avbtool', '/usr/bin/avbtool', '--avb-key', str(args.avb_key),
                '--busybox', str(args.busybox), '--lvm-conf', str(ROOT / 'tools/lvm2/lvm.conf'),
                '--kmi-out', str(args.kmi_out), '--rom-id', 'rom2',
                '--metadata-filesystem', 'ext4', '--esp-size-mib', '512',
                '--output-dir', str(args.payload), '--overwrite']
    for flag, name in [('esuinit', 'esuinit'), ('esud', 'esud'), ('host-esud', 'host-esud'),
                       ('thin-activate', 'thin-activate'), ('fw-views', 'fw-views'),
                       ('lvm', 'lvm'), ('ota-stage', 'ota-stage'), ('boot-hal', 'esu-bootctl')]:
        assembly.extend([f'--{flag}', str(args.artifacts / name)])
    for flag, name in [('core-module', 'kernelesp'), ('thin-module', 'thin'),
                       ('gpt-module', 'gpt'), ('efivarfs-module', 'efivarfs'),
                       ('efivar-store-module', 'efivar_store')]:
        assembly.extend([f'--{flag}', str(args.modules / f'{name}.ko')])
    if not proof.command('payload-boot-patch', assembly):
        for name in ('VG-reserve-ROM2-install', 'rdinit-idle-boot', 'missing-KMI-denial',
                     'cancel-staging', 'self-OTA-staging-sealed', 'target-staged-boot',
                     'kill-promote-resume', 'ESP-hash-LV-removal', 'second-idle-boot'):
            proof.blocked(name, 'payload producer failed; see payload-boot-patch log')
        return proof.finish()
    sys.path.insert(0, str(args.lab))
    sys.path.insert(0, str(ROOT))
    from tools.cuttlefish.ota_lane import execute
    execute(proof, args)
    return proof.finish()


if __name__ == '__main__':
    raise SystemExit(main())
