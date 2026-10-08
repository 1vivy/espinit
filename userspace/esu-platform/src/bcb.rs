// SPDX-License-Identifier: GPL-3.0-only
//! The command and status prefix of AOSP's misc bootloader message.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Request Surfacer/GBL fastboot on the next ordinary restart.
///
/// Only command[32] and status[32] are replaced. Recovery, stage and reserved
/// bytes belong to other boot-chain consumers and must never be written here.
pub fn request_bootloader(misc: &Path, cause: &str) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(misc)?;
    // Prove the record exists before replacing it; a short block device is an
    // error, never a partially initialized message.
    let mut existing = [0u8; 64];
    file.read_exact_at(&mut existing, 0)?;
    let mut prefix = [0u8; 64];
    let command = b"bootonce-bootloader";
    prefix[..command.len()].copy_from_slice(command);
    for (destination, character) in prefix[32..63].iter_mut().zip(cause.chars()) {
        *destination = if character.is_ascii_graphic() || character == ' ' {
            character as u8
        } else {
            b'_'
        };
    }
    file.write_all_at(&prefix, 0)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Misc(std::path::PathBuf);

    impl Misc {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "esu-bcb-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            use std::io::Write;
            file.write_all(&[0xa5; 2048]).unwrap();
            Self(path)
        }
    }

    impl Drop for Misc {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn exact_wire_bytes_preserve_recovery_stage_and_reserved() {
        let misc = Misc::new();
        request_bootloader(&misc.0, "esu:Storage:misc").unwrap();
        let bytes = fs::read(&misc.0).unwrap();
        let mut expected = [0u8; 64];
        expected[..19].copy_from_slice(b"bootonce-bootloader");
        expected[32..48].copy_from_slice(b"esu:Storage:misc");
        assert_eq!(&bytes[..64], &expected);
        assert_eq!(&bytes[64..], &[0xa5; 1984]);
    }

    #[test]
    fn status_is_ascii_sanitized_truncated_and_nul_terminated() {
        let misc = Misc::new();
        request_bootloader(&misc.0, "\0\n\u{e9}abcdefghijklmnopqrstuvwxyz0123456789").unwrap();
        let bytes = fs::read(&misc.0).unwrap();
        assert_eq!(&bytes[32..63], b"___abcdefghijklmnopqrstuvwxyz01");
        assert_eq!(bytes[63], 0);
        assert_eq!(&bytes[64..], &[0xa5; 1984]);
    }

    #[test]
    fn short_misc_is_not_extended_or_written() {
        let misc = Misc::new();
        fs::write(&misc.0, [0xa5; 63]).unwrap();
        assert_eq!(
            request_bootloader(&misc.0, "failure").unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(fs::read(&misc.0).unwrap(), [0xa5; 63]);
    }
}
