//! Fetching a release tarball and unpacking it into a source tree.
//!
//! Most upstreams that are not Rust projects release a tarball rather than a
//! tag, and an upstream tarball beside a separate `debian/` directory is the
//! native Debian model. A component may therefore name an archive and the
//! SHA-256 it must hash to, and take its packaging from a [packaging
//! overlay](crate::Component::packaging).
//!
//! # The digest is the trust anchor
//!
//! The archive is verified against the declared digest before anything is
//! unpacked, on every run — whether it was fetched this run or found in the
//! cache. Nothing about the transport is trusted: a hostile mirror, a corrupted
//! proxy, and a truncated download all produce an archive that does not hash to
//! what the recipe declared, and the component fails rather than building
//! something no one asked for.
//!
//! That is why the fetch is `curl` rather than an HTTP client of src2deb's own.
//! It carries no part of the integrity claim, so what it costs is one host
//! prerequisite and what it buys is TLS, redirects, proxies, and every
//! authentication scheme a mirror might want, all maintained by people who do
//! that for a living. The same position the Debian provisioner takes when it
//! fetches an index over plain HTTP and verifies the archive signature above
//! the transport.
//!
//! # The cache
//!
//! A fetched archive is kept under `<work>/tarballs/`, named by its digest.
//! Two components declaring the same archive share one download, a recipe that
//! changes a digest names a different file rather than a stale one, and a host
//! with no network — or no `curl` — still builds from what is already there.
//!
//! An archive only ever appears in the cache under a name it hashed to: the
//! fetch writes to a `.partial` file, verifies it, and renames it into place
//! only then.
//!
//! # Unpacking
//!
//! Extraction is [`ferroday_cage::provision::Tarball`], which src2deb already
//! depends on for its build roots. It detects gzip, xz, and zstd from the
//! stream's content rather than from a file name, reads ustar, GNU, and pax,
//! and resolves every entry strictly beneath the destination — so an archive
//! carrying an absolute path, a `..`, or a symlink chain leading out fails
//! rather than writing outside the work directory.
//!
//! The destination is emptied first, so each run unpacks the archive as it
//! stands rather than over the leavings of the run before. That is the
//! guarantee a git source gets from `git checkout --force` and a path source
//! from its fresh copy, and a patch series depends on it: a patch applied twice
//! over one tree does not apply.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result, io_error};

/// The program that fetches an archive.
///
/// A host prerequisite, alongside `git`. It is probed before use rather than
/// assumed, so a host without it is told what to install instead of being shown
/// an error about a program that could not be launched.
const CURL: &str = "curl";

/// An unpacked release archive.
pub(crate) struct Unpacked {
    /// The tree the archive unpacked to. See [`archive_root`].
    pub tree: PathBuf,
    /// The digest the archive was verified against, in the lowercase
    /// hexadecimal a measured digest takes.
    ///
    /// Equal to what the recipe declared, since a mismatch never reaches here,
    /// and spelled the way src2deb measures rather than the way the recipe
    /// happened to write it — so the fingerprint and the manifest record one
    /// value for one archive.
    pub digest: String,
}

/// Fetches the archive at `url` if the cache does not already hold it, verifies
/// it against `declared`, and unpacks it into `dest`.
///
/// `component` names the component for error attribution alone. `cache` is the
/// work directory's archive cache, and `dest` is emptied before the archive is
/// unpacked into it.
///
/// See the module documentation for what is trusted and what is not.
pub(crate) fn unpack(
    component: &str,
    cache: &Path,
    url: &str,
    declared: &str,
    dest: &Path,
) -> Result<Unpacked> {
    // Lowercase because that is the spelling a measured digest takes, so the
    // cache holds one file per archive however the recipe wrote the hash.
    let declared = declared.to_ascii_lowercase();
    let archive = cache.join(&declared);

    if !archive.is_file() {
        std::fs::create_dir_all(cache).map_err(|err| io_error("creating", cache, err))?;
        // Fetched beside its destination rather than to it, so the cache never
        // holds a file under a name it has not been verified against — an
        // interrupted fetch leaves a `.partial` and nothing else.
        let partial = cache.join(format!("{declared}.partial"));
        fetch(component, url, &partial)?;
        verify(component, &partial, &declared, url)?;
        std::fs::rename(&partial, &archive).map_err(|err| io_error("renaming", &partial, err))?;
    } else {
        // Verified again on the way out of the cache, so "nothing is unpacked
        // that does not hash to what the recipe declared" holds however the
        // file got there.
        verify(component, &archive, &declared, url)?;
    }

    extract(component, &archive, dest)?;
    Ok(Unpacked {
        tree: archive_root(dest)?,
        digest: declared,
    })
}

/// Fetches `url` to `dest` with [`CURL`].
///
/// The flags are the ones that make an unattended fetch behave: fail on an HTTP
/// error status rather than saving the error page as the archive, follow the
/// redirects a release URL is built out of, retry a transient failure rather
/// than failing a component over one, and stay quiet unless something goes
/// wrong.
///
/// The protocols are restricted in both directions. A recipe may name `file://`
/// for a local mirror, but a *redirect* may not reach it: a mirror that could
/// redirect a fetch onto the build host's own filesystem chooses what the
/// archive is, and the digest check would then be verifying a file the recipe
/// never named against a hash it did.
fn fetch(component: &str, url: &str, dest: &Path) -> Result<()> {
    if !probe(CURL) {
        return Err(Error::Source {
            component: component.to_string(),
            reason: format!(
                "{url} has to be fetched, but `{CURL}` is not available; install \
                 curl, or put the archive in the work directory's tarball cache \
                 by hand"
            ),
        });
    }

    let output = Command::new(CURL)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            "--proto",
            "=file,http,https",
            "--proto-redir",
            "=http,https",
            "--output",
        ])
        .arg(dest)
        .arg("--")
        .arg(url)
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| Error::Source {
            component: component.to_string(),
            reason: format!("running {CURL} to fetch {url}: {err}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    // A failed fetch may still have created the output file, and a later run
    // must not mistake it for an archive it can resume from.
    let _ = std::fs::remove_file(dest);
    Err(Error::Source {
        component: component.to_string(),
        reason: format!(
            "fetching {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
    })
}

/// Fails the component unless `archive` hashes to `declared`, removing the file
/// when it does not.
///
/// Removing it is what makes the failure recoverable: an archive that no longer
/// matches is one a later run must fetch again, and leaving it in place would
/// fail every run after this one with no remedy but clearing the cache by hand.
fn verify(component: &str, archive: &Path, declared: &str, url: &str) -> Result<()> {
    let measured = digest_of(archive)?;
    if measured == declared {
        return Ok(());
    }
    let _ = std::fs::remove_file(archive);
    Err(Error::Source {
        component: component.to_string(),
        reason: format!(
            "the archive fetched from {url} hashes to {measured}, and the recipe \
             declares sha256 = {declared:?}. Nothing was unpacked. Correct the \
             recipe if the archive is the one you meant, and treat the mismatch \
             as a compromised or corrupted mirror if it is not"
        ),
    })
}

/// The SHA-256 of the file at `path`, in lowercase hexadecimal.
///
/// Read in chunks rather than whole, since a release archive is the one input
/// src2deb reads that has no bound on its size.
fn digest_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    /// Large enough that the read syscall is not what bounds throughput, and
    /// small enough to sit on the stack.
    const CHUNK: usize = 64 * 1024;

    let mut file = std::fs::File::open(path).map_err(|err| io_error("opening", path, err))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| io_error("reading", path, err))?;
        if read == 0 {
            return Ok(crate::fingerprint::hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

/// Unpacks `archive` into `dest`, which is emptied first.
fn extract(component: &str, archive: &Path, dest: &Path) -> Result<()> {
    use ferroday_cage::provision::{ProvisionRequest, Provisioner, Tarball};

    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|err| io_error("clearing", dest, err))?;
    }
    std::fs::create_dir_all(dest).map_err(|err| io_error("creating", dest, err))?;
    Tarball::new(archive)
        .provision(&ProvisionRequest::new(dest))
        .map_err(|err| Error::Source {
            component: component.to_string(),
            reason: format!("unpacking {}: {err}", archive.display()),
        })
}

/// The tree an archive unpacked to: the single directory it holds when it holds
/// exactly one, and `dest` itself otherwise.
///
/// A release archive conventionally carries everything under one directory
/// named for the release — `foo-1.2.3/` — so that unpacking it in a shared
/// directory does not scatter files. Descending into it is what keeps the
/// version out of the recipe twice: a `subdir` naming that directory would have
/// to be edited every time the archive's version moved, and would say nothing
/// the archive does not already say.
///
/// An archive laid out any other way is taken as it stands, which is the
/// honest reading of one that was not built to be unpacked into a directory of
/// its own. A [`subdir`](crate::Source::subdir) applies within whichever this
/// gives, so a component nested inside a release archive is still reachable.
///
/// One directory is never a wrapper: `debian/`. A distribution publishes its
/// packaging as an archive holding exactly that and nothing else, and there the
/// single directory is the payload rather than something wrapped around it —
/// descending into it would hand a packaging overlay the inside of the very
/// directory it exists to supply.
///
/// The entry's own type decides, so a lone symlink is not descended into: it
/// resolves outside the archive as often as not, and following one would put
/// the build somewhere the archive never described.
fn archive_root(dest: &Path) -> Result<PathBuf> {
    let mut entries = std::fs::read_dir(dest)
        .map_err(|err| io_error("reading", dest, err))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|err| io_error("reading", dest, err))?;
    let Some(only) = entries.pop().filter(|_| entries.is_empty()) else {
        return Ok(dest.to_path_buf());
    };
    let directory = only
        .file_type()
        .map_err(|err| io_error("inspecting", only.path(), err))?
        .is_dir();
    match directory && only.file_name() != PACKAGING {
        true => Ok(only.path()),
        false => Ok(dest.to_path_buf()),
    }
}

/// The directory a packaging overlay supplies, which an archive holding it
/// alone is already at the root of. See [`archive_root`].
const PACKAGING: &str = "debian";

/// Whether `program` can be launched at all.
///
/// Probed rather than assumed so a missing prerequisite is reported as one.
/// Left to the fetch itself, the run would fail with an error about a program
/// that could not be started, which reads as a defect in src2deb rather than as
/// something to install.
fn probe(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique scratch directory.
    fn scratch(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "src2deb-tarball-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_digest_is_the_lowercase_hex_sha256_of_the_file() {
        let root = scratch("digest");
        let path = root.join("archive");
        std::fs::write(&path, b"").unwrap();
        // The empty digest, which is the one value that can be checked against
        // a published constant rather than against this implementation.
        assert_eq!(
            digest_of(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            digest_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_digest_reads_a_file_larger_than_one_chunk() {
        // The loop, rather than the single-read case every small file takes.
        let root = scratch("digest-chunks");
        let path = root.join("archive");
        std::fs::write(&path, vec![b'x'; 200 * 1024]).unwrap();
        let chunked = digest_of(&path).unwrap();

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(vec![b'x'; 200 * 1024]);
        assert_eq!(chunked, crate::fingerprint::hex(&hasher.finalize()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mismatched_archive_is_removed_so_a_later_run_can_fetch_it_again() {
        let root = scratch("verify");
        let path = root.join("archive");
        std::fs::write(&path, b"not what was declared").unwrap();
        let err = verify(
            "pkg",
            &path,
            &"0".repeat(64),
            "https://example.invalid/a.tar.gz",
        )
        .expect_err("a mismatch fails the component")
        .to_string();
        assert!(err.contains("Nothing was unpacked"), "{err}");
        assert!(
            !path.exists(),
            "the mismatched archive was left for the next run to fail on again",
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_matching_archive_verifies_and_stays() {
        let root = scratch("verify-ok");
        let path = root.join("archive");
        std::fs::write(&path, b"abc").unwrap();
        verify(
            "pkg",
            &path,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "https://example.invalid/a.tar.gz",
        )
        .expect("the digest matches");
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_release_archives_single_directory_becomes_the_tree() {
        // What every archive built by `git archive`, `make dist`, or a forge's
        // release page looks like.
        let root = scratch("root-single");
        std::fs::create_dir_all(root.join("foo-1.2.3/src")).unwrap();
        assert_eq!(archive_root(&root).unwrap(), root.join("foo-1.2.3"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_holding_only_debian_is_already_at_its_root() {
        // What a distribution publishes beside an orig tarball. Here the single
        // directory is the payload rather than a wrapper, and descending into it
        // would hand a packaging overlay the inside of the directory it supplies.
        let root = scratch("root-debian");
        std::fs::create_dir_all(root.join("debian")).unwrap();
        std::fs::write(root.join("debian/control"), "Source: pkg\n").unwrap();
        assert_eq!(archive_root(&root).unwrap(), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_that_unpacks_in_place_is_taken_as_it_stands() {
        let root = scratch("root-flat");
        std::fs::write(root.join("configure"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(archive_root(&root).unwrap(), root);

        // A single entry that is not a directory is not a root either.
        let file = scratch("root-file");
        std::fs::write(file.join("only.txt"), "").unwrap();
        assert_eq!(archive_root(&file).unwrap(), file);

        // Nor is a single symlink, which resolves outside the archive as often
        // as not.
        let link = scratch("root-link");
        std::os::unix::fs::symlink("/etc", link.join("only")).unwrap();
        assert_eq!(archive_root(&link).unwrap(), link);

        // An empty archive has no root to find, and fails later for having no
        // debian/control rather than here.
        let empty = scratch("root-empty");
        assert_eq!(archive_root(&empty).unwrap(), empty);

        for dir in [root, file, link, empty] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
