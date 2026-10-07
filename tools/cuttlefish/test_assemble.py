"""Host-only behavior tests; no device, build or VM required."""
import unittest

import tomllib

from . import assemble


class PlatformPackaging(unittest.TestCase):
    def test_manifest_kernel_list_and_module_precedence(self):
        manifest, rom = assemble.configurations("ext4", "android-a")
        value = tomllib.loads(manifest)
        self.assertEqual([(m["name"], m["path"]) for m in value["modules"]], [
            ("kernelesp", "lib/kernelesp.ko"), ("thin", "lib/thin.ko"),
            ("gpt", "lib/gpt.ko"), ("efivarfs", "lib/efivarfs.ko")])
        self.assertEqual(value["modules_order"], ["thin", "fw-views"])
        supplied, _ = assemble.configurations("ext4", "android-a", boot_hal=True)
        self.assertEqual(tomllib.loads(supplied)["modules_order"], ["boot-hal", "thin", "fw-views"])
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


if __name__ == "__main__":
    unittest.main()
