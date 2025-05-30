//! Parse and generate systemd tmpfiles.d entries.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as WriteFmt;
use std::io::{BufRead, BufReader, Write as StdWrite};
use std::iter::Peekable;
use std::num::NonZeroUsize;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
use cap_std::fs::MetadataExt;
use cap_std::fs::{Dir, Permissions, PermissionsExt};
use cap_std_ext::cap_std;
use cap_std_ext::dirext::CapStdExtDirExt;
use rustix::fs::Mode;
use rustix::path::Arg;
use thiserror::Error;

const TMPFILESD: &str = "usr/lib/tmpfiles.d";
/// The path to the file we use for generation
const BOOTC_GENERATED_PREFIX: &str = "bootc-autogenerated-var";

/// The number of times we've generated a tmpfiles.d
#[derive(Debug, Default)]
struct BootcTmpfilesGeneration(u32);

impl BootcTmpfilesGeneration {
    fn increment(&mut self) {
        // SAFETY: We shouldn't ever wrap here
        self.0 = self.0.checked_add(1).unwrap();
    }

    fn path(&self) -> Utf8PathBuf {
        format!("{TMPFILESD}/{BOOTC_GENERATED_PREFIX}-{}.conf", self.0).into()
    }
}

/// An error when translating tmpfiles.d.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("I/O (fmt) error")]
    Fmt(#[from] std::fmt::Error),
    #[error("I/O error on {path}: {err}")]
    PathIo { path: PathBuf, err: std::io::Error },
    #[error("User not found for id {0}")]
    UserNotFound(uzers::uid_t),
    #[error("Group not found for id {0}")]
    GroupNotFound(uzers::gid_t),
    #[error("Invalid non-UTF8 username: {uid} {name}")]
    NonUtf8User { uid: uzers::uid_t, name: String },
    #[error("Invalid non-UTF8 groupname: {gid} {name}")]
    NonUtf8Group { gid: uzers::gid_t, name: String },
    #[error("Missing {TMPFILESD}")]
    MissingTmpfilesDir {},
    #[error("Found /var/run as a non-symlink")]
    FoundVarRunNonSymlink {},
    #[error("Malformed tmpfiles.d")]
    MalformedTmpfilesPath,
    #[error("Malformed tmpfiles.d line {0}")]
    MalformedTmpfilesEntry(String),
    #[error("Unsupported regular file for tmpfiles.d {0}")]
    UnsupportedRegfile(PathBuf),
    #[error("Unsupported file of type {ty:?} for tmpfiles.d {path}")]
    UnsupportedFile {
        ty: rustix::fs::FileType,
        path: PathBuf,
    },
}

/// The type of Result.
pub type Result<T> = std::result::Result<T, Error>;

fn escape_path<W: std::fmt::Write>(path: &Path, out: &mut W) -> std::fmt::Result {
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.is_empty() {
        return Err(std::fmt::Error);
    }

    if let Some(s) = path.as_os_str().as_str().ok() {
        if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
            return write!(out, "{s}");
        }
    }

    for c in path_bytes.iter().copied() {
        let is_special = c == b'\\';
        let is_printable = c.is_ascii_alphanumeric() || c.is_ascii_punctuation();
        if is_printable && !is_special {
            out.write_char(c as char)?;
        } else {
            match c {
                b'\\' => out.write_str(r"\\")?,
                b'\n' => out.write_str(r"\n")?,
                b'\t' => out.write_str(r"\t")?,
                b'\r' => out.write_str(r"\r")?,
                o => write!(out, "\\x{:02x}", o)?,
            }
        }
    }
    std::fmt::Result::Ok(())
}

fn impl_unescape_path_until<I>(
    src: &mut Peekable<I>,
    buf: &mut Vec<u8>,
    end_of_record_is_quote: bool,
) -> Result<()>
where
    I: Iterator<Item = u8>,
{
    let should_take_next = |c: &u8| {
        let c = *c;
        if end_of_record_is_quote {
            c != b'"'
        } else {
            !c.is_ascii_whitespace()
        }
    };
    while let Some(c) = src.next_if(should_take_next) {
        if c != b'\\' {
            buf.push(c);
            continue;
        };
        let Some(c) = src.next() else {
            return Err(Error::MalformedTmpfilesPath);
        };
        let c = match c {
            b'\\' => b'\\',
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'x' => {
                let mut s = String::new();
                s.push(
                    src.next()
                        .ok_or_else(|| Error::MalformedTmpfilesPath)?
                        .into(),
                );
                s.push(
                    src.next()
                        .ok_or_else(|| Error::MalformedTmpfilesPath)?
                        .into(),
                );

                u8::from_str_radix(&s, 16).map_err(|_| Error::MalformedTmpfilesPath)?
            }
            _ => return Err(Error::MalformedTmpfilesPath),
        };
        buf.push(c);
    }
    Ok(())
}

fn unescape_path<I>(src: &mut Peekable<I>) -> Result<PathBuf>
where
    I: Iterator<Item = u8>,
{
    let mut r = Vec::new();
    if let Some(_) = src.next_if_eq(&b'"') {
        impl_unescape_path_until(src, &mut r, true)?;
    } else {
        impl_unescape_path_until(src, &mut r, false)?;
    };
    let r = OsString::from_vec(r);
    Ok(PathBuf::from(r))
}

/// Canonicalize and escape a path value for tmpfiles.d
/// At the current time the only canonicalization we do is remap /var/run -> /run.
fn canonicalize_escape_path<W: std::fmt::Write>(path: &Path, out: &mut W) -> std::fmt::Result {
    // systemd-tmpfiles complains loudly about writing to /var/run;
    // ideally, all of the packages get fixed for this but...eh.
    let path = if path.starts_with("/var/run") {
        let rest = &path.as_os_str().as_bytes()[4..];
        Path::new(OsStr::from_bytes(rest))
    } else {
        path
    };
    escape_path(path, out)
}

/// In tmpfiles.d we only handle directories and symlinks. Directories
/// just have a mode, and symlinks just have a target.
enum FileMeta {
    Directory(Mode),
    Symlink(PathBuf),
}

impl FileMeta {
    fn from_fs(dir: &Dir, path: &Path) -> Result<Option<Self>> {
        let meta = dir.symlink_metadata(path)?;
        let ftype = meta.file_type();
        let r = if ftype.is_dir() {
            FileMeta::Directory(Mode::from_raw_mode(meta.mode()))
        } else if ftype.is_symlink() {
            let target = dir.read_link_contents(path)?;
            FileMeta::Symlink(target)
        } else {
            return Ok(None);
        };
        Ok(Some(r))
    }
}

/// Translate a filepath entry to an equivalent tmpfiles.d line.
pub(crate) fn translate_to_tmpfiles_d(
    abs_path: &Path,
    meta: FileMeta,
    username: &str,
    groupname: &str,
) -> Result<String> {
    let mut bufwr = String::new();

    let filetype_char = match &meta {
        FileMeta::Directory(_) => 'd',
        FileMeta::Symlink(_) => 'L',
    };
    write!(bufwr, "{} ", filetype_char)?;
    canonicalize_escape_path(abs_path, &mut bufwr)?;

    match meta {
        FileMeta::Directory(mode) => {
            write!(bufwr, " {mode:04o} {username} {groupname} - -")?;
        }
        FileMeta::Symlink(target) => {
            bufwr.push_str(" - - - - ");
            canonicalize_escape_path(&target, &mut bufwr)?;
        }
    };

    Ok(bufwr)
}

/// The result of a tmpfiles.d generation run
#[derive(Debug, Default)]
pub struct TmpfilesWrittenResult {
    /// Set if we generated entries; this is the count and the path.
    pub generated: Option<(NonZeroUsize, Utf8PathBuf)>,
    /// Total number of unsupported files that were skipped
    pub unsupported: usize,
}

/// Translate the content of `/var` underneath the target root to use tmpfiles.d.
pub fn var_to_tmpfiles<U: uzers::Users, G: uzers::Groups>(
    rootfs: &Dir,
    users: &U,
    groups: &G,
) -> Result<TmpfilesWrittenResult> {
    let (existing_tmpfiles, generation) = read_tmpfiles(rootfs)?;

    // We should never have /var/run as a non-symlink. Don't recurse into it, it's
    // a hard error.
    if let Some(meta) = rootfs.symlink_metadata_optional("var/run")? {
        if !meta.is_symlink() {
            return Err(Error::FoundVarRunNonSymlink {});
        }
    }

    // Require that the tmpfiles.d directory exists; it's part of systemd.
    if !rootfs.try_exists(TMPFILESD)? {
        return Err(Error::MissingTmpfilesDir {});
    }

    let mut entries = BTreeSet::new();
    let mut prefix = PathBuf::from("/var");
    let mut unsupported = Vec::new();
    convert_path_to_tmpfiles_d_recurse(
        &mut entries,
        &mut unsupported,
        users,
        groups,
        rootfs,
        &existing_tmpfiles,
        &mut prefix,
        false,
    )?;

    // If there's no entries, don't write a file
    let Some(entries_count) = NonZeroUsize::new(entries.len()) else {
        return Ok(TmpfilesWrittenResult::default());
    };

    let path = generation.path();
    // This should not exist
    assert!(!rootfs.try_exists(&path)?);

    rootfs.atomic_replace_with(&path, |bufwr| -> Result<()> {
        let mode = Permissions::from_mode(0o644);
        bufwr.get_mut().as_file_mut().set_permissions(mode)?;

        for line in entries.iter() {
            bufwr.write_all(line.as_bytes())?;
            writeln!(bufwr)?;
        }
        if !unsupported.is_empty() {
            let (samples, rest) = bootc_utils::iterator_split(unsupported.iter(), 5);
            for elt in samples {
                writeln!(bufwr, "# bootc ignored: {elt:?}")?;
            }
            let rest = rest.count();
            if rest > 0 {
                writeln!(bufwr, "# bootc ignored: ...and {rest} more")?;
            }
        }
        Ok(())
    })?;

    Ok(TmpfilesWrittenResult {
        generated: Some((entries_count, path)),
        unsupported: unsupported.len(),
    })
}

/// Recursively explore target directory and translate content to tmpfiles.d entries. See
/// `convert_var_to_tmpfiles_d` for more background.
///
/// This proceeds depth-first and progressively deletes translated subpaths as it goes.
/// `prefix` is updated at each recursive step, so that in case of errors it can be
/// used to pinpoint the faulty path.
fn convert_path_to_tmpfiles_d_recurse<U: uzers::Users, G: uzers::Groups>(
    out_entries: &mut BTreeSet<String>,
    out_unsupported: &mut Vec<PathBuf>,
    users: &U,
    groups: &G,
    rootfs: &Dir,
    existing: &BTreeMap<PathBuf, String>,
    prefix: &mut PathBuf,
    readonly: bool,
) -> Result<()> {
    let relpath = prefix.strip_prefix("/").unwrap();
    for subpath in rootfs.read_dir(relpath)? {
        let subpath = subpath?;
        let meta = subpath.metadata()?;
        let fname = subpath.file_name();
        prefix.push(fname);

        let has_tmpfiles_entry = existing.contains_key(prefix);

        // Translate this file entry.
        if !has_tmpfiles_entry {
            let entry = {
                // SAFETY: We know this path is absolute
                let relpath = prefix.strip_prefix("/").unwrap();
                let Some(tmpfiles_meta) = FileMeta::from_fs(rootfs, &relpath)? else {
                    out_unsupported.push(relpath.into());
                    assert!(prefix.pop());
                    continue;
                };
                let uid = meta.uid();
                let gid = meta.gid();
                let user = users
                    .get_user_by_uid(meta.uid())
                    .ok_or_else(|| Error::UserNotFound(uid))?;
                let username = user.name();
                let username: &str = username.to_str().ok_or_else(|| Error::NonUtf8User {
                    uid,
                    name: username.to_string_lossy().into_owned(),
                })?;
                let group = groups
                    .get_group_by_gid(gid)
                    .ok_or_else(|| Error::GroupNotFound(gid))?;
                let groupname = group.name();
                let groupname: &str = groupname.to_str().ok_or_else(|| Error::NonUtf8Group {
                    gid,
                    name: groupname.to_string_lossy().into_owned(),
                })?;
                translate_to_tmpfiles_d(&prefix, tmpfiles_meta, &username, &groupname)?
            };
            out_entries.insert(entry);
        }

        if meta.is_dir() {
            // SAFETY: We know this path is absolute
            let relpath = prefix.strip_prefix("/").unwrap();
            // Avoid traversing mount points by default
            if rootfs.open_dir_noxdev(relpath)?.is_some() {
                convert_path_to_tmpfiles_d_recurse(
                    out_entries,
                    out_unsupported,
                    users,
                    groups,
                    rootfs,
                    existing,
                    prefix,
                    readonly,
                )?;
                let relpath = prefix.strip_prefix("/").unwrap();
                if !readonly {
                    rootfs.remove_dir_all(relpath)?;
                }
            }
        } else {
            // SAFETY: We know this path is absolute
            let relpath = prefix.strip_prefix("/").unwrap();
            if !readonly {
                rootfs.remove_file(relpath)?;
            }
        }
        assert!(prefix.pop());
    }
    Ok(())
}

/// Convert /var for the current root to use systemd tmpfiles.d.
#[allow(unsafe_code)]
pub fn convert_var_to_tmpfiles_current_root() -> Result<TmpfilesWrittenResult> {
    let rootfs = Dir::open_ambient_dir("/", cap_std::ambient_authority())?;

    // See the docs for why this is unsafe
    let usergroups = unsafe { uzers::cache::UsersSnapshot::new() };

    var_to_tmpfiles(&rootfs, &usergroups, &usergroups)
}

/// The result of processing tmpfiles.d
#[derive(Debug)]
pub struct TmpfilesResult {
    /// The resulting tmpfiles.d entries
    pub tmpfiles: BTreeSet<String>,
    /// Paths which could not be processed
    pub unsupported: Vec<PathBuf>,
}

/// Convert /var for the current root to use systemd tmpfiles.d.
#[allow(unsafe_code)]
pub fn find_missing_tmpfiles_current_root() -> Result<TmpfilesResult> {
    use uzers::cache::UsersSnapshot;

    let rootfs = Dir::open_ambient_dir("/", cap_std::ambient_authority())?;

    // See the docs for why this is unsafe
    let usergroups = unsafe { UsersSnapshot::new() };

    let existing_tmpfiles = read_tmpfiles(&rootfs)?.0;

    let mut prefix = PathBuf::from("/var");
    let mut tmpfiles = BTreeSet::new();
    let mut unsupported = Vec::new();
    convert_path_to_tmpfiles_d_recurse(
        &mut tmpfiles,
        &mut unsupported,
        &usergroups,
        &usergroups,
        &rootfs,
        &existing_tmpfiles,
        &mut prefix,
        true,
    )?;
    Ok(TmpfilesResult {
        tmpfiles,
        unsupported,
    })
}

/// Read all tmpfiles.d entries in the target directory, and return a mapping
/// from (file path) => (single tmpfiles.d entry line)
fn read_tmpfiles(rootfs: &Dir) -> Result<(BTreeMap<PathBuf, String>, BootcTmpfilesGeneration)> {
    let Some(tmpfiles_dir) = rootfs.open_dir_optional(TMPFILESD)? else {
        return Ok(Default::default());
    };
    let mut result = BTreeMap::new();
    let mut generation = BootcTmpfilesGeneration::default();
    for entry in tmpfiles_dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let (Some(stem), Some(extension)) =
            (Path::new(&name).file_stem(), Path::new(&name).extension())
        else {
            continue;
        };
        if extension != "conf" {
            continue;
        }
        if let Ok(s) = stem.as_str() {
            if s.starts_with(BOOTC_GENERATED_PREFIX) {
                generation.increment();
            }
        }
        let r = BufReader::new(entry.open()?);
        for line in r.lines() {
            let line = line?;
            if line.is_empty() || line.starts_with("#") {
                continue;
            }
            let path = tmpfiles_entry_get_path(&line)?;
            result.insert(path.to_owned(), line);
        }
    }
    Ok((result, generation))
}

fn tmpfiles_entry_get_path(line: &str) -> Result<PathBuf> {
    let err = || Error::MalformedTmpfilesEntry(line.to_string());
    let mut it = line.as_bytes().iter().copied().peekable();
    // Skip leading whitespace
    while let Some(_) = it.next_if(|c| c.is_ascii_whitespace()) {}
    // Skip the file type
    let mut found_ftype = false;
    while let Some(_) = it.next_if(|c| !c.is_ascii_whitespace()) {
        found_ftype = true
    }
    if !found_ftype {
        return Err(err());
    }
    // Skip trailing whitespace
    while let Some(_) = it.next_if(|c| c.is_ascii_whitespace()) {}
    unescape_path(&mut it)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::fs::DirBuilder;
    use cap_std_ext::cap_std::fs::DirBuilderExt as _;

    #[test]
    fn test_tmpfiles_entry_get_path() {
        let cases = [
              ("z /dev/kvm          0666 - kvm -", "/dev/kvm"),
              ("d /run/lock/lvm 0700 root root -", "/run/lock/lvm"),
              ("a+      /var/lib/tpm2-tss/system/keystore   -    -    -     -           default:group:tss:rwx", "/var/lib/tpm2-tss/system/keystore"),
              ("d \"/run/file with spaces/foo\" 0700 root root -", "/run/file with spaces/foo"),
            (
                r#"d /spaces\x20\x20here/foo 0700 root root -"#,
                "/spaces  here/foo",
            ),
        ];
        for (input, expected) in cases {
            let path = tmpfiles_entry_get_path(input).unwrap();
            assert_eq!(path, Path::new(expected), "Input: {input}");
        }
    }

    fn newroot() -> Result<cap_std_ext::cap_tempfile::TempDir> {
        let root = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        root.create_dir_all(TMPFILESD)?;
        Ok(root)
    }

    fn mock_userdb() -> uzers::mock::MockUsers {
        let testuid = rustix::process::getuid();
        let testgid = rustix::process::getgid();
        let mut users = uzers::mock::MockUsers::with_current_uid(testuid.as_raw());
        users.add_user(uzers::User::new(
            testuid.as_raw(),
            "testuser",
            testgid.as_raw(),
        ));
        users.add_group(uzers::Group::new(testgid.as_raw(), "testgroup"));
        users
    }

    #[test]
    fn test_tmpfiles_d_translation() -> anyhow::Result<()> {
        // Prepare a minimal rootfs as playground.
        let rootfs = &newroot()?;
        let userdb = &mock_userdb();

        let mut db = DirBuilder::new();
        db.recursive(true);
        db.mode(0o755);

        rootfs.write(
            Path::new(TMPFILESD).join("systemd.conf"),
            indoc::indoc! { r#"
            d /var/lib 0755 - - -
            d /var/lib/private 0700 root root -
            d /var/log/private 0700 root root -
        "#},
        )?;

        // Add test content.
        rootfs.ensure_dir_with("var/lib/systemd", &db)?;
        rootfs.ensure_dir_with("var/lib/private", &db)?;
        rootfs.ensure_dir_with("var/lib/nfs", &db)?;
        let global_rwx = Permissions::from_mode(0o777);
        rootfs.ensure_dir_with("var/lib/test/nested", &db).unwrap();
        rootfs.set_permissions("var/lib/test", global_rwx.clone())?;
        rootfs.set_permissions("var/lib/test/nested", global_rwx)?;
        rootfs.symlink("../", "var/lib/test/nested/symlink")?;
        rootfs.symlink_contents("/var/lib/foo", "var/lib/test/absolute-symlink")?;

        var_to_tmpfiles(rootfs, userdb, userdb).unwrap();

        // This is the first run
        let mut gen = BootcTmpfilesGeneration(0);
        let autovar_path = &gen.path();
        assert!(rootfs.try_exists(autovar_path).unwrap());
        let entries: Vec<String> = rootfs
            .read_to_string(autovar_path)
            .unwrap()
            .lines()
            .map(|s| s.to_owned())
            .collect();
        let expected = &[
            "L /var/lib/test/absolute-symlink - - - - /var/lib/foo",
            "L /var/lib/test/nested/symlink - - - - ../",
            "d /var/lib/nfs 0755 testuser testgroup - -",
            "d /var/lib/systemd 0755 testuser testgroup - -",
            "d /var/lib/test 0777 testuser testgroup - -",
            "d /var/lib/test/nested 0777 testuser testgroup - -",
        ];
        similar_asserts::assert_eq!(entries, expected);
        assert!(!rootfs.try_exists("var/lib").unwrap());

        // Now pretend we're doing a layered container build, and so we need
        // a new tmpfiles.d run
        rootfs.create_dir_all("var/lib/gen2-test")?;
        let w = var_to_tmpfiles(rootfs, userdb, userdb).unwrap();
        let wg = w.generated.as_ref().unwrap();
        assert_eq!(wg.0, NonZeroUsize::new(1).unwrap());
        assert_eq!(w.unsupported, 0);
        gen.increment();
        let autovar_path = &gen.path();
        assert_eq!(autovar_path, &wg.1);
        assert!(rootfs.try_exists(autovar_path).unwrap());
        Ok(())
    }

    /// Verify that we emit ignores for regular files
    #[test]
    fn test_log_regfile() -> anyhow::Result<()> {
        // Prepare a minimal rootfs as playground.
        let rootfs = &newroot()?;
        let userdb = &mock_userdb();

        rootfs.create_dir_all("var/log/dnf")?;
        rootfs.write("var/log/dnf/dnf.log", b"some dnf log")?;
        rootfs.create_dir_all("var/log/foo")?;
        rootfs.write("var/log/foo/foo.log", b"some other log")?;

        let gen = BootcTmpfilesGeneration(0);
        var_to_tmpfiles(rootfs, userdb, userdb).unwrap();
        let tmpfiles = rootfs.read_to_string(&gen.path()).unwrap();
        let ignored = tmpfiles
            .lines()
            .filter(|line| line.starts_with("# bootc ignored"))
            .count();
        assert_eq!(ignored, 2);
        Ok(())
    }

    #[test]
    fn test_canonicalize_escape_path() {
        let intact_cases = vec!["/", "/var", "/var/foo", "/run/foo"];
        for entry in intact_cases {
            let mut s = String::new();
            canonicalize_escape_path(Path::new(entry), &mut s).unwrap();
            similar_asserts::assert_eq!(&s, entry);
        }

        let quoting_cases = &[
            ("/var/foo bar", r#"/var/foo\x20bar"#),
            ("/var/run", "/run"),
            ("/var/run/foo bar", r#"/run/foo\x20bar"#),
        ];
        for (input, expected) in quoting_cases {
            let mut s = String::new();
            canonicalize_escape_path(Path::new(input), &mut s).unwrap();
            similar_asserts::assert_eq!(&s, expected);
        }
    }

    #[test]
    fn test_translate_to_tmpfiles_d() {
        let path = Path::new(r#"/var/foo bar"#);
        let username = "testuser";
        let groupname = "testgroup";
        {
            // Directory
            let meta = FileMeta::Directory(Mode::from_raw_mode(0o721));
            let out = translate_to_tmpfiles_d(path, meta, username, groupname).unwrap();
            let expected = r#"d /var/foo\x20bar 0721 testuser testgroup - -"#;
            similar_asserts::assert_eq!(out, expected);
        }
        {
            // Symlink
            let meta = FileMeta::Symlink("/mytarget".into());
            let out = translate_to_tmpfiles_d(path, meta, username, groupname).unwrap();
            let expected = r#"L /var/foo\x20bar - - - - /mytarget"#;
            similar_asserts::assert_eq!(out, expected);
        }
    }
}
