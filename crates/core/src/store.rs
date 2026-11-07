//! SQLite storage for forwards, projects and settings.
//!
//! The schema extends v1's (`~/.kprtfwd/kprtfwd.db`). Schema changes are
//! additive, so the v1 binary keeps working on a migrated file.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Progress;
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension, Row, params};

use crate::model::{Forward, ForwardId, ForwardSettings, Project};
use crate::validate::{self, ValidationError};

/// How long a connection waits for another process's lock, such as v1 writing
/// the file.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version this code reads and writes, kept in `PRAGMA user_version`.
/// v1 never set it, so a v1 database reports 0.
pub const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid data: {0}")]
    Invalid(#[from] ValidationError),
    #[error("unknown forward {0}")]
    UnknownForward(ForwardId),
    #[error("forward {0} already exists")]
    ForwardExists(ForwardId),
    #[error("forward {id}: {field} {value} is not a valid port")]
    BadPort {
        id: ForwardId,
        field: &'static str,
        value: i64,
    },
    #[error("database schema version {0} is newer than this kprtfwd supports ({SCHEMA_VERSION})")]
    UnsupportedVersion(i64),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// The schema exactly as v1 creates it. `IF NOT EXISTS` makes this a no-op on
/// a v1 database, so it only matters for new files.
const V1_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS port_forwards (
    id TEXT PRIMARY KEY,
    context TEXT NOT NULL,
    namespace TEXT NOT NULL,
    service TEXT NOT NULL,
    port_remote INTEGER NOT NULL,
    port_local INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS project_port_forwards (
    project_id INTEGER,
    port_forward_id TEXT,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY (port_forward_id) REFERENCES port_forwards(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, port_forward_id)
);
CREATE INDEX IF NOT EXISTS idx_port_forwards_context ON port_forwards(context);
CREATE INDEX IF NOT EXISTS idx_port_forwards_namespace ON port_forwards(namespace);
CREATE INDEX IF NOT EXISTS idx_port_forwards_service ON port_forwards(service);
";

/// v2: desired state per forward and a key/value table for settings. v1 never
/// enabled foreign keys, so its files can hold memberships whose forward or
/// project is gone; they are dropped.
const V2_MIGRATION: &str = "
ALTER TABLE port_forwards ADD COLUMN enabled INTEGER NOT NULL DEFAULT 0;
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
DELETE FROM project_port_forwards
WHERE NOT EXISTS (SELECT 1 FROM port_forwards f WHERE f.id = project_port_forwards.port_forward_id)
   OR NOT EXISTS (SELECT 1 FROM projects p WHERE p.id = project_port_forwards.project_id);
";

const SELECT_FORWARD: &str =
    "SELECT id, context, namespace, service, port_remote, port_local, enabled FROM port_forwards";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database file, creating it if needed, and migrates it to the
    /// current schema. Before an existing older database is migrated, a copy
    /// is written next to it (`kprtfwd.db.v1-backup`); an existing copy is
    /// never overwritten. If the copy fails, the database is not migrated and
    /// the next open tries again.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        // Create the file with owner-only permissions before SQLite does.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;

        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        migrate(&mut conn, Some(path))?;
        Ok(Self { conn })
    }

    /// Reads the database file into memory and migrates the copy. The file
    /// itself is opened read-only and never changed.
    pub fn snapshot(path: &Path) -> Result<Self> {
        let source = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut conn = Connection::open_in_memory()?;
        rusqlite::backup::Backup::new(&source, &mut conn)?.run_to_completion(
            1024,
            Duration::ZERO,
            None,
        )?;
        configure(&conn)?;
        migrate(&mut conn, None)?;
        Ok(Self { conn })
    }

    /// Copies the database at `from` to `to`, which must not exist yet. The
    /// source is opened read-only and not changed; the copy is consistent even
    /// while another process uses the source, and waits up to 5 s for one
    /// that writes it. `to` appears only once the copy is complete: a failed
    /// or interrupted copy leaves nothing there.
    pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
        let source = Connection::open_with_flags(
            from,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.busy_timeout(BUSY_TIMEOUT)?;
        if let Some(dir) = to.parent().filter(|d| !d.as_os_str().is_empty()) {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        if to.exists() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists", to.display()),
            )));
        }
        write_backup(&source, to)
    }

    /// An empty in-memory database with the current schema.
    pub fn in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&mut conn, None)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        user_version(&self.conn)
    }

    /// All forwards, sorted by context, namespace, service and port.
    pub fn forwards(&self) -> Result<Vec<Forward>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_FORWARD} ORDER BY context, namespace, service, port_remote, id"
        ))?;
        let rows = stmt.query_map([], ForwardRow::read)?;
        rows.map(|row| row?.into_forward()).collect()
    }

    pub fn forward(&self, id: &ForwardId) -> Result<Option<Forward>> {
        self.conn
            .query_row(
                &format!("{SELECT_FORWARD} WHERE id = ?1"),
                [id.as_str()],
                ForwardRow::read,
            )
            .optional()?
            .map(ForwardRow::into_forward)
            .transpose()
    }

    /// Adds a new forward; see [`Store::add_forwards`].
    pub fn add_forward(&mut self, forward: &Forward) -> Result<()> {
        self.add_forwards(std::slice::from_ref(forward))
    }

    /// Adds new forwards, all or none: if one is invalid or its ID is taken,
    /// nothing is added. An existing forward is never replaced.
    pub fn add_forwards(&mut self, forwards: &[Forward]) -> Result<()> {
        for forward in forwards {
            validate::validate_forward(forward)?;
        }
        let tx = self.conn.transaction()?;
        for forward in forwards {
            insert_forward(&tx, forward)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Changes the settings of forward `id`. Its enabled flag stays as
    /// stored, whatever a client read before. Returns the forward's ID.
    pub fn edit_forward(
        &mut self,
        id: &ForwardId,
        settings: &ForwardSettings,
    ) -> Result<ForwardId> {
        validate::validate_settings(settings)?;
        let changed = self.conn.execute(
            "UPDATE port_forwards
             SET context = ?2, namespace = ?3, service = ?4, port_remote = ?5, port_local = ?6
             WHERE id = ?1",
            params![
                id.as_str(),
                settings.context,
                settings.namespace,
                settings.service,
                settings.remote_port,
                settings.local_port,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownForward(id.clone()));
        }
        Ok(id.clone())
    }

    /// Deletes the forward and its project memberships. Returns whether it
    /// existed.
    pub fn delete_forward(&mut self, id: &ForwardId) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM port_forwards WHERE id = ?1", [id.as_str()])?;
        Ok(deleted > 0)
    }

    /// Sets the enabled flag of all given forwards. If one ID is unknown,
    /// nothing is changed.
    pub fn set_enabled(&mut self, ids: &[ForwardId], enabled: bool) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut update = tx.prepare("UPDATE port_forwards SET enabled = ?1 WHERE id = ?2")?;
            for id in ids {
                if update.execute(params![enabled, id.as_str()])? == 0 {
                    return Err(StoreError::UnknownForward(id.clone()));
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Enables exactly the given forwards and disables all others. If one ID
    /// is unknown, nothing is changed.
    pub fn set_enabled_exactly(&mut self, ids: &[ForwardId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE port_forwards SET enabled = 0", [])?;
        {
            let mut update = tx.prepare("UPDATE port_forwards SET enabled = 1 WHERE id = ?1")?;
            for id in ids {
                if update.execute([id.as_str()])? == 0 {
                    return Err(StoreError::UnknownForward(id.clone()));
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All projects with their forward IDs, sorted by name. Members whose
    /// forward is gone are left out: v1 can write them into a migrated file,
    /// as it never enables foreign keys.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.name, m.port_forward_id
             FROM projects p
             LEFT JOIN project_port_forwards m ON m.project_id = p.id
                 AND EXISTS (SELECT 1 FROM port_forwards f WHERE f.id = m.port_forward_id)
             ORDER BY p.name, m.port_forward_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut projects: Vec<Project> = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let member: Option<String> = row.get(1)?;
            match projects.last_mut() {
                Some(project) if project.name == name => {
                    project.forwards.extend(member.map(ForwardId::new));
                }
                _ => projects.push(Project {
                    name,
                    forwards: member.map(ForwardId::new).into_iter().collect(),
                }),
            }
        }
        Ok(projects)
    }

    /// Creates the project or replaces its member list. If one forward is
    /// unknown, nothing is changed.
    pub fn upsert_project(&mut self, project: &Project) -> Result<()> {
        validate::validate_project(project)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO projects (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            [&project.name],
        )?;
        let project_id: i64 = tx.query_row(
            "SELECT id FROM projects WHERE name = ?1",
            [&project.name],
            |row| row.get(0),
        )?;
        tx.execute(
            "DELETE FROM project_port_forwards WHERE project_id = ?1",
            [project_id],
        )?;
        {
            let mut exists = tx.prepare("SELECT 1 FROM port_forwards WHERE id = ?1")?;
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO project_port_forwards (project_id, port_forward_id)
                 VALUES (?1, ?2)",
            )?;
            for id in &project.forwards {
                if !exists.exists([id.as_str()])? {
                    return Err(StoreError::UnknownForward(id.clone()));
                }
                insert.execute(params![project_id, id.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes the project; its forwards stay. Returns whether it existed.
    pub fn delete_project(&mut self, name: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM projects WHERE name = ?1", [name])?;
        Ok(deleted > 0)
    }

    /// Whether all forwards are paused.
    pub fn paused(&self) -> Result<bool> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'paused'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.as_deref() == Some("true"))
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES ('paused', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [if paused { "true" } else { "false" }],
        )?;
        Ok(())
    }
}

/// Inserts a forward whose ID must be free. Memberships v1 left for the ID,
/// of a forward deleted meanwhile, are dropped first: a new forward starts
/// in no project.
fn insert_forward(conn: &Connection, forward: &Forward) -> Result<()> {
    let id = forward.id.as_str();
    let mut exists = conn.prepare_cached("SELECT 1 FROM port_forwards WHERE id = ?1")?;
    if exists.exists([id])? {
        return Err(StoreError::ForwardExists(forward.id.clone()));
    }
    conn.execute(
        "DELETE FROM project_port_forwards WHERE port_forward_id = ?1",
        [id],
    )?;
    conn.execute(
        "INSERT INTO port_forwards
             (id, context, namespace, service, port_remote, port_local, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            forward.context,
            forward.namespace,
            forward.service,
            forward.remote_port,
            forward.local_port,
            forward.enabled,
        ],
    )?;
    Ok(())
}

/// A forward as stored; ports are converted and checked afterwards so the
/// error can name the forward.
struct ForwardRow {
    id: ForwardId,
    context: String,
    namespace: String,
    service: String,
    remote_port: i64,
    local_port: i64,
    enabled: bool,
}

impl ForwardRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: ForwardId::new(row.get::<_, String>(0)?),
            context: row.get(1)?,
            namespace: row.get(2)?,
            service: row.get(3)?,
            remote_port: row.get(4)?,
            local_port: row.get(5)?,
            enabled: row.get(6)?,
        })
    }

    fn into_forward(self) -> Result<Forward> {
        let remote_port = port(&self.id, "remote port", self.remote_port)?;
        let local_port = port(&self.id, "local port", self.local_port)?;
        Ok(Forward {
            id: self.id,
            context: self.context,
            namespace: self.namespace,
            service: self.service,
            remote_port,
            local_port,
            enabled: self.enabled,
        })
    }
}

fn port(id: &ForwardId, field: &'static str, value: i64) -> Result<u16> {
    u16::try_from(value)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| StoreError::BadPort {
            id: id.clone(),
            field,
            value,
        })
}

fn configure(conn: &Connection) -> Result<()> {
    // Off by default per connection; without it the cascades are inert.
    conn.pragma_update(None, "foreign_keys", true)?;
    // v1 may hold the file open while v2 migrates it.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Brings the schema to [`SCHEMA_VERSION`] in one transaction. For a file with
/// existing data, a backup copy is written first.
fn migrate(conn: &mut Connection, file: Option<&Path>) -> Result<()> {
    let version = user_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedVersion(version));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if let Some(path) = file
        && table_exists(conn, "port_forwards")?
    {
        write_backup(conn, &backup_path(path, version))?;
    }

    let tx = conn.transaction()?;
    if version < 1 {
        tx.execute_batch(V1_SCHEMA)?;
    }
    if version < 2 {
        tx.execute_batch(V2_MIGRATION)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    Ok(stmt.exists([name])?)
}

/// `kprtfwd.db` at schema 0 or 1 becomes `kprtfwd.db.v1-backup`.
fn backup_path(path: &Path, version: i64) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".v{}-backup", version.max(1)));
    PathBuf::from(name)
}

/// Copies the database of `conn` to `target`, unless `target` exists. The copy
/// is written to `<target>.tmp` and renamed to `target` once it is complete,
/// so a failed or interrupted copy never stands under the final name, and the
/// next call starts over.
fn write_backup(conn: &Connection, target: &Path) -> Result<()> {
    if target.exists() {
        return Ok(());
    }
    let mut temp = target.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let result = write_complete_copy(conn, &temp).and_then(|()| Ok(fs::rename(&temp, target)?));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_complete_copy(conn: &Connection, temp: &Path) -> Result<()> {
    // Left over from an interrupted copy.
    match fs::remove_file(temp) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
        _ => {}
    }
    // Create it owner-only first; SQLite keeps the permissions of an existing file.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    conn.backup(MAIN_DB, temp, None::<fn(Progress)>)?;
    file.sync_all()?;
    Ok(())
}
