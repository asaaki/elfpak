//! Fuzz the ELF parser.
//!
//! `ElfMetadata::parse_bytes` is the boundary between `elfpak` and untrusted
//! binary input: everything downstream works on the parsed domain model. It must
//! return a value or an error for any byte sequence, and never panic.

#![no_main]

use elfpak_core::ElfMetadata;
use libfuzzer_sys::fuzz_target;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    if let Ok(metadata) = ElfMetadata::parse_bytes(Path::new("<fuzz>"), data) {
        // Touch the fields the resolver relies on, so lazily decoded data is
        // exercised too rather than only the header.
        let _ = metadata.architecture.lib_token();
        let _ = metadata.interpreter.as_deref();
        for soname in &metadata.needed {
            let _ = soname.len();
        }
        for entry in metadata.rpath.iter().chain(metadata.runpath.iter()) {
            let _ = entry.len();
        }
    }
});
