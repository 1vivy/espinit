#!/usr/bin/env python3
"""Materialize the pinned native product exactly, then optionally build egyskd/policy.

The pristine submodule is never patched. Hunk positions and every old/context byte
must match; unlike git apply/patch this deliberately has no offset or fuzz search.
Run `materialize --check` to compare a generated tree to the production series.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

PIN = "e8915d9db15f5aae93973ffe65068e34df375a6a"
ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor/Magisk"
PATCHES = ROOT / "patches/Magisk"
PRODUCT = ROOT / "product"
DEFAULT_OUTPUT = ROOT / "out/Magisk"
HUNK = re.compile(rb"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?:.*)\n$")


def run(*args: object, cwd: Path | None = None, env: dict | None = None) -> None:
    subprocess.run([str(x) for x in args], cwd=cwd, env=env, check=True)


def git_bytes(*args: str, cwd: Path = VENDOR) -> bytes:
    return subprocess.check_output(["git", "-C", str(cwd), *args])


def safe_path(raw: bytes, prefix: bytes) -> str:
    if not raw.startswith(prefix):
        raise ValueError(f"invalid patch path: {raw!r}")
    name = raw[len(prefix):].decode("utf-8")
    path = PurePosixPath(name)
    if not name or path.is_absolute() or any(p in ("..", ".git") for p in path.parts):
        raise ValueError(f"unsafe patch path: {name}")
    return name


def exact_patch(tree: Path, patch: Path) -> set[str]:
    lines = patch.read_bytes().splitlines(keepends=True)
    i = 0
    changed = set()
    while i < len(lines):
        if not lines[i].startswith(b"diff --git "):
            raise ValueError(f"{patch.name}: expected diff header at line {i + 1}")
        i += 1
        mode = None
        while i < len(lines) and not lines[i].startswith(b"--- "):
            line = lines[i]
            if line.startswith(b"new file mode "):
                mode = int(line.split()[-1], 8)
            elif not line.startswith(b"index "):
                raise ValueError(f"unsupported patch metadata: {line!r}")
            i += 1
        old_name = lines[i][4:].rstrip(b"\n")
        i += 1
        if not lines[i].startswith(b"+++ "):
            raise ValueError("missing new path")
        name = safe_path(lines[i][4:].rstrip(b"\n"), b"b/")
        if old_name != b"/dev/null" and safe_path(old_name, b"a/") != name:
            raise ValueError("rename patches are not supported")
        dest = tree / name
        if dest.is_symlink():
            raise ValueError(f"refusing symlink patch target: {name}")
        old = [] if old_name == b"/dev/null" else dest.read_bytes().splitlines(keepends=True)
        if old_name == b"/dev/null" and dest.exists():
            raise ValueError(f"new patch target already exists: {name}")
        result = []
        cursor = 0
        i += 1
        hunks = 0
        while i < len(lines) and not lines[i].startswith(b"diff --git "):
            match = HUNK.fullmatch(lines[i])
            if not match:
                raise ValueError(f"invalid hunk header: {lines[i]!r}")
            start, count, new_start, new_count = [int(v) if v is not None else 1 for v in match.groups()]
            offset = start - 1 if count else start
            if offset < cursor or offset > len(old):
                raise ValueError(f"{name}: overlapping/out-of-bounds hunk")
            result.extend(old[cursor:offset])
            expected_new = new_start - 1 if new_count else new_start
            if len(result) != expected_new:
                raise ValueError(f"{name}: new hunk offset mismatch")
            cursor = offset
            consumed = produced = 0
            i += 1
            while i < len(lines) and lines[i][:1] in (b" ", b"-", b"+"):
                op, data = lines[i][:1], lines[i][1:]
                i += 1
                if i < len(lines) and lines[i] == b"\\ No newline at end of file\n":
                    data = data.removesuffix(b"\n")
                    i += 1
                if op != b"+":
                    if cursor >= len(old) or old[cursor] != data:
                        raise ValueError(f"{name}:{cursor + 1}: exact patch context mismatch")
                    cursor += 1
                    consumed += 1
                if op != b"-":
                    result.append(data)
                    produced += 1
            if consumed != count or produced != new_count:
                raise ValueError(f"{name}: hunk count mismatch")
            hunks += 1
        if not hunks:
            raise ValueError(f"{name}: empty patch")
        result.extend(old[cursor:])
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"".join(result))
        if mode is not None:
            dest.chmod(mode & 0o777)
        changed.add(name)
    return changed


def series() -> list[Path]:
    names = [line.strip() for line in (PATCHES / "series").read_text().splitlines()
             if line.strip() and not line.startswith("#")]
    if not names or len(names) != len(set(names)):
        raise ValueError("empty or duplicate patch series")
    for name in names:
        if Path(name).name != name or not name.endswith(".patch"):
            raise ValueError(f"invalid series entry: {name}")
    return [PATCHES / name for name in names]


def install_product(tree: Path) -> set[str]:
    """Install owned adapter and derive both native identities from one source."""
    values = json.loads((PRODUCT / "identity.json").read_text())
    if values.get("EGYSK_PRODUCT") is not True:
        raise ValueError("owned native sources require EGYSK_PRODUCT")
    rust = "//! Generated from product/identity.json by scripts/magisk.py.\n"
    rust += "".join(
        f"pub const {key}: {'bool' if isinstance(value, bool) else '&str'} = {json.dumps(value)};\n"
        for key, value in values.items()
    )
    mapping = {
        "JAVA_PACKAGE_NAME": "APP_PACKAGE_NAME", "SECURE_DIR": "SECURE_DIR",
        "MODULEROOT": "MODULEROOT", "DATABIN": "DATABIN", "MAGISKDB": "MAGISKDB",
        "INTLROOT": "INTERNAL_DIR", "SEPOL_PROC_DOMAIN": "SEPOL_PROC_DOMAIN",
        "SEPOL_FILE_TYPE": "SEPOL_FILE_TYPE", "EGYSK_PRODUCT": "EGYSK_PRODUCT",
    }
    cpp = "// Generated from product/identity.json by scripts/magisk.py.\n#pragma once\n"
    cpp += "".join(
        f"#define {key} {int(values[value]) if isinstance(values[value], bool) else json.dumps(values[value])}\n"
        for key, value in mapping.items()
    )
    destination = tree / "product"
    destination.mkdir()
    for name in ("identity.json", "dispatch.rs"):
        shutil.copy2(PRODUCT / name, destination / name)
    for name, contents in (("identity.rs", rust), ("identity.hpp", cpp)):
        (destination / name).write_text(contents)
    return {f"product/{name}" for name in ("identity.json", "dispatch.rs", "identity.rs", "identity.hpp")}


def materialize(output: Path, check: bool, dependencies: bool) -> None:
    if git_bytes("rev-parse", "HEAD").decode().strip() != PIN:
        raise ValueError(f"vendor/Magisk must be pinned at {PIN}")
    if git_bytes("status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"):
        raise ValueError("vendor/Magisk has tracked modifications")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".magisk-", dir=output.parent) as temp:
        tree = Path(temp) / "source"
        run("git", "clone", "--shared", "--no-checkout", VENDOR, tree)
        run("git", "checkout", "--detach", PIN, cwd=tree)
        changed = set()
        for patch in series():
            changed.update(exact_patch(tree, patch))
        changed.update(install_product(tree))
        if output.exists():
            # Compare all tracked regular sources, not just a marker or the edited
            # files. Build output/untracked toolchain files are intentionally ignored.
            tracked = git_bytes("ls-files", "-z", cwd=tree).decode().split("\0")
            for name in sorted(set(tracked) | changed):
                if not name or not (tree / name).is_file():
                    continue
                source, actual = tree / name, output / name
                if (not actual.is_file() or actual.is_symlink() != source.is_symlink()
                        or source.read_bytes() != actual.read_bytes()
                        or (source.stat().st_mode & 0o111) != (actual.stat().st_mode & 0o111)):
                    raise ValueError(f"generated source differs: {actual}; preserve it and use a fresh --output")
            print(f"Exact materialization matches {output}")
        else:
            if check:
                raise ValueError(f"missing generated tree: {output}")
            tree.rename(output)
            print(f"Materialized {PIN} + {len(series())} exact patches at {output}")
    if dependencies:
        run("git", "submodule", "update", "--init", "--recursive", cwd=output)


def build(args: argparse.Namespace) -> None:
    materialize(args.output, False, True)
    ndk = args.ndk.resolve()
    if not (ndk / "ndk-build").is_file():
        raise ValueError(f"missing provisioned ONDK: {ndk}")
    env = os.environ.copy()
    env["MAGISK_ONDK"] = str(ndk)
    env.setdefault("ANDROID_HOME", str(ndk.parent.parent))
    config = args.output / "egysk-build.prop"
    config.write_text(f"abiList={args.abi}\nversion=egysk-{PIN[:8]}\nversionCode=1000000\n")
    command = [sys.executable, args.output / "build.py", "-v", "-c", config]
    if not args.debug:
        command.append("-r")
    run(*command, "native", "magisk", "magiskpolicy", cwd=args.output, env=env)
    destination = args.artifacts.resolve() / args.abi / "bin"
    destination.mkdir(parents=True, exist_ok=True)
    for name in ("egyskd", "magiskpolicy"):
        shutil.copy2(args.output / "native/out" / args.abi / name, destination / name)
    flags = (args.output / "native/out/generated/flags.h").read_text()
    version = re.search(r'^#define MAGISK_VERSION\s+"([^"]+)"$', flags, re.MULTILINE)
    code = re.search(r"^#define MAGISK_VER_CODE\s+(\d+)$", flags, re.MULTILINE)
    if version is None or code is None:
        raise ValueError("missing generated native version flags")
    helper = (args.output / "scripts/util_functions.sh").read_text()
    if helper.count("#MAGISK_VERSION_STUB") != 1:
        raise ValueError("installer version injection marker changed")
    helper = helper.replace(
        "#MAGISK_VERSION_STUB",
        f"MAGISK_VER={shlex.quote(version[1])}\nMAGISK_VER_CODE={code[1]}",
    )
    (destination / "util_functions.sh").write_text(helper)
    (destination / "util_functions.sh").chmod(0o755)
    print(f"Native package inputs: {destination} (BusyBox supplied independently)")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    actions = parser.add_subparsers(dest="action", required=True)
    material = actions.add_parser("materialize")
    material.add_argument("--check", action="store_true")
    material.add_argument("--init-dependencies", action="store_true")
    native = actions.add_parser("build")
    native.add_argument("--ndk", type=Path, required=True, help="provisioned Magisk ONDK r30.1")
    native.add_argument("--abi", choices=("arm64-v8a", "armeabi-v7a", "x86_64", "x86", "riscv64"), default="arm64-v8a")
    native.add_argument("--debug", action="store_true")
    native.add_argument("--artifacts", type=Path, default=ROOT / "out/native")
    args = parser.parse_args()
    args.output = args.output.resolve()
    # The library path in the patch is deliberately relative to out/<tree>.
    if args.output.parent != ROOT / "out":
        parser.error("--output must be an immediate child of the repository out directory")
    try:
        if args.action == "materialize":
            materialize(args.output, args.check, args.init_dependencies)
        else:
            build(args)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"magisk.py: {error}\n")


if __name__ == "__main__":
    main()
