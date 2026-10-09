"""Pure CF proof helpers; never starts a VM or contacts adb."""
import struct
import tempfile
import unittest
import zlib
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import tomllib

from . import ota_guest
from .ota_disk import augment_rom, firmware_bases, split_frp_gpt
from .ota_guest import STAGE
from .ota_lane import private_adb_environment
from .ota_proof import Proof, boot_slot, decode_stage


class ProofHelpers(unittest.TestCase):
    def test_stage_vectors(self):
        for number, name in enumerate(('none', 'staging', 'sealed', 'promote')):
            wire = b'\x07\0\0\0GBT1' + bytes((number, 0, 0, 0))
            self.assertEqual(decode_stage(wire), name)
        for wire in (b'', b'\x07\0\0\0GBT1\x04\0\0\0',
                     b'\x07\0\0\0GBT1\0\0\0\x01', b'\x03\0\0\0GBT1\0\0\0\0'):
            with self.assertRaises(ValueError):
                decode_stage(wire)

    def test_explicit_uboot_emulation(self):
        self.assertEqual(boot_slot(('cvd', 'start', '--boot_slot=a', '--resume=true'), 'b'),
                         ('cvd', 'start', '--boot_slot=b', '--resume=true'))
        with self.assertRaises(ValueError):
            boot_slot((), 'c')
        with self.assertRaises(ValueError):
            boot_slot((), 'b')

    def test_firmware_inventory_uses_actual_pairs(self):
        names = {'boot_a', 'boot_b', 'vbmeta_a', 'vbmeta_b', 'system_a',
                 'system_b', 'vendor_a', 'vendor_b', 'lonely_a'}
        self.assertEqual(firmware_bases(names), ['system', 'vendor'])
        with self.assertRaises(ValueError):
            firmware_bases({'boot_a', 'boot_b'})

    def test_augment_preserves_roles_and_adds_passthrough(self):
        generated = ('schema_version=1\nid="rom2"\n[[partitions]]\n'
                     'name="boot_a"\nbackend="rom-image:boot"\nread_only=false\n')
        physical = ('[[partitions]]\nname="boot_a"\nbackend="/dev/block/by-name/boot_a"\n'
                    '[[partitions]]\nname="bdsvars"\nbackend="/dev/block/by-name/frp"\n'
                    '[[partitions]]\nname="vbmeta_a"\nbackend="/dev/block/by-name/vbmeta_a"\n')
        partitions = tomllib.loads(augment_rom(generated, physical))['partitions']
        self.assertEqual(partitions[0]['backend'], 'rom-image:boot')
        self.assertEqual(partitions[1]['backend'], '/dev/block/by-name/frp')
        self.assertEqual([entry['name'] for entry in partitions], ['boot_a', 'bdsvars'])

    def test_projection_limit(self):
        physical = '\n'.join(f'[[partitions]]\nname="p{i}"\nbackend="/dev/block/by-name/p{i}"'
                             for i in range(129))
        with self.assertRaises(ValueError):
            augment_rom('partitions=[]', physical)

    def test_stage_probe_uses_the_boot_hal_runtime_mount(self):
        self.assertEqual(Path(STAGE).parent, Path('/dev/efivars'))

    def test_private_adb_server_is_used_by_every_inherited_client(self):
        environment = private_adb_environment(5047)
        self.assertEqual(environment['ADB_SERVER_PORT'], '5047')
        self.assertEqual(environment['ANDROID_ADB_SERVER_PORT'], '5047')
        self.assertEqual(environment['ADB_SERVER_SOCKET'], 'tcp:5047')

    def test_bootstrap_gpt_split_preserves_frp_and_repairs_crcs(self):
        parsed = SimpleNamespace(entry_count=2, entry_size=128, entries_offset=512,
                                 array_bytes=256, header_offset=0, header_size=92)
        original = bytearray(1024)
        original[512:544] = bytes(range(1, 33))
        struct.pack_into('<QQ', original, 544, 312, 2359)
        original[568:640] = 'frp'.encode('utf-16le').ljust(72, b'\0')
        rewritten = split_frp_gpt(bytes(original), parsed)
        self.assertEqual(rewritten[512:544], original[512:544])
        self.assertEqual(rewritten[568:640], original[568:640])
        self.assertEqual(rewritten[768:], original[768:])
        self.assertEqual(struct.unpack_from('<QQ', rewritten, 544), (312, 1335))
        self.assertEqual(struct.unpack_from('<QQ', rewritten, 672), (1336, 2359))
        self.assertNotEqual(rewritten[528:544], rewritten[656:672])
        self.assertEqual(rewritten[696:768].decode('utf-16le').rstrip('\0'), 'bdsvars')
        self.assertEqual(struct.unpack_from('<I', rewritten, 88)[0], zlib.crc32(rewritten[512:768]))
        header = bytearray(rewritten[:92])
        recorded = struct.unpack_from('<I', header, 16)[0]
        struct.pack_into('<I', header, 16, 0)
        self.assertEqual(recorded, zlib.crc32(header))
        with self.assertRaises(ValueError):
            split_frp_gpt(rewritten, parsed)
        struct.pack_into('<Q', original, 552, 2360)
        with self.assertRaises(ValueError):
            split_frp_gpt(bytes(original), parsed)

    def test_promote_requires_staged_cpio_hash_and_removes_staged_name(self):
        hashes = {'boot.img': 'boot-hash', 'esu.cpio': 'cpio-hash'}
        for actual in ('cpio-hash', 'different'):
            with self.subTest(cpio_hash=actual), \
                    patch.object(ota_guest, 'stage', side_effect=[3, 0]), \
                    patch.object(ota_guest, 'absent_volumes'), \
                    patch.object(ota_guest, 'shell',
                                 side_effect=['', '', 'boot-hash  boot.img',
                                              f'{actual}  esu.cpio', '']) as shell:
                if actual == hashes['esu.cpio']:
                    ota_guest.promote(object(), hashes, kill=True)
                    shell.assert_called_with(
                        unittest.mock.ANY,
                        f'test ! -e {ota_guest.ESP}/rom/rom2/esu.stage.cpio')
                else:
                    with self.assertRaisesRegex(ValueError, 'promoted esu.cpio hash differs'):
                        ota_guest.promote(object(), hashes, kill=True)

    def test_command_failure_is_not_a_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            proof = Proof(Path(directory) / 'result')
            self.assertFalse(proof.command('fail', ['/usr/bin/false']))
            self.assertEqual(proof.rows[0]['status'], 'FAIL')
            self.assertEqual(proof.rows[0]['exit'], 1)
            proof.blocked('boot', 'failed prerequisite')
            self.assertEqual(proof.rows[1]['status'], 'NOT RUN')


if __name__ == '__main__':
    unittest.main()
