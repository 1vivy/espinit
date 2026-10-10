"""Synthetic ELF admission contracts; no compiler or checked-in binaries."""
import json
import contextlib
import io
import subprocess
import sys
import struct
import tempfile
import unittest
from collections.abc import Iterable, Mapping
from pathlib import Path
from . import kmi_modules as compat
VERMAGIC = "6.12-phone SMP preempt mod_unload modversions aarch64"
ARM64_FLAGS = compat.ARCHES[183][1]
X86_64_FLAGS = compat.ARCHES[62][1]
def elf_file(sections: Mapping[str, bytes], symbols: Iterable[tuple[str, int]], kind: int = 1, machine: int = 183) -> bytes:
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
    struct.pack_into("<HHI", body, 16, kind, machine, 1)
    headers = [bytes(64)]
    headers.append(struct.pack("<IIQQQQIIQQ", 1, 3, 0, 0, len(body), len(names), 0, 0, 1, 0))
    body.extend(names)
    string_index = list(contents).index(".strtab") + 2
    for name, data in contents.items():
        typ = 2 if name == ".symtab" else 3 if name == ".strtab" else 4 if name.startswith(".rela.") else 1
        link = string_index if typ == 2 else list(contents).index(".symtab") + 2 if typ == 4 else 0
        info = 1 if typ == 2 else list(contents).index(name[5:]) + 2 if typ == 4 else 0
        flags = 6 if name == ".text" else 0
        headers.append(struct.pack("<IIQQQQIIQQ", offsets[name], typ, flags, 0, len(body), len(data), link, info, 8, 24 if typ in (2, 4) else 0))
        body.extend(data)
    struct.pack_into("<Q", body, 40, len(body))
    struct.pack_into("<HHH", body, 52, 64, 0, 0)
    struct.pack_into("<HHH", body, 58, 64, len(headers), 1)
    return bytes(body) + b"".join(headers)


def version_data(records: Iterable[tuple[str, int]]) -> bytes:
    return b"".join(struct.pack("<Q56s", crc, name.encode()) for name, crc in records)


def module_file(name: str = "gpt", versions: bytes | None = None, extra: Mapping[str, bytes] | None = None, imports: Iterable[str] = ("known", "private"), machine: int = 183) -> bytes:
    sections = {".text": b"\0" * 4, ".modinfo": f"name={name}\0vermagic={VERMAGIC}\0".encode()}
    symbols = [(symbol, 0) for symbol in imports]
    if versions is None:
        versions = version_data((("module_layout", 0x12345678), ("known", 0x11223344)))
    sections["__versions"] = versions
    sections.update(extra or {})
    return elf_file(sections, symbols, machine=machine)


class KmiModuleCompatibility(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "out"
        (self.output / "include/generated").mkdir(parents=True)
        (self.output / "include/generated/utsrelease.h").write_text('#define UTS_RELEASE "different-release"\n')
        (self.output / "Module.symvers").write_text("0x12345678 module_layout vmlinux EXPORT_SYMBOL\n0x11223344 known vmlinux EXPORT_SYMBOL\n")
        (self.output / "System.map").write_text("0000000000000001 T private\n")
        self.module = self.root / "gpt.ko"

    def verify(self, data):
        self.module.write_bytes(data)
        return compat.verify_module(self.module, "gpt", self.output)

    def test_nonversioned_import_in_system_map_accepted(self):
        self.assertEqual(self.verify(module_file()), {"versioned": 2, "kallsyms": ["private"]})

    def test_nonversioned_import_absent_rejected(self):
        with self.assertRaisesRegex(compat.CompatibilityError, "absent from System.map: missing"):
            self.verify(module_file(imports=("known", "missing")))

    def test_release_ignored_flags_enforced(self):
        for release in ("6.12-phone", "6.12.58-android16-6-other"):
            self.verify(module_file(extra={".modinfo": f"name=gpt\0vermagic={release} {ARM64_FLAGS}\0".encode()}))
        for flags in ("SMP mod_unload modversions aarch64", "SMP preempt modversions aarch64", "SMP preempt mod_unload modversions x86_64", X86_64_FLAGS, ARM64_FLAGS + " extra"):
            with self.subTest(flags=flags), self.assertRaisesRegex(compat.CompatibilityError, "vermagic"):
                self.verify(module_file(extra={".modinfo": f"name=gpt\0vermagic=release {flags}\0".encode()}))

    def test_x86_64_modules_carry_their_own_vermagic_and_tree(self):
        """Cuttlefish x86_64 modules: EM_X86_64 with the arch-less vermagic, and no arm64 tree."""
        x86 = {".modinfo": f"name=gpt\0vermagic=6.12.74-android16-6 {X86_64_FLAGS}\0".encode()}
        self.verify(module_file(extra=x86, machine=62))
        with self.assertRaisesRegex(compat.CompatibilityError, "vermagic"):
            self.verify(module_file(machine=62))
        with self.assertRaisesRegex(compat.CompatibilityError, "ET_REL aarch64 or x86_64"):
            self.verify(module_file(machine=40))
        (self.output / ".config").write_text("CONFIG_64BIT=y\nCONFIG_ARM64=y\n")
        with self.assertRaisesRegex(compat.CompatibilityError, "x86_64 module for a arm64 KMI tree"):
            self.verify(module_file(extra=x86, machine=62))
        self.verify(module_file())
        (self.output / ".config").write_text("CONFIG_X86_64=y\n")
        self.verify(module_file(extra=x86, machine=62))
        (self.output / ".config").write_text("CONFIG_X86_64=y\nCONFIG_ARM64=y\n")
        with self.assertRaisesRegex(compat.CompatibilityError, "neither or both"):
            self.verify(module_file())

    def test_x86_loaded_relocations_reject_got_but_ignore_debug(self):
        extra = {".modinfo": f"name=gpt\0vermagic=release {X86_64_FLAGS}\0".encode(),
                 ".text": bytes(8), ".debug_info": bytes(8),
                 ".rela.debug_info": struct.pack("<QQq", 0, (1 << 32) | 9, 0)}
        for kind in (0, 1, 2, 4, 10, 11, 24):
            with self.subTest(kind=kind):
                extra[".rela.text"] = struct.pack("<QQq", 0, (1 << 32) | kind, 0)
                self.assertEqual(self.verify(module_file(extra=extra, machine=62)),
                                 {"versioned": 2, "kallsyms": ["private"]})
        for kind in (9, 41, 42):
            with self.subTest(kind=kind), self.assertRaisesRegex(
                    compat.CompatibilityError, f"unsupported x86_64 type {kind} in .rela.text"):
                extra[".rela.text"] = struct.pack("<QQq", 0, (1 << 32) | kind, 0)
                self.verify(module_file(extra=extra, machine=62))
        extra[".rela.text"] = bytes(23)
        with self.assertRaisesRegex(compat.CompatibilityError, "malformed x86_64 RELA"):
            self.verify(module_file(extra=extra, machine=62))

    def test_extended_versions_and_coexisting_tables(self):
        for end in (b"\0", b"\0\0"):
            extra = {"__version_ext_names": b"module_layout\0known" + end,
                     "__version_ext_crcs": struct.pack("<II", 0x12345678, 0x11223344)}
            self.assertEqual(self.verify(module_file(versions=b"", extra=extra))["versioned"], 2)
            self.verify(module_file(extra=extra))
        extra["__version_ext_crcs"] = struct.pack("<II", 0x12345678, 1)
        with self.assertRaisesRegex(compat.CompatibilityError, "basic/extended CRC mismatch"):
            self.verify(module_file(extra=extra))
        with self.assertRaisesRegex(compat.CompatibilityError, "extended count mismatch"):
            self.verify(module_file(versions=b"", extra={"__version_ext_names": b"module_layout\0", "__version_ext_crcs": bytes(8)}))

    def test_wrong_crc_name_type_and_truncated_elf(self):
        for data, reason in ((module_file(versions=version_data((("module_layout", 1),))), "CRC mismatch"),
                             (module_file("thin"), "module name mismatch"),
                             (elf_file({}, [], kind=2), "ET_REL"),
                             (module_file()[:-1], "truncated")):
            with self.subTest(reason=reason), self.assertRaisesRegex(compat.CompatibilityError, reason):
                self.verify(data)

    def test_receipt_binds_inputs_and_module(self):
        imports = self.verify(module_file())
        identity = {"branch": "android16-6.12", "generation": 6}
        receipt = compat.provenance(self.output, self.module, identity, imports)
        self.assertEqual(set(receipt), {"schema_version", "kmi", "kmi_out_inputs", "module_sha256", "imports"})
        compat.receipt_path(self.module).write_text(json.dumps(receipt))
        self.assertEqual(len(compat.verify_payload(self.output, [self.module], "android16-6.12", 6, "aarch64")), 1)
        (self.output / "System.map").write_text("0000000000000001 T private\n0000000000000002 T extra\n")
        with self.assertRaisesRegex(compat.CompatibilityError, "stale or mismatched"):
            compat.verify_payload(self.output, [self.module], "android16-6.12", 6, "aarch64")

    def test_explicit_identity_is_not_generation_six_only(self):
        source = self.root / "source"
        source.mkdir()
        (source / "build.config.constants").write_text("BRANCH=android15-6.6\nKMI_GENERATION=9\n")
        self.assertEqual(compat.kmi_identity(source, "android15-6.6", 9),
                         {"branch": "android15-6.6", "generation": 9})
        with self.assertRaisesRegex(compat.CompatibilityError, "generation mismatch"):
            compat.kmi_identity(source, "android15-6.6", 6)
        with self.assertRaisesRegex(compat.CompatibilityError, "source branch"):
            compat.kmi_identity(source, "android16-6.12", 9)
        with self.assertRaisesRegex(compat.CompatibilityError, "exact ACK branch"):
            compat.kmi_identity(source, "android15", 9)

    def test_legacy_and_bazel_identity_reject_conflicting_sources(self):
        for index, (name, branch, generation) in enumerate((
                ("build.config.common", "android12-5.10", 9),
                ("bazel/constants.scl", "android17-6.18", 5))):
            with self.subTest(name=name):
                source = self.root / str(index)
                identity = source / name
                identity.parent.mkdir(parents=True)
                identity.write_text(f'BRANCH="{branch}"\nKMI_GENERATION={generation}\n')
                self.assertEqual(compat.kmi_identity(source, branch, generation),
                                 {"branch": branch, "generation": generation})
                constants = source / "build.config.constants"
                constants.write_text(f"KMI_GENERATION={generation + 1}\n")
                with self.assertRaisesRegex(compat.CompatibilityError, "generation"):
                    compat.kmi_identity(source, branch, generation)
                constants.write_text(f"KMI_GENERATION={generation}\nBRANCH=android99-9.9\n")
                with self.assertRaisesRegex(compat.CompatibilityError, "source branch"):
                    compat.kmi_identity(source, branch, generation)

    def test_hyphenated_artifact_and_explicit_architecture(self):
        self.module.write_bytes(module_file("dm_thin_pool"))
        compat.verify_module(self.module, "dm-thin-pool", self.output, "aarch64")
        with self.assertRaisesRegex(compat.CompatibilityError, "requested architecture"):
            compat.verify_module(self.module, "dm-thin-pool", self.output, "x86_64")

    def test_receipt_rejects_wrong_explicit_identity(self):
        imports = self.verify(module_file())
        identity = {"branch": "android15-6.6", "generation": 9}
        compat.receipt_path(self.module).write_text(json.dumps(
            compat.provenance(self.output, self.module, identity, imports)))
        compat.verify_payload(self.output, [self.module], "android15-6.6", 9, "aarch64")
        with self.assertRaisesRegex(compat.CompatibilityError, "receipt identity"):
            compat.verify_payload(self.output, [self.module], "android15-6.6", 8, "aarch64")

    def test_success_exit_with_objtool_diagnostic_is_rejected_and_logged(self):
        log = self.root / "module.build.log"
        for diagnostic in ("warning: objtool: naked return",
                           "warning: objtool: indirect call",
                           "warning: objtool: data relocation to !ENDBR",
                           "objtool: error: invalid instruction"):
            with self.subTest(diagnostic=diagnostic), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(compat.CompatibilityError, "rejects module admission"):
                    compat.run_kbuild([sys.executable, "-c", f"print({diagnostic!r})"], {}, log)
                self.assertIn(diagnostic, log.read_text())

    def test_private_modpost_warning_is_not_suppressed_or_rejected(self):
        log = self.root / "module.build.log"
        diagnostic = 'WARNING: modpost: "private_symbol" undefined!'
        with contextlib.redirect_stderr(io.StringIO()):
            compat.run_kbuild([sys.executable, "-c", f"print({diagnostic!r})"], {}, log)
        self.assertIn(diagnostic, log.read_text())

    def test_stream_boundary_and_nonzero_build_keep_logs(self):
        log = self.root / "module.build.log"
        diagnostic = "x" * 65530 + "warning: objtool: indirect jump"
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(compat.CompatibilityError, "objtool"):
                compat.run_kbuild([sys.executable, "-c", f"print({diagnostic!r})"], {}, log)
            with self.assertRaises(subprocess.CalledProcessError):
                compat.run_kbuild([sys.executable, "-c", "print('compiler error'); raise SystemExit(2)"], {}, log, append=True)
        self.assertIn(diagnostic, log.read_text())
        self.assertIn("compiler error", log.read_text())

if __name__ == "__main__":
    unittest.main()
