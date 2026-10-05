# Host `ksud boot-patch`

`ksud` is the Linux **artifact builder**. `espinitd` remains the Android daemon;
its command surface is not imported into the host binary. The host tool restores
only the historical `android_bootimg` parsing, compression, patching and CPIO
mechanics, pinned to the previously used upstream revision.

There is no phone connection, device discovery, block-device input, flashing,
OTA/update-engine integration, partition selection, backup/restore, kernel
replacement, embedded kernel/module, root-manager integration, shell enablement,
or adbd configuration. No input executable or payload script is executed.

## Build and prerequisites

From the repository root, build the host executable (select your Linux host
triple if your Cargo configuration otherwise selects Android):

```sh
cargo build --locked --release -p espinitd --bin ksud --target x86_64-unknown-linux-gnu
```

Put the resulting `ksud` on your host PATH. Packaging needs Python 3.11 or newer
as `python3`; it invokes the **embedded, unmodified** committed
`scripts/phone_modules.py` with `-I` and its `verify` action. The script is
embedded at build time, so an installed `ksud` does not need the source checkout.
It does need access to the exact kernel source/output/captured configuration
recorded in each module's existing `.ko.compat.json` build receipt.

The exact-phone module build recipes documented in the repository produce those
receipts. Build modules with the same `ESPINIT_GENERATION` as the payload. Keep
the receipts alongside their `.ko` files. Do not hand-edit receipts, CRC tables,
or vermagic strings. `ksud` never manufactures or repairs them. A stripped module
without its generation object/symbol table is not admissible.

Build the supplied PID-1 `espinit` as a static ELF64 little-endian AArch64 or
x86-64 executable. It must retain `.note.espinit` with the manifest generation.
The Android daemon and package/tool binaries must retain their generation notes
and use the same target architecture. The host builder's own generation is
recorded separately; it need not equal the payload generation. The supplied
static BusyBox interpreter is a third-party binary and is architecture/static-link
checked, not required to contain an espinit generation note.

## Prepare the payload source

`--payload` names the **contents of the ESP `/espinit` directory**, not the ESP
root or a source checkout. It is copied in full, including kernel scripts and
additional tools. Prepare this tree before invoking the builder:

```text
payload/
  manifest.toml
  roms/
    rom1.toml
  bin/
    espinitd                  executable Android daemon
    busybox                   executable static interpreter
    thin-activate             when required by the thin early script
  modules/
    espinit.ko
    espinit.ko.compat.json
    thin.ko                   if listed in manifest.modules
    thin.ko.compat.json
    thin/early.sh             when using the current thin activation flow
    gpt.ko                    required for managed ROMs
    gpt.ko.compat.json
    boot-hal/module.toml      normal managed platform package
    ...                       every declared package source file
    tiny-espsu/module.toml
    ...
  receipts/                   may be absent; created in the output
```

Use the current `espinit/manifest.example.toml`, per-ROM schema and checked-in
package manifests, with real values and files. The builder does not generate
placeholder ROMs or stamp over generations. It reuses PID-1's strict manifest,
ROM and platform package validators:

- `manifest.toml` must have schema 1, a valid generation, explicit ROM directory,
  ordered modules beginning with `espinit`, and `[platform]` for complete staging.
- `--rom rom1` selects `<manifest.rom>/rom1.toml`, whose ID and generation must
  agree. All copied ROM TOMLs must agree with the generation and managed-module
  rules. Managed ROMs require `gpt`; unmanaged ROMs must not list it.
- Normal and recovery package plans must be complete. All listed packages,
  including ones skipped at runtime in unmanaged mode, are validated. An
  unlisted `module.toml`, `.ko`, or orphan compatibility receipt is rejected.
- Current exact-target verification supports the repository's `espinit`, `thin`
  and `gpt` modules. It checks their ELF generation objects and executes the
  existing full compatibility verifier against the staged bytes, not merely a
  module hash comparison. All module receipts must identify the same exact
  kernel inputs. Those inputs must still exist and match their recorded hashes.
- `bin/espinit` is populated from `--espinit`. If already present in the source,
  it must be byte-identical; stale PID-1 copies fail rather than being hidden.
- Supply every helper used by your scripts. Scripts are copied but never run or
  interpreted by the builder. Additional ELF tools in `bin/` are generation and
  architecture checked. Runtime readiness/projection is not a host packaging
  claim.
- An `esp-file:` backend is checked for an existing nonempty file. Since the
  input is the `/espinit` subtree, such backends must use
  `esp-file:espinit/<path-within-payload>`; files elsewhere on an existing ESP are
  not implicitly borrowed. By-name/mapper/loop configuration is structurally
  validated only; no host or phone device is opened.

Keep source trees stable during packaging. Symlinks (including root ancestors),
non-regular files, `..`, unsafe relative names, case-insensitive name collisions,
and trailing-dot names are rejected. Source paths may be explicit absolute or
working-directory-relative paths; there are no implicit defaults. Input files
must be regular files. The output must be outside the source payload, must not
already exist, and must have an existing parent directory.

## One packaging command

The stock boot or init_boot image is required because its effective `/init` is
preserved in the per-ROM takeover archive:

```sh
ksud boot-patch \
  --espinit /build/espinit-static \
  --payload /build/payload \
  --rom rom1 \
  --boot /build/stock/init_boot.img \
  --out /build/artifacts/rom1
```

The output path must be new for every invocation; the tool will not overwrite an
existing directory, even an empty one. Successful stdout is the receipt path.
Errors return a nonzero exit status and do not publish a partial output tree.

## Output and later provisioning

```text
rom1/
  espinit.cpio
  receipt.json
  patched.img                 unsigned emulator/test image
  esp/                        contents for later ESP provisioning
    rom/
      rom1/
        espinit.cpio
    espinit/
      manifest.toml
      roms/rom1.toml
      bin/espinit
      bin/espinitd
      bin/busybox
      modules/...
      receipts/
```

The canonical firmware artifact is a deterministic legacy-LZ4 stream containing
one newc overlay. It installs espinit as executable `/init` and copies the stock
image's effective executable `/init` to the reserved `/init.espinit`, both mode
`0755` with normalized metadata. The overlay contains no modules or debug policy.
The packager rejects an absent/non-static/non-AArch64 stock init and any existing
`/init.espinit` collision.

The firmware archive location is **ESP `/rom/<rom-id>/espinit.cpio`**, matching the
physical-phone `/rom/rom1/espinit.cpio` convention. The byte-identical output-root
`espinit.cpio` is also provided. Manifest and ROM configuration stay under the
separate `/espinit` payload contract. A later provisioning tool consumes the
**whole `esp/` tree** plus `receipt.json`; this command creates no filesystem image
and mounts or provisions nothing.

`receipt.json` schema 1 binds:

- tool name, package version, tool build generation, and embedded verifier SHA-256;
- selected ROM, payload generation, archive path and the bootconfig selector;
- SHA-256, byte size and normalized mode for the explicit PID1, stock image and
  every copied payload source;
- SHA-256, byte size and mode for every published artifact except the receipt
  itself, plus the complete directory list (including empty receipt storage);
- the successful module verifier report and the unsigned test image contract.

Paths in artifact maps are output-relative; `payload/` source keys are relative
to `--payload`. No temporary directory name, output directory name or wall-clock
time is recorded. Thus identical input bytes/modes, kernel receipts and tool
produce identical archive, image and receipt bytes. Directory/file timestamps
are not part of this tree format; ordinary source file modes are normalized to
`0755` when executable and `0644` otherwise. The receipt is evidence of checked
consistency, **not a signature or a runtime boot-success receipt**.

All data files are written and fsynced in a private sibling staging directory.
Directories are fsynced and the complete result is published with Linux
`renameat2(RENAME_NOREPLACE)`, then the parent is fsynced. Pre-publication failure
removes the private staging directory. A failure of the final parent fsync may
leave the complete published output and reports an error; it is not silently
reported as durable success. Sources are only opened for reading.

## Stock-image contract and limits

`--boot` accepts Android boot/init_boot header v3/v4 regular files with an
existing newc ramdisk and executable AArch64 `/init`. It does not accept
vendor_boot, raw ramdisks, legacy headers, device nodes, or a ramdisk already
containing the reserved `/init.espinit`. Existing ramdisk archives are
bounded/framing-checked and preserved byte-for-byte; `patched.img` appends the
uncompressed takeover overlay before restoring the source compression. Kernel
bytes are preserved.

The header command line retains unrelated arguments, removes stale `rdinit` and
ROM selectors, and ends with exactly:

```text
androidboot.espinit.rom=rom1
```

Firmware delivery uses the same selector as bootconfig and appends the
legacy-LZ4 `espinit.cpio`; it does not generate `rdinit`. The tool does not
modify vendor_boot or bootloader configuration.

`patched.img` is **unsigned conventional-test-only**: prior GKI signatures and
AVB tail data are deliberately omitted because they no longer authenticate the
changed bytes. It is not flash-ready. The source image is never modified.

OTA support is intentionally absent, not an automatic next stage of this command.

## Host regression gates

Run from the repository root on a Linux host:

```sh
cargo test --locked -p espinitd --bin ksud --test host_cli --target x86_64-unknown-linux-gnu
cargo clippy --locked -p espinitd --bin ksud --test host_cli --target x86_64-unknown-linux-gnu -- -D warnings
cargo fmt --all -- --check
```

Tests use explicit synthetic ELF/config fixtures; they run the real embedded
module verifier, not mocked admission. They cover takeover metadata and
compression, generation/receipt/CRC mismatches, atomic publication,
reproducibility, source preservation, stock-init collision checks, boot parsing,
kernel preservation, selector replacement and unsigned-image behavior. These
host tests do not prove a device boot or real-module target compatibility.
