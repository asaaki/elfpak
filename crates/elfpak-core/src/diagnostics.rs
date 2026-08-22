//! Every diagnostic code `elfpak` can print, in one place.
//!
//! The CLI renders a failure as `error[E2001]` and a warning as
//! `warning[E2005]`. Scripts match on those codes, so they are stable, and a
//! code means exactly one thing. Errors and warnings share one namespace, so
//! they are declared together; a single list is the only thing that can prove
//! there are no duplicates.
//!
//! The family says what a code is about: `E1xxx` reads an object, `E2xxx`
//! resolves a dependency, `E3xxx` touches a path, `E4xxx` is configuration,
//! `E5xxx` is verification.

/// Declare the codes and the list [`ALL`] the uniqueness check reads, so that a
/// code cannot be added to one without appearing in the other.
macro_rules! codes {
    (
        errors { $($(#[$error_doc:meta])* $error:ident = $error_code:literal;)* }
        warnings { $($(#[$warning_doc:meta])* $warning:ident = $warning_code:literal;)* }
    ) => {
        /// Codes for [`crate::Error`], one per variant. See [`crate::Error::code`].
        pub mod error {
            $($(#[$error_doc])* pub const $error: &str = $error_code;)*
        }

        /// Codes for [`crate::plan::Warning`]. A warning never fails a build; it
        /// reports something the analysis found that the bundle cannot express.
        pub mod warning {
            $($(#[$warning_doc])* pub const $warning: &str = $warning_code;)*
        }

        /// Every code `elfpak` can print, errors first.
        ///
        /// The uniqueness check reads this, as can anything else that
        /// enumerates the namespace. A caller that wants one code names it
        /// directly.
        pub const ALL: &[&str] = &[$($error_code,)* $($warning_code,)*];
    };
}

codes! {
    errors {
        /// A read or write of a named path failed, carrying the OS error.
        /// Probing a directory that does not exist is not an error; a missing
        /// candidate is an ordinary answer during a lookup.
        IO = "E1000";
        /// A file begins with the ELF magic but does not parse: truncated, or
        /// with a header whose offsets do not describe its own contents. Such a
        /// file is skipped while probing search directories, and reported only
        /// for a file `elfpak` was told to read.
        ELF = "E1001";
        /// The input does not start with `\x7fELF`: a shell script, a wrapper,
        /// or the wrong file.
        NOT_ELF = "E1002";
        /// The executable targets a machine `elfpak` does not package for. The
        /// raw `e_machine` is named too, because the supported set is smaller
        /// than the set the parser can identify.
        UNSUPPORTED_ARCHITECTURE = "E1003";
        /// A named bound was reached: nodes or edges in the closure, or
        /// directories in one lookup. Every bound sits far above what a real
        /// program produces, so this means synthetic or malformed input rather
        /// than a large application.
        LIMIT_EXCEEDED = "E1005";
        /// A source file's digest or size no longer matched the plan when its
        /// bytes were copied. Something wrote to the source root during the
        /// run, and the output would no longer match its own manifest.
        SOURCE_CHANGED = "E1006";
        /// A `DT_NEEDED` name or `PT_INTERP` matched nothing. The directories
        /// searched are listed in the order the loader would have tried them.
        UNRESOLVED_LIBRARY = "E2001";
        /// The closure needs a library `--allow-library` does not name. The
        /// allow-list is a contract: a new native dependency fails the build
        /// instead of growing the image.
        DISALLOWED_LIBRARY = "E2002";
        /// A candidate with the right name has the wrong machine, ELF class or
        /// endianness. It is reported in preference to plain absence, because
        /// it names what was found; the usual cause is a host library reached
        /// while packaging from a sysroot for another architecture.
        INCOMPATIBLE_ARCHITECTURE = "E2003";
        /// A runtime policy feature found nothing to contribute:
        /// `--ca-certificates` with no trust store in the source root,
        /// `--tzdata` with no zoneinfo. A bundle that silently shipped neither
        /// would fail at its first HTTPS request.
        MISSING_RUNTIME_FILE = "E2004";
        /// A path resolved outside the root it belongs to, or through a
        /// symlinked parent leading out of the output directory.
        PATH_ESCAPE = "E3001";
        /// An `--include` names a path the source root does not have. `E2004`
        /// covers the same situation for runtime policy, which probes several
        /// candidate locations; an `--include` names exactly one.
        MISSING_SOURCE_PATH = "E3002";
        /// Resolving a logical path took more symlink hops than glibc's
        /// `SYMLOOP_MAX`, so it is a path the loader would not resolve either.
        SYMLINK_LOOP = "E3003";
        /// The caller asked for something that does not hold together: an
        /// unparseable `--user`, no output, an `--install` landing on a library
        /// the closure needs at that exact path, or a plan grown past
        /// [`crate::plan::PLAN_ENTRIES_MAX`].
        CONFIG = "E4001";
        /// A manifest could not be read, or does not parse as one.
        MANIFEST = "E4002";
        /// `elfpak verify` found at least one problem. This carries the
        /// counts; the problems themselves are printed as they are found.
        VERIFY_FAILED = "E5001";
    }
    warnings {
        /// An object has an undefined reference to a `dlopen`-family function,
        /// so it may load libraries no `DT_NEEDED` names. Nothing is known to
        /// be missing; `--include` covers anything that is.
        DLOPEN = "E1004";
        /// A library was found through the build host's `ld.so.cache`, its
        /// `ld.so.conf`, or `--library-path`. None of those travel with the
        /// bundle, so the packaged loader will not look where the library sits.
        LIBRARY_UNREACHABLE = "E2005";
        /// `--install` moves an executable that declares `$ORIGIN`-relative
        /// search paths, so those paths no longer point where they did.
        EXECUTABLE_RELOCATED = "E2006";
        /// `--user` records an identity in `passwd`/`group`, and neither file
        /// was asked for, so an application that looks its own uid up finds
        /// nothing.
        USER_WITHOUT_PASSWD_GROUP = "E4003";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `error[E1006]` and a `warning[E1006]` that mean different things
    /// make the codes useless to match on. This is why they share one list.
    #[test]
    fn no_two_diagnostics_share_a_code() {
        let mut sorted = ALL.to_vec();
        sorted.sort_unstable();
        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(sorted, unique, "a diagnostic code is used twice");
    }

    #[test]
    fn codes_are_well_formed() {
        for code in ALL {
            let digits = code.strip_prefix('E').expect("codes start with `E`");
            assert_eq!(digits.len(), 4, "{code}");
            assert!(digits.bytes().all(|b| b.is_ascii_digit()), "{code}");
        }
    }
}
