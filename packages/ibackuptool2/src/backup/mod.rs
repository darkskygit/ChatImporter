mod file;
mod info;
mod manifest;
mod status;

use super::*;
pub use file::{BackupFile, FileInfo};
pub use info::BackupInfo;
pub use manifest::{BackupManifest, BackupManifestLockdown};
pub use status::BackupStatus;

use std::cmp::min;
use std::convert::TryFrom;
use std::fs::read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::serialize::OwnedData;
use rusqlite::{Connection, DatabaseName, OpenFlags};

pub struct Backup {
    pub path: PathBuf,
    pub manifest: BackupManifest,
    pub info: BackupInfo,
    pub status: BackupStatus,
    pub files: Vec<BackupFile>,
    file_resolver: Arc<dyn BackupFileResolver>,
}

impl std::fmt::Debug for Backup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backup")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .field("info", &self.info)
            .field("status", &self.status)
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

pub trait BackupFileResolver: Send + Sync {
    fn resolve(&self, backup_root: &Path, file: &BackupFile) -> Result<PathBuf, BackupError>;
}

#[derive(Debug, Default)]
pub struct StandardBackupFileResolver;

impl BackupFileResolver for StandardBackupFileResolver {
    fn resolve(&self, backup_root: &Path, file: &BackupFile) -> Result<PathBuf, BackupError> {
        if file.fileid.len() < 2 {
            return Err(BackupError::FileNotFound);
        }
        Ok(backup_root.join(&file.fileid[..2]).join(&file.fileid))
    }
}

impl Backup {
    /// Create from root backup path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Backup, Box<dyn std::error::Error>> {
        use ::plist::from_file;
        let status: BackupStatus = from_file(path.as_ref().join("Status.plist"))?;

        let info: BackupInfo = from_file(path.as_ref().join("Info.plist"))?;

        let manifest: BackupManifest = from_file(path.as_ref().join("Manifest.plist"))?;

        Ok(Backup {
            path: path.as_ref().to_path_buf(),
            manifest,
            status,
            info,
            files: vec![],
            file_resolver: Arc::new(StandardBackupFileResolver),
        })
    }

    pub fn with_file_resolver(mut self, resolver: Arc<dyn BackupFileResolver>) -> Self {
        self.file_resolver = resolver;
        self
    }

    /// Parse the keybag contained in the manifest.
    pub fn parse_keybag(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(bag) = &self.manifest.backup_key_bag {
            self.manifest.keybag = Some(KeyBag::init(bag.to_vec())?);
        }

        Ok(())
    }

    pub fn get_keybag(&self) -> Option<&KeyBag> {
        match &self.manifest.keybag {
            Some(kb) => Some(kb),
            None => None,
        }
    }

    // Public lookup helper for downstream importers that need to resolve manifest files by id.
    #[allow(dead_code)]
    pub fn find_fileid(&self, fileid: &str) -> Option<BackupFile> {
        for file in &self.files {
            if file.fileid == fileid {
                return Some(file.clone());
            }
        }

        None
    }

    // Public lookup helper for downstream importers that need a single well-known backup path.
    #[allow(dead_code)]
    pub fn find_path(&self, domain: &str, path: &str) -> Option<BackupFile> {
        for file in &self.files {
            if file.domain == domain && file.relative_filename == path {
                return Some(file.clone());
            }
        }

        None
    }

    pub fn find_wildcard_paths(&self, domain: &str, path: &str) -> Vec<BackupFile> {
        use wildmatch::WildMatch;
        let matcher = WildMatch::new(path);
        let mut paths = vec![];
        for file in &self.files {
            if file.domain == domain && matcher.is_match(&file.relative_filename) {
                paths.push(file.clone());
            }
        }
        paths
    }

    pub fn find_regex_paths(&self, domain: &str, path: &str) -> Vec<BackupFile> {
        use regex::Regex;
        if let Ok(matcher) = Regex::new(path) {
            let mut paths = vec![];
            for file in &self.files {
                if file.domain == domain && matcher.is_match(&file.relative_filename) {
                    paths.push(file.clone());
                }
            }
            paths
        } else {
            vec![]
        }
    }

    // Public file reader for downstream crates; ibackuptool2 tests may not call it in every target.
    #[allow(dead_code)]
    pub fn read_file(&self, file: &BackupFile) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let finpath = self.file_resolver.resolve(&self.path, file)?;

        debug!("read file path: {}", finpath.display());

        if !finpath.is_file() {
            return Err(BackupError::InManifestButNotFound.into());
        }

        let contents = read(&finpath)?;

        // if the file
        if self.manifest.is_encrypted {
            debug!("file {} is encrypted, decrypting...", finpath.display());
            let keybag = self.get_keybag().ok_or(BackupError::NoKeybag)?;
            let mut fileinfo = file.fileinfo.clone().ok_or(BackupError::NoFileInfo)?;
            fileinfo.unwrap_encryption_key(keybag)?;
            let encryption_key = fileinfo
                .encryption_key
                .as_ref()
                .ok_or(BackupError::NoEncryptionKey)?;
            let dec = decrypt_with_key(encryption_key, &contents);
            let sliced_dec = dec[..min(fileinfo.size as usize, dec.len())].to_vec();
            debug!("file {} is now decrypted...", finpath.display());
            return Ok(sliced_dec);
        }

        Ok(contents)
    }

    /// Unwrap all individual file encryption keys
    pub fn unwrap_file_keys(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let keybag = match &self.manifest.keybag {
            Some(kb) => kb,
            None => return Ok(()),
        };

        info!("unwrapping file keys...");
        for file in self.files.iter_mut() {
            if file.fileinfo.is_some() {
                if let Some(fileinfo) = file.fileinfo.as_mut() {
                    fileinfo.unwrap_encryption_key(keybag)?;
                }
            }
        }
        info!("unwrapping file keys... [done]");

        Ok(())
    }

    /// Load the list of files, from the backup's manifest file.
    pub fn parse_manifest(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.files.clear();

        let conn: Connection = if self.manifest.is_encrypted {
            let path = self.path.join("Manifest.db");
            let contents = read(&path)?;
            let manifest_key = self
                .manifest
                .manifest_key_unwrapped
                .as_ref()
                .ok_or(BackupError::NoManifestKey)?;
            let mut dec = decrypt_with_key(manifest_key, &contents);
            debug!("decrypted {} bytes from manifest.", dec.len());
            debug!(
                "decrypted manifest header: {}",
                hex::encode(&dec[..min(dec.len(), 16)])
            );
            normalize_sqlite_header_for_memory_read(&mut dec);
            manifest_connection_from_bytes(dec)?
        } else {
            Connection::open_with_flags(
                self.path.join("Manifest.db"),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?
        };

        let mut stmt =
            conn.prepare("SELECT fileid, domain, relativePath, flags, file from Files")?;
        let rows = stmt.query_map([], |row| {
            // fileid equals sha1(format!("{}-{}", domain, relative_filename))
            let fileid: String = row.get(0)?;
            let domain: String = row.get(1)?;
            let relative_filename: String = row.get(2)?;
            let flags: i64 = row.get(3)?;
            let file: Vec<u8> = row.get(4)?;
            use ::plist::Value;

            let cur = std::io::Cursor::new(file);
            let fileinfo = Value::from_reader(cur)
                .map_err(|err| {
                    error!("failed to parse file info plist: {}", err);
                    err
                })
                .ok()
                .and_then(|val| match FileInfo::try_from(val) {
                    Ok(res) => Some(res),
                    Err(err) => {
                        error!("failed to parse file info: {}", err);
                        None
                    }
                });

            Ok(BackupFile {
                fileid,
                domain,
                relative_filename,
                flags,
                fileinfo,
            })
        })?;

        // Add each item to the internal list
        for item in rows.flatten() {
            self.files.push(item);
        }

        Ok(())
    }
}

fn manifest_connection_from_bytes(
    bytes: Vec<u8>,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let mut conn = Connection::open_in_memory()?;
    let size = bytes.len();
    let alloc_size = rusqlite::ffi::sqlite3_uint64::try_from(size)?;
    let ptr = unsafe { rusqlite::ffi::sqlite3_malloc64(alloc_size) as *mut u8 };
    let ptr = std::ptr::NonNull::new(ptr).ok_or(BackupError::FileNotFound)?;
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), size);
        let data = OwnedData::from_raw_nonnull(ptr, size);
        conn.deserialize(DatabaseName::Main, data, true)?;
    }
    Ok(conn)
}

fn normalize_sqlite_header_for_memory_read(bytes: &mut [u8]) {
    const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
    if bytes.len() >= 20
        && &bytes[..SQLITE_HEADER.len()] == SQLITE_HEADER
        && (bytes[18] == 2 || bytes[19] == 2)
    {
        debug!(
            "normalizing WAL-mode sqlite manifest header for memory read: write={}, read={}",
            bytes[18], bytes[19]
        );
        bytes[18] = 1;
        bytes[19] = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::plist::Value;
    use std::sync::Arc;

    struct FixedResolver {
        path: PathBuf,
    }

    impl BackupFileResolver for FixedResolver {
        fn resolve(&self, _backup_root: &Path, _file: &BackupFile) -> Result<PathBuf, BackupError> {
            Ok(self.path.clone())
        }
    }

    fn backup_with_resolver(path: PathBuf) -> Backup {
        Backup {
            path: PathBuf::from("/unused/root"),
            manifest: BackupManifest {
                is_encrypted: false,
                version: "9.1".into(),
                date: "2026-05-25".into(),
                system_domains_version: "20".into(),
                was_passcode_set: false,
                manifest_key: None,
                lockdown: BackupManifestLockdown {
                    product_version: "18.0".into(),
                    product_type: "iPhone".into(),
                    build_version: None,
                    unique_device_id: "device-id".into(),
                    serial_number: "serial".into(),
                    device_name: "Test Phone".into(),
                },
                backup_key_bag: None,
                keybag: None,
                manifest_key_unwrapped: None,
            },
            info: BackupInfo {
                build_version: None,
                device_name: None,
                guid: None,
                iccid: None,
                imei: None,
                meid: None,
                phone_number: None,
                product_type: "iPhone".into(),
                product_name: None,
                product_version: "18.0".into(),
                serial_number: None,
                target_identifier: "device-id".into(),
                target_type: "Device".into(),
                unique_identifier: None,
                itunes_version: None,
            },
            status: BackupStatus {
                backup_state: "new".into(),
                date: "2026-05-25".into(),
                is_full_backup: true,
                snapshot_state: "finished".into(),
                uuid: "backup-uuid".into(),
                version: "2.4".into(),
            },
            files: Vec::new(),
            file_resolver: Arc::new(FixedResolver { path }),
        }
    }

    #[test]
    fn read_file_uses_backup_file_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let resolved_path = dir.path().join("resolved-object");
        std::fs::write(&resolved_path, b"resolved bytes").unwrap();
        let backup = backup_with_resolver(resolved_path);
        let file = BackupFile {
            fileid: "aabbcc".into(),
            domain: "HomeDomain".into(),
            relative_filename: "Library/Test".into(),
            flags: 1,
            fileinfo: None,
        };

        assert_eq!(backup.read_file(&file).unwrap(), b"resolved bytes");
    }

    #[test]
    fn manifest_connection_from_bytes_reads_without_filesystem_database() {
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE Files (
                    fileid TEXT NOT NULL,
                    domain TEXT NOT NULL,
                    relativePath TEXT NOT NULL,
                    flags INTEGER NOT NULL,
                    file BLOB NOT NULL
                );
                INSERT INTO Files VALUES ('aabbcc', 'HomeDomain', 'Library/Test', 1, X'00');",
            )
            .unwrap();
        let data = source.serialize(DatabaseName::Main).unwrap().to_vec();

        let conn = manifest_connection_from_bytes(data).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM Files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn wal_mode_manifest_bytes_are_normalized_for_memory_read() {
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE Files (
                    fileid TEXT NOT NULL,
                    domain TEXT NOT NULL,
                    relativePath TEXT NOT NULL,
                    flags INTEGER NOT NULL,
                    file BLOB NOT NULL
                );
                INSERT INTO Files VALUES ('aabbcc', 'HomeDomain', 'Library/Test', 1, X'00');",
            )
            .unwrap();
        let mut data = source.serialize(DatabaseName::Main).unwrap().to_vec();
        data[18] = 2;
        data[19] = 2;

        normalize_sqlite_header_for_memory_read(&mut data);

        assert_eq!(&data[18..20], &[1, 1]);
        let conn = manifest_connection_from_bytes(data).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM Files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    fn write_minimal_backup_metadata(dir: &Path) {
        let mut status = ::plist::Dictionary::new();
        status.insert("BackupState".into(), "new".into());
        status.insert("Date".into(), "2026-05-25".into());
        status.insert("IsFullBackup".into(), true.into());
        status.insert("SnapshotState".into(), "finished".into());
        status.insert("UUID".into(), "backup-uuid".into());
        status.insert("Version".into(), "2.4".into());
        ::plist::to_file_xml(dir.join("Status.plist"), &Value::Dictionary(status)).unwrap();

        let mut info = ::plist::Dictionary::new();
        info.insert("Product Type".into(), "iPhone".into());
        info.insert("Product Version".into(), "18.0".into());
        info.insert("Target Identifier".into(), "device-id".into());
        info.insert("Target Type".into(), "Device".into());
        ::plist::to_file_xml(dir.join("Info.plist"), &Value::Dictionary(info)).unwrap();

        let mut lockdown = ::plist::Dictionary::new();
        lockdown.insert("ProductVersion".into(), "18.0".into());
        lockdown.insert("ProductType".into(), "iPhone".into());
        lockdown.insert("UniqueDeviceID".into(), "device-id".into());
        lockdown.insert("SerialNumber".into(), "serial".into());
        lockdown.insert("DeviceName".into(), "Test Phone".into());
        let mut manifest = ::plist::Dictionary::new();
        manifest.insert("IsEncrypted".into(), false.into());
        manifest.insert("Version".into(), "9.1".into());
        manifest.insert("Date".into(), "2026-05-25".into());
        manifest.insert("SystemDomainsVersion".into(), "20".into());
        manifest.insert("WasPasscodeSet".into(), false.into());
        manifest.insert("Lockdown".into(), Value::Dictionary(lockdown));
        ::plist::to_file_xml(dir.join("Manifest.plist"), &Value::Dictionary(manifest)).unwrap();
    }

    #[test]
    fn parse_manifest_ignores_unreadable_fileinfo_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());

        let conn = Connection::open(dir.path().join("Manifest.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE Files (
                fileid TEXT NOT NULL,
                domain TEXT NOT NULL,
                relativePath TEXT NOT NULL,
                flags INTEGER NOT NULL,
                file BLOB NOT NULL
            );
            INSERT INTO Files VALUES ('aabbcc', 'HomeDomain', 'Library/Test', 1, X'00');",
        )
        .unwrap();
        drop(conn);

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        assert_eq!(backup.files.len(), 1);
        assert!(backup.files[0].fileinfo.is_none());
    }

    #[test]
    fn parse_manifest_ignores_invalid_keyed_archive_fileinfo_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());

        let mut invalid_archive = Vec::new();
        ::plist::to_writer_binary(
            &mut invalid_archive,
            &Value::Dictionary(::plist::Dictionary::new()),
        )
        .unwrap();

        let conn = Connection::open(dir.path().join("Manifest.db")).unwrap();
        conn.execute(
            "CREATE TABLE Files (
                fileid TEXT NOT NULL,
                domain TEXT NOT NULL,
                relativePath TEXT NOT NULL,
                flags INTEGER NOT NULL,
                file BLOB NOT NULL
            );",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Files VALUES ('aabbcc', 'HomeDomain', 'Library/Test', 1, ?1)",
            [&invalid_archive],
        )
        .unwrap();
        drop(conn);

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        assert_eq!(backup.files.len(), 1);
        assert!(backup.files[0].fileinfo.is_none());
    }

    #[test]
    fn parse_manifest_ignores_keyed_archive_with_non_dictionary_root() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());

        let mut top = ::plist::Dictionary::new();
        top.insert("root".into(), Value::Uid(::plist::Uid::new(0)));
        let mut archive = ::plist::Dictionary::new();
        archive.insert("$archiver".into(), "NSKeyedArchiver".into());
        archive.insert("$top".into(), Value::Dictionary(top));
        archive.insert(
            "$objects".into(),
            Value::Array(vec![Value::String("not-a-dict".into())]),
        );
        let mut archive_data = Vec::new();
        ::plist::to_writer_binary(&mut archive_data, &Value::Dictionary(archive)).unwrap();

        let conn = Connection::open(dir.path().join("Manifest.db")).unwrap();
        conn.execute(
            "CREATE TABLE Files (
                fileid TEXT NOT NULL,
                domain TEXT NOT NULL,
                relativePath TEXT NOT NULL,
                flags INTEGER NOT NULL,
                file BLOB NOT NULL
            );",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Files VALUES ('aabbcc', 'HomeDomain', 'Library/Test', 1, ?1)",
            [&archive_data],
        )
        .unwrap();
        drop(conn);

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        assert_eq!(backup.files.len(), 1);
        assert!(backup.files[0].fileinfo.is_none());
    }

    #[test]
    fn invalid_wrapped_key_returns_error_instead_of_panicking() {
        let kek = vec![0; 32];
        let wrapped = vec![0; 16];
        assert!(matches!(
            unwrap_key(&kek, &wrapped),
            Err(BackupError::InvalidPassword)
        ));
    }
}
