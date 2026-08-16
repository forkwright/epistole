//! Architectural fitness functions.
//!
//! These assert structural properties that ordinary unit tests cannot,
//! because the property is about where code lives rather than what it
//! computes.

use std::path::{Path, PathBuf};

/// Repository `src/` directory, resolved from the manifest dir so the
/// test is independent of the working directory the runner picks.
fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("read_dir {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// Strip comments, then remove *all* whitespace.
///
/// WARNING: two traps live here, and both produced a silently-passing rule
/// before they were fixed.
///
/// 1. The durability bypass this rule exists to catch was written across
///    four lines (`state` / `.store` / `.sends` / `.insert(...)`), so a
///    line-oriented scan never saw it. Joining lines with a single space is
///    not enough either — that yields `.sends .insert(`, which still fails
///    to match `.sends.insert(`. Only removing whitespace outright makes
///    the formatted and unformatted spellings converge.
/// 2. Comments name these calls when explaining them, so a rule that keeps
///    comments fires on its own documentation.
fn normalize(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        let code = match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        };
        out.extend(code.chars().filter(|c| !c.is_whitespace()));
    }
    out
}

/// The keyspace handles inside `Store`.
const KEYSPACES: [&str; 4] = ["subscribers", "sends", "deliveries", "rate_limits"];

/// Mutating fjall keyspace operations.
const MUTATORS: [&str; 2] = ["insert", "remove"];

#[test]
fn keyspace_mutations_stay_inside_the_store_module() {
    // WHY: `Store` owns the durability class of every write (see the
    // module docs on `store.rs`). A caller that reaches through to a
    // keyspace handle picks fjall's buffered default instead, which
    // acknowledges consent transitions that a power loss can still lose —
    // forkwright/epistole#69. Routing every write through a typed `Store`
    // method is what keeps that choice in one place.
    //
    // WHY the two-part exclusion: the `store` module spans `src/store.rs`
    // plus every file under `src/store/`. Excluding only the literal
    // `store.rs` path would make this rule fire on the module's own
    // methods wherever one of its submodules defines them.
    let mut sources = Vec::new();
    rust_sources(&src_dir(), &mut sources);
    assert!(!sources.is_empty(), "found no Rust sources under src/");

    let store_module = src_dir().join("store.rs");
    let store_dir = src_dir().join("store");
    let mut offenders = Vec::new();

    for path in sources {
        if path == store_module || path.starts_with(&store_dir) {
            continue;
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => panic!("read {}: {e}", path.display()),
        };
        let flat = normalize(&source);
        for keyspace in KEYSPACES {
            for mutator in MUTATORS {
                if flat.contains(&format!(".{keyspace}.{mutator}(")) {
                    offenders.push(format!("{}: .{keyspace}.{mutator}(", path.display()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "keyspace writes must go through a typed Store method that states its \
         durability class, not through a keyspace handle:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn store_keyspace_handles_are_private() {
    // WHY: the rule above is only enforceable while the handles cannot
    // escape the module. A `pub` or `pub(crate)` field would let a future
    // caller bypass the durability boundary and still compile.
    let store_module = src_dir().join("store.rs");
    let source = match std::fs::read_to_string(&store_module) {
        Ok(s) => s,
        Err(e) => panic!("read {}: {e}", store_module.display()),
    };
    let flat = normalize(&source);

    let mut exposed = Vec::new();
    for field in KEYSPACES.iter().chain(std::iter::once(&"database")) {
        for visibility in ["pub", "pub(crate)", "pub(super)"] {
            for ty in ["Keyspace", "Database"] {
                if flat.contains(&format!("{visibility}{field}:{ty}")) {
                    exposed.push(format!("{visibility} {field}: {ty}"));
                }
            }
        }
    }

    assert!(
        exposed.is_empty(),
        "Store's fjall handles must stay private so every write goes through a \
         method that chooses a durability mode; exposed: {}",
        exposed.join(", ")
    );
}
