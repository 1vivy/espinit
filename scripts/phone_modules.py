#!/usr/bin/env python3
"""Build and admit exact-target espinit LKMs; never manufacture version data.

KERNEL_CONFIG is an independently captured target config, not a defconfig or a
copy of the candidate output's .config. A build receipt binds stable CRC build
settings and all checked kernel inputs to the final module bytes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import struct
import subprocess
import sys
from collections.abc import Iterator, Mapping, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, NamedTuple, TypedDict, cast

PathInput = str | os.PathLike[str]
Config = dict[str, str]
SymbolCrcs = dict[str, int]
# `Elf64_Shdr` fields: name, type, flags, addr, offset, size, link, info,
# addralign, entsize. `Elf64_Sym` fields: name, info, other, shndx, value, size.
# The reader keeps these `struct` shapes rather than named records so that
# admission never allocates a second record per section header or symbol.
SectionHeader = tuple[int, int, int, int, int, int, int, int, int, int]
RawSymbol = tuple[int, int, int, int, int, int]


class Symbol(NamedTuple):
    """One symbol table entry reduced to the fields admission checks."""
    name: str
    info: int
    shndx: int
    value: int


class ModuleReport(TypedDict):
    """Admission result for one module; this is the serialized report shape."""
    module: str
    imports: int
    exports: int
    import_versions: int
    non_kmi_imports: list[str]
    vermagic: str
    btf: bool


class Provenance(TypedDict):
    """Build receipt binding the packaged bytes to the exact kernel inputs."""
    schema_version: int
    kernel_src: str
    kernel_out: str
    kernel_config: str
    stable: str
    inputs: dict[str, str]
    module_sha256: str


class ConfigReport(TypedDict):
    """Serialized result of the config-only action."""
    config: str


class Arguments(argparse.Namespace):
    """CLI field contract; `argparse` fills every field before use.

    The initializers below are type-checker only: `argparse` must apply its own
    defaults, which it skips for attributes that already exist at runtime.
    """
    action: str
    kernel_src: str
    kernel_out: str
    kernel_config: str
    module: str | None
    jobs: int
    espinit: Path | None
    thin: Path | None
    gpt: Path | None

    if TYPE_CHECKING:
        action = ""
        kernel_src = ""
        kernel_out = ""
        kernel_config = ""
        module = None
        jobs = 13
        espinit = None
        thin = None
        gpt = None


ROOT = Path(__file__).resolve().parents[1]
MODULES: dict[str, Path] = {"espinit": ROOT / "kernel", "thin": ROOT / "modules/thin", "gpt": ROOT / "modules/gpt"}


class CompatibilityError(ValueError):
    pass


def require(condition: object, reason: str) -> None:
    if not condition:
        raise CompatibilityError(reason)


def digest(path: PathInput) -> str:
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Elf:
    """Bounded ELF64 little-endian section/symbol reader (no host tool parsing)."""

    path: Path
    data: bytes
    kind: int
    machine: int
    headers: list[SectionHeader]
    sections: dict[str, SectionHeader]

    def __init__(self, path: PathInput) -> None:
        self.path = Path(path)
        self.data = self.path.read_bytes()
        require(len(self.data) >= 64 and self.data[:7] == b"\x7fELF\x02\x01\x01", "elf: expected little-endian ELF64")
        self.kind, self.machine = struct.unpack_from("<HH", self.data, 16)
        offset = struct.unpack_from("<Q", self.data, 40)[0]
        size, count, names_index = struct.unpack_from("<HHH", self.data, 58)
        require(size == 64 and count > 0 and names_index < count, "elf: invalid section table")
        table = self.slice(offset, size * count)
        self.headers = list(struct.iter_unpack("<IIQQQQIIQQ", table))
        names_header = self.headers[names_index]
        names = self.slice(names_header[4], names_header[5])
        self.sections = {}
        for header in self.headers[1:]:
            name = self.string(names, header[0])
            require(name not in self.sections, f"elf: duplicate section {name}")
            self.sections[name] = header
            if header[1] != 8:  # SHT_NOBITS has no file contents.
                _ = self.slice(header[4], header[5])

    def slice(self, offset: int, size: int) -> bytes:
        require(offset <= len(self.data) and size <= len(self.data) - offset, "elf: truncated section")
        return self.data[offset:offset + size]

    @staticmethod
    def string(data: bytes, offset: int) -> str:
        require(offset < len(data), "elf: invalid string offset")
        end = data.find(b"\0", offset)
        require(end >= 0, "elf: unterminated string")
        return data[offset:end].decode("ascii")

    def section(self, name: str) -> bytes:
        header = self.sections.get(name)
        return self.slice(header[4], header[5]) if header else b""

    def symbols(self) -> list[Symbol]:
        require(".symtab" in self.sections, "elf: missing symbol table")
        header = self.sections[".symtab"]
        require(header[9] == 24 and header[5] % 24 == 0 and header[6] < len(self.headers), "elf: malformed symbol table")
        strings = self.headers[header[6]]
        names = self.slice(strings[4], strings[5])
        records: Iterator[RawSymbol] = struct.iter_unpack("<IBBHQQ", self.section(".symtab"))
        return [Symbol(self.string(names, name), info, shndx, value)
                for name, info, _, shndx, value, _ in records]


def config_values(path: PathInput) -> Config:
    values: Config = {}
    for line in Path(path).read_text().splitlines():
        if line.startswith("CONFIG_") and "=" in line:
            key, value = line.split("=", 1)
            require(key not in values, f"config: duplicate {key}")
            values[key] = value
        elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
            key = line[2:-11]
            require(key not in values, f"config: duplicate {key}")
            values[key] = "n"
    require(values, f"config: no configuration in {path}")
    return values

# Kconfig computes these by linking a tiny hosted C program. Android's hermetic
# kernel build supplies a userspace sysroot, while an otherwise identical
# standalone module build does not need one. They describe the build
# environment, not kernel code or module ABI, so they are excluded from the
# captured target/config equivalence check. The output still has to agree with
# its own generated auto.conf and autoconf.h below.
HOST_LINK_PROBES = frozenset({"CONFIG_CC_CAN_LINK", "CONFIG_CC_CAN_LINK_STATIC"})


def check_config(source: PathInput | None, output: PathInput | None, target: PathInput | None, stable: str = "1") -> Config:
    if source is None or output is None or target is None or not all(str(path) for path in (source, output, target)):
        raise CompatibilityError("config: KERNEL_SRC, KERNEL_OUT and KERNEL_CONFIG are required")
    source, output, target = (Path(path).resolve(strict=True) for path in (source, output, target))
    require((source / "Makefile").is_file(), "config: KERNEL_SRC lacks Makefile")
    require(target != output / ".config", "config: KERNEL_CONFIG must be an independent target capture")
    expected, actual = config_values(target), config_values(output / ".config")
    require(expected.get("CONFIG_MODVERSIONS") == "y", "config: target requires CONFIG_MODVERSIONS=y")
    require(stable == "1", "config: KBUILD_GENDWARFKSYMS_STABLE must be 1")
    for key in ("CONFIG_MODVERSIONS", "CONFIG_GENDWARFKSYMS", "CONFIG_MODULES", "CONFIG_ARM64"):
        if expected.get(key) == "y":
            require(actual.get(key) == "y", f"config: required {key}=y")
    different = sorted(
        key
        for key in (expected.keys() | actual.keys()) - HOST_LINK_PROBES
        if expected.get(key, "n") != actual.get(key, "n")
    )
    require(not different, "config: output differs from target: " + ", ".join(different[:12]))
    require(actual.get("CONFIG_MODULES") == "y", "config: CONFIG_MODULES=y required")
    auto = config_values(output / "include/config/auto.conf")
    make_values = {
        key: value[1:-1] if len(value) >= 2 and value.startswith('"') and value.endswith('"') else value
        for key, value in actual.items()
    }
    different = sorted(key for key in make_values.keys() | auto.keys() if make_values.get(key, "n") != auto.get(key, "n"))
    require(not different, "config: stale auto.conf: " + ", ".join(different[:12]))
    require((output / "include/generated/autoconf.h").is_file(), "config: missing generated autoconf.h")
    generated: Config = dict(re.findall(r"^#define (CONFIG_\w+) (.+)$", (output / "include/generated/autoconf.h").read_text(), re.MULTILINE))
    expected_macros: Config = {}
    for key, value in actual.items():
        if value != "n":
            expected_macros[key + ("_MODULE" if value == "m" else "")] = "1" if value in ("y", "m") else value
    require(generated == expected_macros, "config: stale autoconf.h")
    # Kbuild creates this source link for a separate output tree.
    require(source == output or (output / "source").resolve() == source, "config: KERNEL_OUT/source does not identify KERNEL_SRC")
    for name in ("Module.symvers", "vmlinux", "include/generated/utsrelease.h"):
        require((output / name).is_file() and (output / name).stat().st_size > 0, f"config: missing populated {name}; modules_prepare is insufficient")
    return actual


def symvers(path: PathInput) -> SymbolCrcs:
    result: SymbolCrcs = {}
    for line in Path(path).read_text().splitlines():
        fields = line.split()
        require(len(fields) >= 4, f"symvers: malformed line in {path}")
        crc, name = int(fields[0], 16), fields[1]
        require(name not in result, f"symvers: duplicate {name}")
        require(0 <= crc <= 0xffffffff, f"symvers: invalid CRC for {name}")
        result[name] = crc
    return result


def import_versions(elf: Elf) -> SymbolCrcs:
    normal = elf.section("__versions")
    crcs, names = elf.section("__version_ext_crcs"), elf.section("__version_ext_names")
    require(normal or (crcs and names), "import-versions: missing or empty __versions (including module_layout)")
    tables: list[list[tuple[int, str]]] = []
    if normal:
        require(len(normal) % 64 == 0, "import-versions: malformed __versions records")
        tables.append([(crc, Elf.string(name, 0)) for crc, name in struct.iter_unpack("<Q56s", normal)])
    if crcs or names:
        require(crcs and len(crcs) % 4 == 0 and names.endswith(b"\0"), "import-versions: malformed extended versions")
        strings = names[:-1].split(b"\0")
        # modpost emits concatenated C strings with explicit per-name NULs,
        # followed by the C initializer's implicit final NUL.
        if strings and strings[-1] == b"":
            _ = strings.pop()
        require(len(strings) == len(crcs) // 4, "import-versions: extended count mismatch")
        tables.append([(crc[0], name.decode("ascii")) for crc, name in zip(struct.iter_unpack("<I", crcs), strings)])
    versions: SymbolCrcs = {}
    for records in tables:
        seen: set[str] = set()
        for crc, name in records:
            require(name and name not in seen and 0 <= crc <= 0xffffffff, "import-versions: invalid or duplicate record")
            require(name not in versions or versions[name] == crc, f"import-versions: basic/extended CRC mismatch for {name}")
            seen.add(name)
            versions[name] = crc
        require("module_layout" in seen, "import-versions: missing module_layout")
    return versions


def expected_vermagic(config: Mapping[str, str], output: Path, machine: int) -> str:
    match = re.fullmatch(r'#define UTS_RELEASE "([^"\n]+)"\s*', (output / "include/generated/utsrelease.h").read_text())
    if match is None:
        raise CompatibilityError("vermagic: malformed utsrelease.h")
    parts = [cast("str", match[1])]
    for key, word in (("CONFIG_SMP", "SMP"), ("CONFIG_PREEMPT_RT", "preempt_rt")):
        if config.get(key) == "y":
            parts.append(word)
    if config.get("CONFIG_PREEMPT_RT") != "y" and (config.get("CONFIG_PREEMPT_BUILD") == "y" or config.get("CONFIG_PREEMPT") == "y"):
        parts.append("preempt")
    for key, word in (("CONFIG_MODULE_UNLOAD", "mod_unload"), ("CONFIG_MODVERSIONS", "modversions")):
        if config.get(key) == "y":
            parts.append(word)
    if machine == 183:
        parts.append("aarch64")
    require(config.get("CONFIG_RANDSTRUCT") != "y", "vermagic: RANDSTRUCT target needs an exact reference vermagic implementation")
    return " ".join(parts)


def verify_module(path: PathInput, name: str, config: Mapping[str, str], output: Path, kernel_symbols: set[str] | None = None) -> ModuleReport:
    elf = Elf(path)
    machine = 183 if config.get("CONFIG_ARM64") == "y" else 62
    require(elf.kind == 1 and elf.machine == machine, "elf: module type/target architecture mismatch")
    symbols = elf.symbols()
    imports = {symbol.name for symbol in symbols if symbol.name and symbol.shndx == 0 and symbol.info >> 4 in (1, 2)}
    exports = {symbol.name.removeprefix("__ksymtab_") for symbol in symbols if symbol.shndx and symbol.info & 15 != 3 and symbol.name.startswith("__ksymtab_")}
    export_count = 0
    for suffix in ("", "_gpl", "_unused", "_unused_gpl", "_gpl_future"):
        table = elf.section("__ksymtab" + suffix)
        crc = elf.section("__kcrctab" + suffix)
        if table:
            require(crc, f"export-crcs: missing __kcrctab{suffix} for __ksymtab{suffix}")
            width = 12 if config.get("CONFIG_HAVE_ARCH_PREL32_RELOCATIONS") == "y" else 24
            require(len(table) % width == 0 and len(crc) == len(table) // width * 4, f"export-crcs: malformed/count mismatch __kcrctab{suffix}")
            export_count += len(table) // width
        else:
            require(not crc, f"export-crcs: orphan __kcrctab{suffix}")
    require(export_count == len(exports), "export-crcs: symbol/table export count mismatch")
    versions = import_versions(elf)
    exported = symvers(output / "Module.symvers")
    for symbol, crc in versions.items():
        require(symbol in exported, f"import-versions: {symbol} absent from target Module.symvers")
        require(crc == exported[symbol], f"import-versions: CRC mismatch for {symbol}")
    for symbol in imports & exported.keys():
        require(symbol in versions, f"import-versions: missing CRC for {symbol}")
    non_kmi = imports - exported.keys()
    require(name != "thin" or not non_kmi, "imports: thin must use exported KMI: " + ", ".join(sorted(non_kmi)))
    if kernel_symbols is None:
        kernel = Elf(output / "vmlinux")
        require(kernel.machine == machine, "elf: vmlinux architecture mismatch")
        kernel_symbols = {symbol.name for symbol in kernel.symbols() if symbol.shndx}
    missing = imports - kernel_symbols
    require(not missing, "imports: absent from exact vmlinux: " + ", ".join(sorted(missing)))
    modinfo = elf.section(".modinfo").split(b"\0")
    vermagic = [entry[9:].decode("ascii") for entry in modinfo if entry.startswith(b"vermagic=")]
    require(vermagic == [expected_vermagic(config, output, machine)], "vermagic: does not match exact target configuration/release")
    require([entry[5:].decode("ascii") for entry in modinfo if entry.startswith(b"name=")] == [name], "modinfo: module name mismatch")
    btf = bool(elf.section(".BTF"))
    require(btf == (config.get("CONFIG_DEBUG_INFO_BTF_MODULES") == "y"), "btf: .BTF presence differs from target config")
    if config.get("CONFIG_DEBUG_INFO_BTF_MODULES") == "y" and config.get("CONFIG_DEBUG_INFO_BTF_MODULES_DISTILLED_BASE") == "y":
        require(elf.section(".BTF.base"), "btf: missing distilled .BTF.base")
    return {"module": name, "imports": len(imports), "exports": len(exports), "import_versions": len(versions), "non_kmi_imports": sorted(non_kmi), "vermagic": vermagic[0], "btf": btf}


def receipt_path(module: PathInput) -> Path:
    return Path(str(module) + ".compat.json")


def provenance(source: Path, output: Path, target: Path, module: PathInput) -> Provenance:
    inputs = {str(path): digest(path) for path in (target, source / "Makefile", output / ".config", output / "include/config/auto.conf", output / "include/generated/autoconf.h", output / "include/generated/utsrelease.h", output / "Module.symvers", output / "vmlinux")}
    return {"schema_version": 1, "kernel_src": str(source), "kernel_out": str(output), "kernel_config": str(target), "stable": "1", "inputs": inputs, "module_sha256": digest(module)}


def verify_payload(source: PathInput, output: PathInput, target: PathInput, modules: Mapping[str, PathInput]) -> list[ModuleReport]:
    config = check_config(source, output, target)
    source, output, target = (Path(path).resolve(strict=True) for path in (source, output, target))
    kernel = Elf(output / "vmlinux")
    require(kernel.machine == (183 if config.get("CONFIG_ARM64") == "y" else 62), "elf: vmlinux architecture mismatch")
    kernel_symbols = {symbol.name for symbol in kernel.symbols() if symbol.shndx}
    reports: list[ModuleReport] = []
    for name, path in modules.items():
        reports.append(verify_module(path, name, config, output, kernel_symbols))
        require(receipt_path(path).is_file(), f"provenance: missing {receipt_path(path)}; rebuild through phone recipe")
        receipt = cast("Provenance", json.loads(receipt_path(path).read_text()))
        require(receipt == provenance(source, output, target, path), f"provenance: stale or mismatched build receipt for {name}")
    return reports


def build(args: Arguments) -> ModuleReport:
    module_name = args.module
    if module_name is None:
        raise CompatibilityError("build: --module is required")
    config = check_config(args.kernel_src, args.kernel_out, args.kernel_config, os.environ.get("KBUILD_GENDWARFKSYMS_STABLE", "1"))
    source, output, target = (Path(path).resolve(strict=True) for path in (args.kernel_src, args.kernel_out, args.kernel_config))
    require(config.get("CONFIG_ARM64") == "y" or config.get("CONFIG_X86_64") == "y", "config: only arm64 and x86_64 targets are supported")
    arch = "arm64" if config.get("CONFIG_ARM64") == "y" else "x86_64"
    require(1 <= args.jobs <= 13, "build: JOBS must be 1..13")
    directory = MODULES[module_name]
    module = directory / (module_name + ".ko")
    receipt_path(module).unlink(missing_ok=True)
    # Do not inherit command-line/config overrides from an outer make or shell.
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CONFIG_", "KBUILD_")) and key not in ("MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES", "KCFLAGS", "KCPPFLAGS", "CFLAGS_MODULE", "LDFLAGS_MODULE")}
    command = ["make", "-C", str(source), "O=" + str(output), "M=" + str(directory), "ARCH=" + arch, "LLVM=1", "KBUILD_GENDWARFKSYMS_STABLE=1", "CONFIG_ESPINIT=m"]
    if module_name != "thin":
        command.append("KBUILD_MODPOST_WARN=1")
    if module_name == "thin":
        command += ["HOSTCFLAGS=-Wno-unknown-warning-option -Wno-error=incompatible-pointer-types-discards-qualifiers", "KCFLAGS=-Wno-unknown-warning-option -Wno-error=default-const-init-var-unsafe -Wno-error=default-const-init-field-unsafe -Wno-error=unterminated-string-initialization -Wno-error=uninitialized"]
    # Clean first: enabling stable CRC generation must not reuse unversioned objects.
    _ = subprocess.run(command + ["clean"], env=env, check=True)
    _ = subprocess.run(command + ["modules", f"-j{args.jobs}"], env=env, check=True)
    config = check_config(source, output, target)
    report = verify_module(module, module_name, config, output)
    _ = receipt_path(module).write_text(json.dumps(provenance(source, output, target, module), indent=2, sort_keys=True) + "\n")
    return report


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("action", choices=("config", "build", "verify"))
    for key in ("kernel-src", "kernel-out", "kernel-config"):
        _ = parser.add_argument("--" + key, default=os.environ.get(key.upper().replace("-", "_")), required=not os.environ.get(key.upper().replace("-", "_")))
    _ = parser.add_argument("--module", choices=MODULES)
    _ = parser.add_argument("--jobs", type=int, default=13)
    for name in MODULES:
        _ = parser.add_argument("--" + name, type=Path)
    args = parser.parse_args(argv, namespace=Arguments())
    result: ModuleReport | ConfigReport | list[ModuleReport]
    try:
        if args.action == "build":
            result = build(args)
        elif args.action == "config":
            _ = check_config(args.kernel_src, args.kernel_out, args.kernel_config, os.environ.get("KBUILD_GENDWARFKSYMS_STABLE", "1"))
            result = ConfigReport(config="compatible")
        else:
            modules = {name: path for name, path in (("espinit", args.espinit), ("thin", args.thin), ("gpt", args.gpt)) if path is not None}
            require(modules, "verify: specify --espinit, --thin and/or --gpt")
            result = verify_payload(args.kernel_src, args.kernel_out, args.kernel_config, modules)
        print(json.dumps({"status": "accepted", "result": result}, indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, struct.error, subprocess.CalledProcessError) as error:
        print(json.dumps({"status": "rejected", "reason": str(error)}, sort_keys=True))
        print(f"phone-modules: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
