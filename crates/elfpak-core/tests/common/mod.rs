//! Builds a synthetic sysroot out of small C fixtures.
//!
//! Everything is compiled with `-nostdlib` so the dependency graph contains
//! exactly the objects the fixture declares, with no libc noise.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use elfpak_core::ElfMetadata;

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

    fn populate(&self) {
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

        // A dependency that is simply not there.
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

        // A file that merely has the right name must never satisfy a lookup.
        self.mkdir("/opt/decoy");
        self.write("/opt/decoy/libbase.so.1", "this is not an ELF object\n");

        // /lib is a symlink to usr/lib, like on a merged-/usr distribution.
        self.symlink("usr/lib", "/lib");

        self.install_interpreter("/bin/app-default");
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

/// Minimal `glibc-ld.so.cache1.1` image.
pub fn ld_cache(entries: &[(&str, &str)]) -> Vec<u8> {
    const HEADER: usize = 48;
    const ENTRY: usize = 24;

    let mut strings = Vec::new();
    let mut offsets = Vec::new();
    for (soname, path) in entries {
        let key = strings.len() as u32;
        strings.extend_from_slice(soname.as_bytes());
        strings.push(0);
        let value = strings.len() as u32;
        strings.extend_from_slice(path.as_bytes());
        strings.push(0);
        offsets.push((key, value));
    }
    let base = (HEADER + entries.len() * ENTRY) as u32;

    let mut out = Vec::new();
    out.extend_from_slice(b"glibc-ld.so.cache");
    out.extend_from_slice(b"1.1");
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.push(0);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    for (key, value) in &offsets {
        out.extend_from_slice(&0x0300_0003u32.to_le_bytes());
        out.extend_from_slice(&(key + base).to_le_bytes());
        out.extend_from_slice(&(value + base).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }
    out.extend_from_slice(&strings);
    out
}
