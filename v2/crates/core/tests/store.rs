use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kprtfwd_core::store::SCHEMA_VERSION;
use kprtfwd_core::{Forward, ForwardId, Project, Store, StoreError, ValidationError};
use rusqlite::Connection;

const V1_FIXTURE: &str = include_str!("fixtures/v1.sql");

/// Writes the v1 fixture to `<dir>/kprtfwd.db`, in WAL mode like v1.
fn v1_database(dir: &Path) -> PathBuf {
    let path = dir.join("kprtfwd.db");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        .unwrap();
    conn.execute_batch(V1_FIXTURE).unwrap();
    path
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

/// The rollback path: v1 must keep working on a migrated database. These are
/// the statements v1's store uses.
#[test]
fn v1_statements_work_after_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = v1_database(dir.path());
    drop(Store::open(&path).unwrap());

    let v1 = Connection::open(&path).unwrap();
    v1.pragma_update(None, "foreign_keys", true).unwrap();
    v1.execute(
        "INSERT INTO port_forwards (id, context, namespace, service, port_remote, port_local)
         VALUES (?, ?, ?, ?, ?, ?)",
        (
            "staging.shop.api.api-80",
            "staging",
            "shop",
            "api",
            80,
            18091,
        ),
    )
    .unwrap();
    v1.execute(
        "UPDATE port_forwards SET context = ?, namespace = ?, service = ?, port_remote = ?, port_local = ? WHERE id = ?",
        ("staging", "shop", "api", 80, 18092, "staging.shop.api.api-80"),
    )
    .unwrap();
    let listed: Vec<String> = v1
        .prepare("SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards ORDER BY context, namespace, service")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(listed.len(), 5);
    v1.execute("INSERT INTO projects (name) VALUES (?)", ["api"])
        .unwrap();
    v1.execute(
        "INSERT INTO project_port_forwards (project_id, port_forward_id) VALUES (?, ?)",
        (3, "staging.shop.api.api-80"),
    )
    .unwrap();
    v1.execute(
        "DELETE FROM port_forwards WHERE id = ?",
        ["orbstack.cache.redis.redis-6379"],
    )
    .unwrap();
    drop(v1);

    let store = Store::open(&path).unwrap();
    let api = store
        .forward(&id("staging.shop.api.api-80"))
        .unwrap()
        .unwrap();
    assert_eq!(api.local_port, 18092);
    assert!(!api.enabled, "rows inserted by v1 get the default");
    assert_eq!(store.forwards().unwrap().len(), 4);
    let api_project = store
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "api")
        .unwrap();
    assert_eq!(api_project.forwards, vec![id("staging.shop.api.api-80")]);
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
