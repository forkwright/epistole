//! Keyspace path safety: refuses to open a path that resolves inside
//! another keyspace's `partitions/` directory.

use std::path::Path;

use crate::error::{Error, Result};

/// Refuse to open a keyspace whose path resolves INSIDE another
/// keyspace's `partitions/` subdirectory. This is a fleet footgun (per
/// `feedback_fjall_nested_keyspace_pitfall.md`): nested keyspaces trap
/// lsm-tree's V1-format check on later opens, leaving the data
/// permanently un-readable.
///
/// Reaudit finding #30: the previous implementation walked the lexical
/// path components only, so a symlink like `/tmp/data ->
/// /var/lib/epistole/data/partitions/sends` bypassed the check —
/// `/tmp/data` has no `partitions` ancestor lexically, but the
/// canonical destination does. We now:
///   1. Canonicalize the path's parent (the path itself may not exist
///      yet — fjall creates it). Symlinks resolve to their targets.
///   2. Walk the canonical path's components.
///   3. Reject if any ancestor name is `partitions`.
pub(crate) fn guard_against_nested_keyspace(path: &Path) -> Result<()> {
    // The path may not exist yet (fjall creates it). Canonicalize the
    // nearest existing ancestor and join the remaining path components.
    let canonical = canonicalize_with_nonexistent(path).map_err(|e| Error::Store {
        reason: format!("canonicalize {}: {e}", path.display()),
    })?;

    let mut cur = canonical.parent();
    while let Some(p) = cur {
        if p.file_name().and_then(|n| n.to_str()) == Some("partitions") {
            return Err(Error::Store {
                reason: format!(
                    "refusing to open: {} canonicalizes to {} which is inside a parent \
                     keyspace's partitions/ directory (nested keyspaces trap lsm-tree's \
                     V1-format check; pick a path outside any existing fjall keyspace — \
                     see feedback_fjall_nested_keyspace_pitfall.md)",
                    path.display(),
                    canonical.display()
                ),
            });
        }
        cur = p.parent();
    }
    Ok(())
}

/// Canonicalize `path`, walking up to the nearest existing ancestor and
/// re-appending the missing tail. Symlinks resolve to their targets —
/// including BROKEN symlinks (whose target does not exist), which the
/// previous Phase 1.5.2 implementation silently passed through (audit
/// finding #33).
///
/// Algorithm:
///   1. Make the path absolute.
///   2. Walk parents using `symlink_metadata().is_ok()` rather than
///      `Path::exists()`. The former returns true for a broken
///      symlink (the link itself exists; only its target is missing);
///      the latter returns false because it follows. Without this,
///      a broken symlink slipped past as "doesn't exist" and the
///      stripped name was re-appended literally to the canonical
///      parent, hiding the symlink target from the guard.
///   3. Once we hit an existing ancestor, canonicalize it.
///   4. For each tail component, if it's a symlink, read its target
///      (recursively if needed) and canonicalize the result. If it's
///      a regular not-yet-existing file, append literally.
fn canonicalize_with_nonexistent(path: &Path) -> std::io::Result<std::path::PathBuf> {
    canonicalize_bounded(path, 0)
}

/// Maximum broken-symlink hops resolved before giving up.
///
/// WHY: `std::fs::canonicalize` stops a symlink loop itself with `ELOOP`,
/// but the broken-symlink fallback below re-enters this function with the
/// link's target and so has no such backstop. `a -> b`, `b -> a` with a
/// missing target recurses until the stack is exhausted, which aborts the
/// process inside `Store::open` before any error can be reported. Linux
/// caps a resolution chain at 40 links; matching that bound rejects the
/// same paths the kernel would while keeping every legitimate chain.
const MAX_SYMLINK_HOPS: usize = 40;

fn canonicalize_bounded(path: &Path, hops: usize) -> std::io::Result<std::path::PathBuf> {
    if hops > MAX_SYMLINK_HOPS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TooManyLinks,
            format!(
                "symlink chain at {} exceeded {MAX_SYMLINK_HOPS} hops — the path is \
                 circular or too deeply linked",
                path.display()
            ),
        ));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    // Walk up to the first ancestor whose entry exists in the
    // filesystem (regular file/dir OR symlink, broken or not).
    // symlink_metadata does NOT follow symlinks, so a broken symlink
    // counts as existing — we want to inspect it.
    let mut existing: &Path = &absolute;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match existing.parent() {
            Some(parent) if parent != existing => {
                if let Some(name) = existing.file_name() {
                    tail.push(name);
                }
                existing = parent;
            }
            _ => break,
        }
    }

    // Resolve `existing` itself. If it's a symlink, follow target
    // chains via canonicalize (which fails on broken symlinks — the
    // safer behavior here, since a broken symlink target may not
    // exist YET but will be created by fjall).
    let mut canonical = match std::fs::canonicalize(existing) {
        Ok(p) => p,
        Err(_e) => {
            // Broken symlink at the leaf of the existing chain.
            // Read its target manually + canonicalize the target's
            // PARENT (which must exist for fjall to write there).
            let target = std::fs::read_link(existing)?;
            let resolved_target = if target.is_absolute() {
                target
            } else {
                existing.parent().map(|p| p.join(&target)).unwrap_or(target)
            };
            // Recurse: the target itself may be a chain of symlinks
            // or may not exist yet. The `canonicalize_bounded` call
            // resolves what it can and returns the rest literally.
            canonicalize_bounded(&resolved_target, hops + 1)?
        }
    };
    for component in tail.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

#[cfg(test)]
mod store_guard_tests {
    use super::*;
    use crate::store::Store;

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_symlink_into_nested_partition_dir() {
        // Reaudit #30 regression: a symlink whose target is inside a
        // parent keyspace's partitions/ directory must be rejected.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let real_keyspace = tmp.path().join("real-keyspace");
        let real_partition = real_keyspace.join("partitions").join("sends");
        std::fs::create_dir_all(&real_partition).expect("create real");

        let symlink = tmp.path().join("sneaky-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_partition, &symlink).expect("symlink");

        let result = guard_against_nested_keyspace(&symlink);
        assert!(
            result.is_err(),
            "guard must reject symlink that resolves into a parent partitions/ dir"
        );
        let err = match result {
            Ok(()) => unreachable!("asserted is_err above"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("partitions"),
            "error should reference the partitions ancestor, got: {msg}"
        );
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn allows_clean_path() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("subdir-that-does-not-exist-yet");
        guard_against_nested_keyspace(&path).expect("clean path passes");
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_broken_symlink_into_partitions() {
        // Reaudit #33: a BROKEN symlink (target does not exist) whose
        // target path walks through `partitions/` slipped past the
        // Phase 1.5.2 guard because Path::exists() returns false on
        // broken symlinks. The fix uses symlink_metadata + manual
        // read_link to inspect the target.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // Target does NOT exist yet — broken symlink.
        let broken_target = tmp
            .path()
            .join("future-keyspace")
            .join("partitions")
            .join("sends");
        let symlink = tmp.path().join("broken-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&broken_target, &symlink).expect("symlink");

        let result = guard_against_nested_keyspace(&symlink);
        assert!(
            result.is_err(),
            "guard must reject a broken symlink whose target name walks through partitions/"
        );
        let err = match result {
            Ok(()) => unreachable!("asserted is_err above"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("partitions"),
            "error should reference partitions, got: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_circular_symlink_instead_of_overflowing_the_stack() {
        // #43: the broken-symlink fallback in canonicalize_with_nonexistent
        // re-entered itself with the link target and carried no bound, so
        // `a -> b`, `b -> a` recursed until the stack was exhausted and the
        // process aborted inside Store::open. Reaching this assertion at all
        // means the recursion terminated.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::os::unix::fs::symlink(&b, &a).expect("symlink a -> b");
        std::os::unix::fs::symlink(&a, &b).expect("symlink b -> a");

        let result = Store::open(&a);

        assert!(
            result.is_err(),
            "a circular symlink chain must return an error, not abort the process"
        );
    }

    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn accepts_a_symlink_chain_shorter_than_the_hop_bound() {
        // The bound must reject cycles without rejecting the legitimate
        // chains a deploy may have, so prove one still resolves.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let real = tmp.path().join("real-data");
        std::fs::create_dir(&real).expect("create real dir");

        let mut previous = real.clone();
        for hop in 0..8 {
            let link = tmp.path().join(format!("hop-{hop}"));
            std::os::unix::fs::symlink(&previous, &link).expect("symlink hop");
            previous = link;
        }

        guard_against_nested_keyspace(&previous).expect("a bounded chain must pass the guard");
    }
}
