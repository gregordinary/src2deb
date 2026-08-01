//! Host architecture detection and the native/foreign relation.
//!
//! A recipe that names no architecture builds for the host, so src2deb needs
//! the host's Debian architecture name to default the target and to tell whether
//! a build runs foreign. ferroday-cage performs the authoritative binfmt
//! preflight when it bootstraps a foreign root; this module only detects the
//! host and mirrors ferroday-cage's native/foreign rule, reading the host from
//! `uname` the same way, so the two never disagree on which builds are foreign.

/// The Debian architecture name for the host this binary runs on, from `uname`.
///
/// Returns the raw machine name for a host this table does not know, so a caller
/// on an unusual host can still proceed by naming the architecture in the recipe.
/// The mapping mirrors ferroday-cage's, so a build src2deb calls native is one
/// ferroday-cage bootstraps without a binfmt handler.
pub fn host_architecture() -> String {
    let uname = rustix::system::uname();
    let machine = uname.machine().to_string_lossy();
    match machine.as_ref() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7l" | "armv6l" => "armhf",
        "i686" | "i586" | "i386" => "i386",
        "ppc64le" => "ppc64el",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        other => other,
    }
    .to_string()
}

/// Reports why a Debian architecture name is unsafe to use, or `None` when it is
/// safe.
///
/// The name becomes a path segment in the local pool (`binary-<arch>`) and a
/// whitespace-separated field in the build root's plan key, so it must be a
/// single benign token: an embedded separator would place the pool index outside
/// its suite, and embedded whitespace would blur the plan key's fields and make
/// two different plans hash alike.
///
/// The check is deliberately shape-based rather than a list of known
/// architectures. [`host_architecture`] passes an unrecognized machine name
/// through so an unusual host can still build, and a recipe may name an
/// architecture this crate has never heard of; ferroday-cage's bootstrap is the
/// authority on whether the architecture actually exists.
pub fn architecture_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("is empty")
    } else if name.contains("..") {
        Some("contains \"..\"")
    } else if name.contains(['/', '\\', '\0']) {
        Some("contains a path separator")
    } else if name.starts_with('-') {
        Some("starts with '-'")
    } else if name.contains(char::is_whitespace) {
        Some("contains whitespace")
    } else {
        None
    }
}

/// Whether building `target` on `host` runs foreign — through a `qemu-user`
/// binfmt handler rather than the CPU directly.
///
/// Mirrors ferroday-cage's rule: identical architectures run natively, and an
/// amd64 host runs i386 through its IA-32 compatibility mode, so both are
/// native. Every other pair is foreign and needs an emulator. The relation is
/// directional — an i386 host cannot run amd64.
pub fn is_foreign(host: &str, target: &str) -> bool {
    !(host == target || matches!((host, target), ("amd64", "i386")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_architecture_is_a_debian_name() {
        // Whatever the test host is, the detected name is non-empty and carries
        // no `uname` spelling this table maps.
        let host = host_architecture();
        assert!(!host.is_empty());
        assert_ne!(host, "x86_64");
        assert_ne!(host, "aarch64");
    }

    #[test]
    fn native_and_foreign_pairs_match_ferroday_cages_rule() {
        // Identical is native; amd64 runs i386 natively; the reverse and any
        // unrelated pair is foreign.
        assert!(!is_foreign("amd64", "amd64"));
        assert!(!is_foreign("arm64", "arm64"));
        assert!(!is_foreign("amd64", "i386"));
        assert!(is_foreign("i386", "amd64"));
        assert!(is_foreign("amd64", "arm64"));
        assert!(is_foreign("arm64", "armhf"));
    }

    #[test]
    fn an_unsafe_architecture_name_is_rejected() {
        for (name, reason) in [
            ("", "is empty"),
            ("../evil", "contains \"..\""),
            ("a/b", "contains a path separator"),
            ("-rf", "starts with '-'"),
            ("amd64 trixie", "contains whitespace"),
            ("amd64\n", "contains whitespace"),
        ] {
            assert_eq!(
                architecture_name_error(name),
                Some(reason),
                "for name {name:?}"
            );
        }
    }

    #[test]
    fn an_ordinary_architecture_name_is_accepted() {
        // Including one this crate's table does not know, which a recipe may
        // still legitimately name.
        for name in ["amd64", "arm64", "ppc64el", "riscv64", "loong64"] {
            assert_eq!(architecture_name_error(name), None, "for name {name:?}");
        }
    }
}
