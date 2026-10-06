#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Compile the real backend, then compare its writes with the Rust CLI."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
REPO = Path(os.environ.get('BDSVARS_REPO', '/home/vivy/Projects/efisp-projects/gbl-bds-rs'))
FIXTURE = Path(os.environ.get('BDSVARS_FIXTURE', '/home/vivy/Projects/efisp-projects/gbl-bds-lab/records/20261006T040000Z-phone-recovery-reads/evidence/bdsvars-1MiB.bin'))
GUID = '7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b'

class BackendContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not (REPO / 'tools/bdsvars/Cargo.toml').is_file():
            raise unittest.SkipTest(f'bdsvars CLI source absent in BDSVARS_REPO={REPO}')
        if not FIXTURE.is_file():
            raise unittest.SkipTest(f'recovery fixture absent: {FIXTURE}')
        cls.build = tempfile.TemporaryDirectory(prefix='esu-efivarfs-host-')
        cls.addClassCleanup(cls.build.cleanup)
        cls.host = Path(cls.build.name) / 'host'
        subprocess.run([os.environ.get('CC', 'cc'), '-std=gnu11', '-Wall', '-Wextra', '-Werror', '-I' + str(ROOT / 'test/include'), str(ROOT / 'test/host.c'), '-o', str(cls.host)], check=True)
        cls.env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / 'target-bdsvars'))
        cls.env['PATH'] = str(Path.home() / '.cargo/bin') + ':' + cls.env['PATH']
        subprocess.run(['cargo', 'build', '--locked', '-p', 'bdsvars'], cwd=REPO, env=cls.env, check=True)
        cls.cli = ROOT / 'target-bdsvars/debug/bdsvars'


    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.c = Path(self.tmp.name) / 'c.bin'
        self.rust = Path(self.tmp.name) / 'rust.bin'
        shutil.copyfile(FIXTURE, self.c)
        shutil.copyfile(FIXTURE, self.rust)
        self.data = Path(self.tmp.name) / 'data'
        self.data.write_bytes(b'esu-contract\x00\xff')

    def host_run(self, *args):
        return subprocess.run([str(self.host), str(self.c), *map(str, args)], text=True, capture_output=True, check=True)

    def rust_run(self, *args):
        return subprocess.run([str(self.cli), '--image', str(self.rust), *map(str, args)], text=True, capture_output=True, check=True)

    def test_enumeration_and_values(self):
        self.assertEqual(self.host_run('list').stdout, self.rust_run('list').stdout)

    def test_set_update_delete_bytes_and_phases(self):
        for name in ['EsuContractProbe', 'Slot-rom1']:
            before = self.c.read_bytes()
            trace = self.host_run('set', name, '7', self.data).stderr.splitlines()
            self.rust_run('set', '--name', name, '--guid', GUID, '--attributes', '0x7', '--data-file', self.data)
            self.assertEqual(self.c.read_bytes(), self.rust.read_bytes())
            self.assertEqual([x for x in trace if x.startswith('flush')], [f'flush {n}' for n in range(1, 7)])
            groups = [[]]
            for line in trace:
                if line.startswith('flush'): groups.append([])
                else: groups[-1].append(line.split())
            self.assertEqual(len(groups[1]), 1)
            pos = int(groups[1][0][1])
            self.assertEqual(groups[1][0][2], '60')
            self.assertEqual(before[pos:pos+60], b'\xff' * 60)
            self.assertEqual(groups[2], [['write', str(pos+2), '1', '7f']])
            self.assertEqual(groups[3][0][1], str(pos+60))
            self.assertEqual(groups[4], [['write', str(pos+2), '1', '3f']])
            for change in groups[0]: self.assertEqual(change[2:], ['1', '3e'])
            for change in groups[5]: self.assertEqual(change[2:], ['1', '3c'])
            delete = self.host_run('delete', name)
            self.rust_run('delete', '--name', name, '--guid', GUID)
            self.assertEqual(self.c.read_bytes(), self.rust.read_bytes())
            self.assertEqual(delete.stderr.count('flush'), 1)

    def test_attributes_rejected_without_writes(self):
        before = self.c.read_bytes()
        for attr in ['0', '3', '15', '128']:
            result = self.host_run('set', 'InvalidAttrs', attr, self.data)
            self.assertEqual(result.stdout, 'status 2\n')
            self.assertEqual(result.stderr, '')
            self.assertEqual(self.c.read_bytes(), before)

    def test_full_store_no_reclaim(self):
        _, capacity, remaining, maximum = map(int, self.host_run('query').stdout.split())
        self.assertGreater(capacity, remaining)
        self.assertEqual(maximum, capacity - 64)
        # A two-code-unit name has six name bytes; make the record consume all free bytes.
        self.data.write_bytes(b'F' * (remaining - 60 - 6))
        self.assertEqual(self.host_run('set', 'FF', '7', self.data).stdout, 'status 0\n')
        self.assertEqual(int(self.host_run('query').stdout.split()[2]), 0)
        before = self.c.read_bytes()
        result = self.host_run('set', 'Full', '7', self.data)
        self.assertEqual(result.stdout, 'status 9\n')
        self.assertEqual(result.stderr, '')
        self.assertEqual(before, self.c.read_bytes())

    def test_readback_failure_reloads(self):
        result = self.host_run('corrupt', 'Slot-rom1', '7', self.data)
        original = self.rust_run('get', '--name', 'Slot-rom1', '--guid', GUID).stdout
        hex_data = original.split('data: ')[1].strip()
        self.assertEqual(result.stdout, f'status 7\nreload 0 7 {hex_data}\n')
        # First phase only transitioned the predecessor: it must remain visible.
        self.assertEqual(self.host_run('list').stdout, self.rust_run('list').stdout)

    def test_invalid_geometry_and_required_device(self):
        result = subprocess.run([str(self.host), str(self.c), 'missing-dev'], capture_output=True, text=True)
        self.assertEqual(result.stdout, 'init -22\n')
        for offset in [16, 40, 50, 72, 92]:
            image = bytearray(FIXTURE.read_bytes()); image[offset] ^= 1
            self.c.write_bytes(image)
            result = subprocess.run([str(self.host), str(self.c), 'list'], capture_output=True, text=True)
            self.assertEqual(result.stdout, 'init -22\n')

if __name__ == '__main__':
    unittest.main(verbosity=2)
