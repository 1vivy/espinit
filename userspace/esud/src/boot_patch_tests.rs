//! Byte-level synthetic fixtures only: never build/load a module or run a phone.
//! The production verifier still runs unmodified against these exact fixture inputs.
use super::*;
use std::os::unix::fs::symlink;

const GENERATION: &str = "host-test-1";
const DYNAMIC_LINKER: &[u8] = b"/system/bin/linker64\0";
type ElfSection<'a> = (&'a str, Vec<u8>, u32, u64, u32, u64);

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn elf_fixture(kind: u16, dynamic: bool, sections: Vec<ElfSection<'_>>) -> Vec<u8> {
    let executable_headers = if dynamic { 176 } else { 120 };
    let mut bytes = vec![0u8; if kind == 2 { executable_headers } else { 64 }];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    put16(&mut bytes, 16, kind);
    put16(&mut bytes, 18, 183);
    put32(&mut bytes, 20, 1);
    put16(&mut bytes, 52, 64);
    put16(&mut bytes, 58, 64);
    let mut strings = vec![0];
    let mut names = Vec::new();
    for (name, ..) in &sections {
        names.push(strings.len() as u32);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    let names_offset = strings.len() as u32;
    strings.extend_from_slice(b".shstrtab\0");
    let mut headers = vec![[0u8; 64]];
    for ((_, content, kind, flags, link, entry_size), name) in sections.into_iter().zip(names) {
        let mut header = [0u8; 64];
        put32(&mut header, 0, name);
        put32(&mut header, 4, kind);
        put64(&mut header, 8, flags);
        put64(&mut header, 24, bytes.len() as u64);
        put64(&mut header, 32, content.len() as u64);
        put32(&mut header, 40, link);
        put64(&mut header, 48, 1);
        put64(&mut header, 56, entry_size);
        headers.push(header);
        bytes.extend(content);
    }
    let mut header = [0u8; 64];
    put32(&mut header, 0, names_offset);
    put32(&mut header, 4, 3);
    put64(&mut header, 24, bytes.len() as u64);
    put64(&mut header, 32, strings.len() as u64);
    put64(&mut header, 48, 1);
    bytes.extend(strings);
    headers.push(header);
    let table = bytes.len() as u64;
    put64(&mut bytes, 40, table);
    put16(&mut bytes, 60, headers.len() as u16);
    put16(&mut bytes, 62, headers.len() as u16 - 1);
    for header in headers {
        bytes.extend_from_slice(&header);
    }
    if kind == 2 {
        put64(&mut bytes, 24, 0x400078);
        put64(&mut bytes, 32, 64);
        put16(&mut bytes, 54, 56);
        put16(&mut bytes, 56, if dynamic { 2 } else { 1 });
        put32(&mut bytes, 64, program_header::PT_LOAD);
        put32(&mut bytes, 68, 5);
        put64(&mut bytes, 80, 0x400000);
        if dynamic {
            put32(&mut bytes, 120, program_header::PT_INTERP);
            put64(&mut bytes, 128, executable_headers as u64);
            put64(&mut bytes, 152, DYNAMIC_LINKER.len() as u64);
            put64(&mut bytes, 160, DYNAMIC_LINKER.len() as u64);
            put64(&mut bytes, 168, 1);
        }
        let size = bytes.len() as u64;
        put64(&mut bytes, 96, size);
        put64(&mut bytes, 104, size);
        put64(&mut bytes, 112, 4096);
    }
    bytes
}

fn binary_fixture_kind(generation: &str, dynamic: bool) -> Vec<u8> {
    let mut note = vec![0u8; 84];
    put32(&mut note, 0, 8);
    put32(&mut note, 4, 64);
    put32(&mut note, 8, 1);
    note[12..20].copy_from_slice(b"ESPINIT\0");
    note[20..20 + generation.len()].copy_from_slice(generation.as_bytes());
    let mut sections = vec![
        (".text", vec![0; 4], 1, 6, 0, 0),
        (".note.espinit", note, 7, 2, 0, 0),
    ];
    if dynamic {
        sections.insert(0, (".interp", DYNAMIC_LINKER.to_vec(), 1, 2, 0, 0));
    }
    elf_fixture(2, dynamic, sections)
}

fn binary_fixture(generation: &str) -> Vec<u8> {
    binary_fixture_kind(generation, false)
}

fn module_fixture(generation: &str) -> Vec<u8> {
    named_module_fixture("kernelesp", generation)
}

fn named_module_fixture(name: &str, generation: &str) -> Vec<u8> {
    let mut versions = Vec::new();
    for (name, crc) in [("module_layout", 0x1234_5678u64), ("known", 0x1122_3344)] {
        let mut record = [0u8; 64];
        put64(&mut record, 0, crc);
        record[8..8 + name.len()].copy_from_slice(name.as_bytes());
        versions.extend(record);
    }
    let mut symbols = vec![0u8; 72];
    put32(&mut symbols, 24, 1);
    symbols[28] = 0x10; // known: undefined global import
    put32(&mut symbols, 48, 7);
    symbols[52] = 1; // generation: local object
    put16(&mut symbols, 54, 4);
    put64(&mut symbols, 64, generation.len() as u64 + 1);
    elf_fixture(
        1,
        false,
        vec![
            (".text", vec![0; 4], 1, 6, 0, 0),
            (
                ".modinfo",
                format!("name={name}\0vermagic=6.12-test modversions aarch64\0").into_bytes(),
                1,
                2,
                0,
                0,
            ),
            ("__versions", versions, 1, 2, 0, 0),
            (".data", format!("{generation}\0").into_bytes(), 1, 3, 0, 0),
            (
                ".strtab",
                format!(
                    "\0known\0{}\0",
                    if name == "kernelesp" {
                        "ksu_build_generation"
                    } else {
                        "generation"
                    }
                )
                .into_bytes(),
                3,
                0,
                0,
                0,
            ),
            (".symtab", symbols, 2, 0, 5, 24),
        ],
    )
}

fn decode_legacy_lz4(encoded: &[u8]) -> Vec<u8> {
    assert!(encoded.starts_with(&LZ4_LEGACY_MAGIC));
    let mut decoded = Vec::new();
    let mut offset = LZ4_LEGACY_MAGIC.len();
    while offset < encoded.len() {
        let size = u32::from_le_bytes(encoded[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let mut block = vec![0u8; LZ4_BLOCK_SIZE];
        let written = lz4::block::decompress_to_buffer(
            &encoded[offset..offset + size],
            Some(LZ4_BLOCK_SIZE as i32),
            &mut block,
        )
        .unwrap();
        decoded.extend_from_slice(&block[..written]);
        offset += size;
    }
    decoded
}

struct Fixture {
    root: tempfile::TempDir,
    args: BootPatchArgs,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("payload");
        for dir in ["bin", "roms", "modules", "receipts"] {
            fs::create_dir_all(payload.join(dir)).unwrap();
        }
        let binary = binary_fixture(GENERATION);
        let pid1 = root.path().join("esu");
        fs::write(&pid1, &binary).unwrap();
        for path in ["bin/esud", "bin/busybox"] {
            let path = payload.join(path);
            fs::write(&path, &binary).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(payload.join("manifest.toml"), format!("schema_version = 1\ngeneration = \"{GENERATION}\"\nrom = \"roms\"\n[platform]\nmetadata_filesystem = \"ext4\"\npackages = []\n[[modules]]\nname = \"kernelesp\"\npath = \"modules/kernelesp.ko\"\nparams = \"\"\n")).unwrap();
        fs::write(payload.join("roms/rom1.toml"), format!("schema_version = 1\ngeneration = \"{GENERATION}\"\nid = \"rom1\"\nmanaged = false\n")).unwrap();
        let source = root.path().join("kernel-src");
        let output = root.path().join("kernel-out");
        let config = root.path().join("captured.config");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(output.join("include/config")).unwrap();
        fs::create_dir_all(output.join("include/generated")).unwrap();
        symlink(&source, output.join("source")).unwrap();
        fs::write(source.join("Makefile"), "VERSION = 6\n").unwrap();
        let config_text = "CONFIG_MODULES=y\nCONFIG_ARM64=y\nCONFIG_MODVERSIONS=y\n";
        for path in [
            &config,
            &output.join(".config"),
            &output.join("include/config/auto.conf"),
        ] {
            fs::write(path, config_text).unwrap();
        }
        fs::write(
            output.join("include/generated/autoconf.h"),
            "#define CONFIG_MODULES 1\n#define CONFIG_ARM64 1\n#define CONFIG_MODVERSIONS 1\n",
        )
        .unwrap();
        fs::write(
            output.join("include/generated/utsrelease.h"),
            "#define UTS_RELEASE \"6.12-test\"\n",
        )
        .unwrap();
        fs::write(output.join("Module.symvers"), "0x12345678\tmodule_layout\tvmlinux\tEXPORT_SYMBOL\n0x11223344\tknown\tvmlinux\tEXPORT_SYMBOL\n").unwrap();
        let mut symbols = vec![0u8; 48];
        put32(&mut symbols, 24, 1);
        symbols[28] = 0x10;
        put16(&mut symbols, 30, 1);
        fs::write(
            output.join("vmlinux"),
            elf_fixture(
                2,
                false,
                vec![
                    (".text", vec![0; 4], 1, 6, 0, 0),
                    (".strtab", b"\0known\0".to_vec(), 3, 0, 0, 0),
                    (".symtab", symbols, 2, 0, 2, 24),
                ],
            ),
        )
        .unwrap();
        let module = module_fixture(GENERATION);
        fs::write(payload.join("modules/kernelesp.ko"), &module).unwrap();
        // Test-only provenance for this synthetic kernel. Production has no path
        // that creates compatibility receipts or writes kernel version data.
        let mut inputs = BTreeMap::new();
        for path in [
            &config,
            &source.join("Makefile"),
            &output.join(".config"),
            &output.join("include/config/auto.conf"),
            &output.join("include/generated/autoconf.h"),
            &output.join("include/generated/utsrelease.h"),
            &output.join("Module.symvers"),
            &output.join("vmlinux"),
        ] {
            inputs.insert(
                path.to_str().unwrap().to_owned(),
                digest(&fs::read(path).unwrap()),
            );
        }
        let receipt = serde_json::json!({"schema_version": 1, "kernel_src": source, "kernel_out": output, "kernel_config": config, "stable": "1", "inputs": inputs, "module_sha256": digest(&module)});
        fs::write(
            payload.join("modules/kernelesp.ko.compat.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let mut stock = Cpio::new();
        stock
            .add(
                "init",
                CpioEntry::regular(0o755, Box::new(binary_fixture("stock-init"))),
            )
            .unwrap();
        let mut ramdisk = Vec::new();
        stock.dump(&mut ramdisk).unwrap();
        let boot = root.path().join("init_boot.img");
        fs::write(&boot, stock_boot(4, &ramdisk)).unwrap();
        let args = BootPatchArgs {
            esuinit: pid1,
            payload,
            rom: "rom1".to_owned(),
            out: root.path().join("result"),
            boot,
        };
        Self { root, args }
    }

    fn managed(&self) {
        let payload = &self.args.payload;
        let manifest = payload.join("manifest.toml");
        let text = fs::read_to_string(&manifest)
            .unwrap()
            .replace("packages = []", "packages = [\"boot-hal\", \"tiny-espsu\"]");
        fs::write(
            manifest,
            format!(
                "{text}\n[[modules]]\nname = \"gpt\"\npath = \"modules/gpt.ko\"\nparams = \"\"\n"
            ),
        )
        .unwrap();
        let rom = payload.join("roms/rom1.toml");
        let text = fs::read_to_string(&rom)
            .unwrap()
            .replace("managed = false", "managed = true");
        fs::write(rom, format!("{text}\n[[partitions]]\nname = \"metadata\"\nbackend = \"/dev/mapper/metadata\"\nread_only = false\n")).unwrap();
        let module = named_module_fixture("gpt", GENERATION);
        fs::write(payload.join("modules/gpt.ko"), &module).unwrap();
        let mut receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(payload.join("modules/kernelesp.ko.compat.json")).unwrap(),
        )
        .unwrap();
        receipt["module_sha256"] = digest(&module).into();
        fs::write(
            payload.join("modules/gpt.ko.compat.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        for (id, manifest) in [
            (
                "boot-hal",
                include_str!("../../../esu/modules/boot-hal/module.toml"),
            ),
            (
                "tiny-espsu",
                include_str!("../../../esu/modules/tiny-espsu/module.toml"),
            ),
        ] {
            fs::create_dir_all(payload.join(format!("modules/{id}"))).unwrap();
            fs::write(
                payload.join(format!("modules/{id}/module.toml")),
                manifest.replace("release-1", GENERATION),
            )
            .unwrap();
        }
        for path in [
            "modules/boot-hal/android.hardware.boot-service.gblbds",
            "modules/tiny-espsu/tiny-espsu",
        ] {
            fs::write(payload.join(path), binary_fixture(GENERATION)).unwrap();
        }
        for (path, text) in [
            ("modules/boot-hal/boot-gblbds.rc", "# fixture initrc\n"),
            ("modules/tiny-espsu/install.sh", "#!/bin/sh\nexit 0\n"),
            ("modules/tiny-espsu/policy.cil", "; fixture policy\n"),
        ] {
            fs::write(payload.join(path), text).unwrap();
        }
    }

    fn reject(&self, expected: &str) {
        let error = patch(&self.args).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(!self.args.out.exists());
        assert!(!fs::read_dir(self.root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".esud-boot-patch-")
        }));
    }
}

fn stock_boot(version: u32, ramdisk: &[u8]) -> Vec<u8> {
    let kernel = b"preserved-kernel";
    let mut image = vec![0u8; 8192 + ramdisk.len().next_multiple_of(4096)];
    image[..8].copy_from_slice(b"ANDROID!");
    put32(&mut image, 8, kernel.len() as u32);
    put32(&mut image, 12, ramdisk.len() as u32);
    put32(&mut image, 20, if version == 3 { 1580 } else { 1584 });
    put32(&mut image, 40, version);
    let command = b"console=ttyS0 rdinit=/old androidboot.esu.rom=old quiet";
    image[44..44 + command.len()].copy_from_slice(command);
    image[4096..4096 + kernel.len()].copy_from_slice(kernel);
    image[8192..8192 + ramdisk.len()].copy_from_slice(ramdisk);
    image
}

#[test]
fn canonical_archive_is_a_deterministic_kernel_su_style_lz4_overlay() {
    let binary = binary_fixture(GENERATION);
    let real_init = binary_fixture("stock-init");
    let overlay = takeover_cpio(binary.clone(), real_init.clone()).unwrap();
    assert_eq!(legacy_lz4(&overlay).unwrap(), legacy_lz4(&overlay).unwrap());
    assert_eq!(overlay.len() % 512, 0);
    validate_cpio(&overlay).unwrap();
    let cpio = Cpio::load_from_data(&overlay).unwrap();
    assert_eq!(cpio.entries().len(), 2);
    assert_eq!(cpio.entry_by_name("init").unwrap().data().unwrap(), binary);
    assert_eq!(
        cpio.entry_by_name("init.real").unwrap().data().unwrap(),
        real_init
    );
    let archive = legacy_lz4(&overlay).unwrap();
    assert!(archive.starts_with(&LZ4_LEGACY_MAGIC));
    assert_eq!(decode_legacy_lz4(&archive), overlay);
    let field = |index: usize| {
        u32::from_str_radix(
            std::str::from_utf8(&overlay[6 + index * 8..14 + index * 8]).unwrap(),
            16,
        )
        .unwrap()
    };
    assert_eq!(field(1), 0o100755);
    for index in [2, 3, 5, 7, 8, 9, 10, 12] {
        assert_eq!(field(index), 0);
    }
}

#[test]
fn cpio_traversal_truncation_and_bad_crc_fail() {
    for name in ["../escape", "dir/../../escape"] {
        let mut cpio = Cpio::new();
        cpio.add(name, CpioEntry::regular(0o644, Box::new(vec![1])))
            .unwrap();
        let mut bytes = Vec::new();
        cpio.dump(&mut bytes).unwrap();
        assert!(validate_cpio(&bytes).is_err());
    }
    let archive = takeover_cpio(vec![1, 2, 3], vec![4, 5, 6]).unwrap();
    for size in [1, 100, 110, 115, 119] {
        assert!(validate_cpio(&archive[..size]).is_err());
    }
    let mut bad_crc = archive;
    bad_crc[5] = b'2';
    assert!(validate_cpio(&bad_crc).is_err());
}

#[test]
fn host_transaction_is_complete_deterministic_and_nonmutating() {
    let mut fixture = Fixture::new();
    let original = fs::read(fixture.args.payload.join("modules/kernelesp.ko")).unwrap();
    patch(&fixture.args).unwrap();
    let first_receipt = fs::read(fixture.args.out.join("receipt.json")).unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&first_receipt).unwrap();
    assert_eq!(receipt["archive_path"], "rom/rom1/esu.cpio");
    assert_eq!(receipt["generation"], GENERATION);
    assert_eq!(receipt["boot_contract"], "androidboot.esu.rom=rom1");
    assert_eq!(receipt["boot_image"], "unsigned-conventional-test-only");
    assert_eq!(receipt["module_verification"]["status"], "accepted");
    assert_eq!(
        fs::read(fixture.args.out.join("esu.cpio")).unwrap(),
        fs::read(fixture.args.out.join("esp/rom/rom1/esu.cpio")).unwrap()
    );
    assert!(fixture.args.out.join("patched.img").is_file());
    assert!(fixture.args.out.join("esp/esu/receipts").is_dir());
    assert!(fixture.args.out.join("esp/esu/roms/rom1.toml").is_file());
    for (path, info) in receipt["artifacts"].as_object().unwrap() {
        assert_eq!(
            digest(&fs::read(fixture.args.out.join(path)).unwrap()),
            info["sha256"].as_str().unwrap()
        );
    }
    fixture.args.out = fixture.root.path().join("second");
    patch(&fixture.args).unwrap();
    assert_eq!(
        first_receipt,
        fs::read(fixture.args.out.join("receipt.json")).unwrap()
    );
    assert_eq!(
        original,
        fs::read(fixture.args.payload.join("modules/kernelesp.ko")).unwrap()
    );
    assert!(!fixture.args.payload.join("bin/esuinit").exists());
}

#[test]
fn required_boot_path_preserves_source_kernel_and_saved_init() {
    let mut fixture = Fixture::new();
    let mut stock = Cpio::new();
    let saved_init = binary_fixture("saved-stock-init");
    stock
        .add(
            "init",
            CpioEntry::regular(0o755, Box::new(saved_init.clone())),
        )
        .unwrap();
    stock
        .add(
            "original",
            CpioEntry::regular(0o640, Box::new(b"stock data".to_vec())),
        )
        .unwrap();
    let mut ramdisk = Vec::new();
    stock.dump(&mut ramdisk).unwrap();
    let source = stock_boot(4, &ramdisk);
    let boot_path = fixture.root.path().join("init_boot.img");
    fs::write(&boot_path, &source).unwrap();
    fixture.args.boot = boot_path.clone();
    patch(&fixture.args).unwrap();
    assert_eq!(fs::read(boot_path).unwrap(), source);
    let image = fs::read(fixture.args.out.join("patched.img")).unwrap();
    let parsed = BootImage::parse(&image).unwrap();
    assert_eq!(
        parsed.get_blocks().get_kernel().unwrap().get_data(),
        b"preserved-kernel"
    );
    let cmdline = parsed.get_header().get_cmdline();
    let cmdline =
        std::str::from_utf8(&cmdline[..cmdline.iter().position(|byte| *byte == 0).unwrap()])
            .unwrap();
    assert_eq!(cmdline, "console=ttyS0 quiet androidboot.esu.rom=rom1");
    let mut rebuilt = Vec::new();
    parsed
        .get_blocks()
        .get_ramdisk()
        .unwrap()
        .dump(&mut rebuilt, false)
        .unwrap();
    assert!(rebuilt.starts_with(&ramdisk));
    let cpio = Cpio::load_from_data(&rebuilt).unwrap();
    assert_eq!(
        cpio.entry_by_name("init").unwrap().data().unwrap(),
        fs::read(&fixture.args.esuinit).unwrap()
    );
    assert_eq!(
        cpio.entry_by_name("init.real").unwrap().data().unwrap(),
        saved_init
    );
    assert!(!cpio.exists("kernelsu.ko"));
    assert_eq!(
        patch_boot(
            &source,
            &decode_legacy_lz4(&fs::read(fixture.args.out.join("esu.cpio")).unwrap()),
            "rom1",
        )
        .unwrap(),
        image
    );
}

#[test]
fn init_boot_without_kernel_or_ramdisk_is_supported_and_signatures_are_omitted() {
    for version in [3, 4] {
        let mut source = stock_boot(version, &[]);
        source.truncate(4096);
        put32(&mut source, 8, 0);
        if version == 4 {
            put32(&mut source, 1580, 4096);
            source.extend(vec![0x55; 4096]);
        }
        source.extend(b"untrusted AVB tail");
        let overlay = takeover_cpio(vec![1, 2, 3], vec![4, 5, 6]).unwrap();
        let patched = patch_boot(&source, &overlay, "rom1").unwrap();
        let image = BootImage::parse(&patched).unwrap();
        assert!(image.get_blocks().get_kernel().is_none());
        if version == 4 {
            assert_eq!(image.get_header().get_signature_size(), 0);
        }
        let mut archive = Vec::new();
        image
            .get_blocks()
            .get_ramdisk()
            .unwrap()
            .dump(&mut archive, false)
            .unwrap();
        let cpio = Cpio::load_from_data(&archive).unwrap();
        assert!(cpio.exists("init"));
        assert!(cpio.exists("init.real"));
    }
}

#[test]
fn missing_manifest_rom_or_module_receipt_never_publishes() {
    for path in [
        "manifest.toml",
        "roms/rom1.toml",
        "modules/kernelesp.ko.compat.json",
    ] {
        let fixture = Fixture::new();
        fs::remove_file(fixture.args.payload.join(path)).unwrap();
        fixture.reject("No such file");
    }
}

#[test]
fn generation_disagreement_in_every_component_fails_closed() {
    for path in ["roms/rom1.toml", "bin/esud", "modules/kernelesp.ko"] {
        let fixture = Fixture::new();
        let path = fixture.args.payload.join(path);
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            fs::write(
                &path,
                fs::read_to_string(&path)
                    .unwrap()
                    .replace(GENERATION, "other"),
            )
            .unwrap();
        } else if path.extension().is_some_and(|extension| extension == "ko") {
            fs::write(&path, module_fixture("other")).unwrap();
        } else {
            fs::write(&path, binary_fixture("other")).unwrap();
        }
        fixture.reject("generation");
    }
    let fixture = Fixture::new();
    fs::write(&fixture.args.esuinit, binary_fixture("other")).unwrap();
    fixture.reject("generation");
}

#[test]
fn receipt_hash_and_exact_kernel_inputs_are_not_trusted() {
    let fixture = Fixture::new();
    let path = fixture
        .args
        .payload
        .join("modules/kernelesp.ko.compat.json");
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt["module_sha256"] = "0".repeat(64).into();
    fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    fixture.reject("receipt mismatch");
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("kernel-out/Module.symvers"), "0x00000000\tmodule_layout\tvmlinux\tEXPORT_SYMBOL\n0x11223344\tknown\tvmlinux\tEXPORT_SYMBOL\n").unwrap();
    fixture.reject("CRC mismatch");
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("kernel-src/Makefile"),
        "VERSION = 7\n",
    )
    .unwrap();
    fixture.reject("mismatched build receipt");
}

#[test]
fn traversal_symlink_orphan_module_and_existing_output_fail_closed() {
    let mut fixture = Fixture::new();
    fixture.args.rom = "../rom1".to_owned();
    fixture.reject("RomIdInvalid");
    let fixture = Fixture::new();
    let path = fixture.args.payload.join("manifest.toml");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("modules/kernelesp.ko", "../kernelesp.ko"),
    )
    .unwrap();
    fixture.reject("Path");
    let fixture = Fixture::new();
    symlink("/etc/passwd", fixture.args.payload.join("escape")).unwrap();
    fixture.reject("symlink");
    let fixture = Fixture::new();
    fs::write(fixture.args.payload.join("modules/unlisted.ko"), b"orphan").unwrap();
    fixture.reject("unlisted module");
    let fixture = Fixture::new();
    fs::create_dir(&fixture.args.out).unwrap();
    fs::write(fixture.args.out.join("keep"), b"user file").unwrap();
    assert!(patch(&fixture.args).is_err());
    assert_eq!(
        fs::read(fixture.args.out.join("keep")).unwrap(),
        b"user file"
    );
}

#[test]
fn malformed_boot_and_dynamic_pid1_are_rejected() {
    for source in [vec![], vec![0; 4096], b"ANDROID!".to_vec()] {
        assert!(patch_boot(&source, b"", "rom1").is_err());
    }
    let mut source = stock_boot(3, &[]);
    put32(&mut source, 40, 2);
    assert!(patch_boot(&source, b"", "rom1").is_err());
    let fixture = Fixture::new();
    let binary = binary_fixture_kind(GENERATION, true);
    fs::write(&fixture.args.esuinit, binary).unwrap();
    assert!(patch(&fixture.args).is_err());
    assert!(!fixture.args.out.exists());
    let fixture = Fixture::new();
    let path = fixture.args.payload.join("bin/thin-activate");
    fs::write(path, binary_fixture_kind(GENERATION, true)).unwrap();
    fixture.reject("statically linked");
}

/// List `bin/fw-views` as an ordered userspace helper module with the given
/// manifest name, parameters, file mode and generation note.
fn with_helper(fixture: &Fixture, name: &str, params: &str, mode: u32, generation: &str) {
    use std::os::unix::fs::PermissionsExt;

    let payload = &fixture.args.payload;
    let manifest = payload.join("manifest.toml");
    fs::write(
        &manifest,
        format!(
            "{}\n[[modules]]\nname = \"{name}\"\npath = \"bin/fw-views\"\nparams = \"{params}\"\n",
            fs::read_to_string(&manifest).unwrap()
        ),
    )
    .unwrap();
    let path = payload.join("bin/fw-views");
    fs::write(&path, binary_fixture(generation)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn an_ordered_helper_module_must_be_an_executable_static_binary() {
    use std::os::unix::fs::PermissionsExt;

    // A listed helper is admitted when it is an executable, interpreter-free
    // binary of this payload generation: PID 1 runs it from the ESP through its
    // own early.sh and never loads it into the kernel.
    let fixture = Fixture::new();
    with_helper(&fixture, "fw-views", "", 0o755, GENERATION);
    patch(&fixture.args).unwrap();

    for (name, params, mode, generation, expected) in [
        (
            "fw-views",
            "",
            0o644,
            GENERATION,
            "helper module is not executable",
        ),
        (
            "fw-views",
            "debug=1",
            0o755,
            GENERATION,
            "takes no parameters",
        ),
        ("fw-views", "", 0o755, "release-9", "generation"),
        (
            "helper",
            "",
            0o755,
            GENERATION,
            "does not admit helper module",
        ),
    ] {
        let fixture = Fixture::new();
        with_helper(&fixture, name, params, mode, generation);
        fixture.reject(expected);
    }

    // A dynamic helper cannot run where PID 1 has no linker.
    let fixture = Fixture::new();
    with_helper(&fixture, "fw-views", "", 0o755, GENERATION);
    fs::write(
        fixture.args.payload.join("bin/fw-views"),
        binary_fixture_kind(GENERATION, true),
    )
    .unwrap();
    fs::set_permissions(
        fixture.args.payload.join("bin/fw-views"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fixture.reject("statically linked");

    // A manifest that lists the helper without shipping it fails closed.
    let fixture = Fixture::new();
    with_helper(&fixture, "fw-views", "", 0o755, GENERATION);
    fs::remove_file(fixture.args.payload.join("bin/fw-views")).unwrap();
    fixture.reject("helper module is not executable");
}

#[test]
fn managed_payload_stages_both_packages_and_rejects_stale_or_missing_sources() {
    let mut fixture = Fixture::new();
    fixture.managed();
    patch(&fixture.args).unwrap();
    for path in [
        "modules/gpt.ko.compat.json",
        "modules/boot-hal/boot-gblbds.rc",
        "modules/tiny-espsu/install.sh",
        "modules/tiny-espsu/policy.cil",
    ] {
        assert_eq!(
            fs::read(fixture.args.payload.join(path)).unwrap(),
            fs::read(fixture.args.out.join("esp/esu").join(path)).unwrap()
        );
    }
    fixture.args.out = fixture.root.path().join("rejected");
    fs::write(
        fixture.args.payload.join("modules/tiny-espsu/tiny-espsu"),
        binary_fixture("stale"),
    )
    .unwrap();
    fixture.reject("generation");
    let fixture = Fixture::new();
    fixture.managed();
    fs::remove_file(fixture.args.payload.join("modules/tiny-espsu/policy.cil")).unwrap();
    fixture.reject("No such file");
    let fixture = Fixture::new();
    fixture.managed();
    let receipt = fixture.args.payload.join("modules/gpt.ko.compat.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    value["kernel_out"] = "/different/kernel/output".into();
    fs::write(receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    fixture.reject("receipts disagree");
}

#[test]
fn generated_names_cannot_collide_on_fat_and_pid1_cannot_hide_a_stale_copy() {
    let fixture = Fixture::new();
    fs::remove_dir(fixture.args.payload.join("receipts")).unwrap();
    fs::create_dir(fixture.args.payload.join("Receipts")).unwrap();
    fixture.reject("case-folding collision");
    let fixture = Fixture::new();
    fs::write(
        fixture.args.payload.join("bin/Esuinit"),
        binary_fixture(GENERATION),
    )
    .unwrap();
    fixture.reject("case-folding collision");
    let fixture = Fixture::new();
    fs::write(
        fixture.args.payload.join("bin/esuinit"),
        binary_fixture("stale"),
    )
    .unwrap();
    fixture.reject("disagrees");
}

#[test]
fn commandline_preserves_quoted_arguments_and_has_one_explicit_contract() {
    let command =
        br#"console=ttyS0 label="one  two" "rdinit=/old" androidboot.esu.rom=old rdinit=/another"#;
    assert_eq!(
        boot_cmdline(command, "rom1").unwrap(),
        "console=ttyS0 label=\"one  two\" androidboot.esu.rom=rom1"
    );
    assert!(boot_cmdline(b"label=\"unterminated", "rom1").is_err());
    assert!(boot_cmdline(&[b'x'; 1536], "rom1").is_err());
}
