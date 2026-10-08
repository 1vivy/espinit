#!/usr/bin/env python3
"""Build and admit arm64 ACK KMI modules, with schema-2 input receipts."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
import re
import struct
import subprocess
import sys
from collections.abc import Iterator, Sequence
from pathlib import Path
from typing import NamedTuple, TypedDict, cast

PathInput = str | os.PathLike[str]
SymbolCrcs = dict[str, int]
SectionHeader = tuple[int, int, int, int, int, int, int, int, int, int]
RawSymbol = tuple[int, int, int, int, int, int]
class Symbol(NamedTuple):
    name: str
    info: int
    shndx: int
    value: int


class Kmi(TypedDict):
    branch: str
    generation: int


class Imports(TypedDict):
    versioned: int
    kallsyms: list[str]


class Provenance(TypedDict):
    schema_version: int
    kmi: Kmi
    kmi_out_inputs: dict[str, str]
    module_sha256: str
    imports: Imports


class ModuleReport(TypedDict):
    module: str
    kmi: Kmi
    imports: Imports

ROOT = Path(__file__).resolve().parents[1]
MODULES = {"kernelesp": ROOT / "kernel", **{name: ROOT / "modules" / name for name in ("thin", "gpt", "efivarfs", "efivar_store")}}
FLAGS = "SMP preempt mod_unload modversions aarch64"
INPUTS = ("Module.symvers", "System.map", "include/generated/utsrelease.h")
BRANCH = "android16-6.12"
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


def kmi_identity(source: Path) -> Kmi:
    constants = (source / "build.config.constants").read_text()
    match = re.search(r"^KMI_GENERATION=(\d+)\s*$", constants, re.MULTILINE)
    if match is None:
        raise CompatibilityError("kmi: missing KMI_GENERATION in build.config.constants")
    require(int(match[1]) == 6, "kmi: expected android16-6.12 generation 6")
    return {"branch": BRANCH, "generation": int(match[1])}


def verify_module(path: PathInput, name: str, output: Path) -> Imports:
    elf = Elf(path)
    require(elf.kind == 1 and elf.machine == 183, "elf: expected ET_REL aarch64")
    imports = {s.name for s in elf.symbols() if s.name and s.shndx == 0 and s.info >> 4 in (1, 2)}
    versions = import_versions(elf)
    exported = symvers(output / "Module.symvers")
    for symbol, crc in versions.items():
        require(symbol in exported, f"import-versions: {symbol} absent from KMI Module.symvers")
        require(crc == exported[symbol], f"import-versions: CRC mismatch for {symbol}")
    kallsyms = imports - versions.keys()
    kernel_symbols = {line.split()[2] for line in (output / "System.map").read_text().splitlines() if len(line.split()) >= 3}
    missing = kallsyms - kernel_symbols
    require(not missing, "imports: absent from System.map: " + ", ".join(sorted(missing)))
    modinfo = elf.section(".modinfo").split(b"\0")
    vermagic = [entry[9:].decode("ascii") for entry in modinfo if entry.startswith(b"vermagic=")]
    require(len(vermagic) == 1 and len(vermagic[0].split()) == 6 and " ".join(vermagic[0].split()[1:]) == FLAGS, "vermagic: expected release followed by " + FLAGS)
    require([entry[5:].decode("ascii") for entry in modinfo if entry.startswith(b"name=")] == [name], "modinfo: module name mismatch")
    return {"versioned": len(versions), "kallsyms": sorted(kallsyms)}


def receipt_path(module: PathInput) -> Path:
    return Path(str(module) + ".compat.json")


def provenance(output: Path, module: PathInput, identity: Kmi, imports: Imports) -> Provenance:
    result: Provenance = {"schema_version": 2, "kmi": identity,
                         "kmi_out_inputs": {str(output / name): digest(output / name) for name in INPUTS},
                         "module_sha256": digest(module), "imports": imports}
    return result


def verify_payload(output: PathInput, modules: Sequence[PathInput]) -> list[ModuleReport]:
    output = Path(output).resolve(strict=True)
    reports: list[ModuleReport] = []
    for module in modules:
        name = Path(module).stem
        imports = verify_module(module, name, output)
        receipt = cast(Provenance, json.loads(receipt_path(module).read_text()))
        identity = cast(Kmi, receipt.get("kmi"))
        require(identity == {"branch": BRANCH, "generation": 6}, "kmi: expected android16-6.12 generation 6")
        constants = output / "source/build.config.constants"
        if constants.is_file():
            require(identity == kmi_identity(constants.parent), "kmi: receipt/source identity mismatch")
        require(receipt == provenance(output, module, identity, imports), f"provenance: stale or mismatched build receipt for {name}")
        reports.append({"module": name, "kmi": identity, "imports": imports})
    return reports


def efvs_source(directory: Path) -> Path:
    """Build only the recorded revision, not an arbitrary checkout or working tree."""
    revision = (directory / "SOURCE_REVISION").read_text().strip()
    require(re.fullmatch(r"[0-9a-f]{40}", revision), "efvs: invalid source revision")
    checkout = directory / ".source"
    if not checkout.exists():
        repository = os.environ.get("EFIVAR_STORE_REPO", "https://github.com/1vivy/efivar-store.git")
        subprocess.run(["/usr/bin/git", "clone", "--no-checkout", repository, str(checkout)], check=True)
    present = subprocess.run(["/usr/bin/git", "-C", str(checkout), "cat-file", "-e", revision + "^{commit}"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if present.returncode != 0:
        subprocess.run(["/usr/bin/git", "-C", str(checkout), "fetch", "origin", revision], check=True)
    subprocess.run(["/usr/bin/git", "-C", str(checkout), "checkout", "--detach", revision], check=True)
    require(subprocess.check_output(["/usr/bin/git", "-C", str(checkout), "status", "--porcelain", "--untracked-files=no"]).strip() == b"", "efvs: source checkout is dirty")
    return checkout / "linux"


def build(args: argparse.Namespace) -> ModuleReport:
    require(len(args.module) == 1 and args.module[0] in MODULES, "build: specify one module name")
    require(args.kmi_src, "build: --kmi-src or KMI_SRC is required")
    source, output = Path(args.kmi_src).resolve(strict=True), Path(args.kmi_out).resolve(strict=True)
    identity = kmi_identity(source)
    require(1 <= args.jobs <= 13, "build: jobs must be 1..13")
    name = args.module[0]
    directory = MODULES[name]
    module = directory / (name + ".ko")
    receipt_path(module).unlink(missing_ok=True)
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CONFIG_", "KBUILD_")) and key not in ("MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES", "KCFLAGS", "KCPPFLAGS", "CFLAGS_MODULE", "LDFLAGS_MODULE")}
    build_directory = efvs_source(directory) if name == "efivar_store" else directory
    if name == "efivar_store":
        env["RUSTC_BOOTSTRAP"] = "1"
        env["RUSTC"] = os.environ.get("EFVS_RUSTC", str(Path.home() / ".rustup/toolchains/1.82.0-x86_64-unknown-linux-gnu/bin/rustc"))
    llvm = os.environ.get("EFVS_LLVM", "/usr/bin/") if name == "efivar_store" else "1"
    command = ["make", "-C", str(source), "O=" + str(output), "M=" + str(build_directory), "ARCH=arm64", "LLVM=" + llvm, "KBUILD_GENDWARFKSYMS_STABLE=1", "KBUILD_MODPOST_WARN=1", "CONFIG_KERNELESP=m"]
    if name == "efivar_store":
        command.append("RUSTC=" + env["RUSTC"])
    subprocess.run(command + ["clean"], env=env, check=True)
    subprocess.run(command + ["modules", f"-j{args.jobs}"], env=env, check=True)
    if name == "efivar_store":
        import shutil
        shutil.copyfile(build_directory / (name + ".ko"), module)
    imports = verify_module(module, name, output)
    receipt_path(module).write_text(json.dumps(provenance(output, module, identity, imports), indent=2, sort_keys=True) + "\n")
    return {"module": name, "kmi": identity, "imports": imports}


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "verify"))
    parser.add_argument("--kmi-src", default=os.environ.get("KMI_SRC"))
    parser.add_argument("--kmi-out", default=os.environ.get("KMI_OUT"), required=not os.environ.get("KMI_OUT"))
    parser.add_argument("--module", action="append", required=True)
    parser.add_argument("--jobs", type=int, default=13)
    args = parser.parse_args(argv)
    try:
        result = build(args) if args.action == "build" else verify_payload(args.kmi_out, args.module)
        print(json.dumps({"status": "accepted", "kmi": {"branch": BRANCH, "generation": 6}, "result": result}, indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, struct.error, subprocess.CalledProcessError) as error:
        print(json.dumps({"status": "rejected", "reason": str(error)}, sort_keys=True))
        print(f"kmi-modules: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
