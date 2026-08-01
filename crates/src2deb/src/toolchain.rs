//! The pinned Rust toolchain a build root carries.
//!
//! A recipe may pin a rustup toolchain in place of the archive's own Rust, for a
//! component whose crates need a compiler newer than the suite ships. The
//! toolchain is installed into the build root while the root is being
//! provisioned, not while a build is running, so it is fetched once per root
//! rather than once per build pass — under the layered strategy the difference
//! is a download per run against a download per component per run.
//!
//! The root's plan key carries the pinned version (see
//! [`crate::provision`]), so a recipe that changes it reprovisions rather than
//! reusing a root holding the toolchain it replaced.
//!
//! # Residual trust
//!
//! The transport is pinned (`--proto '=https' --tlsv1.2`) and the toolchain
//! version is exact, but `rustup-init.sh` itself is not checksum-pinned. It is
//! the standard rustup bootstrap, and it is what a pinned toolchain adds to a
//! build's trust beyond the signed Debian archive.

/// Where rustup is installed and where its proxies land, pinned explicitly
/// rather than left to follow `HOME`, so the toolchain a provision installs is
/// the toolchain a build finds.
const RUSTUP_HOME: &str = "/root/.rustup";
/// The cargo home holding the rustup proxies (`cargo`, `rustc`) on `PATH`.
const CARGO_HOME: &str = "/root/.cargo";

/// The shell prelude that points a command at the pinned toolchain: the two
/// homes, and `.cargo/bin` leading `PATH` so `cargo` and `rustc` resolve to the
/// rustup proxies rather than to the archive's binaries.
///
/// Both build passes carry this. It only names paths, so it is safe in the
/// offline pass, where nothing may be fetched.
pub fn prelude() -> String {
    format!(
        "export RUSTUP_HOME={RUSTUP_HOME} CARGO_HOME={CARGO_HOME}\n\
         export PATH={CARGO_HOME}/bin:$PATH\n"
    )
}

/// The shell script that installs rustup and the `version` toolchain into a
/// build root. Run in a cage over the root with the host network, while the
/// root is being provisioned.
///
/// `rustup-init.sh` is downloaded to a file and then run, rather than piped
/// straight into a shell: a piped fetch failure would be masked by the shell's
/// own exit status — the same trap that hides a broken `cargo vendor` — whereas
/// a separate `curl` fails the `-e` script outright.
pub fn install_script(version: &str) -> String {
    format!(
        "{}\
         curl --proto '=https' --tlsv1.2 -sSfL https://sh.rustup.rs -o /tmp/rustup-init.sh\n\
         sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
         --default-toolchain {version}\n\
         rm -f /tmp/rustup-init.sh\n",
        prelude()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prelude_pins_both_homes_and_leads_the_path_with_the_proxies() {
        let prelude = prelude();
        assert!(prelude.contains("RUSTUP_HOME=/root/.rustup"));
        assert!(prelude.contains("CARGO_HOME=/root/.cargo"));
        // Leading, not trailing: the archive's `cargo` is installed too, and the
        // pinned one has to win.
        assert!(prelude.contains("export PATH=/root/.cargo/bin:$PATH"));
    }

    #[test]
    fn the_install_script_pins_the_transport_and_the_toolchain_version() {
        let script = install_script("1.97.0");
        assert!(script.contains("--proto '=https' --tlsv1.2"));
        assert!(script.contains("--default-toolchain 1.97.0"));
        // Fetched to a file and then run, so a failed download fails the script
        // rather than being swallowed by a pipeline's exit status.
        assert!(script.contains("-o /tmp/rustup-init.sh"));
        assert!(!script.contains("| sh"));
        // The install lands where the prelude looks for it.
        assert!(script.starts_with(&prelude()));
    }
}
