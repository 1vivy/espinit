// SPDX-License-Identifier: Apache-2.0
//! Retained ELF note, read without executing an Android binary from PID 1.
const fn note() -> [u8; 84] {
    let mut bytes = [0; 84];
    bytes[0] = 8; // namesz, little endian
    bytes[4] = 64; // descsz
    bytes[8] = 1; // espinit generation note version
    let name = b"ESPINIT\0";
    let mut i = 0;
    while i < name.len() {
        bytes[12 + i] = name[i];
        i += 1;
    }
    let generation = env!("ESPINIT_GENERATION").as_bytes();
    i = 0;
    while i < generation.len() {
        bytes[20 + i] = generation[i];
        i += 1;
    }
    bytes
}

#[used]
#[unsafe(link_section = ".note.espinit")]
static GENERATION_NOTE: [u8; 84] = note();

pub fn generation() -> &'static str {
    // The live reference keeps the note through linker garbage collection/LTO.
    let bytes = std::hint::black_box(&GENERATION_NOTE);
    std::str::from_utf8(&bytes[20..20 + env!("ESPINIT_GENERATION").len()])
        .expect("build.rs validates ASCII generation")
}
