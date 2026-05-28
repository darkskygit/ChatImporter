use super::account::UserDB;
use super::*;

#[allow(non_camel_case_types)]
pub(super) struct Extractor {
    backup: Backup,
    user_info: HashMap<String, UserDB>,
}

impl Extractor {
    pub fn from_backup(backup: Backup) -> Result<Self, Box<dyn std::error::Error>> {
        let user_info = Self::get_user_info(&backup);
        if user_info.is_empty() {
            warn!("no complete user database found in backup");
        }
        Ok(Self { backup, user_info })
    }

    fn get_user_info(backup: &Backup) -> HashMap<String, UserDB> {
        const MATCHED_NAME: [&str; 5] = [
            "WCDB_Contact.sqlite",
            "MM.sqlite",
            "message_",
            "mmsetting.archive",
            "session.db",
        ];
        let mut user_map = HashMap::new();
        let paths = [
            backup.find_wildcard_paths(DOMAIN, "*/WCDB_Contact.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/MM.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/message_*.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/mmsetting.archive"),
            backup.find_wildcard_paths(DOMAIN, "*/mmsetting.archive.*"),
            backup.find_wildcard_paths(DOMAIN, "*/session/session.db"),
        ];
        if paths.iter().all(Vec::is_empty) {
            warn!(
                "no database files found in backup domain {}; expected WCDB_Contact.sqlite, MM.sqlite/message_*.sqlite, mmsetting.archive, and session.db",
                DOMAIN
            );
        }
        for file in paths.iter().flatten() {
            let path = Path::new(&file.relative_filename);
            if MATCHED_NAME.contains(&path.name_str())
                || path.name_str().starts_with(MATCHED_NAME[2])
                || path.name_str().starts_with(MATCHED_NAME[3])
            {
                if let Some(mut user_id) = path
                    .strip_prefix("Documents")
                    .ok()
                    .and_then(|p| p.components().next())
                    .map(|user_id| user_id.name_str().to_string())
                {
                    if user_id == "MMappedKV" {
                        if let Some(wxid) = mmsetting_archive_wxid(path.name_str()) {
                            user_id = gen_md5(wxid);
                        } else {
                            continue;
                        };
                    }
                    if let Some(user) = user_map.remove(&user_id) {
                        let user: UserDB = user;
                        user_map.insert(user_id, user.with(backup, file));
                    } else {
                        user_map.insert(
                            user_id.clone(),
                            UserDB::new(
                                backup,
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
                user.build(backup)
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

pub(super) fn mmsetting_archive_wxid(filename: &str) -> Option<&str> {
    let wxid = filename.strip_prefix("mmsetting.archive.")?;
    if wxid.is_empty() || wxid.ends_with(".crc") {
        None
    } else {
        Some(wxid)
    }
}
