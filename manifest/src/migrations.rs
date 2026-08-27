//! The `NNNN_name.sql` migrations directory, read and validated in one place.
//!
//! Three consumers have to agree on exactly what a migrations directory contains, and on what
//! makes one invalid:
//!
//! - `skyzen::embed_migrations!` reads it at **compile time** and bakes the files into the binary;
//! - `skyzen migrate` reads it at **deploy time** and applies the same files to a live database;
//! - the checksum recorded in `_skyzen_migrations` is what later tells the two apart.
//!
//! If the macro and the CLI disagreed about which files count, or about how a checksum is
//! computed, a deployment would look clean while running different SQL from the binary. So the
//! scan, the filename rules and the checksum live here — the crate both the macro and the CLI
//! already depend on — rather than being written twice.
//!
//! This module deliberately holds no database types: it hands back plain owned data, which
//! `skyzen-macros` turns into `include_str!` tokens and `skyzen-cli` turns into
//! `skyzen_services::sql::Migration` values.

use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The directory a `[[database]]` entry looks in when it names no `migrations_dir`.
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// The shape every migration file name has to have, quoted in error messages.
const NAME_SHAPE: &str =
    "migrations are named `<version>_<name>.sql`, e.g. `0001_create_users.sql`";

/// One migration file, read and checksummed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFile {
    /// The number the file name starts with. Files are applied in ascending order of this value,
    /// so `0002_x.sql` runs after `0001_y.sql` regardless of how they sort as strings.
    pub version: u64,
    /// The part of the file name between the version and the `.sql` suffix.
    pub name: String,
    /// Where the file is, for error messages and for `include_str!`.
    pub path: PathBuf,
    /// The file's contents, verbatim.
    pub sql: String,
    /// SHA-256 over the contents with CRLF normalized to LF — see [`checksum`].
    pub checksum: [u8; 32],
}

impl MigrationFile {
    /// The checksum as lowercase hex, which is the form stored in `_skyzen_migrations`.
    #[must_use]
    pub fn checksum_hex(&self) -> String {
        hex::encode(self.checksum)
    }

    /// The file's own name, which is what error messages should name rather than the full path.
    #[must_use]
    pub fn file_name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }
}

/// Everything that can go wrong reading a migrations directory.
#[derive(Debug, thiserror::Error)]
pub enum MigrationsError {
    /// The directory does not exist.
    #[error("migrations directory {path} does not exist")]
    MissingDirectory {
        /// The directory that was looked for.
        path: PathBuf,
    },
    /// The path exists but is not a directory.
    #[error("migrations path {path} is not a directory")]
    NotADirectory {
        /// The offending path.
        path: PathBuf,
    },
    /// The directory could not be listed.
    #[error("failed to read migrations directory {path}: {source}")]
    ReadDirectory {
        /// The directory being listed.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// A migration file could not be read.
    #[error("failed to read migration {path}: {source}")]
    ReadFile {
        /// The file being read.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// A file in the directory is not named like a migration.
    ///
    /// Skipping it silently is what lets `0001-init.sql` or `0001_init.sq` sit in the directory
    /// looking applied while never running, so an unrecognized name is a hard error instead.
    #[error("{path} is not a migration: {reason}; {NAME_SHAPE}")]
    FileName {
        /// The offending file.
        path: PathBuf,
        /// What is wrong with the name.
        reason: &'static str,
    },
    /// Two files claim the same version.
    #[error(
        "migrations `{first}` and `{second}` both declare version {version}; \
         every migration needs its own version"
    )]
    DuplicateVersion {
        /// The version both files claim.
        version: u64,
        /// The first file, in directory order.
        first: String,
        /// The second file.
        second: String,
    },
}

/// SHA-256 over `sql`, with CRLF line endings normalized to LF first.
///
/// The checksum is what detects edited history, so it has to answer "is this the same migration?"
/// and not "did this file arrive over a different git checkout?". A repository checked out on
/// Windows with `core.autocrlf` on holds byte-different files with identical SQL; hashing the raw
/// bytes would report every one of them as edited on the first deploy from that machine. The
/// embedded text itself is never normalized — `include_str!` bakes the file verbatim — only what
/// is hashed.
#[must_use]
pub fn checksum(sql: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    if sql.contains('\r') {
        hasher.update(sql.replace("\r\n", "\n").as_bytes());
    } else {
        hasher.update(sql.as_bytes());
    }
    hasher.finalize().into()
}

/// Read every migration in `dir`, in ascending version order.
///
/// Hidden entries (a leading `.`, which is how `.DS_Store` and editor swap files arrive) and
/// subdirectories are skipped. Every other entry must be a migration, or the scan fails naming it.
/// An existing but empty directory is not an error: a project can have declared its migrations
/// directory before writing the first migration.
///
/// # Errors
///
/// Returns [`MigrationsError`] when the directory is missing or unreadable, when a file is not
/// named `<version>_<name>.sql`, when a file cannot be read, or when two files claim one version.
pub fn load(dir: &Path) -> Result<Vec<MigrationFile>, MigrationsError> {
    if !dir.exists() {
        return Err(MigrationsError::MissingDirectory {
            path: dir.to_path_buf(),
        });
    }
    if !dir.is_dir() {
        return Err(MigrationsError::NotADirectory {
            path: dir.to_path_buf(),
        });
    }

    let mut files = Vec::new();
    let entries = fs::read_dir(dir).map_err(|source| MigrationsError::ReadDirectory {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| MigrationsError::ReadDirectory {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let raw_name = entry.file_name();
        let file_name = raw_name.to_string_lossy();

        if file_name.starts_with('.') || path.is_dir() {
            continue;
        }

        let (version, name) =
            parse_file_name(&file_name).map_err(|reason| MigrationsError::FileName {
                path: path.clone(),
                reason,
            })?;
        let sql = fs::read_to_string(&path).map_err(|source| MigrationsError::ReadFile {
            path: path.clone(),
            source,
        })?;

        files.push(MigrationFile {
            version,
            name: name.to_owned(),
            checksum: checksum(&sql),
            sql,
            path,
        });
    }

    // Sorting by version and *then* rejecting duplicates is what makes the order the run order:
    // the file names only ever order correctly by accident (`0010` sorts before `0009` the moment
    // the digit count changes), so the parsed number is the authority.
    files.sort_by(|left, right| {
        left.version
            .cmp(&right.version)
            .then_with(|| left.path.cmp(&right.path))
    });
    for pair in files.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(MigrationsError::DuplicateVersion {
                version: pair[0].version,
                first: pair[0].file_name(),
                second: pair[1].file_name(),
            });
        }
    }

    Ok(files)
}

/// Split `<version>_<name>.sql` into its two parts.
///
/// The version is any run of ASCII digits, so both the sequential (`0001_`) and the timestamp
/// (`20260214093000_`) conventions work; what orders migrations is the number, not the digit
/// count, so the two never have to be told apart.
fn parse_file_name(file_name: &str) -> Result<(u64, &str), &'static str> {
    let stem = file_name
        .strip_suffix(".sql")
        .ok_or("it does not end in `.sql`")?;
    let (digits, name) = stem
        .split_once('_')
        .ok_or("it has no `_` separating the version from the name")?;

    if digits.is_empty() {
        return Err("it starts with `_`, so it carries no version");
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("the part before the first `_` is not a number");
    }
    if name.is_empty() {
        return Err("it has no name after the version");
    }

    let version = digits
        .parse::<u64>()
        .map_err(|_| "the version does not fit in a 64-bit integer")?;
    Ok((version, name))
}

#[cfg(test)]
mod tests {
    use super::{checksum, load, parse_file_name, MigrationsError, DEFAULT_MIGRATIONS_DIR};
    use std::{fs, path::Path};

    fn write(dir: &Path, name: &str, sql: &str) {
        fs::write(dir.join(name), sql).expect("write migration");
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn the_default_directory_is_the_wrangler_one() {
        // `wrangler d1 migrations apply` defaults to `migrations/` too, so a project migrating
        // both a D1 database and a native one keeps one directory rather than two.
        assert_eq!(DEFAULT_MIGRATIONS_DIR, "migrations");
    }

    #[test]
    fn reads_migrations_in_version_order_not_file_name_order() {
        let dir = temp_dir();
        write(dir.path(), "0009_ninth.sql", "SELECT 9;");
        write(dir.path(), "0010_tenth.sql", "SELECT 10;");
        write(dir.path(), "1_first.sql", "SELECT 1;");

        let files = load(dir.path()).expect("scan");
        let versions: Vec<u64> = files.iter().map(|file| file.version).collect();
        assert_eq!(versions, vec![1, 9, 10]);
        assert_eq!(files[2].name, "tenth");
        assert_eq!(files[0].sql, "SELECT 1;");
    }

    #[test]
    fn a_timestamp_version_is_just_a_bigger_number() {
        let dir = temp_dir();
        write(dir.path(), "20260214093000_init.sql", "SELECT 1;");
        let files = load(dir.path()).expect("scan");
        assert_eq!(files[0].version, 20_260_214_093_000);
        assert_eq!(files[0].name, "init");
    }

    #[test]
    fn an_empty_directory_is_not_an_error() {
        let dir = temp_dir();
        assert_eq!(load(dir.path()).expect("scan").len(), 0);
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_it() {
        let dir = temp_dir();
        let missing = dir.path().join("nope");
        let error = load(&missing).expect_err("missing");
        assert!(matches!(error, MigrationsError::MissingDirectory { .. }));
        assert!(error.to_string().contains("nope"), "{error}");
    }

    #[test]
    fn hidden_files_and_subdirectories_are_skipped() {
        let dir = temp_dir();
        write(dir.path(), "0001_init.sql", "SELECT 1;");
        write(dir.path(), ".DS_Store", "junk");
        fs::create_dir(dir.path().join("archive")).expect("subdir");

        let files = load(dir.path()).expect("scan");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn a_file_that_is_not_named_like_a_migration_is_a_hard_error() {
        for (name, fragment) in [
            ("0001-init.sql", "no `_`"),
            ("0001_init.sq", "`.sql`"),
            ("v1_init.sql", "not a number"),
            ("_init.sql", "no version"),
            ("0001_.sql", "no name"),
        ] {
            let dir = temp_dir();
            write(dir.path(), name, "SELECT 1;");
            let error = load(dir.path()).expect_err(name);
            assert!(matches!(error, MigrationsError::FileName { .. }), "{name}");
            let rendered = error.to_string();
            assert!(rendered.contains(fragment), "{name}: {rendered}");
            // The message always shows the shape the user should have written.
            assert!(rendered.contains("0001_create_users.sql"), "{rendered}");
        }
    }

    #[test]
    fn two_files_claiming_one_version_name_both() {
        let dir = temp_dir();
        write(dir.path(), "0001_first.sql", "SELECT 1;");
        write(dir.path(), "001_second.sql", "SELECT 2;");

        let error = load(dir.path()).expect_err("duplicate");
        assert!(matches!(error, MigrationsError::DuplicateVersion { .. }));
        let rendered = error.to_string();
        assert!(
            rendered.contains("first") && rendered.contains("second"),
            "{rendered}"
        );
    }

    #[test]
    fn the_checksum_ignores_line_endings_but_nothing_else() {
        assert_eq!(
            checksum("CREATE TABLE t;\n"),
            checksum("CREATE TABLE t;\r\n")
        );
        assert_ne!(checksum("CREATE TABLE t;\n"), checksum("CREATE TABLE u;\n"));
        // A lone `\r` is content, not a line ending, so it still changes the checksum.
        assert_ne!(checksum("a\rb"), checksum("ab"));
    }

    #[test]
    fn the_checksum_is_stable_across_runs() {
        // Written out rather than compared to a recomputation: this value is what a deployed
        // `_skyzen_migrations` row holds, so changing the algorithm has to break this test.
        let dir = temp_dir();
        write(dir.path(), "0001_init.sql", "SELECT 1;\n");
        let files = load(dir.path()).expect("scan");
        assert_eq!(
            files[0].checksum_hex(),
            "b4e0497804e46e0a0b0b8c31975b062152d551bac49c3c2e80932567b4085dcd"
        );
    }

    #[test]
    fn the_file_name_parser_accepts_underscores_inside_the_name() {
        assert_eq!(
            parse_file_name("0003_add_user_email_index.sql").expect("parse"),
            (3, "add_user_email_index")
        );
    }
}
