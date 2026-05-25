pub mod discovery;
pub mod imazing;
pub mod password;
pub mod session;
pub mod source;

pub use discovery::BackupCandidateKind;
pub use imazing::ImazingArchive;
pub use password::PasswordManager;
pub use session::{CandidateDisposition, ImportSession};
pub use source::{
    open_backup_with_password_manager, open_standard_backup_with_password_manager, UnlockDecision,
};

#[cfg(test)]
mod tests {
    use super::{discovery::is_standard_backup_root, source::open_backup, *};
    use crate::backup::discovery::discover_backup_candidates;
    use plist::{Dictionary, Value};
    use rusqlite::Connection;
    use std::fs;

    fn write_plist(path: impl AsRef<std::path::Path>, dict: Dictionary) {
        plist::to_file_xml(path, &Value::Dictionary(dict)).unwrap();
    }

    fn minimal_backup_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        let mut status = Dictionary::new();
        status.insert("BackupState".into(), "new".into());
        status.insert("Date".into(), "2026-05-25".into());
        status.insert("IsFullBackup".into(), true.into());
        status.insert("SnapshotState".into(), "finished".into());
        status.insert("UUID".into(), "backup-uuid".into());
        status.insert("Version".into(), "2.4".into());
        write_plist(dir.path().join("Status.plist"), status);

        let mut info = Dictionary::new();
        info.insert("Product Type".into(), "iPhone".into());
        info.insert("Product Version".into(), "18.0".into());
        info.insert("Target Identifier".into(), "device-id".into());
        info.insert("Target Type".into(), "Device".into());
        write_plist(dir.path().join("Info.plist"), info);

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
        write_plist(dir.path().join("Manifest.plist"), manifest);

        let conn = Connection::open(dir.path().join("Manifest.db")).unwrap();
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
        drop(conn);

        dir
    }

    fn minimal_encrypted_backup_root_without_keybag() -> tempfile::TempDir {
        let dir = minimal_backup_root();
        write_encrypted_manifest(dir.path(), None);
        dir
    }

    fn minimal_encrypted_backup_root_with_malformed_keybag() -> tempfile::TempDir {
        let dir = minimal_backup_root();
        write_encrypted_manifest(dir.path(), Some(vec![0xff; 8]));
        dir
    }

    fn write_encrypted_manifest(path: &std::path::Path, keybag: Option<Vec<u8>>) {
        let mut lockdown = Dictionary::new();
        lockdown.insert("ProductVersion".into(), "18.0".into());
        lockdown.insert("ProductType".into(), "iPhone".into());
        lockdown.insert("UniqueDeviceID".into(), "device-id".into());
        lockdown.insert("SerialNumber".into(), "serial".into());
        lockdown.insert("DeviceName".into(), "Test Phone".into());

        let mut manifest = Dictionary::new();
        manifest.insert("IsEncrypted".into(), true.into());
        manifest.insert("Version".into(), "9.1".into());
        manifest.insert("Date".into(), "2026-05-25".into());
        manifest.insert("SystemDomainsVersion".into(), "20".into());
        manifest.insert("WasPasscodeSet".into(), false.into());
        manifest.insert("Lockdown".into(), Value::Dictionary(lockdown));
        if let Some(keybag) = keybag {
            manifest.insert("BackupKeyBag".into(), Value::Data(keybag));
        }
        write_plist(path.join("Manifest.plist"), manifest);
    }

    #[test]
    fn standard_backup_root_can_be_detected_and_opened() {
        let dir = minimal_backup_root();
        assert!(is_standard_backup_root(dir.path()));

        let backup = open_backup(dir.path()).unwrap();
        assert_eq!(backup.files.len(), 0);
        assert_eq!(
            backup.manifest.lockdown.unique_device_id,
            "device-id".to_string()
        );
    }

    #[test]
    fn missing_manifest_files_is_not_standard_backup_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Info.plist"), b"").unwrap();
        assert!(!is_standard_backup_root(dir.path()));
    }

    #[test]
    fn discovery_finds_standard_roots_from_parent_and_child_paths() {
        let parent = tempfile::tempdir().unwrap();
        let backup = minimal_backup_root();
        let backup_path = parent.path().join("device-backup");
        fs::rename(backup.path(), &backup_path).unwrap();
        fs::create_dir_all(backup_path.join("subdir")).unwrap();

        let from_parent = discover_backup_candidates(&[parent.path().to_path_buf()]).unwrap();
        assert_eq!(from_parent.len(), 1);
        assert!(matches!(
            from_parent[0].kind,
            BackupCandidateKind::StandardRoot(_)
        ));

        let from_child = discover_backup_candidates(&[backup_path.join("subdir")]).unwrap();
        assert_eq!(from_child.len(), 1);
        assert_eq!(
            from_child[0].display_path,
            backup_path.canonicalize().unwrap()
        );
    }

    #[test]
    fn discovery_deduplicates_standard_roots_by_canonical_path() {
        let backup = minimal_backup_root();
        let child = backup.path().join("nested");
        fs::create_dir_all(&child).unwrap();

        let candidates = discover_backup_candidates(&[backup.path().to_path_buf(), child]).unwrap();
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn discovery_lists_imazing_versions_db_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let versions_db = dir
            .path()
            .join("iMazing.Versions")
            .join("Versions")
            .join("device")
            .join("Versions.db");
        fs::create_dir_all(versions_db.parent().unwrap()).unwrap();
        fs::write(&versions_db, b"not parsed in phase 2").unwrap();

        let candidates = discover_backup_candidates(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(matches!(
            candidates[0].kind,
            BackupCandidateKind::ImazingVersionsDb { .. }
        ));
    }

    #[test]
    fn discovery_does_not_import_imazing_snapshot_as_standard_root() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("iMazing.Versions").join("Versions");
        let versions_db = archive.join("device").join("Versions.db");
        let snapshot = archive.join("device").join("2022-06-19-00.20.14");
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(&versions_db, b"not parsed in phase 2").unwrap();
        for name in ["Status.plist", "Info.plist", "Manifest.plist"] {
            fs::write(snapshot.join(name), b"snapshot metadata").unwrap();
        }

        let candidates = discover_backup_candidates(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(matches!(
            candidates[0].kind,
            BackupCandidateKind::ImazingVersionsDb { .. }
        ));
    }

    #[test]
    fn discovery_ignores_misplaced_imazing_versions_db() {
        let dir = tempfile::tempdir().unwrap();
        let versions_db = dir
            .path()
            .join("iMazing.Versions")
            .join("Wrong")
            .join("device")
            .join("Versions.db");
        fs::create_dir_all(versions_db.parent().unwrap()).unwrap();
        fs::write(&versions_db, b"not a supported versions db path").unwrap();

        let candidates = discover_backup_candidates(&[dir.path().to_path_buf()]).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn encrypted_backup_prompt_skip_returns_skip_decision() {
        let dir = minimal_encrypted_backup_root_without_keybag();
        let mut passwords = password::PasswordManager::new();
        let decision = source::open_backup_with_password_manager_and_prompt(
            dir.path(),
            &mut passwords,
            || Ok(password::PasswordInput::Skip),
            std::sync::Arc::new(ibackuptool2::StandardBackupFileResolver),
        )
        .unwrap();

        assert!(matches!(decision, source::UnlockDecision::Skip));
    }

    #[test]
    fn encrypted_backup_fatal_error_does_not_reprompt() {
        let dir = minimal_encrypted_backup_root_without_keybag();
        let mut passwords = password::PasswordManager::new();
        passwords.remember("known".into());
        let result = source::open_backup_with_password_manager_and_prompt(
            dir.path(),
            &mut passwords,
            || panic!("fatal known-password error should not prompt"),
            std::sync::Arc::new(ibackuptool2::StandardBackupFileResolver),
        );

        assert!(matches!(
            result,
            Err(source::BackupOpenError::MissingKeybag)
        ));
    }

    #[test]
    fn encrypted_backup_malformed_keybag_returns_typed_error() {
        let dir = minimal_encrypted_backup_root_with_malformed_keybag();
        let mut passwords = password::PasswordManager::new();
        passwords.remember("known".into());
        let result = source::open_backup_with_password_manager_and_prompt(
            dir.path(),
            &mut passwords,
            || panic!("malformed known-password keybag should not prompt"),
            std::sync::Arc::new(ibackuptool2::StandardBackupFileResolver),
        );

        assert!(matches!(result, Err(source::BackupOpenError::Open(_))));
    }
}
