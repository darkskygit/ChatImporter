use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use ibackuptool2::{BackupError, BackupFile, BackupFileResolver};
use rusqlite::Connection;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImazingArchive {
    pub versions_db: PathBuf,
    device_versions_dir: PathBuf,
    current_backup_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImazingVersionCandidate {
    pub source_id: String,
    pub version_id: i64,
    pub snapshot_dir: PathBuf,
    declared_fileids: HashSet<String>,
    search_roots: Vec<PathBuf>,
}

impl ImazingArchive {
    pub fn open(versions_db: impl AsRef<Path>) -> Result<Self> {
        let versions_db = versions_db.as_ref().canonicalize()?;
        let device_versions_dir = versions_db
            .parent()
            .ok_or_else(|| anyhow!("Versions.db has no parent directory"))?
            .to_path_buf();
        let current_backup_root = device_versions_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .filter(|path| path.is_dir());
        Ok(Self {
            versions_db,
            device_versions_dir,
            current_backup_root,
        })
    }

    pub fn version_candidates(&self) -> Result<Vec<ImazingVersionCandidate>> {
        let conn = Connection::open(&self.versions_db)
            .with_context(|| format!("failed to open {}", self.versions_db.display()))?;
        let versions = read_versions(&conn, &self.device_versions_dir)?;
        let mut candidates = Vec::new();
        for (index, version) in versions.iter().enumerate() {
            let mut search_roots = Vec::new();
            search_roots.push(version.snapshot_dir.clone());
            for previous in versions[..index].iter().rev() {
                search_roots.push(previous.snapshot_dir.clone());
            }
            if let Some(root) = &self.current_backup_root {
                search_roots.push(root.clone());
            }
            candidates.push(ImazingVersionCandidate {
                source_id: format!("imazing:{}:{}", self.versions_db.display(), version.id),
                version_id: version.id,
                snapshot_dir: version.snapshot_dir.clone(),
                declared_fileids: read_declared_fileids(&conn, version.id)?,
                search_roots,
            });
        }
        Ok(candidates)
    }
}

impl ImazingVersionCandidate {
    pub fn file_resolver(&self) -> Arc<dyn BackupFileResolver> {
        Arc::new(ImazingFileResolver {
            declared_fileids: self.declared_fileids.clone(),
            search_roots: self.search_roots.clone(),
        })
    }
}

#[derive(Debug, Clone)]
struct VersionRow {
    id: i64,
    snapshot_dir: PathBuf,
}

#[derive(Debug)]
struct ImazingFileResolver {
    declared_fileids: HashSet<String>,
    search_roots: Vec<PathBuf>,
}

impl BackupFileResolver for ImazingFileResolver {
    fn resolve(&self, _backup_root: &Path, file: &BackupFile) -> Result<PathBuf, BackupError> {
        if file.fileid.len() < 2 {
            return Err(BackupError::FileNotFound);
        }
        for root in &self.search_roots {
            let path = root.join(&file.fileid[..2]).join(&file.fileid);
            if path.is_file() {
                return Ok(path);
            }
        }
        if self.declared_fileids.contains(&file.fileid) {
            Err(BackupError::VersionedFileMissing)
        } else {
            Err(BackupError::FileNotFound)
        }
    }
}

fn read_versions(conn: &Connection, device_versions_dir: &Path) -> Result<Vec<VersionRow>> {
    let columns = table_columns(conn, "VERSIONS")?;
    let id_column = choose_column(&columns, &["version_id", "id", "z_pk", "pk"])
        .unwrap_or_else(|| "rowid".to_string());
    let snapshot_column = choose_column(
        &columns,
        &[
            "snapshot_dir",
            "snapshot",
            "foldername",
            "folder",
            "name",
            "path",
            "relative_path",
            "relativepath",
        ],
    )
    .ok_or_else(|| anyhow!("VERSIONS table has no supported snapshot path column"))?;
    let sql =
        format!("SELECT {id_column}, {snapshot_column} FROM VERSIONS ORDER BY {id_column} ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let snapshot: String = row.get(1)?;
        let snapshot_dir = resolve_snapshot_dir(device_versions_dir, &snapshot);
        Ok(VersionRow { id, snapshot_dir })
    })?;

    let mut versions = Vec::new();
    for row in rows {
        versions.push(row?);
    }
    Ok(versions)
}

fn read_declared_fileids(conn: &Connection, version_id: i64) -> Result<HashSet<String>> {
    let mut fileids = HashSet::new();
    append_declared_fileids(
        conn,
        "VERSIONS_FILES",
        "FILES",
        &["file_id", "fileid", "file", "zfile", "files_id"],
        version_id,
        &mut fileids,
    )?;
    append_declared_fileids(
        conn,
        "VERSIONS_OTHERFILES",
        "OTHERFILES",
        &[
            "otherfile_id",
            "file_id",
            "fileid",
            "otherfile",
            "zotherfile",
            "otherfiles_id",
        ],
        version_id,
        &mut fileids,
    )?;
    Ok(fileids)
}

fn append_declared_fileids(
    conn: &Connection,
    link_table: &str,
    file_table: &str,
    file_ref_candidates: &[&str],
    version_id: i64,
    fileids: &mut HashSet<String>,
) -> Result<()> {
    if !table_exists(conn, link_table)? {
        return Ok(());
    }
    let link_columns = table_columns(conn, link_table)?;
    if let Some(fileid_column) = choose_column(&link_columns, &["hash"]) {
        let version_column = choose_column(
            &link_columns,
            &["version_id", "versionid", "version", "zversion"],
        )
        .ok_or_else(|| anyhow!("{link_table} has no supported version column"))?;
        let sql = format!("SELECT {fileid_column} FROM {link_table} WHERE {version_column} = ?1");
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map([version_id], |row| row.get::<_, String>(0))? {
            fileids.insert(row?);
        }
        return Ok(());
    }

    if !table_exists(conn, file_table)? {
        return Ok(());
    }
    let file_columns = table_columns(conn, file_table)?;
    let version_column = choose_column(
        &link_columns,
        &["version_id", "versionid", "version", "zversion"],
    )
    .ok_or_else(|| anyhow!("{link_table} has no supported version column"))?;
    let file_ref_column = choose_column(&link_columns, file_ref_candidates)
        .ok_or_else(|| anyhow!("{link_table} has no supported file reference column"))?;
    let file_id_column =
        choose_column(&file_columns, &["id", "z_pk", "pk"]).unwrap_or_else(|| "rowid".to_string());
    let fileid_column = choose_column(
        &file_columns,
        &["fileid", "file_id", "hash", "filename", "digest"],
    )
    .ok_or_else(|| anyhow!("{file_table} has no supported fileid column"))?;
    let sql = format!(
        "SELECT f.{fileid_column}
         FROM {link_table} vf
         JOIN {file_table} f ON f.{file_id_column} = vf.{file_ref_column}
         WHERE vf.{version_column} = ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    for row in stmt.query_map([version_id], |row| row.get::<_, String>(0))? {
        fileids.insert(row?);
    }
    Ok(())
}

fn resolve_snapshot_dir(device_versions_dir: &Path, snapshot: &str) -> PathBuf {
    let snapshot = Path::new(snapshot);
    if snapshot.is_absolute() {
        snapshot.to_path_buf()
    } else {
        device_versions_dir.join(snapshot)
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count == 1)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

fn choose_column(columns: &[String], candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        columns
            .iter()
            .find(|column| column.eq_ignore_ascii_case(candidate))
            .cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::source::open_backup_with_password_manager;
    use crate::backup::PasswordManager;
    use plist::{Dictionary, Value};
    use rusqlite::Connection;
    use std::fs;

    fn write_plist(path: impl AsRef<Path>, dict: Dictionary) {
        plist::to_file_xml(path, &Value::Dictionary(dict)).unwrap();
    }

    fn write_minimal_backup_metadata(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let mut status = Dictionary::new();
        status.insert("BackupState".into(), "new".into());
        status.insert("Date".into(), "2026-05-25".into());
        status.insert("IsFullBackup".into(), true.into());
        status.insert("SnapshotState".into(), "finished".into());
        status.insert("UUID".into(), "backup-uuid".into());
        status.insert("Version".into(), "2.4".into());
        write_plist(dir.join("Status.plist"), status);

        let mut info = Dictionary::new();
        info.insert("Product Type".into(), "iPhone".into());
        info.insert("Product Version".into(), "18.0".into());
        info.insert("Target Identifier".into(), "device-id".into());
        info.insert("Target Type".into(), "Device".into());
        write_plist(dir.join("Info.plist"), info);

        let mut lockdown = Dictionary::new();
        lockdown.insert("ProductVersion".into(), "18.0".into());
        lockdown.insert("ProductType".into(), "iPhone".into());
        lockdown.insert("UniqueDeviceID".into(), "device-id".into());
        lockdown.insert("SerialNumber".into(), "serial".into());
        lockdown.insert("DeviceName".into(), "Test Phone".into());
        let mut manifest = Dictionary::new();
        manifest.insert("IsEncrypted".into(), false.into());
        manifest.insert("Version".into(), "9.1".into());
        manifest.insert("Date".into(), "2026-05-25".into());
        manifest.insert("SystemDomainsVersion".into(), "20".into());
        manifest.insert("WasPasscodeSet".into(), false.into());
        manifest.insert("Lockdown".into(), Value::Dictionary(lockdown));
        write_plist(dir.join("Manifest.plist"), manifest);
    }

    fn write_manifest_db(dir: &Path, fileid: &str) {
        let conn = Connection::open(dir.join("Manifest.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE Files (
                fileid TEXT NOT NULL,
                domain TEXT NOT NULL,
                relativePath TEXT NOT NULL,
                flags INTEGER NOT NULL,
                file BLOB NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Files VALUES (?1, 'HomeDomain', 'Library/Test', 1, X'00')",
            [fileid],
        )
        .unwrap();
    }

    fn write_versions_db(path: &Path, rows: &[(i64, &str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE VERSIONS (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE FILES (id INTEGER PRIMARY KEY, fileid TEXT NOT NULL);
             CREATE TABLE VERSIONS_FILES (version_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
             CREATE TABLE OTHERFILES (id INTEGER PRIMARY KEY, fileid TEXT NOT NULL);
             CREATE TABLE VERSIONS_OTHERFILES (version_id INTEGER NOT NULL, otherfile_id INTEGER NOT NULL);",
        )
        .unwrap();
        for (id, snapshot, fileid) in rows {
            conn.execute(
                "INSERT INTO VERSIONS VALUES (?1, ?2)",
                rusqlite::params![id, snapshot],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO FILES VALUES (?1, ?2)",
                rusqlite::params![id, fileid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO VERSIONS_FILES VALUES (?1, ?2)",
                rusqlite::params![id, id],
            )
            .unwrap();
        }
    }

    fn write_versions_db_with_otherfile(
        path: &Path,
        version_id: i64,
        snapshot: &str,
        fileid: &str,
    ) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE VERSIONS (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE FILES (id INTEGER PRIMARY KEY, fileid TEXT NOT NULL);
             CREATE TABLE VERSIONS_FILES (version_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
             CREATE TABLE OTHERFILES (id INTEGER PRIMARY KEY, fileid TEXT NOT NULL);
             CREATE TABLE VERSIONS_OTHERFILES (version_id INTEGER NOT NULL, otherfile_id INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VERSIONS VALUES (?1, ?2)",
            rusqlite::params![version_id, snapshot],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO OTHERFILES VALUES (?1, ?2)",
            rusqlite::params![version_id, fileid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VERSIONS_OTHERFILES VALUES (?1, ?2)",
            rusqlite::params![version_id, version_id],
        )
        .unwrap();
    }

    fn write_versions_db_with_rowid_schema(path: &Path, snapshot: &str, fileid: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE VERSIONS (
                iOSVersion TEXT NOT NULL,
                DeviceName TEXT NOT NULL,
                FolderName TEXT NOT NULL,
                Timestamp INTEGER NOT NULL UNIQUE,
                IsEncrypted INTEGER NOT NULL,
                DeleteStatus INTEGER NOT NULL,
                BackupReason INTEGER NOT NULL,
                OriginalSize INTEGER
            );
            CREATE TABLE FILES (
                VersionID INTEGER NOT NULL,
                Filename TEXT NOT NULL,
                Size INTEGER NOT NULL,
                Modified INTEGER NOT NULL,
                Key BLOB NOT NULL
            );
            CREATE TABLE VERSIONS_FILES (
                VersionID INTEGER NOT NULL,
                FileID INTEGER NOT NULL
            );
            CREATE TABLE OTHERFILES (
                VersionID INTEGER NOT NULL,
                Filename TEXT NOT NULL,
                Size INTEGER NOT NULL,
                Modified INTEGER NOT NULL,
                Tag INTEGER NOT NULL,
                Digest TEXT
            );
            CREATE TABLE VERSIONS_OTHERFILES (
                VersionID INTEGER NOT NULL,
                FileID INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VERSIONS VALUES ('15.4.1', 'iPad', ?1, 1652638407, 1, 0, 0, 42)",
            [snapshot],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO FILES VALUES (1, ?1, 137, 1652637205, X'00')",
            [fileid],
        )
        .unwrap();
        conn.execute("INSERT INTO VERSIONS_FILES VALUES (1, 1)", [])
            .unwrap();
    }

    fn write_object(root: &Path, fileid: &str, bytes: &[u8]) {
        let path = root.join(&fileid[..2]).join(fileid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn version_candidates_are_listed_from_versions_db() {
        let dir = tempfile::tempdir().unwrap();
        let device_dir = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        fs::create_dir_all(&device_dir).unwrap();
        let versions_db = device_dir.join("Versions.db");
        write_versions_db(
            &versions_db,
            &[(1, "snapshot-a", "aabbcc"), (2, "snapshot-b", "ddeeff")],
        );
        fs::create_dir_all(device_dir.join("snapshot-a")).unwrap();
        fs::create_dir_all(device_dir.join("snapshot-b")).unwrap();

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let candidates = archive.version_candidates().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].version_id, 1);
        assert_eq!(
            candidates[1].snapshot_dir,
            device_dir.join("snapshot-b").canonicalize().unwrap()
        );
    }

    #[test]
    fn version_candidates_support_imazing_rowid_foldername_schema() {
        let dir = tempfile::tempdir().unwrap();
        let device_dir = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        fs::create_dir_all(&device_dir).unwrap();
        let versions_db = device_dir.join("Versions.db");
        let fileid = "24769d14db845182625d6c034f54e90e66137b93";
        write_versions_db_with_rowid_schema(&versions_db, "2022-05-16-02.13.27", fileid);
        fs::create_dir_all(device_dir.join("2022-05-16-02.13.27")).unwrap();

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let candidates = archive.version_candidates().unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].version_id, 1);
        assert_eq!(
            candidates[0].snapshot_dir,
            device_dir
                .join("2022-05-16-02.13.27")
                .canonicalize()
                .unwrap()
        );
        assert!(candidates[0].declared_fileids.contains(fileid));
    }

    #[test]
    fn versioned_snapshot_reuses_backup_flow_and_falls_back_to_previous_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let device_dir = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        let previous = device_dir.join("snapshot-a");
        let target = device_dir.join("snapshot-b");
        let versions_db = device_dir.join("Versions.db");
        let fileid = "aabbccddeeff00112233445566778899aabbccdd";
        write_minimal_backup_metadata(&target);
        write_manifest_db(&target, fileid);
        write_minimal_backup_metadata(&previous);
        write_versions_db(
            &versions_db,
            &[(1, "snapshot-a", fileid), (2, "snapshot-b", fileid)],
        );
        write_object(&previous, fileid, b"from previous snapshot");

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let target_candidate = archive.version_candidates().unwrap().pop().unwrap();
        let mut backup = open_backup_with_password_manager(
            &target_candidate.snapshot_dir,
            target_candidate.file_resolver(),
            &mut PasswordManager::new(),
        )
        .unwrap()
        .unwrap_unlocked();
        backup.parse_manifest().unwrap();

        assert_eq!(backup.files.len(), 1);
        assert_eq!(
            backup.read_file(&backup.files[0]).unwrap(),
            b"from previous snapshot"
        );
    }

    #[test]
    fn missing_declared_versioned_file_returns_specific_error() {
        let dir = tempfile::tempdir().unwrap();
        let device_dir = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        let target = device_dir.join("snapshot");
        let versions_db = device_dir.join("Versions.db");
        let fileid = "aabbccddeeff00112233445566778899aabbccdd";
        write_minimal_backup_metadata(&target);
        write_manifest_db(&target, fileid);
        write_versions_db(&versions_db, &[(1, "snapshot", fileid)]);

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let target_candidate = archive.version_candidates().unwrap().pop().unwrap();
        let mut backup = open_backup_with_password_manager(
            &target_candidate.snapshot_dir,
            target_candidate.file_resolver(),
            &mut PasswordManager::new(),
        )
        .unwrap()
        .unwrap_unlocked();
        backup.parse_manifest().unwrap();

        let error = backup.read_file(&backup.files[0]).unwrap_err();
        assert_eq!(error.to_string(), "VersionedFileMissing");
    }

    #[test]
    fn versioned_snapshot_falls_back_to_current_root() {
        let dir = tempfile::tempdir().unwrap();
        let current_root = dir.path();
        let device_dir = current_root
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        let target = device_dir.join("snapshot");
        let versions_db = device_dir.join("Versions.db");
        let fileid = "aabbccddeeff00112233445566778899aabbccdd";
        write_minimal_backup_metadata(&target);
        write_manifest_db(&target, fileid);
        write_versions_db(&versions_db, &[(1, "snapshot", fileid)]);
        write_object(current_root, fileid, b"from current root");

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let target_candidate = archive.version_candidates().unwrap().pop().unwrap();
        let mut backup = open_backup_with_password_manager(
            &target_candidate.snapshot_dir,
            target_candidate.file_resolver(),
            &mut PasswordManager::new(),
        )
        .unwrap()
        .unwrap_unlocked();
        backup.parse_manifest().unwrap();

        assert_eq!(
            backup.read_file(&backup.files[0]).unwrap(),
            b"from current root"
        );
    }

    #[test]
    fn versions_otherfiles_declared_missing_file_returns_specific_error() {
        let dir = tempfile::tempdir().unwrap();
        let device_dir = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device");
        let target = device_dir.join("snapshot");
        let versions_db = device_dir.join("Versions.db");
        let fileid = "aabbccddeeff00112233445566778899aabbccdd";
        write_minimal_backup_metadata(&target);
        write_manifest_db(&target, fileid);
        write_versions_db_with_otherfile(&versions_db, 1, "snapshot", fileid);

        let archive = ImazingArchive::open(&versions_db).unwrap();
        let target_candidate = archive.version_candidates().unwrap().pop().unwrap();
        let mut backup = open_backup_with_password_manager(
            &target_candidate.snapshot_dir,
            target_candidate.file_resolver(),
            &mut PasswordManager::new(),
        )
        .unwrap()
        .unwrap_unlocked();
        backup.parse_manifest().unwrap();

        let error = backup.read_file(&backup.files[0]).unwrap_err();
        assert_eq!(error.to_string(), "VersionedFileMissing");
    }

    trait UnlockDecisionExt {
        fn unwrap_unlocked(self) -> ibackuptool2::Backup;
    }

    impl UnlockDecisionExt for crate::backup::source::UnlockDecision {
        fn unwrap_unlocked(self) -> ibackuptool2::Backup {
            match self {
                Self::Unlocked(backup) => *backup,
                Self::Skip => panic!("expected unlocked backup"),
            }
        }
    }
}
