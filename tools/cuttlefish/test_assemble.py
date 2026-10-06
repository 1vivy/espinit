"""Host-only behavior tests; no device, build or VM required."""
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch
from . import assemble


class PlatformPackaging(unittest.TestCase):
    def test_manifest_kernel_list_and_module_precedence(self):
        manifest, rom = assemble.configurations("ext4", "android-a")
        value = tomllib.loads(manifest)
        self.assertEqual([(m["name"], m["path"]) for m in value["modules"]], [
            ("kernelesp", "lib/kernelesp.ko"), ("thin", "lib/thin.ko"),
            ("gpt", "lib/gpt.ko"), ("efivarfs", "lib/efivarfs.ko")])
        self.assertEqual(value["modules_order"], ["boot-hal", "thin", "fw-views"])
        self.assertNotIn("platform", value)
        self.assertNotIn("generation", value)
        config = tomllib.loads(rom)
        self.assertEqual(config["id"], "android-a")
        self.assertNotIn("generation", config)
        self.assertNotIn("rom_number", config)
        for rom_id in ("", ".", "..", "../a", "a/b", "é", "a b", "x" * 60):
            with self.assertRaises(ValueError):
                assemble.configurations("ext4", rom_id)
        with self.assertRaises(ValueError):
            assemble.configurations("auto", "rom1")

    def test_ordinary_boot_hal_module(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "hal"
            binary.write_bytes(b"built HAL")
            files = assemble.platform_files({"boot_hal": binary}, root / "tree")
            targets = {target: source.read_bytes() for source, target, _ in files}
            prefix = "esu/modules/boot-hal/"
            self.assertEqual(targets[prefix + "vendor/bin/hw/android.hardware.boot-service.qti"], b"built HAL")
            self.assertIn(b"id=boot-hal\n", targets[prefix + "module.prop"])
            self.assertEqual(targets[prefix + "sepolicy.rule"].decode().splitlines(), [
                "allow hal_bootctl_default esu_file dir { search read open getattr write add_name remove_name }",
                "allow hal_bootctl_default esu_file file { create read write open getattr setattr unlink lock ioctl }",
                "allow hal_bootctl_default esu_file filesystem getattr"])
            self.assertIn(b"hal_bootctl_default_exec", targets[prefix + "attrs"])
            self.assertFalse(any(name.endswith(".ko") or "tiny-espsu" in name for name in targets))

    def test_esp_image_copies_pid1_scripts_not_early_scripts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(b"built binary")
            sources = {key: binary for key, _ in assemble.BINARIES}
            sources["boot_hal"] = binary
            manifest, rom = assemble.configurations("ext4", "rom1")
            copies = {}
            def run(args, **_kwargs):
                if args[0] == "mcopy":
                    copies[args[-1]] = Path(args[-2]).read_bytes()
                return b""
            with patch.object(assemble, "run", side_effect=run):
                assemble.build_esp(sources, manifest, rom, root, None, "0123456789ab")
            for module in ("thin", "fw-views"):
                for stage in ("pid1.sh", "pid1-recovery.sh"):
                    self.assertTrue(copies[f"::/esu/modules/{module}/{stage}"].startswith(b"#!/bin/sh\nset -eu\n"))
            self.assertEqual(copies["::/esu/build-id"], b"0123456789ab\n")
            self.assertFalse(any(name.endswith(".ko") or name.endswith("/early.sh") for name in copies))


if __name__ == "__main__":
    unittest.main()
