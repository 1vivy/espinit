"""Host-only behavior tests; no assembler tools, device, build or VM required."""

import struct
import tempfile
import tomllib
import unittest
from pathlib import Path

from . import assemble


def executable(generation: str, duplicate: bool = False) -> bytes:
    names = b"\0.shstrtab\0.note.espinit\0"
    note = struct.pack("<III", 8, 64, 1) + b"ESPINIT\0" + generation.encode().ljust(64, b"\0")
    sections = 4 if duplicate else 3
    section_offset = 64 + len(names) + len(note)
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", header, 16, 3, 183, 1)
    struct.pack_into("<Q", header, 40, section_offset)
    struct.pack_into("<HHH", header, 58, 64, sections, 1)
    string_section = struct.pack("<IIQQQQIIQQ", 1, 3, 0, 0, 64, len(names), 0, 0, 1, 0)
    note_section = struct.pack("<IIQQQQIIQQ", 11, 7, 2, 0, 64 + len(names), len(note), 0, 0, 4, 0)
    return bytes(header) + names + note + bytes(64) + string_section + note_section * (2 if duplicate else 1)


class PlatformPackaging(unittest.TestCase):
    def test_binary_generation_and_note_framing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "hal"
            path.write_bytes(executable("release-1"))
            assemble.artifact_generation(path, "release-1")
            with self.assertRaises(ValueError):
                assemble.artifact_generation(path, "release-2")
            for data in (b"not ELF", executable("release-1")[:-1], executable("release-1", duplicate=True)):
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    assemble.artifact_generation(path, "release-1")

    def test_manifest_preserves_order_and_separates_android_packages(self):
        manifest, rom = assemble.configurations("release-1", "ext4", "android-a")
        value = tomllib.loads(manifest)
        self.assertEqual(
            [(module["name"], module["path"]) for module in value["modules"]],
            [
                ("espinit", "modules/espinit.ko"),
                ("thin", "modules/thin.ko"),
                # The userspace helper runs between `thin` and `gpt`.
                ("fw-views", "bin/fw-views"),
                ("gpt", "modules/gpt.ko"),
            ],
        )
        self.assertEqual(value["platform"], {"metadata_filesystem": "ext4", "packages": ["boot-hal", "tiny-espsu"], "recovery_packages": []})
        self.assertEqual(tomllib.loads(rom)["generation"], value["generation"])
        self.assertEqual(value["rom"], "roms")
        self.assertEqual(tomllib.loads(rom)["id"], "android-a")
        self.assertEqual(tomllib.loads(rom)["rom_number"], 1)
        self.assertNotIn("metadata_shared", rom)
        _, second = assemble.configurations("release-1", "ext4", "android-b")
        self.assertEqual(tomllib.loads(second)["id"], "android-b")
        assemble.configurations("release-1", "ext4", "x" * 59)
        for rom_id in ("", ".", "..", "../a", "a/b", "é", "a b", "x" * 60):
            with self.assertRaises(ValueError):
                assemble.configurations("release-1", "ext4", rom_id)
        for generation, filesystem in (("../bad", "ext4"), ("x", "auto")):
            with self.assertRaises(ValueError):
                assemble.configurations(generation, filesystem, "android-a")

    def test_esp_layout_carries_both_module_scripts_and_the_helper(self):
        self.assertIn("fw_views", assemble.PATHS)
        self.assertIn(("fw_views", "bin/fw-views"), assemble.BINARIES)
        self.assertIn("espinit/modules/fw-views", assemble.ESP_DIRECTORIES)
        self.assertEqual(assemble.FW_EARLY_SCRIPT, "#!/bin/sh\nset -eu\nexec fw-views\n")
        for name in ("thin", "fw-views"):
            self.assertNotIn(name, [key for key, _ in assemble.MODULES])

    def test_packages_use_stamped_contracts_and_exact_generation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tree = root / "tree"
            (tree / "modules").mkdir(parents=True)
            binary = root / "binary"
            binary.write_bytes(executable("release-2"))
            sources = {key: binary for key in ("espinitd", "boot_hal", "tiny_espsu")}
            files = assemble.platform_files(sources, "release-2", tree)
            targets = [target for _, target, _ in files]
            self.assertEqual(len(targets), len(set(targets)))
            for module in ("boot-hal", "tiny-espsu"):
                manifest = tomllib.loads((tree / "modules" / module / "module.toml").read_text())
                self.assertEqual(manifest["id"], module)
                self.assertEqual(manifest["generation"], "release-2")
                for entry in manifest["files"]:
                    self.assertIn(f"espinit/modules/{module}/{entry['source']}", targets)


if __name__ == "__main__":
    unittest.main()
