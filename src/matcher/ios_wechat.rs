use super::*;
use binread::*;
use ibackuptool2::{Backup, BackupFile};
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use std::collections::HashMap;
use std::io::{Cursor, Error, ErrorKind, Write};
use std::str::{from_utf8, Utf8Error};
use std::sync::Arc;
use tempfile::NamedTempFile;

#[derive(BinRead)]
#[br(little, magic = b"\n")]
struct ContactRemark {
    _len: u8,
    #[br(count = _len)]
    remark: Vec<u8>,
}

impl ContactRemark {
    pub fn get_remark(&self) -> Result<&str, Utf8Error> {
        from_utf8(&self.remark)
    }
}

#[derive(Clone)]
struct Contact {
    pub name: String,
    remark: Vec<u8>,
    head: Vec<u8>,
    pub user_type: i32,
}

impl Contact {
    pub fn get_remark(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(Cursor::new(&self.remark)
            .read_le::<ContactRemark>()?
            .get_remark()?
            .into())
    }

    pub fn get_image(&self) -> Result<Option<String>, Utf8Error> {
        lazy_static! {
            static ref URL_MATCHER: Regex =
                Regex::new(r"(http://[a-zA-Z\./_\d]*/0)([^a-zA-Z\./_\d]|$)").unwrap();
        }
        Ok(URL_MATCHER
            .captures(from_utf8(&self.head)?)
            .and_then(|c| c.iter().nth(1).and_then(|i| i))
            .map(|i| i.as_str().trim().into()))
    }
}

#[derive(Clone, Debug)]
struct ChatLine {
    local_id: i64,
    server_id: i64,
    created_time: i64,
    message: String,
    status: u8,
    image_status: u8,
    msg_type: u32,
    is_dest: bool,
}

#[derive(Clone, Default)]
struct UserDB {
    contact: Option<Arc<NamedTempFile>>,
    message: Option<Arc<NamedTempFile>>,
    setting: Option<BackupFile>,
    session: Option<Arc<NamedTempFile>>,
    account_files: HashMap<String, BackupFile>,
    chats: HashMap<String, String>,
    contacts: HashMap<String, Contact>,
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
                debug!(
                    "read file: {}, {}, {}",
                    self.account, file.fileid, file.relative_filename
                );
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
        self.load_chats()?;
        Ok(())
    }

    fn load_settings(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
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

    fn load_chats(&mut self) -> SqliteResult<()> {
        if let Some(conn) = Self::get_conn(self.message.clone())? {
            let contact_keys = self.contacts.keys().map(|s| s.as_str()).collect::<Vec<_>>();
            self.chats = conn
                .prepare(r#"SELECT name FROM sqlite_master where type='table' and name like "Chat\_%" ESCAPE '\'"#)?
                .query_map(params![], |row| {
                    let name: String = row.get(0)?;
                    let hash = &name[5..];
                    if !contact_keys.iter().any(|&i| i == hash) && hash != self.account {
                        warn!("Contact info for chat not exists: {}", hash);
                    }
                    Ok((hash.into(), name))
                })?
                .filter_map(|r| {
                    r.map_err(|e| warn!("failed to parse chat list: {}", e))
                        .ok()
                })
                .collect();
        }
        Ok(())
    }

    fn load_contacts(&mut self) -> SqliteResult<()> {
        if let Some(conn) = Self::get_conn(self.contact.clone())? {
            self.contacts = conn
                .prepare("SELECT userName, dbContactRemark, dbContactHeadImage, type FROM Friend")?
                .query_map(params![], |row| {
                    let name = row.get(0)?;
                    Ok((
                        Self::gen_md5(&name),
                        Contact {
                            name,
                        remark: row.get(1)?,
                        head: row.get(2)?,
                        user_type: row.get(3)?,
                        },
                    ))
                })?
                .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                .collect();
        }
        Ok(())
    }

    pub fn get_contacts(&self) -> Vec<String> {
        self.find_contacts("")
    }

    pub fn find_contacts<S: ToString>(&self, name: S) -> Vec<String> {
        let name = name.to_string();
        let chat_keys = self.chats.keys().map(|s| s.as_str()).collect::<Vec<_>>();
        self.contacts
            .iter()
            .filter_map(|(hash, c)| {
                if chat_keys.iter().any(|&i| i == hash) {
                    (name.is_empty()
                        || hash == &name
                        || c.name.find(&name).is_some()
                        || c.get_remark().ok().and_then(|r| r.find(&name)).is_some())
                    .then(|| {
                        info!(
                            "Chat table found: {}, {}, {}",
                            hash,
                            c.name,
                            c.get_remark()
                                .unwrap_or_else(|e| format!("No Remark: {}", e))
                        );
                        hash
                    })
                } else {
                    debug!(
                        "Chat table not found: {}, {}, {}",
                        hash,
                        c.name,
                        c.get_remark()
                            .unwrap_or_else(|e| format!("No Remark: {}", e))
                    );
                    None
                }
            })
            .cloned()
            .collect()
    }

    fn gen_md5<S: ToString>(user_name: S) -> String {
        use md5::{Digest, Md5};
        format!("{:x}", Md5::digest(user_name.to_string().as_bytes()))
    }

    pub fn load_chat_lines<S: ToString>(&self, user_name: S) -> SqliteResult<Vec<ChatLine>> {
        if let Some(conn) = Self::get_conn(self.message.clone())? {
            let user_name = user_name.to_string();
            Ok(conn
                .prepare(&format!(
                    "SELECT
                        MesLocalID,
                        MesSvrID,
                        CreateTime,
                        Message,
                        Status,
                        ImgStatus,
                        Type,
                        Des
                    FROM
                        Chat_{}",
                    self.chats
                        .keys()
                        .find(|h| h.as_str() == user_name)
                        .map(|s| s.into())
                        .unwrap_or_else(|| Self::gen_md5(user_name))
                ))?
                .query_map(params![], |row| {
                    Ok(ChatLine {
                        local_id: row.get(0)?,
                        server_id: row.get(1)?,
                        created_time: row.get(2)?,
                        message: row.get(3)?,
                        status: row.get(4)?,
                        image_status: row.get(5)?,
                        msg_type: row.get(6)?,
                        is_dest: row.get(7)?,
                    })
                })?
                .filter_map(|r| {
                    r.map_err(|e| warn!("failed to parse chat line: {}", e))
                        .ok()
                })
                .collect())
        } else {
            Ok(vec![])
        }
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

    pub fn get_users(&self) -> Vec<String> {
        self.user_info.keys().cloned().collect()
}

    pub fn get_user_db(&self, user: &str) -> Option<&UserDB> {
        self.user_info.get(user)
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

