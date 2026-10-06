use avb_graft::{
    Error, Footer, ReadError, parse_footer, plan_graft, replacement_footer, validate_vbmeta,
};

fn metadata() -> Vec<u8> {
    let mut bytes = vec![0; 320];
    bytes[..4].copy_from_slice(b"AVB0");
    bytes[4..8].copy_from_slice(&1_u32.to_be_bytes());
    bytes[20..28].copy_from_slice(&64_u64.to_be_bytes());
    bytes
}

fn footer() -> [u8; 64] {
    replacement_footer(Footer {
        original_image_size: 73,
        vbmeta_offset: 128,
        vbmeta_size: 0,
    })
}

/// Untrusted metadata may not overflow a block, truncate a descriptor, or
/// smuggle nonzero image bytes behind a standalone metadata header.
#[test]
fn metadata_bounds_reject_truncation_overflow_and_descriptor_escape() {
    let good = metadata();
    for length in 0..good.len() {
        assert!(validate_vbmeta(&good[..length]).is_err());
    }
    for (offset, value, expected) in [
        (12, u64::MAX - 63, Error::Overflow),
        (20, 63, Error::BlockAlignment),
        (64, 65, Error::MetadataBounds),
        (96, 65, Error::MetadataBounds),
        (104, 8, Error::DescriptorBounds),
    ] {
        let mut bytes = good.clone();
        bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        assert_eq!(validate_vbmeta(&bytes), Err(expected));
    }
    let mut bytes = good.clone();
    bytes[104..112].copy_from_slice(&16_u64.to_be_bytes());
    bytes[264..272].copy_from_slice(&64_u64.to_be_bytes());
    assert_eq!(validate_vbmeta(&bytes), Err(Error::DescriptorBounds));
    bytes[264..272].copy_from_slice(&0_u64.to_be_bytes());
    assert_eq!(validate_vbmeta(&bytes).unwrap().size(), 320);
    bytes.push(0);
    assert_eq!(validate_vbmeta(&bytes).unwrap().size(), 320);
    bytes.push(1);
    assert_eq!(validate_vbmeta(&bytes), Err(Error::TrailingData));
}

/// Only valid fixed-end footer geometry authorizes replacing unusable source
/// metadata. A replacement cannot overwrite payload or enlarge the partition.
#[test]
fn graft_geometry_preserves_payload_and_rejects_nonfitting_metadata() {
    let bytes = metadata();
    let old = footer();
    assert_eq!(parse_footer(&old, 512).unwrap().vbmeta_size, 0);
    let graft = plan_graft(512, &old, &bytes).unwrap();
    assert_eq!(graft.image_size(), 512);
    assert_eq!(graft.metadata_offset(), 128);
    assert_eq!(parse_footer(graft.footer(), 512).unwrap().vbmeta_size, 320);
    assert_eq!(
        plan_graft(511, &old, &bytes).unwrap_err(),
        Error::ReplacementDoesNotFit
    );
    let overlap = replacement_footer(Footer {
        original_image_size: 129,
        vbmeta_offset: 128,
        vbmeta_size: 0,
    });
    assert_eq!(parse_footer(&overlap, 512), Err(Error::FooterGeometry));
    let overflow = replacement_footer(Footer {
        original_image_size: 0,
        vbmeta_offset: u64::MAX,
        vbmeta_size: 1,
    });
    assert_eq!(parse_footer(&overflow, 512), Err(Error::Overflow));
    for length in 0..64 {
        assert!(parse_footer(&old[..length], 512).is_err());
    }
    let mut missing = old;
    missing[..4].fill(0);
    assert_eq!(plan_graft(512, &missing, &bytes).unwrap_err(), Error::Magic);
}

/// Every boundary, including a short original gap before the footer, composes
/// identically for in-memory and callback-backed reads; callbacks never read
/// replaced extents. Out-of-bounds requests fail before touching the backing.
#[test]
fn logical_reads_compose_all_overlay_boundaries() {
    let bytes = metadata();
    let old = footer();
    let graft = plan_graft(544, &old, &bytes).unwrap();
    let original = vec![0xa5; 544];
    let mut expected = original.clone();
    expected[128..448].copy_from_slice(&bytes);
    expected[480..].copy_from_slice(graft.footer());
    for start in 0..=544 {
        for end in start..=544 {
            let mut read = vec![0; end - start];
            graft
                .read(start as u64, &mut read, |offset, part| {
                    let offset = offset as usize;
                    assert!(
                        offset + part.len() <= 128 || (offset >= 448 && offset + part.len() <= 480)
                    );
                    part.copy_from_slice(&original[offset..offset + part.len()]);
                    Ok::<(), ()>(())
                })
                .unwrap();
            assert_eq!(read, expected[start..end]);
        }
    }
    let mut read = vec![0; 544];
    graft.copy_from(&original, 0, &mut read).unwrap();
    assert_eq!(read, expected);
    assert_eq!(
        graft.read(u64::MAX, &mut [0], |_, _| Ok::<(), ()>(())),
        Err(ReadError::Layout(Error::ReadBounds))
    );
    assert_eq!(
        graft.read(544, &mut [0], |_, _| panic!("must not read backing")),
        Err(ReadError::<()>::Layout(Error::ReadBounds))
    );
    assert_eq!(
        graft.read(0, &mut [0], |_, _| Err("io")),
        Err(ReadError::Original("io"))
    );
}
