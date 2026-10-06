#!/usr/bin/env python3
"""Assemble one esu Cuttlefish payload directory.

The payload is exactly the three artifacts the Cuttlefish lane consumes through
`--esu-payload`: `init_boot.img`, `esp.img` and `payload.json`. No virtual
machine, no container and no Cuttlefish binary is started here, and nothing is
written outside the output directory (temporary files live inside it).

Every host tool is invoked through a checked subprocess argument list: there is
no shell, no string interpolation and no glob expansion.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parents[2]
GENERATION = re.compile(r"[A-Za-z0-9._-]{1,63}")
BOOT_MAGIC = b"ANDROID!"
BOOT_HEADER_SIZE = 4096
HEADER_SIZES = {3: 1580, 4: 1584}
NEWC_MAGICS = (b"070701", b"070702")
LZ4_LEGACY_MAGIC = b"\x02\x21\x4c\x18"
LZ4_FRAME_MAGICS = (b"\x04\x22\x4d\x18",)

ARTIFACTS = ("init_boot.img", "esp.img", "payload.json")
PATHS = (
    "stock_init_boot",
    "avbtool",
    "avb_key",
    "esuinit",
    "esud",
    "boot_hal",
    "tiny_espsu",
    "busybox",
    "thin_activate",
    "fw_views",
    "core_module",
    "thin_module",
    "gpt_module",
    "efivarfs_module",
)
BINARIES = (
    ("busybox", "bin/busybox"),
    ("thin_activate", "bin/thin-activate"),
    ("fw_views", "bin/fw-views"),
    ("esud", "bin/esud"),
)
MODULES = (("core_module", "kernelesp"), ("thin_module", "thin"), ("gpt_module", "gpt"), ("efivarfs_module", "efivarfs"))
ESP_DIRECTORIES = (
    "esu",
    "esu/bin",
    "esu/roms",
    "esu/modules",
    "esu/modules/thin",
    "esu/modules/fw-views",
    "esu/modules/boot-hal",
    "esu/modules/tiny-espsu",
    "esu/receipts",
)
EARLY_SCRIPT = "#!/bin/sh\nset -eu\nexec thin-activate\n"
FW_EARLY_SCRIPT = "#!/bin/sh\nset -eu\nexec fw-views\n"
DEFAULT_ESP_MIB = 64
MIN_ESP_MIB = 8


def run(arguments: list[str | Path], *, cwd: Path | None = None, data: bytes | None = None) -> bytes:
    """Run one host tool with an argument list; never a shell."""
    completed = subprocess.run(
        [str(argument) for argument in arguments],
        cwd=cwd,
        input=data,
        check=True,
        capture_output=True,
    )
    return completed.stdout


def configurations(generation: str, metadata_filesystem: str, rom_id: str) -> tuple[str, str]:
    """Render the manifest and the structurally valid placeholder ROM config."""
    if not GENERATION.fullmatch(generation):
        raise ValueError("generation must be 1..63 ASCII letters/digits plus . _ -")
    if metadata_filesystem not in ("ext4", "f2fs"):
        raise ValueError("metadata filesystem must be explicitly ext4 or f2fs")
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,59}", rom_id) or rom_id in (".", ".."):
        raise ValueError("ROM ID must be 1..59 ASCII letters/digits plus . _ -, excluding . and ..")

    head = f'schema_version = 1\ngeneration = "{generation}"\n'
    manifest = head + 'rom = "roms"\n'
    manifest += (
        f'\n[platform]\nmetadata_filesystem = "{metadata_filesystem}"\n'
        'packages = ["boot-hal", "tiny-espsu"]\nrecovery_packages = []\n'
    )
    for name, path in (
        ("kernelesp", "lib/kernelesp.ko"),
        ("thin", "lib/thin.ko"),
        # An ordered userspace helper: it runs `bin/fw-views` through its own
        # early.sh between `thin` and `gpt`, and a ROM without firmware views
        # makes it a no-op.
        ("fw-views", "bin/fw-views"),
        ("gpt", "lib/gpt.ko"),
        ("efivarfs", "lib/efivarfs.ko"),
    ):
        params = "dev=by-name:bdsvars" if name == "efivarfs" else ""
        manifest += f'\n[[modules]]\nname = "{name}"\npath = "{path}"\nparams = "{params}"\n'

    # Valid managed shape with a deliberately impossible backend: the lab lane
    # replaces this file with the complete generated GPT projection before boot.
    # The stock ROM number is explicit, and there is no `metadata_shared`
    # projection: one guest per payload, and Cuttlefish projects the physical
    # metadata partition itself.
    rom = head + f'id = "{rom_id}"\nrom_number = 1\n' + (
        'managed = true\n\n[[partitions]]\nname = "userdata"\n'
        'backend = "/dev/mapper/esu-payload-placeholder"\nread_only = false\n'
    )

    return manifest, rom


def records(archive: bytes) -> Iterator[tuple[str, bytes]]:
    """Validate every concatenated newc archive in one initramfs."""
    position = 0

    while True:
        while position < len(archive) and archive[position] == 0:
            position += 1
        if position == len(archive):
            return
        if archive[position : position + 6] not in NEWC_MAGICS:
            raise ValueError("stock ramdisk is not a concatenation of newc cpio archives")

        while True:
            start = position
            if archive[position : position + 6] not in NEWC_MAGICS:
                raise ValueError("stock ramdisk has invalid newc framing")

            fields = [
                int(archive[position + 6 + index * 8 : position + 14 + index * 8], 16)
                for index in range(13)
            ]
            size, name_size = fields[6], fields[11]
            position += 110

            if name_size < 1 or position + name_size > len(archive):
                raise ValueError("invalid cpio member name bounds")

            name_bytes = archive[position : position + name_size]
            if name_bytes[-1:] != b"\0" or b"\0" in name_bytes[:-1]:
                raise ValueError("invalid cpio member name")

            name = name_bytes[:-1].decode("utf-8")
            if name.startswith("/") or ".." in name.split("/"):
                raise ValueError(f"unsafe stock cpio member: {name}")

            position = (position + name_size + 3) & ~3
            position = (position + size + 3) & ~3
            if position > len(archive):
                raise ValueError("invalid cpio member data bounds")

            yield name.removeprefix("./"), archive[start:position]

            if name == "TRAILER!!!":
                break


def decompress(ramdisk: bytes) -> tuple[str, bytes]:
    if ramdisk.startswith(b"\x1f\x8b"):
        return "gzip", gzip.decompress(ramdisk)
    if ramdisk[:4] == LZ4_LEGACY_MAGIC:
        return "lz4-legacy", run(["lz4", "-d", "-c"], data=ramdisk)
    if ramdisk[:4] in LZ4_FRAME_MAGICS:
        # The session kernel's lib/decompress_unlz4.c accepts ARCHIVE_MAGICNUMBER
        # (legacy) only and errors "invalid header" otherwise, so a frame format
        # image could not boot even unmodified. Refuse it instead of guessing.
        raise ValueError(
            "the stock initramfs uses the LZ4 frame format, which the lane's kernel cannot decompress; "
            "provide the stock image with its legacy LZ4 ramdisk"
        )
    return "none", ramdisk


def compress(mode: str, raw: bytes) -> bytes:
    if mode == "gzip":
        return gzip.compress(raw, mtime=0)
    if mode == "lz4-legacy":
        return run(["lz4", "-l", "-c"], data=raw)
    return raw


def add_pid1(ramdisk: bytes, pid1: Path, work: Path, modules: dict[str, Path] | None = None) -> bytes:
    # Validate every stock archive before preserving it byte-for-byte. Android
    # initramfs commonly concatenates platform and vendor newc archives; the
    # kernel applies later members last, so a final archive installs /esuinitinit
    # without rewriting either stock archive.
    _ = list(records(ramdisk))

    root = work / "entry"
    root.mkdir()
    shutil.copyfile(pid1, root / "esuinit")
    (root / "esuinit").chmod(0o755)
    os.utime(root / "esuinit", (0, 0))
    members = ["esuinit"]
    if modules:
        (root / "lib").mkdir()
        members.append("lib")
        for name, source in sorted(modules.items()):
            target = root / "lib" / f"{name}.ko"
            shutil.copyfile(source, target)
            target.chmod(0o644)
            os.utime(target, (0, 0))
            members.append(f"lib/{name}.ko")

    addition = run(
        ["cpio", "--create", "--format=newc", "--owner=0:0", "--reproducible", "--quiet"],
        cwd=root,
        data=("\n".join(members) + "\n").encode(),
    )
    _ = list(records(addition))

    rebuilt = ramdisk + b"\0" * (-len(ramdisk) % 4) + addition
    rebuilt += b"\0" * (-len(rebuilt) % 512)
    return rebuilt


def repack_init_boot(
    stock: Path, pid1: Path, avbtool: Path, avb_key: Path, work: Path, modules: dict[str, Path] | None = None
) -> Path:
    """Install /esuinit and re-sign the fixed-size Cuttlefish init_boot."""
    original = stock.read_bytes()
    if original[:8] != BOOT_MAGIC or len(original) < BOOT_HEADER_SIZE:
        raise ValueError("stock init_boot is not an Android boot image")

    kernel_size, ramdisk_size = struct.unpack_from("<II", original, 8)
    header_size = struct.unpack_from("<I", original, 20)[0]
    version = struct.unpack_from("<I", original, 40)[0]

    if version not in HEADER_SIZES or kernel_size != 0 or header_size != HEADER_SIZES[version]:
        raise ValueError("only a kernel-free init_boot header v3/v4 is supported")
    if version == 4 and struct.unpack_from("<I", original, 1580)[0] != 0:
        raise ValueError("the stock init_boot carries a boot signature and must be unsigned first")
    if ramdisk_size == 0 or BOOT_HEADER_SIZE + ramdisk_size > len(original):
        raise ValueError("invalid stock ramdisk bounds")

    unpacked = work / "unpack"
    unpacked.mkdir()
    arguments = shlex.split(
        run(["unpack_bootimg", "--boot_img", stock, "--out", unpacked, "--format=mkbootimg"]).decode()
    )
    if arguments.count("--ramdisk") != 1:
        raise ValueError("unpack_bootimg did not emit exactly one --ramdisk argument")

    ramdisk = (unpacked / "ramdisk").read_bytes()
    if ramdisk != original[BOOT_HEADER_SIZE : BOOT_HEADER_SIZE + ramdisk_size]:
        raise ValueError("the unpacked ramdisk does not match the stock header bounds")

    mode, raw = decompress(ramdisk)
    replacement = work / "ramdisk"
    replacement.write_bytes(compress(mode, add_pid1(raw, pid1, work, modules)))

    arguments[arguments.index("--ramdisk") + 1] = str(replacement)
    image = work / "init_boot.img"
    run(["mkbootimg", *arguments, "--output", image])

    rebuilt = image.read_bytes()
    before, after = bytearray(original[:BOOT_HEADER_SIZE]), bytearray(rebuilt[:BOOT_HEADER_SIZE])
    before[12:16] = after[12:16] = b"\0" * 4  # ramdisk size is the only intended change
    if before != after:
        raise ValueError("mkbootimg changed stock header or version fields")
    if len(rebuilt) >= len(original):
        raise ValueError("the new init_boot leaves no room for its AVB footer")

    # Cuttlefish U-Boot verifies init_boot directly. Require the supplied key
    # to authenticate the stock image before using it to sign the replacement.
    run([avbtool, "verify_image", "--image", stock, "--key", avb_key])
    run(
        [
            avbtool,
            "add_hash_footer",
            "--image",
            image,
            "--partition_name",
            "init_boot",
            "--partition_size",
            str(len(original)),
            "--algorithm",
            "SHA256_RSA4096",
            "--key",
            avb_key,
        ]
    )
    run([avbtool, "verify_image", "--image", image, "--key", avb_key])
    if image.stat().st_size != len(original):
        raise ValueError("signed init_boot does not match the stock partition size")

    return image


def esp_image_size(content: int, requested_mib: int | None) -> int:
    if requested_mib is not None:
        if requested_mib < MIN_ESP_MIB:
            raise ValueError(f"--esp-size-mib must be at least {MIN_ESP_MIB}")
        size = requested_mib * 1024 * 1024
        if size < content + 1024 * 1024:
            raise ValueError(f"--esp-size-mib={requested_mib} leaves no room for {content} bytes of payload")
        return size

    needed = content + 16 * 1024 * 1024
    return max(DEFAULT_ESP_MIB * 1024 * 1024, -(-needed // (1024 * 1024)) * 1024 * 1024)


def artifact_generation(path: Path, generation: str) -> None:
    """Check the ELF note without running an Android binary on the host."""
    data = path.read_bytes()
    if data[:6] != b"\x7fELF\x02\x01" or len(data) < 64:
        raise ValueError(f"{path}: expected little-endian ELF64")
    kind, machine = struct.unpack_from("<HH", data, 16)
    if kind not in (2, 3) or machine not in (62, 183):
        raise ValueError(f"{path}: unsupported executable architecture/type")
    offset = struct.unpack_from("<Q", data, 40)[0]
    size, count, names_index = struct.unpack_from("<HHH", data, 58)
    if size != 64 or count == 0 or names_index >= count or offset + size * count > len(data):
        raise ValueError(f"{path}: malformed ELF section table")
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, offset + size * index) for index in range(count)]
    strings = sections[names_index]
    names = data[strings[4]:strings[4] + strings[5]]
    notes = []
    for section in sections:
        if section[0] >= len(names):
            raise ValueError(f"{path}: invalid section name")
        name = names[section[0]:].split(b"\0", 1)[0]
        if name == b".note.espinit":
            notes.append(data[section[4]:section[4] + section[5]])
    expected = struct.pack("<III", 8, 64, 1) + b"ESPINIT\0" + generation.encode().ljust(64, b"\0")
    if notes != [expected]:
        raise ValueError(f"{path}: missing, duplicate, malformed or mismatched generation note")


def platform_files(sources: dict[str, Path], generation: str, tree: Path) -> list[tuple[Path, str, int]]:
    """Use the checked-in module contract, not a second package format."""
    files = []
    for module in ("boot-hal", "tiny-espsu"):
        directory = tree / "modules" / module
        directory.mkdir()
        template = REPOSITORY / "esu/modules" / module / "module.toml"
        text = template.read_text()
        if text.count('generation = "release-1"') != 1:
            raise ValueError(f"invalid generation template: {template}")
        manifest = directory / "module.toml"
        manifest.write_text(text.replace('generation = "release-1"', f'generation = "{generation}"'))
        files.append((manifest, f"esu/modules/{module}/module.toml", 0o644))
    for key in ("esud", "boot_hal", "tiny_espsu"):
        artifact_generation(sources[key], generation)
    files.extend([
        (sources["boot_hal"], "esu/modules/boot-hal/android.hardware.boot-service.gblbds", 0o755),
        (REPOSITORY / "payloads/boot-hal/boot-gblbds.rc", "esu/modules/boot-hal/boot-gblbds.rc", 0o644),
        (sources["tiny_espsu"], "esu/modules/tiny-espsu/tiny-espsu", 0o755),
        (REPOSITORY / "esu/modules/tiny-espsu/install.sh", "esu/modules/tiny-espsu/install.sh", 0o755),
        (REPOSITORY / "esu/modules/tiny-espsu/policy.cil", "esu/modules/tiny-espsu/policy.cil", 0o644),
    ])
    return files


def build_esp(sources: dict[str, Path], manifest: str, rom: str, work: Path, requested_mib: int | None) -> Path:
    tree = work / "esu"
    import tomllib
    rom_relative = f"roms/{tomllib.loads(rom)['id']}.toml"
    for directory in ("bin", "roms", "modules/thin", "modules/fw-views", "receipts"):
        (tree / directory).mkdir(parents=True)
    (tree / "manifest.toml").write_text(manifest)
    (tree / rom_relative).write_text(rom)

    scripts = (("thin", EARLY_SCRIPT), ("fw-views", FW_EARLY_SCRIPT))
    for name, script in scripts:
        path = tree / "modules" / name / "early.sh"
        path.write_text(script)
        path.chmod(0o755)

    files: list[tuple[Path, str, int]] = [
        (tree / "manifest.toml", "esu/manifest.toml", 0o644),
        (tree / rom_relative, f"esu/{rom_relative}", 0o644),
    ]
    for name, _ in scripts:
        files.append(
            (
                tree / "modules" / name / "early.sh",
                f"esu/modules/{name}/early.sh",
                0o755,
            )
        )
    for key, target in BINARIES:
        files.append((sources[key], f"esu/{target}", 0o755))
    # Manifest generation is already validated by configurations().
    generation = tomllib.loads(manifest)["generation"]
    files.extend(platform_files(sources, generation, tree))

    missing = [directory for directory in ESP_DIRECTORIES if not (work / directory).is_dir()]
    if missing:
        raise ValueError(f"internal error: missing ESP directories {missing}")

    content = sum(source.stat().st_size for source, _, _ in files)
    image = work / "esp.img"
    with image.open("wb") as stream:
        stream.truncate(esp_image_size(content, requested_mib))

    # FAT carries no POSIX mode, so executable intent is expressed by the layout
    # and by the initramfs copy of the PID-1 binary.
    run(["mformat", "-i", image, "-v", "ESU", "-N", "45535031", "::"])
    run(["mmd", "-i", image, *(f"::/{directory}" for directory in ESP_DIRECTORIES)])
    for source, target, _ in files:
        run(["mcopy", "-i", image, "-o", source, f"::/{target}"])

    return image


def assemble(arguments: argparse.Namespace) -> None:
    manifest, rom = configurations(arguments.generation, arguments.metadata_filesystem, arguments.rom_id)

    sources = {name: Path(getattr(arguments, name)).resolve(strict=True) for name in PATHS}
    for name, source in sources.items():
        if not source.is_file() or source.stat().st_size == 0:
            raise ValueError(f"--{name.replace('_', '-')}: input must be a nonempty regular file")

    # Run shared KMI admission before creating/replacing any payload image.
    # Run before creating/replacing any payload image.
    run([
        sys.executable, REPOSITORY / "scripts/kmi_modules.py", "verify",
        "--kmi-out", arguments.kmi_out,
        *(item for key, name in MODULES for item in ("--module", sources[key])),
    ])

    output = Path(arguments.output_dir).absolute()
    if output.is_symlink():
        raise ValueError("the output directory must not be a symbolic link")
    output.mkdir(parents=True, exist_ok=True)

    present = sorted(path.name for path in output.iterdir())
    unexpected = [name for name in present if name not in ARTIFACTS]
    if unexpected or (present and not arguments.overwrite):
        raise ValueError(f"output directory is not empty ({present}); pass --overwrite to replace the payload")
    if any(source.parent == output.resolve() for source in sources.values()):
        raise ValueError("no input may live inside the output directory")

    with tempfile.TemporaryDirectory(
        prefix=f".{output.name}.assemble-", dir=output.parent
    ) as directory:
        work = Path(directory)
        init_boot = repack_init_boot(
            sources["stock_init_boot"],
            sources["esuinit"],
            sources["avbtool"],
            sources["avb_key"],
            work,
            {name: sources[key] for key, name in MODULES},
        )
        esp = build_esp(sources, manifest, rom, work, arguments.esp_size_mib)

        images: dict[str, dict[str, object]] = {}
        for path in (init_boot, esp):
            digest = hashlib.sha256()
            with path.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
            images[path.name] = {"sha256": digest.hexdigest(), "size": path.stat().st_size}

        receipt = work / "payload.json"
        receipt.write_text(
            json.dumps(
                {"schema_version": 1, "generation": arguments.generation, "images": images},
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )

        for path in (init_boot, esp, receipt):
            os.replace(path, output / path.name)


def parse_arguments(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in PATHS:
        parser.add_argument(f"--{name.replace('_', '-')}", required=True, metavar="FILE")
    parser.add_argument("--kmi-out", required=True, metavar="PATH")
    parser.add_argument("--metadata-filesystem", required=True, choices=("ext4", "f2fs"))
    parser.add_argument("--generation", required=True, metavar="ID")
    parser.add_argument("--rom-id", required=True, metavar="ID")
    parser.add_argument("--output-dir", required=True, metavar="DIR")
    parser.add_argument("--esp-size-mib", type=int, default=None, metavar="MIB")
    parser.add_argument("--overwrite", action="store_true")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    try:
        assemble(parse_arguments(argv))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"assemble: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr.decode(errors="replace").strip(), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
