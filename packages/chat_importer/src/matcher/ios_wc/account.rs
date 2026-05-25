use super::contact::*;
use super::message::load_record_lines;
use super::mmap::{MMMap, MMType};
use super::session::SessionIndex;
use super::*;

#[derive(Clone, Default)]
pub(super) struct UserDB {
    pub(super) contact: Option<Arc<NamedTempFile>>,
    pub(super) messages: Vec<Arc<NamedTempFile>>,
    pub(super) setting: Option<BackupFile>,
    pub(super) kv_setting: Option<BackupFile>,
    pub(super) session: Option<Arc<NamedTempFile>>,
    pub(super) sessions: SessionIndex,
    pub(super) account_files: HashMap<String, BackupFile>,
    pub(super) chats: HashMap<String, String>,
    pub(super) contacts: HashMap<String, Contact>,
    pub(super) account: String,
    pub(super) wxid: String,
    pub(super) name: String,
    pub(super) head: String,
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
        lazy_static! {
            static ref MESSAGES: Regex = Regex::new(r"^message_\d+.sqlite$").unwrap();
        }
        let filename = Path::new(&file.relative_filename).name_str().to_string();
        if ["WCDB_Contact.sqlite", "MM.sqlite", "session.db"].contains(&filename.as_str())
            || MESSAGES.is_match(&filename)
        {
            if let Ok(tmpfile) = NamedTempFile::new().and_then(|mut tmpfile| {
                debug!(
                    "read file: {}, {}, {}",
                    self.account, file.fileid, file.relative_filename
                );
                let data = backup
                    .read_file(file)
                    .map_err(|e| Error::other(format!("{}", e)))?;
                tmpfile.write_all(&data)?;
                Ok(tmpfile)
            }) {
                match filename.as_str() {
                    "WCDB_Contact.sqlite" => self.contact = Some(Arc::new(tmpfile)),
                    "MM.sqlite" => self.messages.push(Arc::new(tmpfile)),
                    "session.db" => self.session = Some(Arc::new(tmpfile)),
                    _ if filename.starts_with("message_") => self.messages.push(Arc::new(tmpfile)),
                    _ => {}
                }
            } else {
                warn!("Failed to extract file: {}", file.relative_filename);
            }
        } else if filename == "mmsetting.archive" {
            self.setting = Some(file.clone());
        } else if filename.starts_with("mmsetting.archive.") {
            self.kv_setting = Some(file.clone())
        }
        self
    }

    pub fn is_complete(&self) -> bool {
        let has_contact = self.contact.is_some();
        let has_messages = !self.messages.is_empty();
        let has_setting = self.setting.is_some() || self.kv_setting.is_some();
        let has_session = self.session.is_some();
        let ret = has_contact && has_messages && has_setting;
        if !ret {
            warn!(
                "user db incomplete: account={}, wxid={}, name={}, contact_db={}, message_db={}, setting={}, kv_setting={}, session_db={}",
                self.account,
                self.wxid,
                self.name,
                has_contact,
                has_messages,
                self.setting.is_some(),
                self.kv_setting.is_some(),
                has_session
            );
        }
        ret
    }

    pub fn build(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        self.load_settings(backup)?;
        self.validate_owner_identity()?;
        self.load_contacts()?;
        self.load_sessions()?;
        self.load_chats()?;
        Ok(())
    }

    pub(super) fn validate_owner_identity(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.wxid.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("missing wxid for account {}", self.account),
            )
            .into());
        }
        if self.name.trim().is_empty() {
            warn!("missing display name for account {}", self.account);
        }
        if self.head.trim().is_empty() {
            warn!("missing avatar URL for account {}", self.account);
        }
        Ok(())
    }

    fn load_settings(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(setting) = &self.setting {
            let data = backup.read_file(setting)?;
            if let Some(array) = Value::from_reader(Cursor::new(data))
                .map_err(|e| {
                    warn!(
                        "failed to load settings: {}, {}",
                        setting.relative_filename, e
                    )
                })
                .ok()
                .and_then(|plist| {
                    plist
                        .as_dictionary()
                        .and_then(|dict| dict.get("$objects"))
                        .and_then(|obj| obj.as_array())
                        .cloned()
                })
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
                    self.head = array
                        .iter()
                        .filter_map(|v| v.as_string())
                        .find(|s| {
                            s.starts_with("http://")
                                && s.find("mmhead").is_some()
                                && s.find("/132").is_some()
                        })
                        .unwrap_or_default()
                        .to_string();
                }
            } else {
                warn!(
                    "failed to load settings: {}, {}",
                    setting.relative_filename, "array not exists"
                );
            }
        }
        if let Some(setting) = &self.kv_setting {
            let data = backup.read_file(setting)?;
            let map = MMMap::to_map(&data, None);
            self.wxid = if self.wxid.is_empty() {
                map.get("86").map(MMType::as_str).unwrap_or_default().into()
            } else {
                self.wxid.clone()
            };
            self.name = if self.name.is_empty() {
                map.get("88").map(MMType::as_str).unwrap_or_default().into()
            } else {
                self.name.clone()
            };
            self.head = if self.head.is_empty() {
                map.get("headimgurl")
                    .map(MMType::as_str)
                    .unwrap_or_default()
                    .into()
            } else {
                self.head.clone()
            };
        }
        if self.wxid.is_empty() || self.name.is_empty() || self.head.is_empty() {
            warn!(
                r#"lost some account info: "{}", "{}", "{}""#,
                self.wxid, self.name, self.head
            );
        }
        Ok(())
    }

    pub(super) fn get_conn(file: Option<Arc<NamedTempFile>>) -> SqliteResult<Option<Connection>> {
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
        for message in self.messages.iter() {
            if let Some(conn) = Self::get_conn(Some(message.clone()))? {
                let contact_keys = self.contacts.keys().map(|s| s.as_str()).collect::<Vec<_>>();
                let chats = conn
                        .prepare(r#"SELECT name FROM sqlite_master where type='table' and name like "Chat\_%" ESCAPE '\'"#)?
                        .query_map(params![], |row| {
                            let name: String = row.get(0)?;
                            let hash = &name[5..];
                            if !contact_keys.contains(&hash) && hash != self.account {
                                warn!("Contact info for chat not exists: {}", hash);
                            }
                            Ok((hash.into(), name))
                        })?
                        .filter_map(|r| {
                            r.map_err(|e| warn!("failed to parse chat list: {}", e))
                                .ok()
                        })
                        .collect::<HashMap<_, _>>();
                self.chats = self.chats.clone().into_iter().chain(chats).collect();
            }
        }
        Ok(())
    }

    fn load_sessions(&mut self) -> SqliteResult<()> {
        self.sessions = SessionIndex::load(self.session.clone())?;
        Ok(())
    }

    fn get_chat_ids(&self) -> Vec<String> {
        self.chats.keys().cloned().collect::<Vec<_>>()
    }

    fn get_contacts(&self) -> Vec<String> {
        self.find_contacts("")
    }

    pub(super) fn available_chat_summaries(&self) -> Vec<String> {
        let chat_keys = self.chats.keys().map(|s| s.as_str()).collect::<Vec<_>>();
        let mut summaries = self
            .contacts
            .iter()
            .filter(|(hash, _)| chat_keys.contains(&hash.as_str()))
            .map(|(hash, contact)| {
                format!(
                    "{} | {} | {}",
                    hash,
                    contact.name,
                    contact
                        .get_remark()
                        .unwrap_or_else(|e| format!("No Remark: {}", e))
                )
            })
            .collect::<Vec<_>>();
        summaries.extend(
            self.sessions
                .summaries
                .iter()
                .filter(|(chat_id, _)| chat_keys.contains(&gen_md5(chat_id).as_str()))
                .map(|(chat_id, session)| {
                    format!(
                        "{} | {} | {}",
                        gen_md5(chat_id),
                        chat_id,
                        session
                            .display_name
                            .as_deref()
                            .or(session.abstract_text.as_deref())
                            .unwrap_or_default()
                    )
                }),
        );
        summaries.sort();
        summaries.dedup();
        summaries
    }

    pub(super) fn find_contacts<S: ToString>(&self, name: S) -> Vec<String> {
        let name = name.to_string();
        let chat_keys = self.chats.keys().map(|s| s.as_str()).collect::<Vec<_>>();
        let mut contacts = self
            .sessions
            .summaries
            .iter()
            .filter_map(|(chat_id, session)| {
                let hash = gen_md5(chat_id);
                (chat_keys.iter().any(|&i| i == hash)
                    && (name.is_empty()
                        || hash == name
                        || chat_id.contains(&name)
                        || session
                            .display_name
                            .as_deref()
                            .is_some_and(|value| value.contains(&name))))
                .then_some(hash)
            })
            .collect::<Vec<_>>();
        contacts.extend(
            self.contacts
                .iter()
                .filter_map(|(hash, c)| {
                    if chat_keys.iter().any(|&i| i == hash) {
                        (name.is_empty()
                            || hash == &name
                            || c.name.find(&name).is_some()
                            || c.get_remark().ok().and_then(|r| r.find(&name)).is_some())
                        .then(|| {
                            warn!(
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
                .collect::<Vec<_>>(),
        );
        contacts.sort();
        contacts.dedup();
        contacts
    }

    fn load_records<S: ToString>(&self, backup: &Backup, chat_id: S) -> Option<Vec<RecordType>> {
        let chat_id = chat_id.to_string();
        let contact = self.contacts.get(&chat_id).cloned().unwrap_or_else(|| {
            warn!(
                "chat contact missing, preserving source chat id: {}",
                chat_id
            );
            Contact {
                name: chat_id.clone(),
                ..Default::default()
            }
        });
        load_record_lines(&self.messages, &self.chats, &chat_id)
            .map(|lines| self.transform_record_lines(backup, &contact, lines))
            .map_err(|e| warn!("failed to get chat line: {}", e))
            .ok()
    }

    pub fn get_record_names(&self, names: Option<Vec<String>>) -> Vec<String> {
        match names {
            None => self.get_chat_ids(),
            Some(names) if names.is_empty() => self.get_contacts(),
            Some(names) => names,
        }
    }

    pub fn get_records(&self, backup: &Backup, name: String) -> Vec<RecordType> {
        let mut contacts = self.find_contacts(&name);
        if contacts.is_empty() && self.chats.contains_key(&name) {
            contacts.push(name.clone());
        }
        if contacts.is_empty() {
            let available = self.available_chat_summaries();
            if available.is_empty() {
                warn!(
                    "chat selector did not match and no available chat contacts were found: account={}, selector={}",
                    self.account, name
                );
            } else {
                warn!(
                    "chat selector did not match: account={}, selector={}. Available chats: {}",
                    self.account,
                    name,
                    available.join("; ")
                );
            }
        }
        contacts
            .iter()
            .filter_map(|chat_id| {
                info!("Extracting: {} => {}", name, chat_id);
                self.load_records(backup, chat_id)
            })
            .flatten()
            .collect::<Vec<_>>()
    }
}
