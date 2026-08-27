# SQL Migrations

Skyzen migrations are plain `.sql` files in a directory, embedded into the binary at compile time
and applied by a runner that works on every backend `Db` works on — sqlx-backed PostgreSQL, MySQL
and SQLite, Cloudflare D1, and the Aurora Data API.

There is no separate migration DSL, no migration binary to install, and no ORM. A migration is SQL,
and the framework's job is to apply each one exactly once, atomically, and to refuse to run when
the files no longer match what the database recorded.

## The Directory

```
migrations/
  0001_create_users.sql
  0002_add_user_email_index.sql
  0003_seed_roles.sql
```

Every file is named `<version>_<name>.sql`:

- **`<version>`** is a run of ASCII digits. Both conventions work — sequential (`0001_`) and
  timestamped (`20260214093000_`) — because what orders migrations is the number, not the digit
  count. `0010_` therefore runs after `0009_`, which string-sorting the file names would get wrong.
- **`<name>`** is everything between the first `_` and `.sql`. It may contain underscores.

Versions must be unique. Entries whose name starts with `.` (`.DS_Store`, editor swap files) and
subdirectories are skipped; **every other file must be a migration**, or reading the directory
fails naming it. Silently ignoring `0001-init.sql` would leave a file sitting in the directory
looking applied while never running.

An existing but empty directory is fine — a project can declare where its migrations go before
writing the first one.

## Embedding Them

```rust
use skyzen::embed_migrations;
use skyzen_services::Migrations;

static MIGRATIONS: Migrations = embed_migrations!("migrations");
```

The path is relative to the crate's `CARGO_MANIFEST_DIR`, so it names the same directory from
anywhere in the crate. At expansion the macro reads the directory, validates it, computes each
file's checksum, and emits `include_str!` for the contents — so editing a migration rebuilds the
crate. A malformed file name or a repeated version is a compile error pointing at the path literal.

`static` is the usual binding, because `#[skyzen::test(migrations = ...)]` needs a path to name.

## Applying Them

```rust
let report = db.migrate(&MIGRATIONS).await?;
tracing::info!(applied = ?report.applied, skipped = report.skipped, "migrations");
```

`Db::migrate` creates the bookkeeping table if absent, verifies every already-applied migration's
checksum, then applies each pending migration in version order. It is idempotent: a second call
applies nothing and reports everything as skipped.

`Db::migration_status` answers the same questions without changing anything, returning the applied
rows and the pending migrations.

### The Bookkeeping Table

```sql
CREATE TABLE IF NOT EXISTS _skyzen_migrations (
    version    BIGINT PRIMARY KEY,
    name       TEXT NOT NULL,
    checksum   TEXT NOT NULL,
    applied_at TEXT NOT NULL
)
```

`applied_at` is written by the database's own `CURRENT_TIMESTAMP`, never by the process running the
migration, so a machine with a skewed clock cannot write a timestamp that misorders the history.

## Atomicity Per Backend

A migration's statements and the row that records it go into **one** `Db::execute_batch`, so the
schema change and the record of it land together or not at all. What that means depends on the
backend:

| Backend | `execute_batch` is | Atomic across DDL? |
| --- | --- | --- |
| PostgreSQL (sqlx) | a real transaction | yes — PostgreSQL has transactional DDL |
| SQLite (sqlx) | a real transaction | yes |
| MySQL (sqlx) | a real transaction | **no** — MySQL commits implicitly on DDL |
| Cloudflare D1 | D1's own `batch()` | yes |
| Aurora Data API | a Data API transaction | follows the engine (Aurora MySQL has MySQL's behaviour) |

The MySQL caveat is MySQL's, not Skyzen's: a migration that creates a table and then fails keeps
the table, and the version row is not written, so re-running it fails on the table that already
exists. On MySQL, prefer one DDL statement per migration.

Statements are split with the SQL tokenizer, not on the `;` byte, so a semicolon inside a string
literal, a comment or a quoted identifier stays where it belongs:

```sql
INSERT INTO users (email) VALUES ('semi;colon@example.invalid');  -- not two statements
```

A file with no statements at all — empty, or only comments — is an error rather than a silent
no-op.

### `?` in a Migration

Migrations are static SQL and bind no parameters, so a bare `?` outside a string literal or comment
is treated as a bind placeholder, exactly as it is in `db.query(...)`, and the migration fails its
placeholder/parameter count check. This matters on PostgreSQL, where `?` is also the JSONB
key-existence operator: write `jsonb_exists(data, 'key')` rather than `data ? 'key'`. A `?` inside a
string literal or a comment is content and is left alone.

## Checksums and Edited History

Each migration carries the SHA-256 of its SQL, and that hash is stored alongside the version. On
every run, each already-applied migration is compared against the file it was applied from. A
mismatch is a hard error naming the file:

```
migration `create_users` (version 1) has changed since it was applied: the database recorded
checksum aa… but the embedded file hashes to bb…. Applied migrations are immutable — add a new
migration instead of editing this one.
```

This is the one mistake that otherwise hides itself: production keeps the schema the old file
produced while the source says something else, and every later migration is written against a
schema that exists only on a fresh database.

Two deliberate policies:

- **Line endings are normalized before hashing.** CRLF becomes LF, so a Windows checkout does not
  report every migration as edited. The embedded text itself is never normalized — `include_str!`
  bakes the file verbatim — only what is hashed.
- **A version the database has but the build does not is allowed**, and logged. That is what a
  rollback to an older build looks like, and refusing to start would turn a routine rollback into
  an outage.

## Concurrency

Two deployments migrating the same database at once is handled by the primary key rather than by a
lock. Both compute the same pending set, both run the same batch, and the loser's batch fails on
the version row. The runner re-reads the table, sees the version present, and reports
`DbError::Conflict` — not the raw constraint violation, which would read as a bug in the migration.

## Testing

`InMemoryDb::with_migrations` builds a SQLite database through the real runner, so a test's schema
is the schema a deploy produces, `_skyzen_migrations` included:

```rust
let db = InMemoryDb::with_migrations(&MIGRATIONS).await?;
```

`#[skyzen::test]` does the same for the database it injects:

```rust
#[skyzen::test(migrations = MIGRATIONS)]
async fn a_user_can_be_inserted(db: Db) {
    db.query("INSERT INTO users (email) VALUES (?)")
        .bind("ada@example.invalid")
        .execute()
        .await
        .unwrap();
}
```

Each test gets its own database, so tests do not share migrated state. `InMemoryDb::with_schema`
is still there for tests that want a table and do not care about migrations.

Running the real set in tests is what makes "does this migration apply cleanly?" a question the
test suite answers rather than the first deployment.

## From the CLI

```sh
skyzen migrate                          # Cloudflare D1, via `wrangler d1 migrations apply`
skyzen migrate --local                  # the D1 emulator's database, for `skyzen dev`
skyzen migrate status                   # D1's own record, via `wrangler d1 migrations list`
skyzen migrate --provider native        # apply through [native.database.<name>].url_env
skyzen migrate status --provider native # read `_skyzen_migrations` through that connection
skyzen migrate --dry-run                # validate the directory and print the plan; never connects
```

The CLI cannot see inside a crate it has not compiled, so it reads the **same directory** the macro
embeds, through the **same** reader — versions, names and checksums are computed identically on
both paths. A migration applied by `skyzen migrate` is therefore one the application accepts as
already applied, rather than reporting as edited history.

The native path applies to **every** `[[database]]` that has a `[native.database.<name>]` wiring it
can open a connection to, one after another, matching the Cloudflare path's
one-`wrangler`-call-per-database behaviour. Databases without native wiring are skipped with a
warning, and so are the two backends the CLI cannot dial: `backend = "rds-data"`, because the RDS
Data API is an HTTP service reached by ARN rather than a connection, and `backend = "azure-sql"`,
because the CLI links sqlx and sqlx has no T-SQL driver. Their migrations run from the application
itself, through the same embedded set — `db.migrate(MIGRATIONS).await?` at startup.

`--dry-run` reads and validates the directories and prints what would run. It never opens a
connection, so it needs no connection string. It applies to the native path; the Cloudflare path
prints the `wrangler` invocations it would run instead.

`status` takes the same branch as applying, so it always reports on the database `skyzen migrate`
would write to: `wrangler d1 migrations list` under the default `--provider cloudflare`, and the
`_skyzen_migrations` table through `[native.database.<name>].url_env` under `--provider native`. A
project with both wirings therefore never gets D1's answer from SQLite's bookkeeping, or the
reverse.

## Configuration

```toml
[[database]]
name = "main"
type = "sql"
default = true
migrations_dir = "db/changes"   # optional; defaults to "migrations"

[native.database.main]
backend = "postgres"            # postgres | mysql | sqlite | azure-sql | rds-data
url_env = "DATABASE_URL"        # the ADO.NET string for azure-sql; unused by rds-data, which
                                # names its four values itself or reads RDS_* for them
```

`migrations_dir` defaults to `migrations/`, which is also `wrangler d1 migrations apply`'s default,
so a database deployed to D1 and one run natively read the same files without configuring anything.
**Changing it moves only the native path** — wrangler needs its own `migrations_dir`, set through
`[cloudflare.raw]`:

```toml
[cloudflare.raw]
migrations_dir = "db/changes"
```

For SQLite, remember sqlx does not create the file unless the URL says so:

```sh
DATABASE_URL='sqlite://./app.db?mode=rwc'
```

## Errors

| Error | Means |
| --- | --- |
| `DbError::MigrationChanged` | an applied migration's file was edited; add a new migration instead |
| `DbError::Conflict` | another runner applied this version first; nothing of this batch landed |
| `DbError::Unauthorized` | the credentials or role are wrong — a deployment fault, not a request one |
| `DbError::Backend` | the migration's own SQL failed; the batch rolled back |
