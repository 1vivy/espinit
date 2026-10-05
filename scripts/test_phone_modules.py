"""Deterministic ELF/config fixtures; no compiler, phone or external ELF tools."""

from __future__ import annotations

import json
import struct
import subprocess
import tempfile
import unittest
from collections.abc import Iterable, Mapping
from pathlib import Path
from typing import cast, override

from . import phone_modules as compat

CONFIG = """CONFIG_MODULES=y
CONFIG_ARM64=y
CONFIG_MODVERSIONS=y
CONFIG_GENDWARFKSYMS=y
CONFIG_HAVE_ARCH_PREL32_RELOCATIONS=y
CONFIG_SMP=y
CONFIG_PREEMPT_BUILD=y
CONFIG_MODULE_UNLOAD=y
"""
VERMAGIC = "6.12-phone SMP preempt mod_unload modversions aarch64"


def elf_file(sections: Mapping[str, bytes], symbols: Iterable[tuple[str, int]], kind: int = 1) -> bytes:
    """Encode real ELF section/symbol tables instead of mocking parser outputs."""
    contents = dict(sections)
    strings = bytearray(b"\0")
    table = bytearray(24)
    for name, index in symbols:
        offset = len(strings)
        strings.extend(name.encode() + b"\0")
        table.extend(struct.pack("<IBBHQQ", offset, 0x10, 0, index, 0, 0))
    contents[".strtab"] = bytes(strings)
    contents[".symtab"] = bytes(table)
    names = bytearray(b"\0.shstrtab\0")
    offsets = {}
    for name in contents:
        offsets[name] = len(names)
        names.extend(name.encode() + b"\0")
    body = bytearray(64)
    body[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", body, 16, kind, 183, 1)
    headers = [bytes(64)]
    headers.append(struct.pack("<IIQQQQIIQQ", 1, 3, 0, 0, len(body), len(names), 0, 0, 1, 0))
    body.extend(names)
    string_index = list(contents).index(".strtab") + 2
    for name, data in contents.items():
        typ = 2 if name == ".symtab" else 3 if name == ".strtab" else 1
        headers.append(struct.pack("<IIQQQQIIQQ", offsets[name], typ, 0, 0, len(body), len(data), string_index if typ == 2 else 0, 1 if typ == 2 else 0, 8, 24 if typ == 2 else 0))
        body.extend(data)
    struct.pack_into("<Q", body, 40, len(body))
    struct.pack_into("<HHH", body, 52, 64, 0, 0)
    struct.pack_into("<HHH", body, 58, 64, len(headers), 1)
    return bytes(body) + b"".join(headers)


def version_data(records: Iterable[tuple[str, int]]) -> bytes:
    return b"".join(struct.pack("<Q56s", crc, name.encode()) for name, crc in records)


def module_file(name: str = "gpt", exports: bool = False, crc: bool = True, versions: bytes | None = None, extra: Mapping[str, bytes] | None = None, imports: Iterable[str] = ("known", "private")) -> bytes:
    sections = {".text": b"\0" * 4, ".modinfo": f"name={name}\0vermagic={VERMAGIC}\0".encode()}
    symbols = [(symbol, 0) for symbol in imports]
    if versions is None:
        versions = version_data((("module_layout", 0x12345678), ("known", 0x11223344)))
    sections["__versions"] = versions
    if exports:
        sections["__ksymtab_gpl"] = bytes(12)
        symbols.append(("__ksymtab_exported", 2))
        if crc:
            sections["__kcrctab_gpl"] = struct.pack("<I", 0x99887766)
    sections.update(extra or {})
    return elf_file(sections, symbols)


class PhoneModuleCompatibility(unittest.TestCase):
    """One exact-kernel fixture tree per test; `setUp` fills every attribute."""
    # The class-body values only give each fixture attribute a known type; a
    # fresh tree replaces them in `setUp` before any test body runs.
    temp: tempfile.TemporaryDirectory[str] | None = None
    root: Path = Path()
    source: Path = Path()
    output: Path = Path()
    target: Path = Path()
    config: Mapping[str, str] = {}
    module: Path = Path()

    @override
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.temp = temp
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.source, self.output = self.root / "source", self.root / "out"
        self.source.mkdir()
        _ = (self.source / "Makefile").write_text("VERSION = 6\n")
        (self.output / "include/generated").mkdir(parents=True)
        (self.output / "include/config").mkdir()
        (self.output / "source").symlink_to(self.source, target_is_directory=True)
        self.target = self.root / "captured-phone.config"
        for path in (self.target, self.output / ".config", self.output / "include/config/auto.conf"):
            _ = path.write_text(CONFIG)
        _ = (self.output / "include/generated/autoconf.h").write_text("".join("#define " + line.replace("=y", " 1") + "\n" for line in CONFIG.splitlines()))
        _ = (self.output / "include/generated/utsrelease.h").write_text('#define UTS_RELEASE "6.12-phone"\n')
        _ = (self.output / "Module.symvers").write_text("0x12345678\tmodule_layout\tvmlinux\tEXPORT_SYMBOL\n0x11223344\tknown\tvmlinux\tEXPORT_SYMBOL\n")
        _ = (self.output / "vmlinux").write_bytes(elf_file({".text": bytes(4)}, [("known", 2), ("private", 2)], kind=2))
        self.config = compat.check_config(self.source, self.output, self.target)
        self.module = self.root / "module.ko"

    def verify(self, data: bytes, name: str = "gpt") -> compat.ModuleReport:
        _ = self.module.write_bytes(data)
        return compat.verify_module(self.module, name, self.config, self.output)

    def test_gpt_no_exports_requires_real_import_versions(self) -> None:
        report = self.verify(module_file())
        self.assertEqual(report["exports"], 0)
        self.assertEqual(report["non_kmi_imports"], ["private"])
        for data, reason in ((b"", "missing or empty"), (version_data((("known", 0x11223344),)), "missing module_layout"), (version_data((("module_layout", 1),)), "CRC mismatch")):
            with self.subTest(reason=reason), self.assertRaisesRegex(compat.CompatibilityError, reason):
                _ = self.verify(module_file(versions=data))

    def test_thin_failure_shape_names_missing_export_crcs_first(self) -> None:
        # Exact section dimensions from installed generation 56269c50:
        # 11 plain + 128 GPL exports, 153 imports, empty __versions, no CRCs.
        sections = {".text": bytes(4), ".modinfo": b"name=thin\0",
                    "__ksymtab": bytes(0x84), "__ksymtab_gpl": bytes(0x600), "__versions": b""}
        symbols = [(f"import_{index}", 0) for index in range(153)]
        symbols += [(f"__ksymtab_export_{index}", 4 if index < 11 else 5) for index in range(139)]
        with self.assertRaisesRegex(compat.CompatibilityError, "export-crcs: missing __kcrctab for __ksymtab"):
            _ = self.verify(elf_file(sections, symbols), "thin")

    def test_core_exports_require_matching_crc_section_count(self) -> None:
        self.assertEqual(self.verify(module_file("espinit", exports=True), "espinit")["exports"], 1)
        for crc in (b"", bytes(8)):
            with self.subTest(crc=crc), self.assertRaisesRegex(compat.CompatibilityError, "export-crcs"):
                _ = self.verify(module_file("espinit", exports=True, extra={"__kcrctab_gpl": crc}), "espinit")

    def test_missing_import_crc_and_unresolved_non_kmi_fail(self) -> None:
        with self.assertRaisesRegex(compat.CompatibilityError, "missing CRC for known"):
            _ = self.verify(module_file(versions=version_data((("module_layout", 0x12345678),))))
        with self.assertRaisesRegex(compat.CompatibilityError, "absent from exact vmlinux: absent"):
            _ = self.verify(module_file(imports=("absent",)))
        with self.assertRaisesRegex(compat.CompatibilityError, "thin must use exported KMI"):
            _ = self.verify(module_file("thin"), "thin")

    def test_extended_modversions(self) -> None:
        report = self.verify(module_file(versions=b"", extra={"__version_ext_names": b"module_layout\0known\0", "__version_ext_crcs": struct.pack("<II", 0x12345678, 0x11223344)}))
        self.assertEqual(report["import_versions"], 2)
        with self.assertRaisesRegex(compat.CompatibilityError, "extended count mismatch"):
            _ = self.verify(module_file(versions=b"", extra={"__version_ext_names": b"module_layout\0", "__version_ext_crcs": bytes(8)}))

    def test_vermagic_btf_and_malformed_elf_fail_closed(self) -> None:
        with self.assertRaisesRegex(compat.CompatibilityError, "vermagic"):
            _ = self.verify(module_file(extra={".modinfo": b"name=gpt\0vermagic=wrong\0"}))
        with self.assertRaisesRegex(compat.CompatibilityError, "btf"):
            _ = self.verify(module_file(extra={".BTF": bytes(8)}))
        for data in (b"not ELF", module_file()[:-1]):
            with self.subTest(size=len(data)), self.assertRaises(compat.CompatibilityError):
                _ = self.verify(data)

    def test_basic_and_extended_tables_coexist_on_phone(self) -> None:
        extra = {"__version_ext_names": b"module_layout\0known\0\0", "__version_ext_crcs": struct.pack("<II", 0x12345678, 0x11223344)}
        self.assertEqual(self.verify(module_file(extra=extra))["import_versions"], 2)
        extra["__version_ext_crcs"] = struct.pack("<II", 0x12345678, 1)
        with self.assertRaisesRegex(compat.CompatibilityError, "basic/extended CRC mismatch"):
            _ = self.verify(module_file(extra=extra))

    def test_config_rejects_stale_header_and_wrong_source(self) -> None:
        _ = (self.output / "include/generated/autoconf.h").write_text("#define CONFIG_MODVERSIONS 1\n")
        with self.assertRaisesRegex(compat.CompatibilityError, "stale autoconf.h"):
            _ = compat.check_config(self.source, self.output, self.target)
        _ = (self.output / "include/generated/autoconf.h").write_text("".join("#define " + line.replace("=y", " 1") + "\n" for line in CONFIG.splitlines()))
        (self.output / "source").unlink()
        with self.assertRaisesRegex(compat.CompatibilityError, "does not identify KERNEL_SRC"):
            _ = compat.check_config(self.source, self.output, self.target)

    def test_assembler_rejects_before_creating_payload_output(self) -> None:
        from tools.cuttlefish import assemble
        _ = self.module.write_bytes(module_file("espinit", exports=True, crc=False))
        argv = [item for name in assemble.PATHS for item in ("--" + name.replace("_", "-"), str(self.module))]
        destination = self.root / "payload"
        argv += ["--kernel-src", str(self.source), "--kernel-out", str(self.output), "--kernel-config", str(self.target),
                 "--metadata-filesystem", "f2fs", "--generation", "fixture", "--rom-id", "fixture", "--output-dir", str(destination)]
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            assemble.assemble(assemble.parse_arguments(argv))
        self.assertIn(b"export-crcs: missing __kcrctab_gpl", cast("bytes", failure.exception.stderr))
        self.assertFalse(destination.exists())

    def test_config_requires_versions_stable_exact_target_and_preparation(self) -> None:
        for option in ("CONFIG_MODVERSIONS", "CONFIG_GENDWARFKSYMS", "CONFIG_SMP"):
            _ = (self.output / ".config").write_text(CONFIG.replace(option + "=y", option + "=n"))
            with self.subTest(option=option), self.assertRaisesRegex(compat.CompatibilityError, option):
                _ = compat.check_config(self.source, self.output, self.target)
        _ = (self.output / ".config").write_text(CONFIG)
        with self.assertRaisesRegex(compat.CompatibilityError, "STABLE must be 1"):
            _ = compat.check_config(self.source, self.output, self.target, "0")
        with self.assertRaisesRegex(compat.CompatibilityError, "independent target capture"):
            _ = compat.check_config(self.source, self.output, self.output / ".config")
        _ = (self.output / "include/config/auto.conf").write_text(CONFIG.replace("CONFIG_MODVERSIONS=y", "CONFIG_MODVERSIONS=n"))
        with self.assertRaisesRegex(compat.CompatibilityError, "stale auto.conf"):
            _ = compat.check_config(self.source, self.output, self.target)
        _ = (self.output / "include/config/auto.conf").write_text(CONFIG)
        _ = (self.output / "Module.symvers").write_text("")
        with self.assertRaisesRegex(compat.CompatibilityError, "modules_prepare is insufficient"):
            _ = compat.check_config(self.source, self.output, self.target)

    def test_host_link_probes_do_not_change_kernel_or_module_abi(self) -> None:
        target = CONFIG + "CONFIG_CC_CAN_LINK=y\nCONFIG_CC_CAN_LINK_STATIC=y\n"
        _ = self.target.write_text(target)
        checked = compat.check_config(self.source, self.output, self.target)
        self.assertNotIn("CONFIG_CC_CAN_LINK", checked)
        self.assertNotIn("CONFIG_CC_CAN_LINK_STATIC", checked)

    def test_generated_make_config_unquotes_string_values(self) -> None:
        configured = CONFIG + 'CONFIG_DEFAULT_HOSTNAME="phone"\n'
        for path in (self.target, self.output / ".config"):
            _ = path.write_text(configured)
        _ = (self.output / "include/config/auto.conf").write_text(CONFIG + "CONFIG_DEFAULT_HOSTNAME=phone\n")
        header = (self.output / "include/generated/autoconf.h").read_text()
        _ = (self.output / "include/generated/autoconf.h").write_text(
            header + '#define CONFIG_DEFAULT_HOSTNAME "phone"\n'
        )
        checked = compat.check_config(self.source, self.output, self.target)
        self.assertEqual(checked["CONFIG_DEFAULT_HOSTNAME"], '"phone"')

    def test_receipt_binds_packaged_bytes_and_exact_kernel_inputs(self) -> None:
        _ = self.module.write_bytes(module_file())
        with self.assertRaisesRegex(compat.CompatibilityError, "provenance: missing"):
            _ = compat.verify_payload(self.source, self.output, self.target, {"gpt": self.module})
        _ = compat.receipt_path(self.module).write_text(json.dumps(compat.provenance(self.source, self.output, self.target, self.module)))
        self.assertEqual(len(compat.verify_payload(self.source, self.output, self.target, {"gpt": self.module})), 1)
        _ = (self.source / "Makefile").write_text("VERSION = 7\n")
        with self.assertRaisesRegex(compat.CompatibilityError, "stale or mismatched"):
            _ = compat.verify_payload(self.source, self.output, self.target, {"gpt": self.module})


if __name__ == "__main__":
    _ = unittest.main()
