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
use std::fs::{read, write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug)]
pub struct Backup {
    pub path: PathBuf,
    pub manifest: BackupManifest,
    pub info: BackupInfo,
    pub status: BackupStatus,
    pub files: Vec<BackupFile>,
}

impl Backup {
    /// Create from root backup path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Backup, Box<dyn std::error::Error>> {
        use ::plist::from_file;
        let status: BackupStatus =
            from_file(format!("{}/Status.plist", path.as_ref().to_str().unwrap()))?;

        let info: BackupInfo =
            from_file(format!("{}/Info.plist", path.as_ref().to_str().unwrap()))?;

        let manifest: BackupManifest = from_file(format!(
            "{}/Manifest.plist",
            path.as_ref().to_str().unwrap()
        ))?;

        Ok(Backup {
            path: path.as_ref().to_path_buf(),
            manifest,
            status,
            info,
            files: vec![],
        })
    }

    /// Parse the keybag contained in the manifest.
    pub fn parse_keybag(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(bag) = &self.manifest.backup_key_bag {
            self.manifest.keybag = Some(KeyBag::init(bag.to_vec()));
        }

        Ok(())
    }

    pub fn get_keybag(&self) -> Option<&KeyBag> {
        match &self.manifest.keybag {
            Some(kb) => Some(kb),
            None => return None,
        }
    }

    #[allow(dead_code)]
    pub fn find_fileid(&self, fileid: &str) -> Option<BackupFile> {
        for file in &self.files {
            if file.fileid == fileid {
                return Some(file.clone());
            }
        }

        return None;
    }

    #[allow(dead_code)]
    pub fn find_path(&self, domain: &str, path: &str) -> Option<BackupFile> {
        for file in &self.files {
            if file.domain == domain && file.relative_filename == path {
                return Some(file.clone());
            }
        }

        return None;
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
        return paths;
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

    #[allow(dead_code)]
    pub fn read_file(&self, file: &BackupFile) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let path = format!(
            "{}/{}/{}",
            self.path.to_str().expect("path to be str"),
            (&file.fileid)[0..2].to_string(),
            file.fileid
        );
        let finpath = self.path.join(Path::new(&path));

        debug!("read file path: {}", finpath.display());

        if !finpath.is_file() {
            return Err(BackupError::InManifestButNotFound.into());
        }

        let contents = read(&finpath).expect("contents to exist");

        // if the file
        if self.manifest.is_encrypted {
            debug!("file {} is encrypted, decrypting...", finpath.display());
            match &file.fileinfo.as_ref() {
                Some(fileinfo) => match fileinfo.encryption_key.as_ref() {
                    Some(encryption_key) => {
                        let dec = decrypt_with_key(&encryption_key, &contents);
                        let sliced_dec = dec[..min(fileinfo.size as usize, dec.len())].to_vec();
                        debug!("file {} is now decrypted...", finpath.display());
                        return Ok(sliced_dec);
                    }
                    None => {
                        return Err(BackupError::NoEncryptionKey.into());
                    }
                },
                None => {
                    return Err(BackupError::NoFileInfo.into());
                }
            }
        }

        read(Path::new(&path)).map_err(|e| e.into())
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
                let mutable = file.fileinfo.as_mut();
                mutable.map(|s| s.unwrap_encryption_key(keybag));
            }
        }
        info!("unwrapping file keys... [done]");

        Ok(())
    }

    /// Load the list of files, from the backup's manifest file.
    pub fn parse_manifest(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.files.clear();

        let conn: Connection;
        if self.manifest.is_encrypted {
            let path = format!("{}/Manifest.db", self.path.to_str().unwrap());
            let contents = read(Path::new(&path)).unwrap();
            let dec = decrypt_with_key(
                &self.manifest.manifest_key_unwrapped.as_ref().unwrap(),
                &contents,
            );
            debug!("decrypted {} bytes from manifest.", dec.len());
            let home_dir = match dirs::home_dir() {
                Some(res) => match res.to_str() {
                    Some(res) => res.to_string(),
                    None => panic!("Can't convert homedir to string!"),
                },
                None => panic!("Can't find homedir:"),
            };

            let pth = format!("{}/Downloads/decrypted_database.sqlite", home_dir);
            trace!("writing decrypted database: {}", pth);
            let decpath = Path::new(&pth);
            write(&decpath, dec).unwrap();

            conn = Connection::open_with_flags(&decpath, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        } else {
            conn = Connection::open_with_flags(
                format!("{}/Manifest.db", self.path.to_str().unwrap()),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
        }

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
            let val = Value::from_reader(cur).expect("expected to load bplist");

            let fileinfo = match FileInfo::try_from(val) {
                Ok(res) => Some(res),
                Err(err) => {
                    error!("failed to parse file info: {}", err);
                    None
                }
            };

            Ok(BackupFile {
                fileid,
                domain,
                relative_filename,
                flags,
                fileinfo,
            })
        })?;

        // Add each item to the internal list
        for item in rows {
            if let Ok(item) = item {
                self.files.push(item);
            }
        }

        Ok(())
    }
}
