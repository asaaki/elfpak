//! Builds a synthetic sysroot out of small C fixtures.
//!
//! Everything is compiled with `-nostdlib` so the dependency graph contains
//! exactly the objects the fixture declares, with no libc noise.

// This module is compiled into each integration test binary, so items used by
// one test look unreachable from another.
#![allow(unreachable_pub)]
#![allow(dead_code)]

use elfpak_core::ElfMetadata;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

/// Directories the fixture DSOs live in.
const LIB_DIRS: &[&str] = &[
    "/usr/lib",
    "/opt/hidden",
    "/opt/origin/lib",
    "/opt/conf/lib",
    "/opt/cached",
];

pub struct Sysroot {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    src: PathBuf,
}

pub fn have_cc() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

impl Sysroot {
    pub fn build() -> Sysroot {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sysroot");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let sysroot = Sysroot {
            _tmp: tmp,
            root,
            src,
        };
        sysroot.populate();
        sysroot
    }

    pub fn path(&self, logical: &str) -> PathBuf {
        self.root.join(logical.trim_start_matches('/'))
    }

    fn mkdir(&self, logical: &str) -> PathBuf {
        let path = self.path(logical);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(&self, logical: &str, contents: &str) {
        let path = self.path(logical);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn source(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.src.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn cc(&self, args: &[&str]) {
        let output = Command::new("cc").args(args).output().expect("cc runs");
        assert!(
            output.status.success(),
            "cc {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Compile a shared object with an explicit soname.
    fn dso(&self, out_logical: &str, soname: &str, body: &str, needs: &[&str]) {
        let name = out_logical.rsplit('/').next().unwrap().to_string();
        let source = self.source(&format!("{name}.c"), body);
        let out = self.path(out_logical);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();

        let mut args: Vec<String> = vec![
            "-shared".into(),
            "-fPIC".into(),
            "-nostdlib".into(),
            format!("-Wl,-soname,{soname}"),
            "-o".into(),
            out.display().to_string(),
            source.display().to_string(),
        ];
        for need in needs {
            args.push(self.path(need).display().to_string());
            args.push(format!(
                "-Wl,-rpath-link,{}",
                self.path(need).parent().unwrap().display()
            ));
        }
        self.cc(&args.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// Compile a dynamic executable with `-nostdlib`.
    fn exe(&self, out_logical: &str, body: &str, needs: &[&str], extra: &[&str]) {
        let name = out_logical.rsplit('/').next().unwrap().to_string();
        let source = self.source(&format!("{name}.c"), body);
        let out = self.path(out_logical);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();

        let mut args: Vec<String> = vec![
            "-nostdlib".into(),
            "-o".into(),
            out.display().to_string(),
            source.display().to_string(),
        ];
        for need in needs {
            args.push(self.path(need).display().to_string());
            args.push(format!(
                "-Wl,-rpath-link,{}",
                self.path(need).parent().unwrap().display()
            ));
        }
        // Link-time lookup of transitive DSOs; unrelated to the runtime search
        // paths the fixtures are testing.
        for dir in LIB_DIRS {
            args.push(format!("-Wl,-rpath-link,{}", self.path(dir).display()));
        }
        args.extend(extra.iter().map(|s| s.to_string()));
        self.cc(&args.iter().map(String::as_str).collect::<Vec<_>>());
    }

    fn symlink(&self, target: &str, logical: &str) {
        let path = self.path(logical);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// The whole fixture sysroot: libraries, the executables that use them, the
    /// loader configuration, and a few things that must never be picked up.
    fn populate(&self) {
        self.populate_libraries();
        self.populate_executables();
        self.populate_loader_config();
        self.populate_decoys();
        self.install_interpreter("/bin/app-default");
    }

    /// Shared objects, including ones deliberately placed where no default
    /// search directory would find them.
    fn populate_libraries(&self) {
        self.mkdir("/usr/lib");
        self.mkdir("/bin");

        // A versioned DSO reached through a soname symlink.
        self.dso(
            "/usr/lib/libbase.so.1.4.2",
            "libbase.so.1",
            "int base_value(void) { return 42; }\n",
            &[],
        );
        self.symlink("libbase.so.1.4.2", "/usr/lib/libbase.so.1");

        // Transitive dependency: libtop -> libbase.
        self.dso(
            "/usr/lib/libtop.so.1",
            "libtop.so.1",
            "int base_value(void);\nint top_value(void) { return base_value() + 1; }\n",
            &["/usr/lib/libbase.so.1"],
        );

        // libmid's own dependency lives outside every default directory, so it
        // is only reachable through an inherited RPATH.
        self.dso(
            "/opt/hidden/libdeep.so.1",
            "libdeep.so.1",
            "int deep_value(void) { return 7; }\n",
            &[],
        );
        self.dso(
            "/usr/lib/libmid.so.1",
            "libmid.so.1",
            "int deep_value(void);\nint mid_value(void) { return deep_value(); }\n",
            &["/opt/hidden/libdeep.so.1"],
        );

        // $ORIGIN-relative layout.
        self.dso(
            "/opt/origin/lib/libor.so.1",
            "libor.so.1",
            "int or_value(void) { return 3; }\n",
            &[],
        );

        // Reachable only through /etc/ld.so.conf.
        self.dso(
            "/opt/conf/lib/libconf.so.1",
            "libconf.so.1",
            "int conf_value(void) { return 5; }\n",
            &[],
        );

        // Reachable only through /etc/ld.so.cache.
        self.dso(
            "/opt/cached/libcached.so.1",
            "libcached.so.1",
            "int cached_value(void) { return 9; }\n",
            &[],
        );

        // An NSS module: glibc dlopen()s these, so runtime policy adds them
        // rather than DT_NEEDED. Nothing in the fixtures links against it.
        self.dso(
            "/usr/lib/libnss_files.so.2",
            "libnss_files.so.2",
            "int _nss_files_getpwnam_r(void) { return 0; }\n",
            &[],
        );
    }

    /// One executable per way of finding a library: the defaults, an inherited
    /// `DT_RPATH`, a non-inherited `DT_RUNPATH`, `$ORIGIN`, `ld.so.conf`, the
    /// cache, and one whose dependency is missing.
    fn populate_executables(&self) {
        let main = "int top_value(void);\nvoid _start(void) { top_value(); }\n";
        self.exe("/bin/app-default", main, &["/usr/lib/libtop.so.1"], &[]);

        let mid_main = "int mid_value(void);\nvoid _start(void) { mid_value(); }\n";
        self.exe(
            "/bin/app-rpath",
            mid_main,
            &["/usr/lib/libmid.so.1"],
            &["-Wl,--disable-new-dtags,-rpath,/opt/hidden"],
        );
        self.exe(
            "/bin/app-runpath",
            mid_main,
            &["/usr/lib/libmid.so.1"],
            &["-Wl,--enable-new-dtags,-rpath,/opt/hidden"],
        );

        let or_main = "int or_value(void);\nvoid _start(void) { or_value(); }\n";
        self.exe(
            "/opt/origin/bin/app-origin",
            or_main,
            &["/opt/origin/lib/libor.so.1"],
            &["-Wl,--enable-new-dtags,-rpath,$ORIGIN/../lib"],
        );

        let conf_main = "int conf_value(void);\nvoid _start(void) { conf_value(); }\n";
        self.exe(
            "/bin/app-conf",
            conf_main,
            &["/opt/conf/lib/libconf.so.1"],
            &[],
        );

        let cached_main = "int cached_value(void);\nvoid _start(void) { cached_value(); }\n";
        self.exe(
            "/bin/app-cached",
            cached_main,
            &["/opt/cached/libcached.so.1"],
            &[],
        );

        // A dependency that is simply not there: the binary is linked against a
        // real library, then its DT_NEEDED is patched to name a missing one.
        let missing_main = "int base_value(void);\nvoid _start(void) { base_value(); }\n";
        self.exe(
            "/bin/app-missing",
            missing_main,
            &["/usr/lib/libbase.so.1"],
            &[],
        );
        patch_needed(
            &self.path("/bin/app-missing"),
            "libbase.so.1",
            "libgone.so.9",
        );
    }

    /// What tells the loader where to look: `ld.so.conf`, its fragments, and a
    /// real `ld.so.cache` written without ever running `ldconfig`.
    fn populate_loader_config(&self) {
        self.write("/etc/ld.so.conf", "include /etc/ld.so.conf.d/*.conf\n");
        self.write(
            "/etc/ld.so.conf.d/local.conf",
            "# generated\n/opt/conf/lib\n",
        );
        std::fs::write(
            self.path("/etc/ld.so.cache"),
            ld_cache(&[("libcached.so.1", "/opt/cached/libcached.so.1")]),
        )
        .unwrap();
    }

    /// Things that must never be mistaken for a library, and the merged-`/usr`
    /// symlink every mainstream distribution now has.
    fn populate_decoys(&self) {
        // A file that merely has the right name must never satisfy a lookup.
        self.mkdir("/opt/decoy");
        self.write("/opt/decoy/libbase.so.1", "this is not an ELF object\n");

        // /lib is a symlink to usr/lib, like on a merged-/usr distribution.
        self.symlink("usr/lib", "/lib");
    }

    /// Copy the host's dynamic loader to the PT_INTERP path the fixtures declare.
    fn install_interpreter(&self, exe_logical: &str) {
        let metadata = ElfMetadata::parse_file(&self.path(exe_logical)).unwrap();
        let interp = metadata.interpreter.expect("fixtures are dynamic");
        let host = interp.clone();
        let target = self.path(&interp.to_string_lossy());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(&host, &target)
            .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", host.display(), target.display()));
    }
}

/// Overwrite a `DT_NEEDED` string in place; both names must be the same length.
fn patch_needed(path: &Path, from: &str, to: &str) {
    assert_eq!(
        from.len(),
        to.len(),
        "replacement must keep the string size"
    );
    let mut bytes = std::fs::read(path).unwrap();
    let needle = from.as_bytes();
    let position = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("soname present");
    bytes[position..position + needle.len()].copy_from_slice(to.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

fn offset(value: usize) -> u32 {
    u32::try_from(value).expect("fixture offsets fit in u32")
}

/// Minimal `glibc-ld.so.cache1.1` image.
pub fn ld_cache(entries: &[(&str, &str)]) -> Vec<u8> {
    const HEADER: usize = 48;
    const ENTRY: usize = 24;

    let mut strings = Vec::new();
    let mut offsets = Vec::new();
    for (soname, path) in entries {
        let key = offset(strings.len());
        strings.extend_from_slice(soname.as_bytes());
        strings.push(0);
        let value = offset(strings.len());
        strings.extend_from_slice(path.as_bytes());
        strings.push(0);
        offsets.push((key, value));
    }
    let base = offset(HEADER + entries.len() * ENTRY);

    let mut out = Vec::new();
    out.extend_from_slice(b"glibc-ld.so.cache");
    out.extend_from_slice(b"1.1");
    out.extend_from_slice(&offset(entries.len()).to_le_bytes());
    out.extend_from_slice(&offset(strings.len()).to_le_bytes());
    out.push(2); // cache_file_new_flags_endian_little
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    for (key, value) in &offsets {
        out.extend_from_slice(&0x0000_0303u32.to_le_bytes()); // x86_64 ELF libc6
        out.extend_from_slice(&(key + base).to_le_bytes());
        out.extend_from_slice(&(value + base).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }
    out.extend_from_slice(&strings);
    out
}

/// Raw `ldd` output, kept even when the loader reports a missing library.
///
/// `ldd` is a *test oracle* only. `elfpak` itself never runs it.
pub fn ldd_raw(binary: &Path) -> Option<String> {
    let output = Command::new("ldd").arg(binary).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Library paths the glibc loader resolves for `binary`, canonicalized.
///
/// Returns `None` when `ldd` is unavailable or reported a failure, so callers
/// can skip rather than assert against nothing.
pub fn ldd_closure(binary: &Path) -> Option<BTreeSet<PathBuf>> {
    let text = ldd_raw(binary)?;
    if text.contains("not found") {
        return None;
    }
    let mut paths = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("linux-vdso") || line.contains("statically linked") {
            continue;
        }
        let candidate = match line.split_once("=>") {
            Some((_, rest)) => rest.trim(),
            None => line,
        };
        let path = candidate.split(" (").next().unwrap_or("").trim();
        if path.is_empty() || !path.starts_with('/') {
            continue;
        }
        paths.insert(std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)));
    }
    (!paths.is_empty()).then_some(paths)
}

/// The same loader scenarios as [`Sysroot`], but installed at absolute paths on
/// the host so that the real glibc loader resolves them too.
///
/// That makes `ldd` usable as an oracle for RPATH inheritance, RUNPATH
/// non-inheritance and `$ORIGIN` expansion, which a sysroot-only fixture cannot
/// verify without root privileges.
pub struct HostFixtures {
    _tmp: tempfile::TempDir,
    pub dir: PathBuf,
}

impl HostFixtures {
    pub fn build() -> HostFixtures {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let fixtures = HostFixtures { _tmp: tmp, dir };
        fixtures.populate();
        fixtures
    }

    pub fn bin(&self, name: &str) -> PathBuf {
        self.dir.join("bin").join(name)
    }

    fn populate(&self) {
        let lib = self.dir.join("lib");
        let bin = self.dir.join("bin");
        let src = self.dir.join("src");
        for dir in [&lib, &bin, &src, &self.dir.join("hidden")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        self.populate_libraries(&lib, &src);
        self.populate_executables(&lib, &bin, &src);
    }

    /// Write a C source file and return its path.
    fn source(src: &Path, name: &str, body: &str) -> PathBuf {
        let path = src.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn populate_libraries(&self, lib: &Path, src: &Path) {
        let write = |name: &str, body: &str| Self::source(src, name, body);

        // A leaf library reached through a soname symlink.
        cc(&[
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libbase.so.1",
            "-o",
            lib.join("libbase.so.1.4.2").to_str().unwrap(),
            write("base.c", "int base_value(void) { return 42; }\n")
                .to_str()
                .unwrap(),
        ]);
        std::os::unix::fs::symlink("libbase.so.1.4.2", lib.join("libbase.so.1")).unwrap();

        // libtop -> libbase, so the executable's search path has to cover a
        // dependency it does not declare itself.
        cc(&[
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libtop.so.1",
            "-o",
            lib.join("libtop.so.1").to_str().unwrap(),
            write(
                "top.c",
                "int base_value(void);\nint top_value(void) { return base_value(); }\n",
            )
            .to_str()
            .unwrap(),
            lib.join("libbase.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
        ]);

        // The same library, but carrying a DT_RUNPATH of its own that points
        // nowhere useful. glibc suppresses the entire RPATH phase for an object
        // with DT_RUNPATH, including its loaders' RPATHs, so this one cannot
        // reach libbase however the executable that loads it is linked.
        cc(&[
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libtop-runpath.so.1",
            "-o",
            lib.join("libtop-runpath.so.1").to_str().unwrap(),
            write(
                "top-runpath.c",
                "int base_value(void);\nint top_runpath_value(void) { return base_value(); }\n",
            )
            .to_str()
            .unwrap(),
            lib.join("libbase.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
            &format!(
                "-Wl,--enable-new-dtags,-rpath,{}",
                self.dir.join("nowhere").display()
            ),
        ]);
    }

    fn populate_executables(&self, lib: &Path, bin: &Path, src: &Path) {
        let write = |name: &str, body: &str| Self::source(src, name, body);

        let main = write(
            "app.c",
            "int top_value(void);\nvoid _start(void) { top_value(); }\n",
        );
        let base_main = write(
            "app-base.c",
            "int base_value(void);\nvoid _start(void) { base_value(); }\n",
        );

        // DT_RPATH: applies to the whole loading chain.
        cc(&[
            "-nostdlib",
            "-o",
            bin.join("app-rpath").to_str().unwrap(),
            main.to_str().unwrap(),
            lib.join("libtop.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
            &format!("-Wl,--disable-new-dtags,-rpath,{}", lib.display()),
        ]);

        // DT_RUNPATH: applies to this object only, so libbase must not resolve.
        cc(&[
            "-nostdlib",
            "-o",
            bin.join("app-runpath").to_str().unwrap(),
            main.to_str().unwrap(),
            lib.join("libtop.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
            &format!("-Wl,--enable-new-dtags,-rpath,{}", lib.display()),
        ]);

        // DT_RPATH on the executable, DT_RUNPATH on the library it loads. The
        // RPATH covers both libraries, but the loader must not apply it to the
        // library's own lookup.
        cc(&[
            "-nostdlib",
            "-o",
            bin.join("app-rpath-blocked").to_str().unwrap(),
            write(
                "app-blocked.c",
                "int top_runpath_value(void);\nvoid _start(void) { top_runpath_value(); }\n",
            )
            .to_str()
            .unwrap(),
            lib.join("libtop-runpath.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
            &format!("-Wl,--disable-new-dtags,-rpath,{}", lib.display()),
        ]);

        // $ORIGIN expansion, against a dependency with no dependencies of its own.
        cc(&[
            "-nostdlib",
            "-o",
            bin.join("app-origin").to_str().unwrap(),
            base_main.to_str().unwrap(),
            lib.join("libbase.so.1").to_str().unwrap(),
            &format!("-Wl,-rpath-link,{}", lib.display()),
            "-Wl,--enable-new-dtags,-rpath,$ORIGIN/../lib",
        ]);
    }
}

pub fn cc(args: &[&str]) {
    let output = Command::new("cc").args(args).output().expect("cc runs");
    assert!(
        output.status.success(),
        "cc {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
