#!/usr/bin/env python3
"""Build real CF modules in a disposable source copy, retaining KMI receipts."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--kmi-out', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--only', choices=('kernelesp', 'thin', 'gpt', 'efivarfs', 'efivar_store'))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    args.output.mkdir(parents=True, exist_ok=True)
    spec = importlib.util.spec_from_file_location('kmi_modules', root / 'scripts/kmi_modules.py')
    assert spec is not None and spec.loader is not None
    verifier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(verifier)
    env = os.environ.copy()
    for name, directory in verifier.MODULES.items():
        if args.only and name != args.only:
            continue
        isolated = args.output / 'source' / name
        if name == 'efivar_store':
            shutil.copytree(directory / '.source', isolated, dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns('.git', 'target'))
            isolated = isolated / 'linux'
        else:
            shutil.copytree(directory, isolated, dirs_exist_ok=True)
        command = ['make', '-C', str(args.source), f'O={args.kmi_out}', f'M={isolated}',
                   'ARCH=x86_64', 'LLVM=1', 'KBUILD_GENDWARFKSYMS_STABLE=1',
                   'KBUILD_MODPOST_WARN=1', 'CONFIG_KERNELESP=m',
                   'CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER=y']
        if name == 'efivar_store':
            command += ['RUST_TARGET=x86_64-unknown-none',
                        ('RUST_FLAGS=-Z unstable-options -C relocation-model=static '
                         '-C panic=immediate-abort -C force-unwind-tables=no '
                         '-C embed-bitcode=no -C no-redzone=yes -C code-model=kernel -Z plt=yes')]
        subprocess.run(command + ['clean'], env=env, check=True)
        subprocess.run(command + ['modules', '-j6'], env=env, check=True)
        module = args.output / f'{name}.ko'
        shutil.copyfile(isolated / module.name, module)
        imports = verifier.verify_module(module, name, args.kmi_out)
        receipt = verifier.provenance(args.kmi_out, module, verifier.kmi_identity(args.source), imports)
        verifier.receipt_path(module).write_text(json.dumps(receipt, indent=2) + '\n')


if __name__ == '__main__':
    main()
