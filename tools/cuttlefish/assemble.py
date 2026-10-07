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
    "boot_hal",  # optional: omit to assemble the payload without the Boot HAL module
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
GENERATED_MODULE_FILES = ("pid1.sh", "pid1-recovery.sh")
ESP_DIRECTORIES = (
    "esu",
    "esu/bin",
    "esu/roms",
    "esu/modules",
    "esu/modules/thin",
    "esu/modules/fw-views",
    "esu/receipts",
)
BOOT_HAL_DIRECTORIES = (
    "esu/modules/boot-hal",
    "esu/modules/boot-hal/vendor",
    "esu/modules/boot-hal/vendor/bin",
    "esu/modules/boot-hal/vendor/bin/hw",
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


def configurations(metadata_filesystem: str, rom_id: str, *, boot_hal: bool = False) -> tuple[str, str]:
    """Render the manifest and the structurally valid placeholder ROM config."""
    if metadata_filesystem not in ("ext4", "f2fs"):
        raise ValueError("metadata filesystem must be explicitly ext4 or f2fs")
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,59}", rom_id) or rom_id in (".", ".."):
        raise ValueError("ROM ID must be 1..59 ASCII letters/digits plus . _ -, excluding . and ..")

    head = 'schema_version = 1\n'
    order = ["thin", "fw-views"]
    if boot_hal:
        order.insert(0, "boot-hal")
    manifest = head + f'rom = "roms"\nmodules_order = {json.dumps(order)}\n'
    for name, path in (
        ("kernelesp", "lib/kernelesp.ko"),
        ("thin", "lib/thin.ko"),
        ("gpt", "lib/gpt.ko"),
        ("efivarfs", "lib/efivarfs.ko"),
    ):
        params = "dev=by-name:bdsvars" if name == "efivarfs" else ""
        manifest += f'\n[[modules]]\nname = "{name}"\npath = "{path}"\nparams = "{params}"\n'

    # Valid managed shape with a deliberately impossible backend: the lab lane
    # replaces this file with the complete generated GPT projection before boot.
    # One guest per payload; bdsvars supplies the runtime ROM number.
    rom = head + f'id = "{rom_id}"\n' + (
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


def add_pid1(ramdisk: bytes, pid1: Path, work: Path, modules: dict[str, Path], build_id: str) -> bytes:
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
    (root / "esu-build-id").write_text(build_id + "\n")
    (root / "esu-build-id").chmod(0o644)
    os.utime(root / "esu-build-id", (0, 0))
    members = ["esuinit", "esu-build-id"]
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
    stock: Path, pid1: Path, avbtool: Path, avb_key: Path, work: Path, modules: dict[str, Path], build_id: str
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
    replacement.write_bytes(compress(mode, add_pid1(raw, pid1, work, modules, build_id)))

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


def module_metadata(module: str) -> list[Path]:
    """Regular files the module ships at its root, in name order.

    A flag such as `critical`, `disable`, `remove` or `skip_mount` travels
    because the file's presence is the flag; the assembler generates the stage
    scripts itself, so those are not copied from the module directory.
    """
    directory = REPOSITORY / "esu/modules" / module
    return sorted(
        (path for path in directory.iterdir() if path.is_file() and path.name not in GENERATED_MODULE_FILES),
        key=lambda path: path.name,
    )


def platform_files(sources: dict[str, Path], tree: Path) -> list[tuple[Path, str, int]]:
    """Package thin/fw-views and, when supplied, the ordinary Boot HAL module."""
    files: list[tuple[Path, str, int]] = []
    modules = ["thin", "fw-views"]
    if "boot_hal" in sources:
        (tree / "modules/boot-hal/vendor/bin/hw").mkdir(parents=True)
        modules.insert(0, "boot-hal")
        files.append((sources["boot_hal"], "esu/modules/boot-hal/vendor/bin/hw/android.hardware.boot-service.qti", 0o755))
    for module in modules:
        for path in module_metadata(module):
            files.append((path, f"esu/modules/{module}/{path.name}", 0o644))
    return files


def build_esp(sources: dict[str, Path], manifest: str, rom: str, work: Path, requested_mib: int | None, build_id: str) -> Path:
    tree = work / "esu"
    import tomllib
    rom_relative = f"roms/{tomllib.loads(rom)['id']}.toml"
    for directory in ("bin", "roms", "modules/thin", "modules/fw-views", "receipts"):
        (tree / directory).mkdir(parents=True)
    (tree / "manifest.toml").write_text(manifest)
    (tree / rom_relative).write_text(rom)
    (tree / "build-id").write_text(build_id + "\n")

    scripts = (("thin", EARLY_SCRIPT), ("fw-views", FW_EARLY_SCRIPT))
    for name, script in scripts:
        for stage in ("pid1.sh", "pid1-recovery.sh"):
            path = tree / "modules" / name / stage
            path.write_text(script)
            path.chmod(0o755)

    files: list[tuple[Path, str, int]] = [
        (tree / "manifest.toml", "esu/manifest.toml", 0o644),
        (tree / rom_relative, f"esu/{rom_relative}", 0o644),
        (tree / "build-id", "esu/build-id", 0o644),
    ]
    for name, _ in scripts:
        for stage in ("pid1.sh", "pid1-recovery.sh"):
            files.append((tree / "modules" / name / stage, f"esu/modules/{name}/{stage}", 0o755))
    for key, target in BINARIES:
        files.append((sources[key], f"esu/{target}", 0o755))
    files.extend(platform_files(sources, tree))

    directories = ESP_DIRECTORIES + (BOOT_HAL_DIRECTORIES if "boot_hal" in sources else ())
    missing = [directory for directory in directories if not (work / directory).is_dir()]
    if missing:
        raise ValueError(f"internal error: missing ESP directories {missing}")

    content = sum(source.stat().st_size for source, _, _ in files)
    image = work / "esp.img"
    with image.open("wb") as stream:
        stream.truncate(esp_image_size(content, requested_mib))

    # FAT carries no POSIX mode, so executable intent is expressed by the layout
    # and by the initramfs copy of the PID-1 binary.
    run(["mformat", "-i", image, "-v", "ESU", "-N", "45535031", "::"])
    run(["mmd", "-i", image, *(f"::/{directory}" for directory in directories)])
    for source, target, _ in files:
        run(["mcopy", "-i", image, "-o", source, f"::/{target}"])

    return image


def build_identity(sources: dict[str, Path], manifest: str, rom: str) -> tuple[str, dict[str, str]]:
    """SHA256 of sorted artifact SHA256 values, each followed by LF."""
    inputs = {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in sources.items()}
    inputs.update({name: hashlib.sha256(text.encode()).hexdigest() for name, text in
                   (("manifest", manifest), ("rom", rom))})
    for module, script in (("thin", EARLY_SCRIPT), ("fw-views", FW_EARLY_SCRIPT)):
        for stage in ("pid1.sh", "pid1-recovery.sh"):
            inputs[f"{module}/{stage}"] = hashlib.sha256(script.encode()).hexdigest()
    modules = ("boot-hal", "thin", "fw-views") if "boot_hal" in sources else ("thin", "fw-views")
    for module in modules:
        for path in module_metadata(module):
            inputs[f"{module}/{path.name}"] = hashlib.sha256(path.read_bytes()).hexdigest()
    return hashlib.sha256("".join(value + "\n" for value in sorted(inputs.values())).encode()).hexdigest()[:12], inputs


def assemble(arguments: argparse.Namespace) -> None:
    # --boot-hal is the one optional input: absent means the payload carries no
    # Boot HAL module at all. Every other PATHS entry is required.
    boot_hal = getattr(arguments, "boot_hal", None)
    sources = {
        name: Path(getattr(arguments, name)).resolve(strict=True)
        for name in PATHS
        if name != "boot_hal" or boot_hal is not None
    }
    manifest, rom = configurations(arguments.metadata_filesystem, arguments.rom_id, boot_hal="boot_hal" in sources)
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
        build_id, build_id_inputs = build_identity(sources, manifest, rom)
        init_boot = repack_init_boot(
            sources["stock_init_boot"],
            sources["esuinit"],
            sources["avbtool"],
            sources["avb_key"],
            work,
            {name: sources[key] for key, name in MODULES},
            build_id,
        )
        esp = build_esp(sources, manifest, rom, work, arguments.esp_size_mib, build_id)

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
                {"schema_version": 1, "build_id": build_id, "build_id_inputs": build_id_inputs, "images": images},
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
        parser.add_argument(
            f"--{name.replace('_', '-')}", required=name != "boot_hal", metavar="FILE",
            help="optional Boot HAL replacement binary" if name == "boot_hal" else None,
        )
    parser.add_argument("--kmi-out", required=True, metavar="PATH")
    parser.add_argument("--metadata-filesystem", required=True, choices=("ext4", "f2fs"))
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
