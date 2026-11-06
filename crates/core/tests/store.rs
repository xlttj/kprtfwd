use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use kprtfwd_core::store::SCHEMA_VERSION;
use kprtfwd_core::{Forward, ForwardId, Project, Store, StoreError, ValidationError};
use rusqlite::Connection;

const V1_FIXTURE: &str = include_str!("fixtures/v1.sql");

/// Writes the v1 fixture to `<dir>/kprtfwd.db` through [`v1_connection`], so
/// the file has SQLite's default rollback journal like a v1 file.
fn v1_database(dir: &Path) -> PathBuf {
    let path = dir.join("kprtfwd.db");
    v1_connection(&path).execute_batch(V1_FIXTURE).unwrap();
    path
}

/// A connection as released v1 (v1.3.0) opens one: `sql.Open("sqlite", path)`
/// without pragmas, so foreign keys are off and the journal mode is SQLite's
/// default. The bundled SQLite turns foreign keys on by default, hence the
/// pragma.
fn v1_connection(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn
}

// The statements of released v1's store (v1.3.0, pkg/config/sqlite_store.go).
// v1 has no UPDATE: it edits a forward by deleting and adding it again, and a
// project by deleting and creating it again.

/// v1's `Add`.
fn v1_add(v1: &Connection, f: &Forward) {
    v1.execute(
        "INSERT INTO port_forwards (id, context, namespace, service, port_remote, port_local)
         VALUES (?, ?, ?, ?, ?, ?)",
        (
            f.id.as_str(),
            &f.context,
            &f.namespace,
            &f.service,
            f.remote_port,
            f.local_port,
        ),
    )
    .unwrap();
}

/// v1's `DeletePortForward`, which removes the memberships itself.
fn v1_delete_forward(v1: &mut Connection, id: &str) {
    let tx = v1.transaction().unwrap();
    tx.execute(
        "DELETE FROM project_port_forwards WHERE port_forward_id = ?",
        [id],
    )
    .unwrap();
    tx.execute("DELETE FROM port_forwards WHERE id = ?", [id])
        .unwrap();
    tx.commit().unwrap();
}

/// v1's `CreateProject`, which inserts the members without checking them.
fn v1_create_project(v1: &mut Connection, name: &str, members: &[&str]) {
    let tx = v1.transaction().unwrap();
    tx.execute("INSERT INTO projects (name) VALUES (?)", [name])
        .unwrap();
    let project_id = tx.last_insert_rowid();
    for member in members {
        tx.execute(
            "INSERT INTO project_port_forwards (project_id, port_forward_id) VALUES (?, ?)",
            (project_id, member),
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

/// v1's `DeleteProject`; without foreign keys, its memberships stay.
fn v1_delete_project(v1: &Connection, name: &str) {
    v1.execute("DELETE FROM projects WHERE name = ?", [name])
        .unwrap();
}

/// The IDs v1's `GetAll` returns.
fn v1_forward_ids(v1: &Connection) -> Vec<String> {
    v1.prepare(
        "SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards
         ORDER BY context, namespace, service",
    )
    .unwrap()
    .query_map([], |row| row.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// What v1's `GetProjects` returns: names with their member IDs.
fn v1_projects(v1: &Connection) -> Vec<(String, Vec<String>)> {
    let projects: Vec<(i64, String)> = v1
        .prepare("SELECT id, name FROM projects ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut members = v1
        .prepare("SELECT port_forward_id FROM project_port_forwards WHERE project_id = ?")
        .unwrap();
    projects
        .into_iter()
        .map(|(project_id, name)| {
            let ids = members
                .query_map([project_id], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (name, ids)
        })
        .collect()
}

fn id(id: &str) -> ForwardId {
    ForwardId::new(id)
}

fn forward(id_: &str, context: &str, namespace: &str, service: &str, ports: (u16, u16)) -> Forward {
    Forward {
        id: id(id_),
        context: context.into(),
        namespace: namespace.into(),
        service: service.into(),
        remote_port: ports.0,
        local_port: ports.1,
        enabled: false,
    }
}

/// The fixture's forwards in the store's order: context, namespace, service, port.
fn fixture_forwards() -> Vec<Forward> {
    vec![
        forward(
            "orbstack.cache.redis.redis-6379",
            "orbstack",
            "cache",
            "redis",
            (6379, 16379),
        ),
        forward(
            "production.shop.frontend.frontend-80",
            "production",
            "shop",
            "frontend",
            (80, 28090),
        ),
        forward(
            "staging.shop.frontend.frontend-80",
            "staging",
            "shop",
            "frontend",
            (80, 18090),
        ),
        forward(
            "staging.mysql.shop",
            "staging",
            "shop",
            "mysql",
            (3306, 13306),
        ),
    ]
}

fn fixture_projects() -> Vec<Project> {
    vec![
        Project {
            name: "empty".into(),
            forwards: vec![],
        },
        Project {
            name: "shop".into(),
            forwards: vec![
                id("staging.mysql.shop"),
                id("staging.shop.frontend.frontend-80"),
            ],
        },
    ]
}

/// Where a copy to `path` is written before it is renamed to `path`.
fn temp(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// A file SQLite refuses to read, so every copy of it fails.
fn not_a_database(path: &Path) {
    fs::write(path, [0x55; 4096]).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn raw_user_version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn migrates_v1_database_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());

    let store = Store::open(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.forwards().unwrap(), fixture_forwards());
    assert_eq!(store.projects().unwrap(), fixture_projects());
    assert!(!store.paused().unwrap());
}

#[test]
fn writes_backup_before_migrating_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    let backup = dir.path().join("kprtfwd.db.v1-backup");

    drop(Store::open(&path).unwrap());

    assert_eq!(mode(&backup), 0o600);
    assert_eq!(
        raw_user_version(&backup),
        0,
        "backup holds the unmigrated file"
    );
    let rows: i64 = Connection::open(&backup)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM port_forwards", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 4);

    // Already migrated: opening again must not write a new backup.
    fs::remove_file(&backup).unwrap();
    drop(Store::open(&path).unwrap());
    assert!(!backup.exists());
}

/// A backup that cannot be written stops the migration, and the next open
/// writes it and migrates.
#[test]
fn failed_backup_stops_the_migration_until_it_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    let backup = dir.path().join("kprtfwd.db.v1-backup");
    // A directory where the copy is written makes it fail.
    fs::create_dir(temp(&backup)).unwrap();

    assert!(Store::open(&path).is_err());
    assert!(!backup.exists());
    assert_eq!(raw_user_version(&path), 0, "migrated without a backup");

    fs::remove_dir(temp(&backup)).unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.forwards().unwrap(), fixture_forwards());
    assert_eq!(raw_user_version(&backup), 0);
    assert_eq!(mode(&backup), 0o600);
    assert!(!temp(&backup).exists());
}

/// The rollback path: released v1 must keep working on a migrated database,
/// with its own statements and foreign keys off.
#[test]
fn v1_statements_work_after_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    drop(Store::open(&path).unwrap());

    let mut v1 = v1_connection(&path);
    let mut api = forward(
        "staging.shop.api.api-80",
        "staging",
        "shop",
        "api",
        (80, 18091),
    );
    v1_add(&v1, &api);
    // v1 edits the local port by deleting the forward and adding it again.
    v1_delete_forward(&mut v1, api.id.as_str());
    api.local_port = 18092;
    v1_add(&v1, &api);
    assert_eq!(v1_forward_ids(&v1).len(), 5);
    v1_create_project(&mut v1, "api", &["staging.shop.api.api-80"]);
    // v1 edits a project by deleting it and creating it again.
    v1_delete_project(&v1, "shop");
    v1_create_project(&mut v1, "shop", &["staging.mysql.shop"]);
    v1_delete_forward(&mut v1, "orbstack.cache.redis.redis-6379");
    assert_eq!(
        v1_projects(&v1),
        [
            ("api".to_owned(), vec!["staging.shop.api.api-80".to_owned()]),
            ("empty".to_owned(), vec![]),
            ("shop".to_owned(), vec!["staging.mysql.shop".to_owned()]),
        ]
    );
    drop(v1);

    let store = Store::open(&path).unwrap();
    let stored = store.forward(&api.id).unwrap().unwrap();
    assert_eq!(stored.local_port, 18092);
    assert!(!stored.enabled, "rows inserted by v1 get the default");
    assert_eq!(store.forwards().unwrap().len(), 4);
    assert_eq!(
        store.projects().unwrap(),
        [
            Project {
                name: "api".into(),
                forwards: vec![api.id.clone()],
            },
            Project {
                name: "empty".into(),
                forwards: vec![],
            },
            Project {
                name: "shop".into(),
                forwards: vec![id("staging.mysql.shop")],
            },
        ]
    );
}

#[test]
fn snapshot_leaves_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    let before = fs::read(&path).unwrap();

    let store = Store::snapshot(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.forwards().unwrap(), fixture_forwards());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(raw_user_version(&path), 0);
    assert!(!dir.path().join("kprtfwd.db.v1-backup").exists());
}

#[test]
fn copies_a_database_without_changing_it() {
    let dir = tempfile::tempdir().unwrap();
    let from = v1_database(dir.path());
    let before = fs::read(&from).unwrap();
    let to = dir.path().join(".config/kprtfwd/kprtfwd.db");

    Store::copy_file(&from, &to).unwrap();

    assert_eq!(fs::read(&from).unwrap(), before);
    assert_eq!(mode(&to), 0o600);
    assert_eq!(mode(to.parent().unwrap()), 0o700);
    // The copy is migrated where it is opened; the original stays at v1.
    let store = Store::open(&to).unwrap();
    assert_eq!(store.forwards().unwrap(), fixture_forwards());
    assert_eq!(raw_user_version(&from), 0);

    // An existing database is never overwritten.
    assert!(Store::copy_file(&from, &to).is_err());
}

/// The take-over checks whether the target exists; after a failed copy it
/// must not, so the next start copies again.
#[test]
fn failed_copy_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.db");
    not_a_database(&broken);
    let to = dir.path().join(".config/kprtfwd/kprtfwd.db");

    assert!(Store::copy_file(&broken, &to).is_err());
    assert!(!to.exists());
    assert!(!temp(&to).exists());

    let from = v1_database(dir.path());
    Store::copy_file(&from, &to).unwrap();
    assert_eq!(
        Store::snapshot(&to).unwrap().forwards().unwrap(),
        fixture_forwards()
    );
}

#[test]
fn copy_replaces_what_an_interrupted_copy_left() {
    let dir = tempfile::tempdir().unwrap();
    let from = v1_database(dir.path());
    let to = dir.path().join(".config/kprtfwd/kprtfwd.db");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    not_a_database(&temp(&to));

    Store::copy_file(&from, &to).unwrap();

    assert!(!temp(&to).exists());
    assert_eq!(mode(&to), 0o600);
    assert_eq!(
        Store::snapshot(&to).unwrap().forwards().unwrap(),
        fixture_forwards()
    );
}

/// v1 may be writing its file during the take-over: the copy waits for the
/// write and includes it.
#[test]
fn copy_waits_for_a_v1_write() {
    let dir = tempfile::tempdir().unwrap();
    let from = v1_database(dir.path());
    let to = dir.path().join(".config/kprtfwd/kprtfwd.db");
    let api = forward("staging.shop.api-80", "staging", "shop", "api", (80, 18091));
    // In the rollback journal, an exclusive lock keeps readers out.
    let v1 = v1_connection(&from);
    v1.execute_batch("BEGIN EXCLUSIVE").unwrap();
    v1_add(&v1, &api);
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        v1.execute_batch("COMMIT").unwrap();
    });

    let copied = Store::copy_file(&from, &to);

    writer.join().unwrap();
    copied.unwrap();
    let forwards = Store::snapshot(&to).unwrap().forwards().unwrap();
    assert_eq!(forwards.len(), 5);
    assert!(forwards.contains(&api));
}

#[test]
fn creates_new_database_with_private_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join(".kprtfwd");
    let path = data_dir.join("kprtfwd.db");

    let store = Store::open(&path).unwrap();

    assert_eq!(mode(&data_dir), 0o700);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(store.forwards().unwrap().is_empty());
    assert!(
        !data_dir.join("kprtfwd.db.v1-backup").exists(),
        "nothing to back up"
    );
}

#[test]
fn rejects_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kprtfwd.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 3)
        .unwrap();

    assert!(matches!(
        Store::open(&path),
        Err(StoreError::UnsupportedVersion(3))
    ));
    assert!(matches!(
        Store::snapshot(&path),
        Err(StoreError::UnsupportedVersion(3))
    ));
}

#[test]
fn reports_ports_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE port_forwards SET port_local = 70000 WHERE id = 'staging.mysql.shop'",
            [],
        )
        .unwrap();

    let store = Store::snapshot(&path).unwrap();

    match store.forwards() {
        Err(StoreError::BadPort { id, field, value }) => {
            assert_eq!(id.as_str(), "staging.mysql.shop");
            assert_eq!(field, "local port");
            assert_eq!(value, 70000);
        }
        other => panic!("expected BadPort, got {other:?}"),
    }
}

#[test]
fn forward_round_trip() {
    let mut store = Store::in_memory().unwrap();
    let mut f = forward("staging.shop.api-80", "staging", "shop", "api", (80, 18091));

    store.upsert_forward(&f).unwrap();
    assert_eq!(store.forward(&f.id).unwrap(), Some(f.clone()));

    f.local_port = 18099;
    store.upsert_forward(&f).unwrap();
    assert_eq!(store.forwards().unwrap(), vec![f.clone()]);

    store
        .set_enabled(std::slice::from_ref(&f.id), true)
        .unwrap();
    assert!(store.forward(&f.id).unwrap().unwrap().enabled);

    assert!(store.delete_forward(&f.id).unwrap());
    assert!(!store.delete_forward(&f.id).unwrap());
    assert_eq!(store.forward(&f.id).unwrap(), None);
}

#[test]
fn rejects_invalid_forward() {
    let mut store = Store::in_memory().unwrap();
    let f = forward("x", "staging", "Shop", "api", (80, 18091));

    assert!(matches!(
        store.upsert_forward(&f),
        Err(StoreError::Invalid(ValidationError::InvalidName {
            field: "namespace",
            ..
        }))
    ));
    assert!(store.forwards().unwrap().is_empty());
}

#[test]
fn set_enabled_changes_all_or_nothing() {
    let mut store = Store::in_memory().unwrap();
    let f = forward("a", "staging", "shop", "api", (80, 18091));
    store.upsert_forward(&f).unwrap();

    let result = store.set_enabled(&[id("a"), id("missing")], true);

    assert!(matches!(result, Err(StoreError::UnknownForward(missing)) if missing == id("missing")));
    assert!(!store.forward(&id("a")).unwrap().unwrap().enabled);
}

#[test]
fn set_enabled_exactly_switches_the_whole_set() {
    let mut store = Store::in_memory().unwrap();
    for (i, port) in [("a", 18091), ("b", 18092), ("c", 18093)] {
        store
            .upsert_forward(&forward(i, "staging", "shop", "api", (80, port)))
            .unwrap();
    }
    store.set_enabled(&[id("a"), id("b")], true).unwrap();
    let enabled = |store: &Store| -> Vec<String> {
        store
            .forwards()
            .unwrap()
            .into_iter()
            .filter(|f| f.enabled)
            .map(|f| f.id.to_string())
            .collect()
    };

    store.set_enabled_exactly(&[id("b"), id("c")]).unwrap();
    assert_eq!(enabled(&store), ["b", "c"]);

    assert!(matches!(
        store.set_enabled_exactly(&[id("a"), id("missing")]),
        Err(StoreError::UnknownForward(_))
    ));
    assert_eq!(enabled(&store), ["b", "c"], "unchanged after an unknown ID");
}

#[test]
fn projects_replace_members_and_follow_deletes() {
    let mut store = Store::in_memory().unwrap();
    for (i, port) in [("a", 18091), ("b", 18092)] {
        store
            .upsert_forward(&forward(i, "staging", "shop", "api", (80, port)))
            .unwrap();
    }
    let project = |members: &[&str]| Project {
        name: "shop".into(),
        forwards: members.iter().copied().map(id).collect(),
    };

    store.upsert_project(&project(&["a"])).unwrap();
    store.upsert_project(&project(&["a", "b"])).unwrap();
    assert_eq!(store.projects().unwrap(), vec![project(&["a", "b"])]);

    // An unknown member leaves the project as it was.
    let result = store.upsert_project(&project(&["b", "missing"]));
    assert!(matches!(result, Err(StoreError::UnknownForward(_))));
    assert_eq!(store.projects().unwrap(), vec![project(&["a", "b"])]);

    // Deleting a forward removes it from its projects.
    store.delete_forward(&id("a")).unwrap();
    assert_eq!(store.projects().unwrap(), vec![project(&["b"])]);

    assert!(store.delete_project("shop").unwrap());
    assert!(store.projects().unwrap().is_empty());
    assert!(
        store.forward(&id("b")).unwrap().is_some(),
        "forwards outlive their project"
    );
}

#[test]
fn paused_round_trip() {
    let mut store = Store::in_memory().unwrap();
    assert!(!store.paused().unwrap());
    store.set_paused(true).unwrap();
    assert!(store.paused().unwrap());
    store.set_paused(false).unwrap();
    assert!(!store.paused().unwrap());
}
