#!/usr/bin/env python3
"""Build a static loader and admitted helper into an exact KMI/architecture set."""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile

from kmi_modules import Elf, require

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "aarch64": ("aarch64-linux-android", "aarch64", 183),
    "x86_64": ("x86_64-linux-android", "x86_64", 62),
}


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def build(args: argparse.Namespace) -> Path:
    branch = args.branch
    parts = branch.split("-")
    if len(parts) != 2 or not parts[0].startswith("android") or not parts[0][7:].isdigit():
        raise ValueError("branch must be android<version>-<kernel-major>.<kernel-minor>")
    kernel = parts[1].split(".")
    if len(kernel) != 2 or not all(part.isdigit() for part in kernel) or args.generation < 0:
        raise ValueError("invalid branch or generation")
    if not 1 <= args.jobs <= 13:
        raise ValueError("jobs must be in 1..13")
    destination = args.output.resolve() / f"{branch}-{args.generation}" / args.arch
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"artifact set already exists: {destination}")
    ndk = args.ndk.resolve(strict=True)
    prebuilt = ndk / "toolchains/llvm/prebuilt/linux-x86_64"
    triple, machine, elf_machine = TARGETS[args.arch]
    clang = prebuilt / "bin" / f"{triple}26-clang"
    if not clang.is_file():
        raise ValueError(f"missing Android compiler: {clang}")
    resources = Path(subprocess.check_output([str(clang), "--print-resource-dir"], text=True).strip())
    builtins = resources / "lib/linux" / f"libclang_rt.builtins-{machine}-android.a"
    if not builtins.is_file():
        raise ValueError(f"missing static Android builtins: {builtins}")
    env = os.environ.copy()
    env["ANDROID_NDK_HOME"] = str(ndk)
    env[f"CARGO_TARGET_{triple.upper().replace('-', '_')}_LINKER"] = str(clang)
    env[f"CC_{triple.replace('-', '_')}"] = str(clang)
    env[f"AR_{triple.replace('-', '_')}"] = str(prebuilt / "bin/llvm-ar")
    env["RUSTFLAGS"] = " ".join([
        "-C target-feature=+crt-static", "-C link-arg=-Wl,-z,max-page-size=16384",
        f"-C link-arg={builtins}",
    ])
    # A private target directory keeps the static link flags separate from host/native builds.
    target = ROOT / "out/rdinit-target"
    env["CARGO_TARGET_DIR"] = str(target)
    subprocess.run([
        "cargo", "build", "--locked", "--manifest-path", str(ROOT / "Cargo.toml"),
        "--release", "--package", "egyskinit", "--target", triple, "--jobs", str(args.jobs),
    ], env=env, check=True, cwd=ROOT)
    subprocess.run([
        "python3", str(ROOT / "scripts/kmi_modules.py"), "build", "--module-dir", str(ROOT / "kernel"),
        "--module", "egysk", "--branch", branch, "--generation", str(args.generation),
        "--arch", args.arch, "--kmi-src", str(args.kmi_src.resolve(strict=True)),
        "--kmi-out", str(args.kmi_out.resolve(strict=True)), "--jobs", str(args.jobs),
    ], check=True, cwd=ROOT)
    inputs = {
        "egyskinit": target / triple / "release/egyskinit",
        "egysk.ko": ROOT / "kernel/egysk.ko",
        "egysk.ko.compat.json": ROOT / "kernel/egysk.ko.compat.json",
    }
    entry = Elf(inputs["egyskinit"])
    require(entry.kind in (2, 3) and entry.machine == elf_machine, "rdinit: wrong executable architecture")
    address, offset = struct.unpack_from("<QQ", entry.data, 24)
    size, count = struct.unpack_from("<HH", entry.data, 54)
    require(size == 56 and count > 0, "rdinit: invalid program headers")
    segments = list(struct.iter_unpack("<IIQQQQQQ", entry.slice(offset, size * count)))
    require(all(segment[0] != 3 for segment in segments), "rdinit: interpreter is forbidden")
    require(any(segment[0] == 1 and segment[1] & 1 and segment[3] <= address < segment[3] + segment[6]
                for segment in segments), "rdinit: entry is not executable")
    receipt = json.loads(inputs["egysk.ko.compat.json"].read_text())
    if receipt["kmi"] != {"branch": branch, "generation": args.generation}:
        raise ValueError("helper gate returned a different KMI")
    if receipt["module_sha256"] != digest(inputs["egysk.ko"]):
        raise ValueError("helper changed after admission")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".core-artifacts-", dir=destination.parent) as temporary:
        staged = Path(temporary) / "set"
        staged.mkdir()
        for name, source in inputs.items():
            shutil.copyfile(source, staged / name)
            (staged / name).chmod(0o755 if name == "egyskinit" else 0o644)
            if digest(staged / name) != digest(source):
                raise ValueError(f"artifact changed while publishing: {source}")
            with (staged / name).open("rb") as copied:
                os.fsync(copied.fileno())
        descriptor = os.open(staged, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        # Publish all three files atomically without replacing even an empty existing set.
        libc = ctypes.CDLL(None, use_errno=True)
        rename = libc.renameat2
        rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
        rename.restype = ctypes.c_int
        if rename(-100, os.fsencode(staged), -100, os.fsencode(destination), 1) != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), str(destination))
        descriptor = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kmi-src", required=True, type=Path)
    parser.add_argument("--kmi-out", required=True, type=Path)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--generation", required=True, type=int)
    parser.add_argument("--arch", required=True, choices=TARGETS)
    parser.add_argument("--ndk", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--jobs", type=int, default=13)
    args = parser.parse_args()
    try:
        print(build(args))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"build-artifacts: {error}\n")


if __name__ == "__main__":
    main()
