use super::*;
use ibackuptool2::{Backup, BackupFile};
use plist::Value;
use std::collections::HashMap;
use std::io::Cursor;

#[derive(Clone, Default, Debug)]
struct UserDB {
    contact: Option<BackupFile>,
    message: Option<BackupFile>,
    setting: Option<BackupFile>,
    session: Option<BackupFile>,
    account: String,
    wxid: String,
    name: String,
    head: String,
}

impl UserDB {
    pub fn new(user_id: String, file: &BackupFile) -> Self {
        let mut user_db = Self::default();
        user_db.account = user_id;
        user_db.match_path(file)
    }

    pub fn with(self, file: &BackupFile) -> Self {
        self.match_path(file)
    }

    fn match_path(mut self, file: &BackupFile) -> Self {
        match Path::new(&file.relative_filename).name_str() {
            "WCDB_Contact.sqlite" => self.contact = Some(file.clone()),
            "MM.sqlite" => self.message = Some(file.clone()),
            "mmsetting.archive" => self.setting = Some(file.clone()),
            "session.db" => self.session = Some(file.clone()),
            _ => {}
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
                        user_map.insert(user_id, user.with(file));
                    } else {
                        user_map.insert(user_id.clone(), UserDB::new(user_id, file));
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
                user.load_settings(&backup)
                    .map(|_| (user_id.clone(), user))
                    .map_err(|e| warn!("failed to load settings: {}", e))
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

