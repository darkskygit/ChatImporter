use super::*;
use ibackuptool2::{Backup, BackupFile};
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use std::collections::HashMap;
use std::io::{Cursor, Error, ErrorKind, Write};
use std::sync::Arc;
use tempfile::NamedTempFile;

#[derive(Clone)]
struct Contact {
    name: String,
    remark: String,
    head: String,
    user_type: i32,
}

#[derive(Clone, Default)]
struct UserDB {
    contact: Option<Arc<NamedTempFile>>,
    message: Option<Arc<NamedTempFile>>,
    setting: Option<BackupFile>,
    session: Option<Arc<NamedTempFile>>,
    account_files: HashMap<String, BackupFile>,
    contacts: Vec<Contact>,
    account: String,
    wxid: String,
    name: String,
    head: String,
}

impl UserDB {
    pub fn new(
        backup: &Backup,
        account: String,
        file: &BackupFile,
        account_files: Vec<BackupFile>,
    ) -> Self {
        let account_files = account_files
            .iter()
            .map(|file| (file.relative_filename.clone(), file.clone()))
            .collect();
        let user_db = Self {
            account,
            account_files,
            ..Self::default()
        };
        user_db.match_path(backup, file)
    }

    pub fn with(self, backup: &Backup, file: &BackupFile) -> Self {
        self.match_path(backup, file)
    }

    fn match_path(mut self, backup: &Backup, file: &BackupFile) -> Self {
        let filename = Path::new(&file.relative_filename).name_str().to_string();
        if ["WCDB_Contact.sqlite", "MM.sqlite", "session.db"].contains(&filename.as_str()) {
            if let Ok(tmpfile) = NamedTempFile::new().and_then(|mut tmpfile| {
                let data = backup
                    .read_file(file)
                    .map_err(|e| Error::new(ErrorKind::Other, format!("{}", e)))?;
                tmpfile.write_all(&data)?;
                Ok(tmpfile)
            }) {
                match filename.as_str() {
                    "WCDB_Contact.sqlite" => self.contact = Some(Arc::new(tmpfile)),
                    "MM.sqlite" => self.message = Some(Arc::new(tmpfile)),
                    "session.db" => self.session = Some(Arc::new(tmpfile)),
                    _ => {}
                }
            } else {
                warn!("Failed to extract file: {}", file.relative_filename);
            }
        } else if filename == "mmsetting.archive" {
            self.setting = Some(file.clone());
        }
        self
    }

    pub fn is_complete(&self) -> bool {
        let ret = self.contact.is_some()
            && self.message.is_some()
            && self.setting.is_some()
            && self.session.is_some();
        if !ret {
            warn!(
                "user db lost some metadata: {}, {}, {}, {}",
                self.contact.is_some(),
                self.message.is_some(),
                self.setting.is_some(),
                self.session.is_some()
            );
        }
        ret
    }

    pub fn build(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        self.load_settings(backup)?;
        self.load_contacts()?;
        Ok(())
    }

    pub fn load_settings(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(setting) = &self.setting {
            let data = backup.read_file(setting)?;
            if let Some(array) = Value::from_reader(Cursor::new(data))?
                .as_dictionary()
                .and_then(|dict| dict.get("$objects"))
                .and_then(|obj| obj.as_array())
            {
                if array.len() > 3 {
                    self.wxid = array[2]
                        .as_string()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    self.name = array[3]
                        .as_string()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                }
                if array.len() > 50 {
                    self.head = if let Some(head) = array
                        .iter()
                        .filter_map(|v| v.as_string())
                        .filter(|s| {
                            s.starts_with("http://")
                                && s.find("mmhead").is_some()
                                && s.find("/132").is_some()
                        })
                        .next()
                    {
                        head
                    } else {
                        ""
                    }
                    .to_string();
                }
            } else {
                warn!("failed to load settings: {}", setting.relative_filename);
            }
        }
        Ok(())
    }

    fn get_conn(file: Option<Arc<NamedTempFile>>) -> SqliteResult<Option<Connection>> {
        if let Some(file) = file {
            Ok(Some(Connection::open_with_flags(
                file.as_ref(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?))
        } else {
            Ok(None)
        }
    }

    pub fn load_contacts(&mut self) -> SqliteResult<()> {
        if let Some(conn) = Self::get_conn(self.contact.clone())? {
            self.contacts = conn
                .prepare("SELECT userName, dbContactRemark, dbContactHeadImage, type FROM Friend")?
                .query_map(params![], |row| {
                    Ok(Contact {
                        name: row.get(0)?,
                        remark: {
                            let remark: String = row.get(1)?;
                            if remark.len() > 2
                                && remark.chars().next() == Some('\n')
                                && remark.chars().nth(1) >= Some('\0')
                                && remark.chars().nth(1) <= Some('0')
                            {
                                let mut chars = vec![];
                                for c in remark.chars().nth(2) {
                                    if c < '\u{0012}' || c > '\u{0016}' {
                                        chars.push(c);
                                    } else {
                                        break;
                                    }
                                }
                                chars.iter().cloned().collect::<String>()
                            } else {
                                remark
                            }
                        },
                        head: row.get(2)?,
                        user_type: row.get(3)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
        }
        Ok(())
    }
}

#[allow(non_camel_case_types)]
struct iOSWeChatExtractor {
    backup: Backup,
    user_info: HashMap<String, UserDB>,
}

impl iOSWeChatExtractor {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let mut backup = Backup::new(path)?;
        backup.parse_manifest()?;
        let user_info = Self::get_user_info(&backup);
        Ok(Self { backup, user_info })
    }

    fn get_user_info(backup: &Backup) -> HashMap<String, UserDB> {
        const DOMAIN: &str = "AppDomain-com.tencent.xin";
        const MATCHED_NAME: [&str; 4] = [
            "WCDB_Contact.sqlite",
            "MM.sqlite",
            "mmsetting.archive",
            "session.db",
        ];
        let mut user_map = HashMap::new();
        let paths = vec![
            backup.find_wildcard_paths(DOMAIN, "*/WCDB_Contact.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/MM.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/mmsetting.archive"),
            backup.find_wildcard_paths(DOMAIN, "*/session/session.db"),
        ];
        for file in paths.iter().flatten() {
            let path = Path::new(&file.relative_filename);
            if MATCHED_NAME.contains(&path.name_str()) {
                if let Some(user_id) = path
                    .strip_prefix("Documents")
                    .ok()
                    .and_then(|p| p.components().next())
                    .map(|user_id| user_id.name_str().to_string())
                {
                    if let Some(user) = user_map.remove(&user_id) {
                        let user: UserDB = user;
                        user_map.insert(user_id, user.with(&backup, file));
                    } else {
                        user_map.insert(
                            user_id.clone(),
                            UserDB::new(
                                &backup,
                                user_id.clone(),
                                file,
                                backup.find_wildcard_paths(
                                    DOMAIN,
                                    &format!("Documents/{}/*", user_id),
                                ),
                            ),
                        );
                    }
                } else {
                    warn!("Unmatched path: {}", path.display());
                }
            } else {
                warn!("Unknown file name: {}", path.display());
            }
        }
        user_map
            .iter()
            .filter(|(_, user_db)| user_db.is_complete())
            .filter_map(|(user_id, user_db)| {
                let mut user = user_db.clone();
                user.build(&backup)
                    .map(|_| (user_id.clone(), user))
                    .map_err(|e| warn!("failed to init user: {}", e))
                    .ok()
            })
            .collect()
    }
}

#[allow(non_camel_case_types)]
pub struct iOSWeChatMsgMatcher {
    extractor: iOSWeChatExtractor,
}

impl iOSWeChatMsgMatcher {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Box<dyn MsgMatcher>> {
        let extractor = iOSWeChatExtractor::new(path).map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(Box::new(Self { extractor }) as Box<dyn MsgMatcher>)
    }
}

impl MsgMatcher for iOSWeChatMsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        todo!()
    }
}

