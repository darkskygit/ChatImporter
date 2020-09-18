use super::*;
use binread::*;
use ibackuptool2::{Backup, BackupFile};
use num_enum::TryFromPrimitive;
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use serde::Serialize;
use serde_json::to_vec;
use std::collections::HashMap;
use std::convert::TryFrom;
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

    #[allow(dead_code)]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, TryFromPrimitive)]
#[repr(u32)]
enum MsgType {
    Normal = 1,        // 文字/emoji
    Image = 3,         // 图片
    Voice = 34,        // 语音
    ContactShare = 42, // 联系人分享
    Video = 43,        // 视频
    BigEmoji = 47,     // 大表情
    Location = 48,     // 定位
    CustomApp = 49,    // 文件、分享、转账、聊天记录批量转发
    VoipContent = 50,  // 语音/视频通话？
    ShortVideo = 62,   // 短视频？
    System = 10000,    // 系统信息，入群/群改名/他人撤回信息/红包领取提醒等等
    Revoke = 10002,    // 撤回信息修改
    Unknown = u32::MAX,
}

impl Default for MsgType {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Clone, Default, Serialize)]
struct AttachMetadata {
    mtype: MsgType,
    hash: i64,
}

impl AttachMetadata {
    pub fn new(hash: i64) -> Self {
        Self {
            hash,
            ..Default::default()
        }
    }

    pub fn with_type(mut self, mtype: MsgType) -> Self {
        self.mtype = mtype;
        self
    }
}

#[derive(Clone, Debug)]
struct RecordLine {
    local_id: i64,
    server_id: i64,
    created_time: i64,
    message: String,
    status: u8,
    image_status: u8,
    msg_type: MsgType,
    is_dest: bool,
}

impl RecordLine {
    pub fn get_audio(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(AttachMetadata, Vec<u8>)> {
        let path = format!(
            "Documents/{}/Audio/{}/{}.aud",
            account, hashed_user, self.local_id
        );
        backups
            .get(&path)
            .or_else(|| {
                warn!(
                    "audio not found: {}, {}, {}",
                    account, hashed_user, self.local_id
                );
                None
            })
            .and_then(|file| {
                backup
                    .read_file(file)
                    .map_err(|e| {
                        warn!(
                            "failed to read audio: {}, {}, {}, {}",
                            account, hashed_user, self.local_id, e
                        )
                    })
                    .ok()
            })
            .map(|data| (AttachMetadata::new(Blob::new(data.clone()).hash), data))
    }
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

    fn get_chat_ids(&self) -> Vec<String> {
        self.chats.keys().cloned().collect::<Vec<_>>()
    }

    fn get_contacts(&self) -> Vec<String> {
        self.find_contacts("")
    }

    fn find_contacts<S: ToString>(&self, name: S) -> Vec<String> {
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

    fn load_record_lines<S: ToString>(&self, user_name: S) -> SqliteResult<Vec<RecordLine>> {
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
                    Ok(RecordLine {
                        local_id: row.get(0)?,
                        server_id: row.get(1)?,
                        created_time: row.get(2)?,
                        message: row.get(3)?,
                        status: row.get(4)?,
                        image_status: row.get(5)?,
                        msg_type: MsgType::try_from(row.get::<_, u32>(6)?).unwrap_or_else(|t| {
                            warn!("unknown type: {}", t);
                            MsgType::Unknown
                        }),
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

    fn transfrom_record_line(
        &self,
        backup: &Backup,
        line: &RecordLine,
        contact: &Contact,
    ) -> Result<RecordType, String> {
        let is_group = contact.name.ends_with("@chatroom");
        let (sender_id, sender_name, content) = {
            if line.is_dest {
                if is_group {
                    if let Some(idx) = line.message.find('\n') {
                        let id = &line.message[0..idx - 1];
                        if let Some((id, remark)) = self
                            .contacts
                            .get(&Self::gen_md5(id))
                            .and_then(|c| c.get_remark().map(|r| (c.name.clone(), r)).ok())
                        {
                            (id, remark, line.message[idx + 1..].into())
                        } else {
                            debug!("wxid {} not found in contacts", id);
                            (id.into(), "".into(), line.message[idx + 1..].into())
                        }
                    } else if [
                        MsgType::BigEmoji,
                        MsgType::CustomApp,
                        MsgType::Video,
                        MsgType::System,
                        MsgType::Revoke,
                    ]
                    .contains(&line.msg_type)
                    {
                        (
                            contact.name.clone(),
                            contact.get_remark().unwrap_or_default(),
                            line.message.clone(),
                        )
                    } else {
                        return Err(format!(
                            "new line not exists in a group line: {}, {}, {:?}",
                            line.local_id, line.created_time, line.msg_type
                        ));
                    }
                } else {
                    (
                        contact.name.clone(),
                        contact.get_remark().unwrap_or_default(),
                        line.message.clone(),
                    )
                }
            } else {
                (self.wxid.clone(), self.name.clone(), line.message.clone())
            }
        };

        let (content, metadata, attach) = match line.msg_type {
            MsgType::Voice => {
                if let Some((metadata, data)) = line.get_audio(
                    backup,
                    &self.account_files,
                    &self.account,
                    &Self::gen_md5(&contact.name),
                ) {
                    (
                        content,
                        Some(metadata.clone().with_type(line.msg_type.clone())),
                        {
                            let mut map = HashMap::new();
                            map.insert(metadata.hash.to_string(), data);
                            map
                        },
                    )
                } else {
                    (content, None, HashMap::new())
                }
            }
            _ => (content, None, HashMap::new()),
        };

        let record = Record {
            chat_type: "iOS WeChat".into(),
            owner_id: self.wxid.clone(),
            group_id: contact.name.clone(),
            sender_id,
            sender_name,
            content,
            timestamp: line.created_time * 1000,
            metadata: metadata.as_ref().and_then(|m| {
                to_vec(m)
                    .map_err(|e| warn!("failed to serialization metadata: {}", e))
                    .ok()
            }),
            ..Default::default()
        };

        Ok(if metadata.is_some() {
            RecordType::from((record, attach))
        } else {
            RecordType::from(record)
        })
    }

    fn transfrom_record_lines(
        &self,
        backup: &Backup,
        contact: &Contact,
        lines: Vec<RecordLine>,
    ) -> Vec<RecordType> {
        lines
            .iter()
            .fold(Vec::<RecordType>::new(), |mut ret, curr| {
                match self.transfrom_record_line(backup, curr, contact) {
                    Ok(record_type) => record_type
                        .get_record()
                        .and_then(|record| {
                            modify_timestamp(
                                record_type.clone(),
                                ret.iter()
                                    .filter_map(|r| r.get_record())
                                    .filter(|r| {
                                        i64::abs(r.timestamp - record.timestamp) < 1000
                                            && r.sender_id == record.sender_id
                                    })
                                    .map(|r| r.timestamp)
                                    .max(),
                            )
                        })
                        .map(|record| ret.push(record)),
                    Err(e) => Some(error!("failed to transfrom record line: {}", e)),
                };
                ret
            })
    }

    fn load_records<S: ToString>(&self, backup: &Backup, chat_id: S) -> Option<Vec<RecordType>> {
        let chat_id = chat_id.to_string();
        self.contacts
            .get(&chat_id)
            .or_else(|| {
                warn!("failed to get chat contact: {}", chat_id);
                None
            })
            .and_then(|contact| {
                self.load_record_lines(&chat_id)
                    .map(|lines| self.transfrom_record_lines(backup, contact, lines))
                    .map_err(|e| warn!("failed to get chat line: {}", e))
                    .ok()
            })
    }

    pub fn get_records(&self, backup: &Backup, names: Option<Vec<String>>) -> Vec<RecordType> {
        match names {
            None => self.get_chat_ids(),
            Some(names) if names.is_empty() => self.get_contacts(),
            Some(names) => names,
        }
        .iter()
        .flat_map(|name| {
            self.find_contacts(name)
                .iter()
                .filter_map(|chat_id| {
                    info!("Extracting: {} => {}", name, chat_id);
                    self.load_records(backup, chat_id)
                })
                .flatten()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
    }
}

#[allow(non_camel_case_types)]
struct Extractor {
    backup: Backup,
    user_info: HashMap<String, UserDB>,
}

impl Extractor {
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

    pub fn get_user_db(&self, user: &str) -> Option<(&UserDB, &Backup)> {
        self.user_info.get(user).map(|db| (db, &self.backup))
    }
}

#[allow(non_camel_case_types)]
pub struct Matcher {
    extractor: Extractor,
    extract_ids: Vec<String>,
    names: Option<Vec<String>>,
}

impl Matcher {
    pub fn new<P: AsRef<Path>>(path: P, names: Option<Vec<String>>) -> Result<Box<dyn MsgMatcher>> {
        let extractor = Extractor::new(path).map_err(|e| anyhow::anyhow!("{}", e))?;
        let extract_ids = extractor.get_users();
        Ok(Box::new(Self {
            extractor,
            extract_ids,
            names,
        }) as Box<dyn MsgMatcher>)
    }
}

impl MsgMatcher for Matcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        Some(
            self.extract_ids
                .iter()
                .filter_map(|u| self.extractor.get_user_db(u))
                .flat_map(|(user_db, backup)| user_db.get_records(backup, self.names.clone()))
                .collect(),
        )
    }
}
