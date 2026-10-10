use super::*;

fn args(root: &Path, kmi: &str) -> Args {
    Args {
        build_cpio: true,
        legacy_lz4: false,
        kmi: kmi.to_owned(),
        arch: Arch::X86_64,
        artifact_dir: root.to_owned(),
        bootstrap_dir: root.join("bootstrap"),
        out: root.join("archive.cpio"),
        entry: Entry::Egyskinit,
    }
}

#[test]
fn entry_option_accepts_only_the_shared_loader_identity() {
    assert_eq!(
        Entry::from_str("egyskinit", true).unwrap().name(),
        egysk_runtime::context::LOADER
    );
    for alias in ["ksuinit", "esuinit"] {
        assert!(
            Entry::from_str(alias, true).is_err(),
            "{alias} is not an entry"
        );
    }
}

#[test]
fn branch_selector_refuses_multiple_generations_but_exact_selector_is_stable() {
    let root = tempfile::tempdir().unwrap();
    for generation in [6, 7] {
        fs::create_dir_all(
            root.path()
                .join(format!("android16-6.12-{generation}/x86_64")),
        )
        .unwrap();
    }
    assert!(
        select(&args(root.path(), "android16-6.12"))
            .unwrap_err()
            .to_string()
            .contains("resolves to 2")
    );
    let (kmi, path) = select(&args(root.path(), "android16-6.12-6")).unwrap();
    assert_eq!(
        kmi,
        Kmi {
            branch: "android16-6.12".into(),
            generation: 6
        }
    );
    assert_eq!(path, root.path().join("android16-6.12-6/x86_64"));
}

#[test]
fn wrong_architecture_and_symlinked_sets_cannot_satisfy_a_selector() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("android16-6.12-6/aarch64")).unwrap();
    std::os::unix::fs::symlink("aarch64", root.path().join("android16-6.12-6/x86_64")).unwrap();
    assert!(
        select(&args(root.path(), "android16-6.12-6"))
            .unwrap_err()
            .to_string()
            .contains("resolves to 0")
    );
}

#[test]
fn bootstrap_cannot_replace_init_or_follow_links() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("init"), b"do not install").unwrap();
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    assert!(
        capture(root.path(), Path::new(""), &mut files, &mut dirs)
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
    fs::remove_file(root.path().join("init")).unwrap();
    fs::write(root.path().join("real"), b"payload").unwrap();
    std::os::unix::fs::symlink("real", root.path().join("link")).unwrap();
    assert!(
        capture(root.path(), Path::new(""), &mut files, &mut dirs)
            .unwrap_err()
            .to_string()
            .contains("regular files/directories")
    );
}

#[test]
fn legacy_framing_roundtrips_a_full_block_and_partial_final_block() {
    let bytes: Vec<u8> = (0..LZ4_BLOCK + 513)
        .map(|i| (i.wrapping_mul(37) >> 3) as u8)
        .collect();
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(&bytes).unwrap();
    source.rewind().unwrap();
    let mut encoded = tempfile::tempfile().unwrap();
    legacy_lz4(&mut source, &mut encoded).unwrap();
    encoded.rewind().unwrap();
    let mut magic = [0; 4];
    encoded.read_exact(&mut magic).unwrap();
    assert_eq!(magic, 0x184c_2102u32.to_le_bytes());
    let mut decoded = Vec::new();
    for expected_size in [LZ4_BLOCK, 513] {
        let mut length = [0; 4];
        encoded.read_exact(&mut length).unwrap();
        let mut compressed = vec![0; u32::from_le_bytes(length) as usize];
        encoded.read_exact(&mut compressed).unwrap();
        decoded.extend(lz4::block::decompress(&compressed, Some(expected_size as i32)).unwrap());
    }
    assert_eq!(decoded, bytes);
    assert_eq!(
        encoded.read(&mut magic).unwrap(),
        0,
        "no trailer masquerading as another block"
    );
}
