//! Fetching a Debian source package and unpacking it into a source tree.
//!
//! A `.dsc` is the control file of a Debian source package: it names the
//! package's format, the tarballs it is assembled from, and the digest of each.
//! Pointing a component at one turns src2deb into a rebuild engine — the source
//! is already a complete Debian package, so everything downstream (build-order
//! resolution, build roots, the pool, version stamping) works on it unchanged.
//!
//! # The digest is the trust anchor, and the `.dsc` extends it
//!
//! The recipe declares the SHA-256 of the `.dsc` itself, which is verified
//! before it is read. The `.dsc` in turn declares the SHA-256 of every file it
//! names, and each of those is verified before it is unpacked. So one digest in
//! the recipe pins the whole source package, and nothing about the transport or
//! the mirror carries any part of the claim. See [`crate::tarball`], whose cache
//! and fetch this reuses in full.
//!
//! A published `.dsc` is ordinarily PGP-clearsigned. src2deb reads through the
//! armor without checking the signature: the signature says who published the
//! file, and the recipe's declared digest says which exact file was meant, which
//! is the stronger of the two claims for this purpose and the only one that
//! answers "did I get the bytes this recipe was written against".
//!
//! # Unpacking, and what does not happen here
//!
//! The component files are unpacked with the same extractor a release archive
//! gets. **The Debian patch series is not applied**, and does not need to be:
//! `dpkg-buildpackage` calls `dpkg-source --before-build` as its first step,
//! ahead of the build-dependency check and independently of `-nc`, and that is
//! what applies a `3.0 (quilt)` series. So the series is applied inside the
//! cage, by the tool that owns the source format, against the tree the build
//! actually uses.
//!
//! That is what keeps `dpkg-source` off the host. src2deb assembles the tree
//! from tarballs and hands it over; nothing outside the cage has to understand
//! quilt.
//!
//! # No vendor pass
//!
//! A Debian source package carries everything its build needs, which is what
//! makes it a source package. A component built from one therefore skips
//! [pass 1](crate::build) — the step that runs `debian/rules clean` with the
//! host network — and is built entirely inside an isolated cage. It is src2deb's
//! one fully hermetic source kind. See
//! [`VendorPass`](crate::source::VendorPass).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result, io_error};
use crate::tarball;

/// What a caller needs out of a source package.
///
/// A component's own source needs the whole package; a [packaging
/// overlay](crate::Component::packaging) needs only the `debian/` directory,
/// which for the common format lives in a tarball of its own. Naming the need
/// keeps an overlay from fetching an upstream tarball it would then ignore,
/// which for a large package is the whole cost of the resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// The whole source package: upstream, its supplementary components, and
    /// the packaging.
    Source,
    /// Only what carries the `debian/` directory.
    Packaging,
}

/// An unpacked Debian source package.
pub(crate) struct Unpacked {
    /// The tree that holds the package's `debian/` directory.
    pub tree: PathBuf,
    /// The digest of the `.dsc`, in the lowercase hexadecimal a measured digest
    /// takes. Equal to what the recipe declared, since a mismatch never reaches
    /// here.
    pub digest: String,
}

/// Fetches the `.dsc` at `url`, verifies it against `declared`, fetches and
/// verifies the files it names, and unpacks them into `dest`.
///
/// `component` names the component for error attribution alone. `cache` is the
/// work directory's artefact cache, shared with release archives, and `dest` is
/// emptied before anything is unpacked into it.
pub(crate) fn unpack(
    component: &str,
    cache: &Path,
    url: &str,
    declared: &str,
    dest: &Path,
    want: Want,
) -> Result<Unpacked> {
    let control = tarball::cached(component, cache, url, declared)?;
    let text = std::fs::read_to_string(&control.path)
        .map_err(|err| io_error("reading", &control.path, err))?;
    let layout = Layout::parse(component, url, &text)?;

    tarball::clear(dest)?;
    let tree = match (&layout, want) {
        // A native package is one tarball holding the whole thing, so both
        // callers want the same file and take the same tree out of it.
        (Layout::Native { tarball }, _) => {
            let file = fetch(component, cache, url, tarball)?;
            tarball::extract(component, &file.path, dest, &file.url)?;
            tarball::archive_root(dest)?
        }
        // An overlay takes the packaging tarball alone. It holds `debian/` at
        // its top level rather than wrapped in a release directory, so the
        // destination is already the tree the overlay is read from.
        (Layout::Quilt { debian, .. }, Want::Packaging) => {
            let file = fetch(component, cache, url, debian)?;
            tarball::extract(component, &file.path, dest, &file.url)?;
            dest.to_path_buf()
        }
        (
            Layout::Quilt {
                orig,
                components,
                debian,
            },
            Want::Source,
        ) => {
            let file = fetch(component, cache, url, orig)?;
            tarball::extract(component, &file.path, dest, &file.url)?;
            // The upstream tarball's own root, which is what the supplementary
            // components and the packaging are unpacked into.
            let tree = tarball::archive_root(dest)?;
            for (name, entry) in components {
                let file = fetch(component, cache, url, entry)?;
                unpack_component(component, &file, name, &tree, dest)?;
            }
            let file = fetch(component, cache, url, debian)?;
            tarball::extract(component, &file.path, &tree, &file.url)?;
            tree
        }
    };
    Ok(Unpacked {
        tree,
        digest: control.digest,
    })
}

/// Fetches one of the files a `.dsc` names, verified against the digest the
/// `.dsc` declares for it, and returns where the cache holds it.
///
/// The file sits beside the `.dsc` in the archive, so its URL is the `.dsc`'s
/// with the last path segment replaced. A file name that is not a single benign
/// segment never reaches here — [`Layout::parse`] refuses it — so the
/// replacement cannot reach outside the directory the recipe named.
fn fetch(component: &str, cache: &Path, dsc_url: &str, entry: &Entry) -> Result<Fetched> {
    let url = sibling_url(dsc_url, &entry.name);
    let cached = tarball::cached(component, cache, &url, &entry.sha256)?;
    Ok(Fetched {
        path: cached.path,
        url,
    })
}

/// One of a source package's files, fetched and verified.
///
/// The URL travels with the path because the path is a digest in a shared
/// cache: a failure to unpack has to name the file the archive published, not
/// the name the cache happens to hold it under.
struct Fetched {
    path: PathBuf,
    url: String,
}

/// Unpacks a supplementary upstream component tarball into `<tree>/<name>`.
///
/// Such a tarball carries a part of upstream that is released separately, and
/// `dpkg-source` puts its contents at the component's own name within the tree.
/// The tarball itself may or may not wrap them in a directory — both shapes are
/// published — so it is unpacked into a staging directory, its root is taken the
/// same way an upstream tarball's is, and that root is moved into place. The
/// staging directory sits beside the tree under the work directory, so the move
/// is a rename rather than a copy.
fn unpack_component(
    component: &str,
    archive: &Fetched,
    name: &str,
    tree: &Path,
    dest: &Path,
) -> Result<()> {
    let staging = dest.join(format!(".src2deb-component-{name}"));
    tarball::clear(&staging)?;
    tarball::extract(component, &archive.path, &staging, &archive.url)?;
    let root = tarball::archive_root(&staging)?;

    let into = tree.join(name);
    // The upstream tarball may already carry a directory of this name — a
    // package that ships a stub and overlays the real component onto it. The
    // component tarball is the authority for what goes there, as it is for
    // `dpkg-source`.
    if into.exists() {
        std::fs::remove_dir_all(&into).map_err(|err| io_error("clearing", &into, err))?;
    }
    std::fs::rename(&root, &into).map_err(|err| io_error("moving", &root, err))?;
    // A tarball that wrapped its contents leaves the wrapper behind, and that
    // wrapper is not part of the source. One that did not had the staging
    // directory as its own root, so the rename has already moved it and there
    // is nothing left to remove — compared by path rather than probed for, so
    // the two cases are told apart by what happened rather than by what is
    // there afterwards.
    if root == staging {
        return Ok(());
    }
    std::fs::remove_dir_all(&staging).map_err(|err| io_error("clearing", &staging, err))
}

/// The URL of a file sitting beside `url` in the same directory.
///
/// A `.dsc` names its files by bare name, and the archive publishes them in the
/// directory the `.dsc` is in. Handled by replacing the last path segment rather
/// than by parsing the URL, which needs no scheme-aware machinery and is
/// therefore the same operation for `http`, `https`, and `file`.
///
/// A query string or fragment would make the last segment something other than a
/// file name; a `.dsc` URL carrying either is refused by
/// [`Layout::parse`]'s caller-facing check, so this is only ever handed a
/// plain path.
fn sibling_url(url: &str, name: &str) -> String {
    match url.rfind('/') {
        Some(slash) => format!("{}{name}", &url[..=slash]),
        None => name.to_string(),
    }
}

/// How a source package's files assemble into a tree.
///
/// The two shapes cover every format src2deb builds; see [`Layout::parse`] for
/// what is refused and why.
#[derive(Debug)]
enum Layout {
    /// An upstream tarball, zero or more supplementary upstream component
    /// tarballs, and a tarball holding `debian/`. The shape of `3.0 (quilt)`,
    /// which is very nearly every source package in the archive.
    Quilt {
        orig: Entry,
        components: Vec<(String, Entry)>,
        debian: Entry,
    },
    /// One tarball holding the whole package, `debian/` included. The shape of
    /// `3.0 (native)` and of a native `1.0`.
    Native { tarball: Entry },
}

/// One file a `.dsc` names: what it is called, and what it must hash to.
#[derive(Debug)]
struct Entry {
    name: String,
    sha256: String,
}

/// The field naming the source format.
const FORMAT: &str = "Format:";
/// The field naming the files and their SHA-256 digests.
const CHECKSUMS: &str = "Checksums-Sha256:";

/// The formats src2deb assembles a tree from, as a `.dsc` spells them.
const QUILT: &str = "3.0 (quilt)";
const NATIVE: &str = "3.0 (native)";
const ONE_ZERO: &str = "1.0";

impl Layout {
    /// Reads a `.dsc` and works out how its files assemble into a tree.
    ///
    /// `url` appears in failures so a recipe naming the wrong file is told which
    /// one. The text may be PGP-clearsigned, which is how the archive publishes
    /// it; see [`clearsigned_body`].
    ///
    /// Three groups of failure, each reported rather than guessed past:
    ///
    /// - **A format src2deb cannot assemble.** `1.0` with a `.diff.gz` is the
    ///   only one that exists in practice and it is refused: applying that diff
    ///   is a second patch mechanism, for a format that is a fraction of a
    ///   percent of the archive and shrinking. An unrecognized format is refused
    ///   for the plainer reason that nothing here knows what its files mean.
    /// - **A file set that does not match the format**, such as a `3.0 (quilt)`
    ///   naming two upstream tarballs or none. A `.dsc` like that is malformed,
    ///   and assembling what there is of it would build something no one
    ///   described.
    /// - **A file name that is not a single benign path segment.** Names reach a
    ///   URL and a directory name, and a `.dsc` is fetched before it is trusted,
    ///   so this is checked rather than assumed of a well-formed archive.
    fn parse(component: &str, url: &str, text: &str) -> Result<Layout> {
        let refuse = |reason: String| Error::Source {
            component: component.to_string(),
            reason: format!("{url}: {reason}"),
        };

        let body = clearsigned_body(text);
        let format = field(body, FORMAT)
            .ok_or_else(|| refuse(format!("names no {FORMAT} field, so it is not a .dsc")))?;

        let mut orig = None;
        let mut debian = None;
        let mut components = Vec::new();
        let mut plain = Vec::new();
        for entry in checksums(component, url, body)? {
            // Kept beside the entry rather than borrowed from it, so an entry
            // that is claimed by a slot can still be named in a failure about
            // the entry that was already there. A `.dsc` names a handful of
            // files, so the copy costs nothing worth arranging around.
            let name = entry.name.clone();
            if name.ends_with(".diff.gz") {
                return Err(refuse(format!(
                    "declares format {format:?} with the patch file {name:?}, which \
                     src2deb does not apply. Build this package from a source that \
                     carries its packaging as a tree — a git repository, or a \
                     packaging overlay — rather than from its .dsc"
                )));
            }
            // An upstream signature the archive carries beside a tarball it was
            // checked against. Passed over rather than refused, since a `.dsc`
            // naming one is ordinary.
            if name.ends_with(".asc") {
                continue;
            }
            let seen = if is_orig_component(&name) {
                let directory = component_name(&name).ok_or_else(|| {
                    refuse(format!(
                        "names the supplementary tarball {name:?}, whose component \
                         name is not a usable directory name"
                    ))
                })?;
                components.push((directory, entry));
                continue;
            } else if is_orig(&name) {
                orig.replace(entry)
            } else if is_debian(&name) {
                debian.replace(entry)
            } else {
                plain.push(entry);
                continue;
            };
            if let Some(first) = seen {
                return Err(refuse(format!(
                    "names {:?} and {name:?}, and a source package has one of each",
                    first.name,
                )));
            }
        }

        match format {
            QUILT => Ok(Layout::Quilt {
                orig: orig.ok_or_else(|| {
                    refuse(format!("declares {QUILT} and names no upstream tarball"))
                })?,
                components,
                debian: debian.ok_or_else(|| {
                    refuse(format!("declares {QUILT} and names no .debian.tar archive"))
                })?,
            }),
            // A native package's one tarball is neither an upstream tarball nor
            // a packaging one — those names belong to a package built from two —
            // so it is the single plain file the `.dsc` names.
            NATIVE | ONE_ZERO => match (plain.len(), orig, debian, components.len()) {
                (1, None, None, 0) => Ok(Layout::Native {
                    tarball: plain.pop().expect("the length was checked"),
                }),
                _ => Err(refuse(format!(
                    "declares format {format:?}, which is one tarball holding the \
                     whole package, and names something other than that"
                ))),
            },
            other => Err(refuse(format!(
                "declares source format {other:?}; src2deb builds {QUILT}, \
                 {NATIVE}, and native {ONE_ZERO} source packages"
            ))),
        }
    }
}

/// Whether a file name is a tarball of any compression a source package uses.
///
/// The compression is not read from the name — the extractor detects it from the
/// stream's content — so this only has to tell a tarball from the other things a
/// `.dsc` names.
fn is_tarball(name: &str) -> bool {
    name.contains(".tar.") || name.ends_with(".tar")
}

/// Whether a file name is the upstream tarball: `<pkg>_<version>.orig.tar.<ext>`.
fn is_orig(name: &str) -> bool {
    is_tarball(name) && name.contains(".orig.tar")
}

/// Whether a file name is a supplementary upstream component tarball:
/// `<pkg>_<version>.orig-<component>.tar.<ext>`.
fn is_orig_component(name: &str) -> bool {
    is_tarball(name) && name.contains(".orig-")
}

/// Whether a file name is the packaging tarball:
/// `<pkg>_<version>-<revision>.debian.tar.<ext>`.
fn is_debian(name: &str) -> bool {
    is_tarball(name) && name.contains(".debian.tar")
}

/// The component name within a supplementary tarball's file name, or `None` when
/// it is not one that can become a directory.
///
/// The name lies between `.orig-` and the `.tar` that follows it, and becomes a
/// directory within the source tree — so it is held to the same rule a recipe's
/// own names are: a single, non-empty path segment with no traversal and no
/// leading dot, the last so a component cannot land somewhere a directory
/// listing does not show it.
fn component_name(file_name: &str) -> Option<String> {
    let rest = file_name.split_once(".orig-")?.1;
    let name = rest.split_once(".tar")?.0;
    let usable = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.contains(['/', '\\', '\0']);
    usable.then(|| name.to_string())
}

/// The one-line value of `field` in a control paragraph, or `None` when the
/// field is absent.
///
/// The fields read here are all single-line, so a continuation is not something
/// to join — it is a sign the field is not the one being read, and stopping at
/// the first line is the honest reading of it.
fn field<'a>(text: &'a str, field: &str) -> Option<&'a str> {
    lines(text)
        .find_map(|line| line.strip_prefix(field))
        .map(str::trim)
}

/// The files a `.dsc`'s `Checksums-Sha256` field names, each with the digest it
/// must hash to.
///
/// The field is multi-line: every continuation line is indented and reads
/// `<sha256> <size> <name>`. The size is not carried — the digest settles what
/// the file is, and a file of the wrong length cannot hash to the right value.
///
/// A `.dsc` with no such field is refused rather than falling back to the
/// `Files` field beside it, which carries MD5 digests: src2deb would then be
/// verifying a fetched file with a hash that no longer means what a digest is
/// read as meaning.
fn checksums(component: &str, url: &str, text: &str) -> Result<Vec<Entry>> {
    let refuse = |reason: String| Error::Source {
        component: component.to_string(),
        reason: format!("{url}: {reason}"),
    };

    let mut entries = Vec::new();
    let mut within = false;
    for line in lines(text) {
        if line.starts_with(CHECKSUMS) {
            within = true;
            continue;
        }
        if !within {
            continue;
        }
        // A line that is not indented ends the field.
        if !line.starts_with([' ', '\t']) {
            break;
        }
        let mut fields = line.split_whitespace();
        let (Some(sha256), Some(_size), Some(name), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(refuse(format!(
                "has a {CHECKSUMS} line src2deb cannot read: {:?}",
                line.trim(),
            )));
        };
        if !is_sha256(sha256) {
            return Err(refuse(format!(
                "names {name:?} with {sha256:?}, which is not a SHA-256 digest"
            )));
        }
        if !is_bare_name(name) {
            return Err(refuse(format!(
                "names the file {name:?}, which is not a plain file name"
            )));
        }
        entries.push(Entry {
            name: name.to_string(),
            sha256: sha256.to_ascii_lowercase(),
        });
    }

    if entries.is_empty() {
        return Err(refuse(format!(
            "names no files in a {CHECKSUMS} field, so there is no source package \
             to assemble"
        )));
    }
    Ok(entries)
}

/// Whether `value` is 64 hexadecimal characters.
fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether a `.dsc`'s file name is a plain name that can be appended to the
/// archive directory the `.dsc` came from.
///
/// It reaches a URL, so it must not carry a separator or a traversal; it must
/// not be option-like, since it is passed to `curl`; and it must not begin with
/// a dot, which is how a `.dsc` would name something other than a published
/// artefact.
fn is_bare_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && !name.contains("..")
        && !name.contains(['/', '\\', '\0', '?', '#', '%'])
}

/// The message body of a PGP-clearsigned document, or the whole text when it is
/// not signed.
///
/// The archive publishes `.dsc` files clearsigned. The body begins after the
/// armor header and the blank line that ends that header's own fields, and ends
/// at the signature block. Lines within it that would otherwise look like armor
/// are dash-escaped by the signer, so a leading `- ` is removed — which for a
/// `.dsc` is theory rather than practice, and is done because reading the format
/// correctly costs one line.
///
/// The signature is not checked; see the module documentation for why the
/// recipe's declared digest is the stronger claim here.
fn clearsigned_body(text: &str) -> &str {
    const OPEN: &str = "-----BEGIN PGP SIGNED MESSAGE-----";
    const SIGNATURE: &str = "-----BEGIN PGP SIGNATURE-----";

    let Some(after_open) = text.find(OPEN).map(|at| at + OPEN.len()) else {
        return text;
    };
    // The armor header's own fields (`Hash: SHA256`) run to the first blank
    // line; the message begins after it.
    let rest = &text[after_open..];
    let body = match rest.find("\n\n") {
        Some(blank) => &rest[blank + 2..],
        None => return text,
    };
    match body.find(SIGNATURE) {
        Some(end) => &body[..end],
        None => body,
    }
}

/// The lines of a control paragraph, with the dash-escaping a clearsigned
/// message applies removed.
///
/// A signer escapes a line beginning with `-` so it cannot be mistaken for the
/// armor around it. No field src2deb reads begins with one, so this is reading
/// the format correctly rather than handling a case the archive produces — and
/// it costs one line to do rather than to reason about.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(|line| line.strip_prefix("- ").unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A digest of the right shape, distinguished by its first characters.
    fn digest(tag: &str) -> String {
        format!("{tag}{}", "0".repeat(64 - tag.len()))
    }

    fn quilt_dsc() -> String {
        format!(
            "Format: 3.0 (quilt)\n\
             Source: gtk2-engines-murrine\n\
             Version: 0.98.2-4\n\
             Checksums-Sha1:\n\
             \x20ddaca56b6e10736838572014ae9d20b814242615 296944 \
             gtk2-engines-murrine_0.98.2.orig.tar.xz\n\
             Checksums-Sha256:\n\
             \x20{orig} 296944 gtk2-engines-murrine_0.98.2.orig.tar.xz\n\
             \x20{debian} 4316 gtk2-engines-murrine_0.98.2-4.debian.tar.xz\n\
             Files:\n\
             \x20bf01e0097b5f1e164dbcf807f4b9745e 296944 \
             gtk2-engines-murrine_0.98.2.orig.tar.xz\n",
            orig = digest("aaaa"),
            debian = digest("bbbb"),
        )
    }

    #[test]
    fn a_quilt_package_names_its_upstream_and_packaging_tarballs() {
        let layout = Layout::parse("pkg", "https://e.invalid/a.dsc", &quilt_dsc()).unwrap();
        let Layout::Quilt {
            orig,
            components,
            debian,
        } = layout
        else {
            panic!("3.0 (quilt) is a quilt layout");
        };
        assert_eq!(orig.name, "gtk2-engines-murrine_0.98.2.orig.tar.xz");
        assert_eq!(orig.sha256, digest("aaaa"));
        assert_eq!(debian.name, "gtk2-engines-murrine_0.98.2-4.debian.tar.xz");
        assert_eq!(debian.sha256, digest("bbbb"));
        assert!(components.is_empty());
        // The Sha1 field above it and the Files field below it are not read:
        // one carries a weaker digest and the other an MD5, and either would
        // have contributed a duplicate entry for the same file.
    }

    #[test]
    fn supplementary_component_tarballs_are_named_by_their_component() {
        let text = format!(
            "Format: 3.0 (quilt)\n\
             Checksums-Sha256:\n\
             \x20{a} 1 foo_1.0.orig.tar.gz\n\
             \x20{b} 2 foo_1.0.orig-docs.tar.gz\n\
             \x20{c} 3 foo_1.0.orig-test-data.tar.xz\n\
             \x20{d} 4 foo_1.0-1.debian.tar.xz\n",
            a = digest("aaaa"),
            b = digest("bbbb"),
            c = digest("cccc"),
            d = digest("dddd"),
        );
        let layout = Layout::parse("pkg", "https://e.invalid/a.dsc", &text).unwrap();
        let Layout::Quilt {
            orig, components, ..
        } = layout
        else {
            panic!("quilt");
        };
        // The upstream tarball is not mistaken for a component of itself, and
        // a component name may carry a hyphen.
        assert_eq!(orig.name, "foo_1.0.orig.tar.gz");
        let named: Vec<(&str, &str)> = components
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.name.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("docs", "foo_1.0.orig-docs.tar.gz"),
                ("test-data", "foo_1.0.orig-test-data.tar.xz"),
            ],
        );
    }

    #[test]
    fn a_native_package_is_one_tarball() {
        for format in ["3.0 (native)", "1.0"] {
            let text = format!(
                "Format: {format}\n\
                 Checksums-Sha256:\n\
                 \x20{a} 10 foo_1.0.tar.xz\n",
                a = digest("aaaa"),
            );
            let layout = Layout::parse("pkg", "https://e.invalid/a.dsc", &text).unwrap();
            let Layout::Native { tarball } = layout else {
                panic!("{format} is a native layout");
            };
            assert_eq!(tarball.name, "foo_1.0.tar.xz");
        }
    }

    #[test]
    fn a_clearsigned_dsc_is_read_through_its_armor() {
        // What the archive actually publishes. The body is found between the
        // armor header's blank line and the signature block.
        let signed = format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\n\
             Hash: SHA256\n\
             \n\
             {}\n\
             -----BEGIN PGP SIGNATURE-----\n\
             \n\
             iQJJBAEBCAAzFiEEm/uu6GwKpf\n\
             -----END PGP SIGNATURE-----\n",
            quilt_dsc(),
        );
        let layout = Layout::parse("pkg", "https://e.invalid/a.dsc", &signed).unwrap();
        assert!(matches!(layout, Layout::Quilt { .. }));
        // The signature block is not part of the body, so nothing in it can be
        // read as a field.
        assert!(!clearsigned_body(&signed).contains("PGP SIGNATURE"));
        // An unsigned .dsc is read whole.
        assert_eq!(clearsigned_body("Format: 1.0\n"), "Format: 1.0\n");
        // A dash-escaped line reads as the line it stands for.
        let escaped = "- -----BEGIN something\nFormat: 1.0\n";
        assert_eq!(
            lines(escaped).collect::<Vec<_>>(),
            ["-----BEGIN something", "Format: 1.0"]
        );
    }

    #[test]
    fn an_upstream_signature_beside_a_tarball_is_passed_over() {
        // 1,551 files in trixie's source index are one of these. They are not
        // part of the assembly, and refusing a .dsc for naming one would refuse
        // a perfectly ordinary source package.
        let text = format!(
            "Format: 3.0 (quilt)\n\
             Checksums-Sha256:\n\
             \x20{a} 1 foo_1.0.orig.tar.gz\n\
             \x20{b} 2 foo_1.0.orig.tar.gz.asc\n\
             \x20{c} 3 foo_1.0-1.debian.tar.xz\n",
            a = digest("aaaa"),
            b = digest("bbbb"),
            c = digest("cccc"),
        );
        let layout = Layout::parse("pkg", "https://e.invalid/a.dsc", &text).unwrap();
        let Layout::Quilt { orig, .. } = layout else {
            panic!("quilt");
        };
        assert_eq!(orig.name, "foo_1.0.orig.tar.gz");
    }

    #[test]
    fn a_one_zero_package_with_a_diff_is_refused_with_a_remedy() {
        let text = format!(
            "Format: 1.0\n\
             Checksums-Sha256:\n\
             \x20{a} 1 cvs_1.12.13.orig.tar.bz2\n\
             \x20{b} 2 cvs_1.12.13+real-29.diff.gz\n",
            a = digest("aaaa"),
            b = digest("bbbb"),
        );
        let err = Layout::parse("pkg", "https://e.invalid/a.dsc", &text)
            .expect_err("a .diff.gz is not applied")
            .to_string();
        assert!(err.contains("diff.gz"), "{err}");
        assert!(err.contains("packaging overlay"), "{err}");
    }

    #[test]
    fn a_malformed_or_unknown_source_package_is_refused_rather_than_guessed_at() {
        let cases: [(String, &str); 5] = [
            // A format nothing here knows how to assemble.
            (
                format!(
                    "Format: 2.0\nChecksums-Sha256:\n \x20{a} 1 foo.tar.gz\n",
                    a = digest("aaaa")
                ),
                "source format",
            ),
            // Not a .dsc at all.
            ("Source: foo\n".to_string(), "not a .dsc"),
            // A quilt package with no packaging tarball.
            (
                format!(
                    "Format: 3.0 (quilt)\nChecksums-Sha256:\n {a} 1 foo_1.0.orig.tar.gz\n",
                    a = digest("aaaa")
                ),
                ".debian.tar",
            ),
            // A digest that is not one.
            (
                "Format: 1.0\nChecksums-Sha256:\n abc 1 foo.tar.gz\n".to_string(),
                "not a SHA-256",
            ),
            // A file name that would reach outside the archive directory.
            (
                format!(
                    "Format: 1.0\nChecksums-Sha256:\n {a} 1 ../../etc/shadow\n",
                    a = digest("aaaa")
                ),
                "not a plain file name",
            ),
        ];
        for (text, needle) in cases {
            let err = Layout::parse("pkg", "https://e.invalid/a.dsc", &text)
                .expect_err("should be refused")
                .to_string();
            assert!(err.contains(needle), "expected {needle:?} in: {err}");
            // Every failure names the .dsc it came from, since a recipe pointing
            // at the wrong file is the likeliest cause.
            assert!(err.contains("https://e.invalid/a.dsc"), "{err}");
        }
    }

    #[test]
    fn a_component_file_is_fetched_from_beside_its_dsc() {
        assert_eq!(
            sibling_url(
                "https://deb.debian.org/debian/pool/main/g/gtk2/gtk2_1.0-1.dsc",
                "gtk2_1.0.orig.tar.xz",
            ),
            "https://deb.debian.org/debian/pool/main/g/gtk2/gtk2_1.0.orig.tar.xz",
        );
        // A local mirror is the same operation.
        assert_eq!(
            sibling_url("file:///srv/pool/foo_1.0-1.dsc", "foo_1.0.tar.gz"),
            "file:///srv/pool/foo_1.0.tar.gz",
        );
        // A URL with no path at all keeps the name it was given rather than
        // producing something that is not a URL.
        assert_eq!(sibling_url("foo.dsc", "bar.tar.gz"), "bar.tar.gz");
    }

    #[test]
    fn a_supplementary_component_name_must_be_a_usable_directory_name() {
        assert_eq!(
            component_name("foo_1.0.orig-docs.tar.gz").as_deref(),
            Some("docs")
        );
        assert_eq!(
            component_name("foo_1.0.orig-test-data.tar.xz").as_deref(),
            Some("test-data"),
        );
        // The names that would put a directory somewhere other than the tree.
        for name in [
            "foo_1.0.orig-.tar.gz",
            "foo_1.0.orig-...tar.gz",
            "foo_1.0.orig-a/b.tar.gz",
            "foo_1.0.orig-.hidden.tar.gz",
            "foo_1.0.orig.tar.gz",
        ] {
            assert_eq!(component_name(name), None, "{name} should be refused");
        }
    }
}
