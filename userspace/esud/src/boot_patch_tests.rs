//! Byte-level synthetic fixtures only: never build/load a module or run a phone.
//! The production verifier still runs unmodified against these exact fixture inputs.
use super::*;
use std::os::unix::fs::symlink;

const FIXTURE_MARKER: &str = "host-test-1";
const DYNAMIC_LINKER: &[u8] = b"/system/bin/linker64\0";
/// The KMI the fixture kernel output and its receipts declare.
const KMI_DIR: &str = "android16-6.12-6";
/// The five modules every payload must declare, in manifest order.
const CORE_MODULES: [&str; 5] = ["kernelesp", "thin", "gpt", "efivarfs", "efivar_store"];
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

fn binary_fixture_kind(marker: &str, dynamic: bool) -> Vec<u8> {
    let mut sections = vec![(".text", marker.as_bytes().to_vec(), 1, 6, 0, 0)];
    if dynamic {
        sections.insert(0, (".interp", DYNAMIC_LINKER.to_vec(), 1, 2, 0, 0));
    }
    elf_fixture(2, dynamic, sections)
}

fn binary_fixture(marker: &str) -> Vec<u8> {
    binary_fixture_kind(marker, false)
}

fn module_fixture() -> Vec<u8> {
    named_module_fixture("kernelesp")
}

fn named_module_fixture(name: &str) -> Vec<u8> {
    let mut versions = Vec::new();
    for (name, crc) in [("module_layout", 0x1234_5678u64), ("known", 0x1122_3344)] {
        let mut record = [0u8; 64];
        put64(&mut record, 0, crc);
        record[8..8 + name.len()].copy_from_slice(name.as_bytes());
        versions.extend(record);
    }
    let mut symbols = vec![0u8; 48];
    put32(&mut symbols, 24, 1);
    symbols[28] = 0x10; // known: undefined global import
    elf_fixture(
        1,
        false,
        vec![
            (".text", vec![0; 4], 1, 6, 0, 0),
            (
                ".modinfo",
                format!(
                    "name={name}\0vermagic=6.12-test SMP preempt mod_unload modversions aarch64\0"
                )
                .into_bytes(),
                1,
                2,
                0,
                0,
            ),
            ("__versions", versions, 1, 2, 0, 0),
            (".strtab", b"\0known\0".to_vec(), 3, 0, 0, 0),
            (".symtab", symbols, 2, 0, 4, 24),
        ],
    )
}

/// One decoded newc member of the produced overlay.
struct Member {
    name: String,
    mode: u32,
    data: Vec<u8>,
}

/// Decode a legacy-LZ4 stream: magic, then a `u32` block size and block, until
/// the input ends. There is no end marker, exactly as the kernel reads it.
fn decode_legacy_lz4(encoded: &[u8]) -> Vec<u8> {
    assert!(encoded.starts_with(&ota_core::LZ4_LEGACY_MAGIC));
    let mut decoded = Vec::new();
    let mut offset = ota_core::LZ4_LEGACY_MAGIC.len();
    while offset < encoded.len() {
        let size = u32::from_le_bytes(encoded[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let mut block = vec![0u8; ota_core::LZ4_BLOCK_SIZE];
        let written = lz4::block::decompress_to_buffer(
            &encoded[offset..offset + size],
            Some(ota_core::LZ4_BLOCK_SIZE as i32),
            &mut block,
        )
        .unwrap();
        decoded.extend_from_slice(&block[..written]);
        offset += size;
    }
    decoded
}

/// Parse a newc archive by hand: 110-byte headers, 4-byte aligned names and
/// data, up to the `TRAILER!!!` member.
fn parse_newc(archive: &[u8]) -> Vec<Member> {
    let hex = |field: &[u8]| u32::from_str_radix(std::str::from_utf8(field).unwrap(), 16).unwrap();
    let mut members = Vec::new();
    let mut offset = 0;
    loop {
        assert_eq!(&archive[offset..offset + 6], b"070701", "newc magic");
        let header = &archive[offset..offset + 110];
        let mode = hex(&header[14..22]);
        let size = hex(&header[54..62]) as usize;
        let name_length = hex(&header[94..102]) as usize;
        let name_start = offset + 110;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_length - 1])
            .unwrap()
            .to_owned();
        assert_eq!(archive[name_start + name_length - 1], 0, "NUL terminated");
        let data_start = (name_start + name_length).next_multiple_of(4);
        let data = archive[data_start..data_start + size].to_vec();
        offset = (data_start + size).next_multiple_of(4);
        if name == "TRAILER!!!" {
            return members;
        }
        members.push(Member { name, mode, data });
    }
}

/// The members of the overlay a run produced, decoded the way the kernel reads
/// the archive.
fn overlay_members(path: &Path) -> Vec<Member> {
    parse_newc(&decode_legacy_lz4(&fs::read(path).unwrap()))
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
        let binary = binary_fixture(FIXTURE_MARKER);
        let pid1 = root.path().join("esu");
        fs::write(&pid1, &binary).unwrap();
        for path in [
            "bin/esud",
            "bin/busybox",
            "bin/thin-activate",
            "bin/lvm",
            "bin/ota-stage",
        ] {
            let path = payload.join(path);
            fs::write(&path, &binary).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let conf = payload.join("bin/lvm.conf");
        fs::write(&conf, LVM_CONF).unwrap();
        fs::set_permissions(conf, fs::Permissions::from_mode(0o644)).unwrap();
        let mut manifest = "schema_version = 1\nrom = \"roms\"\nmodules_order = []\n".to_owned();
        for name in CORE_MODULES {
            let params = if name == "efivar_store" {
                "dev=by-name:bdsvars"
            } else {
                ""
            };
            manifest.push_str(&format!(
                "[[modules]]\nname = \"{name}\"\npath = \"lib/{name}.ko\"\nparams = \"{params}\"\n"
            ));
        }
        fs::write(payload.join("manifest.toml"), manifest).unwrap();
        fs::write(
            payload.join("roms/rom1.toml"),
            "schema_version = 1\nid = \"rom1\"\nmanaged = true\n[[partitions]]\nname = \"userdata\"\nbackend = \"/dev/mapper/userdata\"\nread_only = false\n",
        )
        .unwrap();
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
        fs::write(output.join("System.map"), "ffff000000001000 T known\n").unwrap();
        let modules = root.path().join("modules");
        fs::create_dir(&modules).unwrap();
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
        let module = module_fixture();
        fs::write(modules.join("kernelesp.ko"), &module).unwrap();
        // Test-only provenance for this synthetic kernel. Production has no path
        // that creates compatibility receipts or writes kernel version data.
        let mut inputs = BTreeMap::new();
        for path in [
            &output.join("Module.symvers"),
            &output.join("System.map"),
            &output.join("include/generated/utsrelease.h"),
        ] {
            inputs.insert(
                path.to_str().unwrap().to_owned(),
                digest(&fs::read(path).unwrap()),
            );
        }
        let receipt = serde_json::json!({"schema_version": 2, "kmi": {"branch": "android16-6.12", "generation": 6}, "kmi_out_inputs": inputs, "imports": {"versioned": 2, "kallsyms": []}, "module_sha256": digest(&module)});
        fs::write(
            modules.join("kernelesp.ko.compat.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        for name in CORE_MODULES.iter().filter(|name| **name != "kernelesp") {
            let bytes = named_module_fixture(name);
            fs::write(modules.join(format!("{name}.ko")), &bytes).unwrap();
            let mut receipt = receipt.clone();
            receipt["module_sha256"] = digest(&bytes).into();
            fs::write(
                modules.join(format!("{name}.ko.compat.json")),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
        }
        let args = BootPatchArgs {
            esuinit: pid1,
            payload,
            modules_dir: modules,
            kmi_out: output,
            rom: "rom1".to_owned(),
            out: root.path().join("result"),
        };
        Self { root, args }
    }

    fn managed(&self) {
        let payload = &self.args.payload;
        let manifest = payload.join("manifest.toml");
        fs::write(
            &manifest,
            fs::read_to_string(&manifest)
                .unwrap()
                .replace("modules_order = []", "modules_order = [\"boot-hal\"]"),
        )
        .unwrap();
        fs::write(payload.join("roms/rom1.toml"), "schema_version = 1\nid = \"rom1\"\nmanaged = true\n[[partitions]]\nname = \"metadata\"\nbackend = \"/dev/mapper/metadata\"\nread_only = false\n").unwrap();
        let directory = payload.join("modules/boot-hal");
        fs::create_dir_all(directory.join("initrc")).unwrap();
        fs::write(
            directory.join("module.prop"),
            include_str!("../../../esu/modules/boot-hal/module.prop"),
        )
        .unwrap();
        fs::write(
            directory.join("sepolicy.rule"),
            include_str!("../../../esu/modules/boot-hal/sepolicy.rule"),
        )
        .unwrap();
        fs::write(
            directory.join("initrc/boot-hal.rc"),
            include_str!("../../../esu/modules/boot-hal/initrc/boot-hal.rc"),
        )
        .unwrap();
        fs::write(payload.join("bin/esu-bootctl"), binary_fixture("hal")).unwrap();
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

#[test]
fn the_overlay_carries_the_entrypoint_build_id_and_module_set_and_no_init() {
    let fixture = Fixture::new();
    patch(&fixture.args).unwrap();
    let members = overlay_members(&fixture.args.out.join("esu.cpio"));

    let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "esu-build-id",
            "esuinit",
            "lib",
            "lib/efivar_store.ko",
            "lib/efivarfs.ko",
            "lib/gpt.ko",
            "lib/kernelesp.ko",
            "lib/thin.ko",
        ]
    );
    for member in &members {
        assert_ne!(
            member.name, "init",
            "the stock entry point is never taken over"
        );
        assert_ne!(member.name, "init.esureal");
        match member.name.as_str() {
            "esuinit" => {
                assert_eq!(member.mode & 0o7777, 0o755);
                assert_eq!(member.data, fs::read(&fixture.args.esuinit).unwrap());
            }
            "lib" => assert_eq!(member.mode & 0o7777, 0o755),
            "esu-build-id" => assert_eq!(member.mode & 0o7777, 0o644),
            name => {
                assert_eq!(member.mode & 0o7777, 0o644);
                let module = name
                    .strip_prefix("lib/")
                    .unwrap()
                    .strip_suffix(".ko")
                    .unwrap();
                assert_eq!(
                    member.data,
                    fs::read(fixture.args.modules_dir.join(format!("{module}.ko"))).unwrap()
                );
            }
        }
    }
}

#[test]
fn the_verified_set_is_placed_for_the_device_selector() {
    let fixture = Fixture::new();
    patch(&fixture.args).unwrap();
    let set = fixture.args.out.join("esp/esu/kmi").join(KMI_DIR);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(set.join("set.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["kmi"]["branch"], "android16-6.12");
    assert_eq!(manifest["kmi"]["generation"], 6);
    assert_eq!(
        manifest["modules"].as_object().unwrap().len(),
        CORE_MODULES.len()
    );
    for name in CORE_MODULES {
        let module = fixture.args.modules_dir.join(format!("{name}.ko"));
        assert_eq!(
            fs::read(set.join("lib").join(format!("{name}.ko"))).unwrap(),
            fs::read(&module).unwrap()
        );
        assert_eq!(
            manifest["modules"][format!("{name}.ko")],
            serde_json::Value::String(digest(&fs::read(&module).unwrap()))
        );
        assert_eq!(
            fs::read(set.join(format!("{name}.ko.compat.json"))).unwrap(),
            fs::read(
                fixture
                    .args
                    .modules_dir
                    .join(format!("{name}.ko.compat.json"))
            )
            .unwrap()
        );
    }
    // The producer and the device share one selector, so what the device will
    // verify is exactly the set this run wrote.
    let selected = ota_core::select_module_set(
        &fixture.args.out.join("esp"),
        &ota_core::Kmi {
            branch: "android16-6.12".to_owned(),
            generation: 6,
        },
    )
    .unwrap();
    assert_eq!(selected.modules().len(), CORE_MODULES.len());

    // A payload that already carries this KMI's set is a second, unverified
    // statement of the same modules and is refused.
    let fixture = Fixture::new();
    let carried = fixture.args.payload.join("kmi").join(KMI_DIR);
    fs::create_dir_all(&carried).unwrap();
    fs::write(carried.join("set.json"), b"{}\n").unwrap();
    fixture.reject("must not carry a module set");
}

#[test]
fn host_transaction_is_complete_deterministic_and_nonmutating() {
    let mut fixture = Fixture::new();
    let original = fs::read(fixture.args.modules_dir.join("kernelesp.ko")).unwrap();
    patch(&fixture.args).unwrap();
    let first_receipt = fs::read(fixture.args.out.join("receipt.json")).unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&first_receipt).unwrap();
    assert_eq!(receipt["archive_path"], "rom/rom1/esu.cpio");
    assert_eq!(receipt["kmi"]["branch"], "android16-6.12");
    assert_eq!(receipt["kmi"]["generation"], 6);
    assert!(receipt.get("boot_contract").is_none());
    assert!(receipt.get("boot_image").is_none());
    assert!(receipt.get("patched").is_none());
    let build_id = receipt["build_id"].as_str().unwrap();
    assert_eq!(build_id.len(), 12);
    let marker = format!("{build_id}\n");
    assert_eq!(
        fs::read(fixture.args.out.join("esp/esu/build-id")).unwrap(),
        marker.as_bytes()
    );
    let members = overlay_members(&fixture.args.out.join("esu.cpio"));
    assert_eq!(
        members
            .iter()
            .find(|member| member.name == "esu-build-id")
            .unwrap()
            .data,
        marker.as_bytes()
    );
    assert_eq!(receipt["module_verification"]["status"], "accepted");
    assert_eq!(
        fs::read(fixture.args.out.join("esu.cpio")).unwrap(),
        fs::read(fixture.args.out.join("esp/rom/rom1/esu.cpio")).unwrap()
    );
    assert!(!fixture.args.out.join("patched.img").exists());
    assert!(fixture.args.out.join("esp/esu/receipts").is_dir());
    assert!(fixture.args.out.join("esp/esu/roms/rom1.toml").is_file());
    assert!(fixture.args.out.join("esp/esu/kmi").join(KMI_DIR).is_dir());
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
        fs::read(fixture.args.modules_dir.join("kernelesp.ko")).unwrap()
    );
    assert!(!fixture.args.payload.join("bin/esuinit").exists());
}

#[test]
fn the_payload_lvm_configuration_must_be_the_tools_file() {
    let fixture = Fixture::new();
    fs::write(fixture.args.payload.join("bin/lvm.conf"), b"devices { }\n").unwrap();
    fixture.reject("differs from tools/lvm2/lvm.conf");

    let fixture = Fixture::new();
    fs::remove_file(fixture.args.payload.join("bin/lvm.conf")).unwrap();
    fixture.reject("No such file");

    let fixture = Fixture::new();
    let conf = fixture.args.payload.join("bin/lvm.conf");
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.reject("not a regular 0644 file");
}

#[test]
fn missing_manifest_rom_or_module_receipt_never_publishes() {
    for path in ["manifest.toml", "roms/rom1.toml"] {
        let fixture = Fixture::new();
        fs::remove_file(fixture.args.payload.join(path)).unwrap();
        fixture.reject("No such file");
    }
    let fixture = Fixture::new();
    fs::remove_file(fixture.args.modules_dir.join("kernelesp.ko.compat.json")).unwrap();
    fixture.reject("No such file");
    let fixture = Fixture::new();
    fs::remove_file(fixture.args.modules_dir.join("kernelesp.ko")).unwrap();
    fixture.reject("required manifest module missing");
    for name in ["efivarfs", "efivar_store"] {
        let fixture = Fixture::new();
        fs::remove_file(fixture.args.modules_dir.join(format!("{name}.ko"))).unwrap();
        fixture.reject("required manifest module missing");
        let fixture = Fixture::new();
        fs::remove_file(
            fixture
                .args
                .modules_dir
                .join(format!("{name}.ko.compat.json")),
        )
        .unwrap();
        fixture.reject("No such file");
    }
}

#[test]
fn build_id_is_sorted_framed_and_retains_duplicate_hashes() {
    let a = digest(b"a");
    let b = digest(b"b");
    let expected = digest(format!("{a}\n{b}\n").as_bytes());
    let mut hashes = [a.as_str(), b.as_str()];
    hashes.sort_unstable();
    let expected = if hashes[0] == a {
        expected
    } else {
        digest(format!("{b}\n{a}\n").as_bytes())
    };
    assert_eq!(
        artifact_build_id([a.as_str(), b.as_str()].into_iter()),
        expected[..12]
    );
    assert_eq!(
        artifact_build_id([b.as_str(), a.as_str()].into_iter()),
        expected[..12]
    );
    assert_ne!(
        artifact_build_id([a.as_str(), a.as_str(), b.as_str()].into_iter()),
        expected[..12]
    );
}

#[test]
fn receipt_hash_and_exact_kernel_inputs_are_not_trusted() {
    let fixture = Fixture::new();
    let path = fixture.args.modules_dir.join("kernelesp.ko.compat.json");
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt["module_sha256"] = "0".repeat(64).into();
    fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    fixture.reject("receipt mismatch");
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("kernel-out/Module.symvers"), "0x00000000\tmodule_layout\tvmlinux\tEXPORT_SYMBOL\n0x11223344\tknown\tvmlinux\tEXPORT_SYMBOL\n").unwrap();
    fixture.reject("CRC mismatch");
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("kernel-out/System.map"),
        "ffff000000001000 T different\n",
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
            .replace("lib/kernelesp.ko", "../kernelesp.ko"),
    )
    .unwrap();
    fixture.reject("Path");
    let fixture = Fixture::new();
    symlink("/etc/passwd", fixture.args.payload.join("escape")).unwrap();
    fixture.reject("symlink");
    let fixture = Fixture::new();
    fs::write(fixture.args.payload.join("modules/unlisted.ko"), b"orphan").unwrap();
    fixture.reject("kernel modules must be supplied");
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
fn a_dynamic_pid1_or_early_helper_is_rejected() {
    let fixture = Fixture::new();
    let binary = binary_fixture_kind(FIXTURE_MARKER, true);
    fs::write(&fixture.args.esuinit, binary).unwrap();
    assert!(patch(&fixture.args).is_err());
    assert!(!fixture.args.out.exists());
    for name in ["thin-activate", "lvm", "ota-stage"] {
        let fixture = Fixture::new();
        let path = fixture.args.payload.join("bin").join(name);
        fs::write(path, binary_fixture_kind(FIXTURE_MARKER, true)).unwrap();
        fixture.reject("statically linked");
    }
}

#[test]
fn managed_payload_rejects_a_mismatched_kmi_receipt() {
    let fixture = Fixture::new();
    fixture.managed();
    let receipt = fixture.args.modules_dir.join("gpt.ko.compat.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    value["kmi_out_inputs"] = serde_json::json!({"/different/Module.symvers": "0".repeat(64)});
    fs::write(receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    fixture.reject("mismatched build receipt");
}

#[test]
fn invalid_optional_policy_rejects_only_admitted_critical_modules() {
    for (critical, skip, rejected) in [
        (false, None, false),
        (true, None, true),
        (true, Some("disable"), false),
        (true, Some("remove"), false),
    ] {
        let fixture = Fixture::new();
        fixture.managed();
        let module = fixture.args.payload.join("modules/boot-hal");
        fs::write(module.join("sepolicy.rule"), "not_a_statement\n").unwrap();
        if critical {
            fs::write(module.join("critical"), b"").unwrap();
        }
        if let Some(marker) = skip {
            fs::write(module.join(marker), b"").unwrap();
        }
        let result = patch(&fixture.args);
        assert_eq!(result.is_err(), rejected, "{result:?}");
        assert_eq!(fixture.args.out.exists(), !rejected);
    }
}

#[test]
fn generated_names_cannot_collide_on_fat_and_pid1_cannot_hide_a_stale_copy() {
    let fixture = Fixture::new();
    fs::remove_dir(fixture.args.payload.join("receipts")).unwrap();
    fs::create_dir(fixture.args.payload.join("Receipts")).unwrap();
    fixture.reject("case-folding collision");
    let fixture = Fixture::new();
    fs::create_dir(fixture.args.payload.join("Kmi")).unwrap();
    fixture.reject("case-folding collision");
    let fixture = Fixture::new();
    fs::write(
        fixture.args.payload.join("bin/Esuinit"),
        binary_fixture(FIXTURE_MARKER),
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
fn rom_owned_images_need_not_be_in_the_payload_but_payload_images_must_exist() {
    for (backend, image, accepted) in [
        ("rom/rom1/boot_a.img", None, true),
        ("rom/rom2/boot_a.img", None, false),
        ("rom/rom1/../boot_a.img", None, false),
        ("esu/images/boot_a.img", None, false),
        ("esu/images/boot_a.img", Some(&b""[..]), false),
        ("esu/images/boot_a.img", Some(&b"payload image"[..]), true),
    ] {
        let fixture = Fixture::new();
        fixture.managed();
        let config = fixture.args.payload.join("roms/rom1.toml");
        let text = format!(
            "{}\n[[partitions]]\nname = \"boot_a\"\nbackend = \"esp-file:{backend}\"\nread_only = false\n",
            fs::read_to_string(&config).unwrap()
        );
        fs::write(&config, &text).unwrap();
        if let Some(bytes) = image {
            fs::create_dir(fixture.args.payload.join("images")).unwrap();
            fs::write(fixture.args.payload.join("images/boot_a.img"), bytes).unwrap();
        }
        let result = patch(&fixture.args);
        assert_eq!(result.is_ok(), accepted, "{backend}: {result:?}");
        if accepted {
            assert_eq!(
                fs::read_to_string(fixture.args.out.join("esp/esu/roms/rom1.toml")).unwrap(),
                text
            );
            if let Some(bytes) = image {
                assert_eq!(
                    fs::read(fixture.args.out.join("esp/esu/images/boot_a.img")).unwrap(),
                    bytes
                );
            }
        } else {
            assert!(!fixture.args.out.exists());
        }
    }
}
