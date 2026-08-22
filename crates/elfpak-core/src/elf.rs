//! ELF parsing boundary.
//!
//! `goblin` types must not leak past this module: everything downstream works
//! with [`ElfMetadata`], which is the only domain model of an ELF object.

use crate::error::{Error, Result, io};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Machine, ELF class and endianness: everything that decides whether two
/// objects can be linked into the same process image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Architecture {
    pub machine: Machine,
    pub class: ElfClass,
    pub endianness: Endianness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Machine {
    X86_64,
    Aarch64,
    I386,
    Arm,
    RiscV64,
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElfClass {
    Elf32,
    Elf64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Executable,
    SharedObject,
    Relocatable,
    Core,
    Other(u16),
}

impl Machine {
    pub(crate) fn from_e_machine(machine: u16) -> Machine {
        match machine {
            3 => Machine::I386,
            40 => Machine::Arm,
            62 => Machine::X86_64,
            183 => Machine::Aarch64,
            243 => Machine::RiscV64,
            other => Machine::Other(other),
        }
    }

    /// glibc's `$PLATFORM` token and the historical `uname -m` spelling.
    pub fn platform_token(&self) -> Option<&'static str> {
        match self {
            Machine::X86_64 => Some("x86_64"),
            Machine::Aarch64 => Some("aarch64"),
            Machine::I386 => Some("i686"),
            Machine::Arm => Some("arm"),
            Machine::RiscV64 => Some("riscv64"),
            Machine::Other(_) => None,
        }
    }

    /// Multiarch tuple used for Debian-style `/lib/<tuple>` directories.
    pub fn debian_multiarch(&self) -> Option<&'static str> {
        match self {
            Machine::X86_64 => Some("x86_64-linux-gnu"),
            Machine::Aarch64 => Some("aarch64-linux-gnu"),
            Machine::I386 => Some("i386-linux-gnu"),
            Machine::RiscV64 => Some("riscv64-linux-gnu"),
            Machine::Arm => Some("arm-linux-gnueabihf"),
            Machine::Other(_) => None,
        }
    }

    pub fn is_supported_target(&self) -> bool {
        matches!(self, Machine::X86_64 | Machine::Aarch64)
    }
}

impl std::fmt::Display for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Machine::X86_64 => f.write_str("x86_64"),
            Machine::Aarch64 => f.write_str("aarch64"),
            Machine::I386 => f.write_str("i386"),
            Machine::Arm => f.write_str("arm"),
            Machine::RiscV64 => f.write_str("riscv64"),
            Machine::Other(_) => f.write_str("unknown"),
        }
    }
}

impl Architecture {
    /// Whether a candidate shared object can satisfy a request from an object
    /// of this architecture.
    pub fn is_compatible_with(&self, other: &Architecture) -> bool {
        self == other
    }

    /// Value of glibc's `$LIB` token for this architecture.
    pub fn lib_token(&self) -> &'static str {
        match (self.machine, self.class) {
            (Machine::X86_64, ElfClass::Elf64) => "lib64",
            (Machine::Aarch64, ElfClass::Elf64) => "lib64",
            (_, ElfClass::Elf64) => "lib64",
            (_, ElfClass::Elf32) => "lib",
        }
    }
}

impl std::fmt::Display for Architecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let class = match self.class {
            ElfClass::Elf32 => "ELF32",
            ElfClass::Elf64 => "ELF64",
        };
        let end = match self.endianness {
            Endianness::Little => "LSB",
            Endianness::Big => "MSB",
        };
        write!(f, "{class} {end} {}", self.machine)
    }
}

/// Everything `elfpak` needs to know about a single ELF object.
#[derive(Debug, Clone)]
pub struct ElfMetadata {
    /// Path the metadata was read from (host path, not logical rootfs path).
    pub path: PathBuf,
    pub architecture: Architecture,
    /// Raw `e_machine`, kept so diagnostics can name what was actually found
    /// even for architectures [`Machine`] does not model.
    pub e_machine: u16,
    pub object_type: ObjectType,
    pub interpreter: Option<PathBuf>,
    pub needed: Vec<String>,
    pub soname: Option<String>,
    pub rpath: Vec<String>,
    pub runpath: Vec<String>,
    /// Whether the object has a `DT_RUNPATH` tag, even if its value is empty.
    pub has_runpath: bool,
    /// `DF_1_NODEFLIB`: skip default library search directories.
    pub nodeflib: bool,
    /// `DF_1_ORIGIN` / `DF_ORIGIN`: object expects `$ORIGIN` processing.
    pub origin_flag: bool,
    pub is_dynamic: bool,
    /// References to `dlopen`-family functions, which static analysis cannot follow.
    pub dlopen_references: Vec<String>,
    pub size: u64,
}

/// `EI_MAG0..EI_MAG3`, the four bytes every ELF object starts with.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

const DF_ORIGIN: u64 = 0x1;
const DF_1_NODEFLIB: u64 = 0x0000_0800;
const DF_1_ORIGIN: u64 = 0x0000_0080;

const DLOPEN_SYMBOLS: &[&str] = &["dlopen", "dlmopen", "__libc_dlopen_mode"];

impl ElfMetadata {
    pub fn parse_file(path: &Path) -> Result<ElfMetadata> {
        let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
        Self::parse_bytes(path, &bytes)
    }

    /// Cheap check used to decide whether a candidate file is worth parsing.
    pub fn looks_like_elf(bytes: &[u8]) -> bool {
        bytes.len() >= ELF_MAGIC.len() && &bytes[..ELF_MAGIC.len()] == ELF_MAGIC
    }

    pub fn parse_bytes(path: &Path, bytes: &[u8]) -> Result<ElfMetadata> {
        if !Self::looks_like_elf(bytes) {
            return Err(Error::NotElf {
                path: path.to_path_buf(),
            });
        }

        let elf = goblin::elf::Elf::parse(bytes).map_err(|e| Error::Elf {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

        let (flags, flags_1) = match &elf.dynamic {
            Some(dynamic) => (dynamic.info.flags, dynamic.info.flags_1),
            None => (0, 0),
        };
        let rpath = parse_search_paths(path, "DT_RPATH", &elf.rpaths)?;
        let runpath = parse_search_paths(path, "DT_RUNPATH", &elf.runpaths)?;
        let has_runpath = elf.dynamic.as_ref().is_some_and(|dynamic| {
            dynamic
                .dyns
                .iter()
                .any(|entry| entry.d_tag == goblin::elf::dynamic::DT_RUNPATH)
        });
        Ok(ElfMetadata {
            path: path.to_path_buf(),
            architecture: architecture_of(&elf),
            e_machine: elf.header.e_machine,
            object_type: object_type_of(elf.header.e_type),
            interpreter: elf.interpreter.map(PathBuf::from),
            needed: elf.libraries.iter().map(|s| s.to_string()).collect(),
            soname: elf.soname.map(|s| s.to_string()),
            rpath,
            runpath,
            has_runpath,
            nodeflib: flags_1 & DF_1_NODEFLIB != 0,
            origin_flag: flags & DF_ORIGIN != 0 || flags_1 & DF_1_ORIGIN != 0,
            is_dynamic: elf.dynamic.is_some(),
            dlopen_references: dlopen_references(&elf),
            size: bytes.len() as u64,
        })
    }

    /// Effective search list contributed by this object: `DT_RUNPATH` wins over
    /// `DT_RPATH`, exactly like the glibc loader.
    pub fn runpath_is_authoritative(&self) -> bool {
        self.has_runpath
    }
}

fn architecture_of(elf: &goblin::elf::Elf<'_>) -> Architecture {
    Architecture {
        machine: Machine::from_e_machine(elf.header.e_machine),
        class: if elf.is_64 {
            ElfClass::Elf64
        } else {
            ElfClass::Elf32
        },
        endianness: if elf.little_endian {
            Endianness::Little
        } else {
            Endianness::Big
        },
    }
}

fn object_type_of(e_type: u16) -> ObjectType {
    match e_type {
        goblin::elf::header::ET_EXEC => ObjectType::Executable,
        goblin::elf::header::ET_DYN => ObjectType::SharedObject,
        goblin::elf::header::ET_REL => ObjectType::Relocatable,
        goblin::elf::header::ET_CORE => ObjectType::Core,
        other => ObjectType::Other(other),
    }
}

/// Undefined `dlopen`-family symbols, i.e. calls out of this object. A defined
/// one is the implementation in libdl and says nothing about what gets loaded.
fn dlopen_references(elf: &goblin::elf::Elf<'_>) -> Vec<String> {
    let mut found = Vec::new();
    for sym in elf.dynsyms.iter() {
        if let Some(name) = elf.dynstrtab.get_at(sym.st_name)
            && sym.st_shndx == 0
            && DLOPEN_SYMBOLS.contains(&name)
        {
            found.push(name.to_string());
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// `DT_RPATH`/`DT_RUNPATH`/`LD_LIBRARY_PATH` are colon separated lists.
///
/// Search paths that depend on a runtime current working directory cannot be
/// reproduced in a relocatable bundle, so they are refused rather than being
/// silently interpreted as `$ORIGIN` paths.
fn parse_search_paths(path: &Path, tag: &str, values: &[&str]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for value in values {
        // An explicitly empty `DT_RUNPATH` is valid and still suppresses
        // `DT_RPATH`; it contributes no directories of its own.
        if value.is_empty() {
            continue;
        }
        for entry in value.split(':') {
            let origin_relative = entry == "$ORIGIN"
                || entry == "${ORIGIN}"
                || entry.starts_with("$ORIGIN/")
                || entry.starts_with("${ORIGIN}/");
            if entry.is_empty() || (!entry.starts_with('/') && !origin_relative) {
                return Err(Error::Config {
                    message: format!(
                        "`{}` contains unsupported {tag} entry `{entry}`; empty and relative loader search paths depend on the runtime working directory",
                        path.display()
                    ),
                });
            }
            paths.push(entry.to_string());
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_machine() -> Machine {
        if cfg!(target_arch = "x86_64") {
            Machine::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Machine::Aarch64
        } else {
            Machine::Other(0)
        }
    }

    #[test]
    fn parses_a_real_dynamic_executable() {
        let exe = std::env::current_exe().unwrap();
        let metadata = ElfMetadata::parse_file(&exe).unwrap();

        assert_eq!(metadata.architecture.class, ElfClass::Elf64);
        assert_eq!(metadata.architecture.endianness, Endianness::Little);
        assert_eq!(metadata.architecture.machine, host_machine());
        assert!(metadata.is_dynamic);
        assert!(metadata.interpreter.is_some(), "test binaries are dynamic");
        assert!(
            metadata.needed.iter().any(|n| n.starts_with("libc.so")),
            "{:?}",
            metadata.needed
        );
        assert!(metadata.size > 0);
    }

    #[test]
    fn rejects_non_elf_input() {
        let err = ElfMetadata::parse_bytes(Path::new("/x"), b"#!/bin/sh\n").unwrap_err();
        assert_eq!(err.code(), "E1002");
        assert!(!ElfMetadata::looks_like_elf(b"MZ"));
        assert!(ElfMetadata::looks_like_elf(b"\x7fELF..."));
    }

    #[test]
    fn truncated_elf_is_an_error_not_a_panic() {
        let exe = std::env::current_exe().unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        let err = ElfMetadata::parse_bytes(&exe, &bytes[..64]).unwrap_err();
        assert_eq!(err.code(), "E1001");
    }

    #[test]
    fn architecture_compatibility_is_exact() {
        let x86 = Architecture {
            machine: Machine::X86_64,
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
        };
        let arm = Architecture {
            machine: Machine::Aarch64,
            ..x86
        };
        let x86_32 = Architecture {
            class: ElfClass::Elf32,
            ..x86
        };
        assert!(x86.is_compatible_with(&x86));
        assert!(!x86.is_compatible_with(&arm));
        assert!(!x86.is_compatible_with(&x86_32));
        assert_eq!(x86.lib_token(), "lib64");
        assert_eq!(x86_32.lib_token(), "lib");
        assert_eq!(arm.machine.debian_multiarch(), Some("aarch64-linux-gnu"));
    }

    #[test]
    fn dynamic_search_paths_refuse_cwd_dependent_entries() {
        let path = Path::new("/app");
        assert!(parse_search_paths(path, "DT_RUNPATH", &["/a:$ORIGIN/lib"]).is_ok());
        assert!(parse_search_paths(path, "DT_RUNPATH", &["/a::/b"]).is_err());
        assert!(parse_search_paths(path, "DT_RUNPATH", &["relative"]).is_err());
    }

    #[test]
    fn only_supported_targets_are_accepted() {
        assert!(Machine::X86_64.is_supported_target());
        assert!(Machine::Aarch64.is_supported_target());
        assert!(!Machine::I386.is_supported_target());
        assert!(!Machine::Other(0xbeef).is_supported_target());
    }

    #[test]
    fn the_raw_machine_is_kept_for_diagnostics() {
        let exe = std::env::current_exe().unwrap();
        let metadata = ElfMetadata::parse_file(&exe).unwrap();
        assert_eq!(
            Machine::from_e_machine(metadata.e_machine),
            metadata.architecture.machine
        );
        assert_eq!(Machine::from_e_machine(243), Machine::RiscV64);
        assert_eq!(Machine::Other(0xbeef).to_string(), "unknown");
    }
}
