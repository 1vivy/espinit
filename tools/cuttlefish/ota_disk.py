"""Offline disposable CF disk setup. Uses real rom-bootgen and scoped host LVM."""
from __future__ import annotations

import json
import struct
import subprocess
import uuid
import zlib
from pathlib import Path

import tomllib

IMAGE_BASES = {'boot', 'init_boot', 'vendor_boot', 'vendor_kernel_boot', 'dtb',
               'dtbo', 'pvmfw', 'vbmeta', 'vbmeta_system', 'vbmeta_vendor'}
GUID = '7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b'


def firmware_bases(names: set[str]) -> list[str]:
    bases = sorted(name[:-2] for name in names if name.endswith('_a') and
                   name[:-2] + '_b' in names and name[:-2] not in IMAGE_BASES | {'super', 'recovery'})
    if len(bases) < 2:
        raise ValueError(f'CF GPT has fewer than two eligible firmware A/B pairs: {bases}')
    return bases[:2]


def augment_rom(generated: str, physical: str) -> str:
    config = tomllib.loads(generated)
    existing = {entry['name'] for entry in config['partitions']}
    lines = [generated]
    for entry in tomllib.loads(physical)['partitions']:
        # Image roles may only be declared by rom-bootgen's operator image set.
        # Leave undeclared physical image partitions stock, not managed aliases.
        name = entry['name']
        if name.endswith(('_a', '_b')) and name[:-2] in IMAGE_BASES:
            continue
        if entry['name'] not in existing:
            lines.extend(['\n[[partitions]]', f'name = {json.dumps(entry["name"])}',
                          f'backend = {json.dumps(entry["backend"])}', 'read_only = false'])
    result = '\n'.join(lines) + '\n'
    if len(tomllib.loads(result)['partitions']) > 128:
        raise ValueError('CF projection count exceeds 128')
    return result


def split_frp_gpt(data: bytes, parsed) -> bytes:
    """Keep stock FRP in the first half and add an independent EFVS partition."""
    source = None
    empty = None
    for index in range(parsed.entry_count):
        offset = parsed.entries_offset + index * parsed.entry_size
        if data[offset:offset + 16] == bytes(16):
            if empty is None:
                empty = offset
            continue
        name = data[offset + 56:offset + 128].decode('utf-16le').split('\0', 1)[0]
        if name == 'bdsvars':
            raise ValueError('bdsvars GPT entry already exists')
        if name == 'frp':
            if source is not None:
                raise ValueError('multiple FRP GPT entries')
            source = offset
    if source is None or empty is None:
        raise ValueError('FRP or free GPT entry is missing')
    first, last = struct.unpack_from('<QQ', data, source + 32)
    if last - first + 1 != 2048:
        raise ValueError('CF FRP must be exactly 1 MiB')
    middle = first + 1024
    result = bytearray(data)
    struct.pack_into('<Q', result, source + 40, middle - 1)
    result[empty:empty + parsed.entry_size] = data[source:source + parsed.entry_size]
    original_guid = uuid.UUID(bytes_le=data[source + 16:source + 32])
    result[empty + 16:empty + 32] = uuid.uuid5(original_guid, 'cf-ota-bdsvars').bytes_le
    struct.pack_into('<Q', result, empty + 32, middle)
    result[empty + 56:empty + 128] = 'bdsvars'.encode('utf-16le').ljust(72, b'\0')
    array_end = parsed.entries_offset + parsed.array_bytes
    struct.pack_into('<I', result, parsed.header_offset + 88,
                     zlib.crc32(result[parsed.entries_offset:array_end]))
    struct.pack_into('<I', result, parsed.header_offset + 16, 0)
    struct.pack_into('<I', result, parsed.header_offset + 16,
                     zlib.crc32(result[parsed.header_offset:parsed.header_offset + parsed.header_size]))
    return bytes(result)


def run(argv: list[str | Path]) -> str:
    print('$', ' '.join(str(arg) for arg in argv), flush=True)
    result = subprocess.run([str(arg) for arg in argv], check=False, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, flush=True)
    result.check_returncode()
    return result.stdout


def prepare(session, payload, adapted, projects: Path, artifacts: Path) -> None:
    """Initialize only session.state/images, never the cache or a physical disk."""
    from lab.android.command_log import Exec
    from lab.android.cuttlefish_runtime import (
        runtime_bytes,
        runtime_path,
        single_runtime_file,
    )
    from lab.android.esu_payload import (
        FOOTER_COMPONENT,
        HEADER_COMPONENT,
        PERSISTENT_FOOTER,
        PERSISTENT_HEADER,
    )
    from lab.android.gpt_identity import component, matching_copies, render_rom

    state = session.state
    header = single_runtime_file(state / 'runtime', HEADER_COMPONENT)
    footer = single_runtime_file(state / 'runtime', FOOTER_COMPONENT)
    if header is None or footer is None:
        raise ValueError('missing assembled GPT')
    partitions = matching_copies(runtime_bytes(session, header), runtime_bytes(session, footer))
    persistent_header = single_runtime_file(state / 'runtime', PERSISTENT_HEADER)
    persistent_footer = single_runtime_file(state / 'runtime', PERSISTENT_FOOTER)
    if persistent_header is None or persistent_footer is None:
        raise ValueError('missing persistent GPT components')
    persistent = matching_copies(runtime_bytes(session, persistent_header),
                                 runtime_bytes(session, persistent_footer))
    partitions = (*partitions, *persistent)
    inventory = state / 'firmware-bases.txt'
    inventory.write_text('\n'.join(firmware_bases({p.name for p in partitions})) + '\n')
    userdata = state / 'images/userdata.img'
    loop = run(['sudo', '-n', 'losetup', '--find', '--show', '--sector-size', '4096', userdata]).strip()
    conf = f'devices {{ use_devicesfile=0 filter=["a|^{loop}$|","r|.*|"] }} activation {{ udev_sync=0 udev_rules=0 }}'
    def lvm(*args: str) -> str:
        return run(['sudo', '-n', 'lvm', *args, '--config', conf])
    created = False
    try:
        # Refuse a host VG collision rather than filtering a second VG named rom.
        collision = subprocess.run(['sudo', '-n', 'vgs', '--noheadings', '-o', 'vg_name'],
                                   check=True, capture_output=True, text=True).stdout.split()
        if 'rom' in collision:
            raise ValueError('host VG rom already exists; refusing CF provisioning')
        lvm('pvcreate', '--yes', loop)
        lvm('vgcreate', 'rom', loop)
        created = True
        free, extent = (int(float(v)) for v in lvm('vgs', '--noheadings', '--units', 'b',
                        '--nosuffix', '-o', 'vg_free_count,vg_extent_size', 'rom').split())
        held = -(-(1024**3) // extent) + 2 * -(-(64 * 1024**2) // extent)
        if free <= held:
            raise ValueError('VG too small for the 1 GiB staging reserve')
        lvm('lvcreate', '--yes', '--type', 'thin-pool', '-l', str(free - held),
            '--poolmetadatasize', '64m', '--zero', 'n', '-n', 'pool', 'rom')
        reserve = int(float(lvm('vgs', '--noheadings', '--units', 'b', '--nosuffix', '-o', 'vg_free', 'rom').strip()))
        if reserve < 1024**3:
            raise ValueError(f'VG reserve is only {reserve}')
        image = state / 'rom2-esp.img'
        image.touch(exist_ok=False)
        with image.open('wb') as stream:
            stream.truncate(512 * 1024**2)
        run(['mkfs.fat', '-F', '32', '-S', '4096', image])
        producer = payload.root / 'producer'
        command = ['sudo', '-n', projects / 'gobbl/target/release/rom-bootgen', image,
                   '--sector-size', '4096', '--id', 'rom2', '--number', '2',
                   '--userdata-dev', loop, '--cpio', producer / 'esu.cpio',
                   '--payload-root', producer / 'esp/esu', '--super-dev', state / 'images/super.img',
                   '--firmware-bases', inventory, '--userdata-size', str(4 * 1024**3)]
        for base in ('boot', 'init_boot', 'vendor_boot'):
            source = state / 'images' / f'{base}.img'
            # Preserve stock updater source bytes, not the appended launcher overlay.
            if base == 'init_boot':
                source = payload.root / 'stock-init_boot.img'
            command.extend(['--image', f'{base}={source}', '--size', f'{base}={source.stat().st_size}'])
        run(command)
        config = run(['mtype', '-i', image, '::/esu/roms/rom2.toml'])
        configured = state / 'rom2.toml'
        physical = render_rom(partitions, 'rom2').replace(
            'backend = "/dev/block/by-name/frp"', 'backend = "/dev/block/by-name/bdsvars"')
        physical += ('\n[[partitions]]\nname = "frp"\n'
                     'backend = "/dev/block/by-name/frp"\nread_only = false\n')
        configured.write_text(augment_rom(config, physical))
        run(['mcopy', '-o', '-i', image, configured, '::/esu/roms/rom2.toml'])
        lvm('lvchange', '-ay', 'rom/super_2', 'rom/metadata_2', 'rom/userdata_2')
        run(['sudo', '-n', 'dd', f'if={state / "images/super.img"}', 'of=/dev/rom/super_2',
             'bs=4M', 'conv=sparse,fsync'])
        run(['sudo', '-n', 'mkfs.ext4', '-F', '/dev/rom/metadata_2'])
        run(['sudo', '-n', 'mkfs.ext4', '-F', '/dev/rom/userdata_2'])
        run(['cp', image, adapted.installed_esp])
    finally:
        # Deactivate only this scoped VG; never remove host metadata.
        try:
            if created:
                subprocess.run(['sudo', '-n', 'lvm', 'vgchange', '-an', 'rom', '--config', conf], check=True)
        finally:
            run(['sudo', '-n', 'losetup', '-d', loop])
    store = artifacts / 'efvs'
    frp_component = single_runtime_file(state / 'runtime', 'factory_reset_protected.img')
    if frp_component is None:
        raise ValueError('missing disposable FRP component')
    original_frp = runtime_bytes(session, frp_component)
    if len(original_frp) != 1024**2:
        raise ValueError('CF FRP component must be exactly 1 MiB')
    frp = state / 'bdsvars-seed.img'
    run([store, 'init', '--efvs', '--image', frp, '--size', str(512 * 1024)])
    slot = b'GBS1\x02\0\0\0\0\xff\0\0\x0f\x07\x01' + bytes(9)
    for name, data in [('BootedRom', b'rom2\0'), ('Slot-rom2', slot),
                       ('Stage-rom2', b'GBT1' + bytes(4)), ('MergeStatus-rom2', b'GBM1' + bytes(4))]:
        seed = state / f'{name}.bin'
        seed.write_bytes(data)
        run([store, 'set', '--image', frp, '--name', name, '--guid', GUID,
             '--attributes', '7', '--data-file', seed])
    combined = state / 'frp-split.img'
    combined.write_bytes(original_frp[:512 * 1024] + frp.read_bytes())
    session.run(Exec('seed-bootstrap-bdsvars', ('docker', 'exec', session.container,
                     'cp', f'/state/{combined.name}', runtime_path(session, frp_component))))
    if any(p.name == 'frp' for p in persistent):
        header, footer = persistent_header, persistent_footer
    copies = []
    for path, backup in ((header, False), (footer, True)):
        original = runtime_bytes(session, path)
        rewritten = split_frp_gpt(original, component(original, backup=backup))
        component(rewritten, backup=backup)
        source = state / f'split-{path.name}'
        source.write_bytes(rewritten)
        session.run(Exec('split-bootstrap-bdsvars', ('docker', 'exec', session.container,
                         'cp', f'/state/{source.name}', runtime_path(session, path))))
        copies.append(runtime_bytes(session, path))
    renamed = matching_copies(*copies)
    print('physical bdsvars:', next(p for p in renamed if p.name == 'bdsvars'))
    persistent_composite = single_runtime_file(state / 'runtime', 'persistent_composite.img')
    if persistent_composite is None:
        raise ValueError('missing disposable persistent composite')
    session.run(Exec('touch-persistent-composite', ('docker', 'exec', session.container,
                     'touch', runtime_path(session, persistent_composite))))
