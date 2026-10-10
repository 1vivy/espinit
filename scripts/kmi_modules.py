#!/usr/bin/env python3
"""Build and admit ACK KMI modules (arm64 phones, x86_64 Cuttlefish), with schema-2 input receipts."""
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

#: ELF machine -> (kbuild ARCH, vermagic flags after the release). x86_64 defines
#: no MODULE_ARCH_VERMAGIC, so its vermagic ends at `modversions`.
ARCHES = {183: ("arm64", "SMP preempt mod_unload modversions aarch64"),
          62: ("x86_64", "SMP preempt mod_unload modversions")}
CONFIG_ARCHES = {"CONFIG_ARM64=y": "arm64", "CONFIG_X86_64=y": "x86_64"}
INPUTS = ("Module.symvers", "System.map", "include/generated/utsrelease.h")
IDENTITY_FILES = ("build.config.constants", "build.config.common", "bazel/constants.scl")
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

    def verify_x86_relocations(self) -> None:
        # arch/x86/kernel/module.c applies only these types. In particular,
        # a freestanding Rust target must not emit userspace GOT references.
        supported = {0, 1, 2, 4, 10, 11, 24}
        for name, header in self.sections.items():
            if header[1] not in (4, 9):  # SHT_RELA, SHT_REL
                continue
            require(header[7] < len(self.headers), f"relocations: invalid target in {name}")
            if not self.headers[header[7]][2] & 2:  # Only SHF_ALLOC targets are loaded.
                continue
            require(header[1] == 4 and header[9] == 24 and header[5] % 24 == 0,
                    f"relocations: malformed x86_64 RELA table {name}")
            for _, info, _ in struct.iter_unpack("<QQq", self.section(name)):
                kind = info & 0xffffffff
                require(kind in supported, f"relocations: unsupported x86_64 type {kind} in {name}")


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


def kernel_arch(output: Path) -> str | None:
    """The kbuild ARCH of a KMI output tree from its `.config`, or None without one."""
    config = output / ".config"
    if not config.is_file():
        return None
    found = {CONFIG_ARCHES[line] for line in config.read_text().splitlines() if line in CONFIG_ARCHES}
    require(len(found) == 1, "kmi-out: .config names neither or both of CONFIG_ARM64 and CONFIG_X86_64")
    return found.pop()


def kmi_identity(source: Path, branch: str, generation: int) -> Kmi:
    require(re.fullmatch(r"android\d+-\d+\.\d+", branch), "kmi: specify an exact ACK branch")
    require(generation >= 0, "kmi: specify an exact nonnegative generation")
    constants = "\n".join((source / name).read_text() for name in IDENTITY_FILES if (source / name).is_file())
    generations = {int(value) for value in re.findall(r"^KMI_GENERATION=[\"']?(\d+)[\"']?\s*$", constants, re.MULTILINE)}
    require(generations == {generation}, "kmi: source generation mismatch or ambiguity")
    branches = set(re.findall(r"^BRANCH=[\"']?([a-z0-9.-]+)[\"']?\s*$", constants, re.MULTILINE))
    require(branches == {branch}, "kmi: missing, mismatched or ambiguous source branch")
    return {"branch": branch, "generation": generation}


def verify_module(path: PathInput, name: str, output: Path, architecture: str | None = None) -> Imports:
    elf = Elf(path)
    require(elf.kind == 1 and elf.machine in ARCHES, "elf: expected ET_REL aarch64 or x86_64")
    arch, flags = ARCHES[elf.machine]
    requested = {"aarch64": "arm64", "x86_64": "x86_64"}.get(architecture) if architecture else None
    require(architecture is None or requested == arch, "elf: requested architecture mismatch")
    tree = kernel_arch(output)
    require(tree is None or tree == arch, f"elf: {arch} module for a {tree} KMI tree")
    if elf.machine == 62:
        elf.verify_x86_relocations()
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
    tokens = vermagic[0].split() if len(vermagic) == 1 else []
    require(len(tokens) == 1 + len(flags.split()) and tokens[1:] == flags.split(), "vermagic: expected release followed by " + flags)
    require([entry[5:].decode("ascii") for entry in modinfo if entry.startswith(b"name=")] == [name.replace("-", "_")], "modinfo: module name mismatch")
    return {"versioned": len(versions), "kallsyms": sorted(kallsyms)}


def receipt_path(module: PathInput) -> Path:
    return Path(str(module) + ".compat.json")


def provenance(output: Path, module: PathInput, identity: Kmi, imports: Imports) -> Provenance:
    result: Provenance = {"schema_version": 2, "kmi": identity,
                         "kmi_out_inputs": {str(output / name): digest(output / name) for name in INPUTS},
                         "module_sha256": digest(module), "imports": imports}
    return result


def verify_payload(output: PathInput, modules: Sequence[PathInput], branch: str,
                   generation: int, architecture: str) -> list[ModuleReport]:
    output = Path(output).resolve(strict=True)
    require(re.fullmatch(r"android\d+-\d+\.\d+", branch) and generation >= 0,
            "kmi: exact branch and generation required")
    reports: list[ModuleReport] = []
    for module in modules:
        name = Path(module).stem
        imports = verify_module(module, name, output, architecture)
        receipt = cast(Provenance, json.loads(receipt_path(module).read_text()))
        identity = cast(Kmi, receipt.get("kmi"))
        require(identity == {"branch": branch, "generation": generation}, "kmi: receipt identity mismatch")
        source = output / "source"
        if any((source / name).is_file() for name in IDENTITY_FILES):
            require(identity == kmi_identity(source, branch, generation), "kmi: receipt/source identity mismatch")
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


def run_kbuild(command: Sequence[str], env: Mapping[str, str], log: Path,
               append: bool = False) -> None:
    """Stream complete build logs, rejecting objtool diagnostics even on exit 0."""
    diagnostic: str | None = None
    overlap = ""
    with log.open("a" if append else "w") as stream:
        stream.write("command: " + repr(list(command)) + "\n")
        with subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True, errors="replace") as process:
            assert process.stdout is not None
            while chunk := process.stdout.readline(65536):
                stream.write(chunk)
                stream.flush()
                print(chunk, end="", file=sys.stderr)
                # Bounded overlap catches a diagnostic split across long output
                # chunks; do not buffer the entire compiler output in memory.
                window = overlap + chunk
                if diagnostic is None and re.search(r"\bobjtool:\s", window, re.IGNORECASE):
                    diagnostic = window[-2048:].strip()
                overlap = window[-128:]
            status = process.wait()
    if status:
        raise subprocess.CalledProcessError(status, command)
    require(diagnostic is None, f"objtool: diagnostic rejects module admission; log {log}: {diagnostic}")


def build(args: argparse.Namespace) -> ModuleReport:
    require(len(args.module) == 1 and args.module_dir, "build: specify one --module and --module-dir")
    require(args.kmi_src, "build: --kmi-src or KMI_SRC is required")
    source, output = Path(args.kmi_src).resolve(strict=True), Path(args.kmi_out).resolve(strict=True)
    identity = kmi_identity(source, args.branch, args.generation)
    output_source = output / "source"
    if any((output_source / name).is_file() for name in IDENTITY_FILES):
        require(identity == kmi_identity(output_source, args.branch, args.generation),
                "kmi: build source/output identity mismatch")
    require(1 <= args.jobs <= 13, "build: jobs must be 1..13")
    name = args.module[0]
    require(re.fullmatch(r"[A-Za-z0-9_-]+", name), "build: invalid module artifact name")
    directory = Path(args.module_dir).resolve(strict=True)
    require((directory / "Kbuild").is_file(), "build: module directory has no standalone Kbuild")
    module = directory / (name + ".ko")
    receipt_path(module).unlink(missing_ok=True)
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CONFIG_", "KBUILD_")) and key not in ("MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES", "KCFLAGS", "KCPPFLAGS", "CFLAGS_MODULE", "LDFLAGS_MODULE")}
    if (directory / "SOURCE_REVISION").is_file():
        _ = efvs_source(directory)
    arch = kernel_arch(output)
    require(arch == {"aarch64": "arm64", "x86_64": "x86_64"}[args.arch],
            "build: KMI output architecture mismatch or missing .config")
    command = ["make", "-C", str(source), "O=" + str(output), "M=" + str(directory), "ARCH=" + str(arch), "LLVM=1", "KBUILD_GENDWARFKSYMS_STABLE=1", "KBUILD_MODPOST_WARN=1"]
    log = directory / (name + ".build.log")
    run_kbuild(command + ["clean"], env, log)
    run_kbuild(command + ["modules", f"-j{args.jobs}"], env, log, append=True)
    imports = verify_module(module, name, output, args.arch)
    receipt_path(module).write_text(json.dumps(provenance(output, module, identity, imports), indent=2, sort_keys=True) + "\n")
    return {"module": name, "kmi": identity, "imports": imports}


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "verify"))
    parser.add_argument("--kmi-src", default=os.environ.get("KMI_SRC"))
    parser.add_argument("--kmi-out", default=os.environ.get("KMI_OUT"), required=not os.environ.get("KMI_OUT"))
    parser.add_argument("--module", action="append", required=True)
    parser.add_argument("--module-dir")
    parser.add_argument("--branch", required=True)
    parser.add_argument("--generation", type=int, required=True)
    parser.add_argument("--arch", choices=("aarch64", "x86_64"), required=True)
    parser.add_argument("--jobs", type=int, default=13)
    args = parser.parse_args(argv)
    try:
        result = build(args) if args.action == "build" else verify_payload(args.kmi_out, args.module, args.branch, args.generation, args.arch)
        print(json.dumps({"status": "accepted", "kmi": {"branch": args.branch, "generation": args.generation}, "result": result}, indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, struct.error, subprocess.CalledProcessError) as error:
        print(json.dumps({"status": "rejected", "reason": str(error)}, sort_keys=True))
        print(f"kmi-modules: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
