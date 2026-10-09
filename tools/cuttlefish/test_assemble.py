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
            ("gpt", "lib/gpt.ko"), ("efivarfs", "lib/efivarfs.ko"),
            ("efivar_store", "lib/efivar_store.ko")])
        self.assertEqual(value["modules_order"], ["thin", "ota", "fw-views"])
        self.assertEqual(value["modules"][-1]["params"], "dev=by-name:bdsvars")
        self.assertEqual(value["modules"][-2]["params"], "")
        supplied, _ = assemble.configurations("ext4", "android-a", boot_hal=True)
        self.assertEqual(tomllib.loads(supplied)["modules_order"], ["boot-hal", "thin", "ota", "fw-views"])
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

    def test_stage_environment_probe_retains_product_exec(self):
        source = "#!/bin/sh\nset -eu\nexec ota-stage\n"
        captured = assemble.instrument_stage_script(source)
        self.assertTrue(captured.startswith("#!/bin/sh\n"))
        self.assertIn('CF-ESU_STAGE=%s', captured)
        self.assertIn('exec > /dev/kmsg 2>&1\n', captured)
        self.assertTrue(captured.endswith("set -eu\nexec ota-stage\n"))
        with self.assertRaises(ValueError):
            assemble.instrument_stage_script("exec ota-stage\n")

    def test_hal_transaction_errors_reach_guest_log(self):
        rc = "service esu.bootctl /debug_ramdisk/esu/bin/esu-bootctl\n    class early_hal\n"
        captured = assemble.instrument_boot_hal_rc(rc)
        self.assertIn('exec /debug_ramdisk/esu/bin/esu-bootctl >/dev/esu-bootctl.log 2>&1', captured)
        self.assertTrue(captured.endswith("    class early_hal\n"))
        with self.assertRaises(ValueError):
            assemble.instrument_boot_hal_rc("")

    def test_pid1_helper_errors_reach_kernel_log(self):
        self.assertIn("exec thin-activate > /dev/kmsg 2>&1", assemble.EARLY_SCRIPT)
        self.assertIn("exec fw-views > /dev/kmsg 2>&1", assemble.FW_EARLY_SCRIPT)


if __name__ == "__main__":
    unittest.main()
