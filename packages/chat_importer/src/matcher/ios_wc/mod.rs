use super::*;
use ibackuptool2::{Backup, BackupFile};
use num_enum::TryFromPrimitive;
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{Cursor, Error, ErrorKind, Write};
use std::iter::IntoIterator;
use std::str::{from_utf8, Utf8Error};
use std::sync::Arc;
use tempfile::NamedTempFile;

const DOMAIN: &str = "AppDomain-com.tencent.xin";

mod account;
mod appmsg;
mod backup;
mod basic;
mod contact;
mod media;
mod message;
mod metadata;
mod mmap;
mod session;
mod system;
mod transform;
mod xml;

#[cfg(test)]
use account::*;
#[cfg(test)]
use appmsg::*;
use backup::*;
#[cfg(test)]
use basic::*;
#[cfg(test)]
use contact::*;
#[cfg(test)]
use media::*;
#[cfg(test)]
use message::*;
use metadata::*;
#[cfg(test)]
use system::*;

struct IosWcMetadataMerger;

#[async_trait::async_trait]
impl MetadataMerger for IosWcMetadataMerger {
    async fn merge(
        &self,
        store: &ChatStore,
        attaches: &Attachments,
        old: Vec<u8>,
        new: Vec<u8>,
    ) -> Option<Vec<u8>> {
        if let Ok((old, new)) = from_slice(&old)
            .map_err(|e| error!("Failed to parse old metadata: {}", e))
            .and_then(|old| {
                from_slice::<IosWcMetadata>(&new)
                    .map_err(|e| panic!("Failed to parse new metadata: {}", e))
                    .map(|new| (old, new))
            })
        {
            to_vec(&new.merge(store, attaches, old).await)
                .map_err(|e| error!("Failed to serialize metadata: {}", e))
                .ok()
        } else {
            Some(new)
        }
    }
}

impl MsgMatcher for Matcher {
    fn import_plan(&self) -> ImportPlan {
        let chats_total = self
            .extract_ids
            .iter()
            .filter_map(|u| self.extractor.get_user_db(u))
            .map(|(user_db, _)| user_db.get_record_names(self.names.clone()).len() as u64)
            .sum::<u64>();
        ImportPlan {
            chats_total: Some(chats_total),
        }
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
        let jobs = self
            .extract_ids
            .iter()
            .filter_map(|u| self.extractor.get_user_db(u))
            .flat_map(|(user_db, backup)| {
                user_db
                    .get_record_names(self.names.clone())
                    .into_iter()
                    .map(move |name| (user_db, backup, name))
            })
            .collect::<Vec<_>>();
        progress.chat_planned(jobs.len() as u64);
        if jobs.is_empty() {
            return Ok(Vec::new());
        }

        let worker_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .min(jobs.len());
        let jobs = Arc::new(std::sync::Mutex::new(
            jobs.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));
        let results = Arc::new(std::sync::Mutex::new(Vec::<RecordBatch>::new()));
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let jobs = Arc::clone(&jobs);
                let results = Arc::clone(&results);
                scope.spawn(move || loop {
                    let Some((user_db, backup, name)) =
                        jobs.lock().expect("ios wc job queue poisoned").pop_front()
                    else {
                        break;
                    };
                    let records = user_db.get_records(backup, name.clone());
                    progress.chat_parsed(records.len() as u64, record_blob_count(&records) as u64);
                    results
                        .lock()
                        .expect("ios wc result queue poisoned")
                        .push(RecordBatch {
                            label: format!("{}:{}", user_db.account, name),
                            records,
                        });
                });
            }
        });
        let mut results = Arc::try_unwrap(results)
            .map_err(|_| anyhow::anyhow!("ios wc result queue still shared"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("ios wc result queue poisoned"))?;
        results.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(results)
    }

    fn get_metadata_merger(&self) -> Option<Box<dyn MetadataMerger>> {
        Some(Box::new(IosWcMetadataMerger))
    }
}

#[allow(non_camel_case_types)]
pub struct Matcher {
    extractor: Extractor,
    extract_ids: Vec<String>,
    names: Option<Vec<String>>,
}

impl Matcher {
    pub fn from_backup(backup: Backup, names: Option<Vec<String>>) -> Result<Box<dyn MsgMatcher>> {
        let extractor = Extractor::from_backup(backup).map_err(|e| anyhow::anyhow!("{}", e))?;
        let extract_ids = extractor.get_users();
        Ok(Box::new(Self {
            extractor,
            extract_ids,
            names,
        }) as Box<dyn MsgMatcher>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgba};
    use std::fs;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn png(seed: u8) -> Vec<u8> {
        let mut image = ImageBuffer::new(10, 10);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([seed.wrapping_add(x as u8), y as u8, 128, 255]);
        }
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn sqlite_bytes(name: &str, build: impl FnOnce(&Connection)) -> Vec<u8> {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        let conn = Connection::open(&path).unwrap();
        build(&conn);
        drop(conn);
        fs::read(path).unwrap()
    }

    fn settings_bytes(wxid: &str, display_name: &str) -> Vec<u8> {
        let mut settings = plist::Dictionary::new();
        settings.insert(
            "$objects".into(),
            Value::Array(vec![
                Value::String("$null".into()),
                Value::String("unused".into()),
                Value::String(wxid.into()),
                Value::String(display_name.into()),
                Value::String("http://wx.qlogo.cn/mmhead/test/132".into()),
            ]),
        );
        let mut settings_data = Vec::new();
        plist::to_writer_binary(&mut settings_data, &Value::Dictionary(settings)).unwrap();
        settings_data
    }

    fn write_backup_file(
        dir: &Path,
        conn: &Connection,
        index: usize,
        relative_path: &str,
        bytes: Vec<u8>,
    ) {
        let fileid = format!("aa{:038x}", index);
        let object_dir = dir.join(&fileid[..2]);
        fs::create_dir_all(&object_dir).unwrap();
        fs::write(object_dir.join(&fileid), bytes).unwrap();
        conn.execute(
            "INSERT INTO Files VALUES (?1, ?2, ?3, 1, X'00')",
            (&fileid, DOMAIN, relative_path),
        )
        .unwrap();
    }

    fn write_manifest_db(dir: &Path, files: Vec<(String, Vec<u8>)>) {
        let conn = Connection::open(dir.join("Manifest.db")).unwrap();
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
        for (index, (relative_path, bytes)) in files.into_iter().enumerate() {
            write_backup_file(dir, &conn, index + 1, &relative_path, bytes);
        }
    }

    fn fixture_contact_db() -> Vec<u8> {
        sqlite_bytes("WCDB_Contact.sqlite", |conn| {
            conn.execute(
                "CREATE TABLE Friend (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB NOT NULL,
                    dbContactChatRoom BLOB NOT NULL,
                    dbContactHeadImage BLOB NOT NULL,
                    type INTEGER NOT NULL
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE OpenIMContact (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB,
                    dbContactHeadImage BLOB,
                    type INTEGER
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE ChatRoom (
                    chatRoomName TEXT NOT NULL,
                    roomData BLOB,
                    roomInfoXml TEXT
                )",
                [],
            )
            .unwrap();
            let contacts = [
                (
                    "friend",
                    vec![10, 5, b'A', b'l', b'i', b'c', b'e'],
                    Vec::new(),
                    3,
                ),
                (
                    "room@chatroom",
                    vec![10, 4, b'R', b'o', b'o', b'm'],
                    b"roomdata-protobuf-placeholder".to_vec(),
                    2,
                ),
                (
                    "member",
                    vec![10, 6, b'M', b'e', b'm', b'b', b'e', b'r'],
                    Vec::new(),
                    3,
                ),
            ];
            for (username, remark, room_data, user_type) in contacts {
                conn.execute(
                    "INSERT INTO Friend VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        username,
                        remark,
                        room_data,
                        b"http://wx.qlogo.cn/mmhead/contact/0".to_vec(),
                        user_type,
                    ),
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO OpenIMContact VALUES (?1, ?2, ?3, ?4)",
                (
                    "openim",
                    b"\n\x06OpenIM".to_vec(),
                    b"http://wx.qlogo.cn/mmhead/openim/0".to_vec(),
                    4,
                ),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ChatRoom VALUES (?1, ?2, ?3)",
                (
                    "room@chatroom",
                    b"<RoomData/>".to_vec(),
                    "<RoomData><Member UserName=\"member\"><DisplayName><![CDATA[Member Name]]></DisplayName></Member><Member UserName=\"missing_member\"><DisplayName><![CDATA[Missing Display]]></DisplayName></Member></RoomData>",
                ),
            )
            .unwrap();
        })
    }

    fn fixture_message_db() -> Vec<u8> {
        sqlite_bytes("message_0.sqlite", |conn| {
            for chat in [
                gen_md5("friend"),
                gen_md5("room@chatroom"),
                gen_md5("openim"),
            ] {
                conn.execute(
                    &format!(
                        "CREATE TABLE Chat_{} (
                            MesLocalID INTEGER NOT NULL,
                            MesSvrID INTEGER NOT NULL,
                            CreateTime INTEGER NOT NULL,
                            Message TEXT NOT NULL,
                            Status INTEGER NOT NULL,
                            ImgStatus INTEGER NOT NULL,
                            Type INTEGER NOT NULL,
                            Des INTEGER NOT NULL
                        )",
                        chat
                    ),
                    [],
                )
                .unwrap();
            }
            let friend_chat = format!("Chat_{}", gen_md5("friend"));
            let room_chat = format!("Chat_{}", gen_md5("room@chatroom"));
            let openim_chat = format!("Chat_{}", gen_md5("openim"));
            let rows = [
            (1, 101, 1_598_219_157, "hello", 1, 0, 1, 0),
            (
                2,
                102,
                1_598_219_158,
                r#"<msg><img cdnthumburl="thumb" cdnmidimgurl="mid" cdnbigimgurl="hd" aeskey="key"/></msg>"#,
                1,
                0,
                3,
                0,
            ),
            (
                3,
                103,
                1_598_219_159,
                r#"<msg><videomsg cdnvideourl="video" aeskey="key"/></msg>"#,
                1,
                0,
                43,
                0,
            ),
            (
                4,
                104,
                1_598_219_160,
                r#"<msg><voicemsg clientmsgid="client" bufid="buffer"/></msg>"#,
                1,
                0,
                34,
                0,
            ),
            (
                5,
                105,
                1_598_219_161,
                r#"<msg><appmsg><title><![CDATA[Doc]]></title><des><![CDATA[Desc]]></des><url><![CDATA[https://example.test]]></url><type>5</type></appmsg></msg>"#,
                1,
                0,
                49,
                0,
            ),
            (6, 106, 1_598_219_162, "system text", 1, 0, 10000, 0),
            (
                7,
                107,
                1_598_219_163,
                "<sysmsg><revokemsg><revokecontent>recalled</revokecontent></revokemsg></sysmsg>",
                1,
                0,
                10002,
                0,
            ),
        ];
            for row in rows {
                conn.execute(
                    &format!(
                        "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        friend_chat
                    ),
                    row,
                )
                .unwrap();
            }
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    room_chat
                ),
                (8, 108, 1_598_219_164, "member:\nhello group", 1, 0, 1, 1),
            )
            .unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    room_chat
                ),
                (
                    10,
                    110,
                    1_598_219_166,
                    "missing_member:\nhello unknown",
                    1,
                    0,
                    1,
                    1,
                ),
            )
            .unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    openim_chat
                ),
                (9, 109, 1_598_219_165, "openim hello", 1, 0, 1, 0),
            )
            .unwrap();
        })
    }

    #[test]
    fn available_chat_summaries_include_selector_ids() {
        let mut user_db = UserDB::default();
        user_db
            .chats
            .insert("chat-hash".into(), "Chat_chat-hash".into());
        user_db.contacts.insert(
            "chat-hash".into(),
            Contact {
                name: "alice".into(),
                ..Default::default()
            },
        );
        user_db.contacts.insert(
            "no-chat".into(),
            Contact {
                name: "bob".into(),
                ..Default::default()
            },
        );

        assert_eq!(user_db.find_contacts("missing"), Vec::<String>::new());
        let summaries = user_db.available_chat_summaries();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].contains("chat-hash"));
        assert!(summaries[0].contains("alice"));
    }

    #[test]
    fn user_db_rejects_missing_wxid() {
        let user_db = UserDB {
            account: "account-a".into(),
            name: "Display Name".into(),
            ..Default::default()
        };

        assert!(user_db.validate_owner_identity().is_err());
    }

    #[test]
    fn user_db_allows_missing_name_and_head_when_wxid_exists() {
        let user_db = UserDB {
            account: "account-a".into(),
            wxid: "wxid_a".into(),
            ..Default::default()
        };

        assert!(user_db.validate_owner_identity().is_ok());
    }

    #[test]
    fn user_db_is_complete_without_session_db() {
        let user_db = UserDB {
            contact: Some(Arc::new(NamedTempFile::new().unwrap())),
            messages: vec![Arc::new(NamedTempFile::new().unwrap())],
            setting: Some(BackupFile {
                fileid: "id".into(),
                domain: DOMAIN.into(),
                relative_filename: "Documents/account-a/mmsetting.archive".into(),
                flags: 1,
                fileinfo: None,
            }),
            session: None,
            ..Default::default()
        };

        assert!(user_db.is_complete());
    }

    #[test]
    fn load_contacts_accepts_minimal_friend_schema() {
        let bytes = sqlite_bytes("WCDB_Contact.sqlite", |conn| {
            conn.execute(
                "CREATE TABLE Friend (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB NOT NULL,
                    dbContactHeadImage BLOB NOT NULL,
                    type INTEGER NOT NULL
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO Friend VALUES (?1, ?2, ?3, ?4)",
                (
                    "minimal",
                    vec![10, 7, b'M', b'i', b'n', b'i', b'm', b'a', b'l'],
                    b"http://wx.qlogo.cn/mmhead/minimal/0".to_vec(),
                    3,
                ),
            )
            .unwrap();
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let mut user_db = UserDB {
            contact: Some(Arc::new(file)),
            ..Default::default()
        };

        user_db.load_contacts().unwrap();

        let contact = user_db.contacts.get(&gen_md5("minimal")).unwrap();
        assert_eq!(contact.name, "minimal");
        assert_eq!(contact.get_remark().unwrap(), "Minimal");
        assert!(contact
            .chatroom
            .as_ref()
            .is_none_or(|room| room.members.is_empty()));
    }

    #[test]
    fn load_contacts_accepts_null_contact_fields() {
        let bytes = sqlite_bytes("WCDB_Contact.sqlite", |conn| {
            conn.execute(
                "CREATE TABLE Friend (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB,
                    dbContactChatRoom BLOB,
                    dbContactHeadImage BLOB,
                    type INTEGER,
                    dbContactProfile BLOB
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE OpenIMContact (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB,
                    dbContactHeadImage BLOB,
                    type INTEGER
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE ChatRoom (
                    chatRoomName TEXT NOT NULL,
                    roomInfoXml TEXT
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO Friend VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    "nullable@chatroom",
                    Option::<Vec<u8>>::None,
                    Option::<Vec<u8>>::None,
                    Option::<Vec<u8>>::None,
                    Option::<i32>::None,
                    Option::<Vec<u8>>::None,
                ),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO OpenIMContact VALUES (?1, ?2, ?3, ?4)",
                (
                    "nullable-openim",
                    Option::<Vec<u8>>::None,
                    Option::<Vec<u8>>::None,
                    Option::<i32>::None,
                ),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ChatRoom VALUES (?1, ?2)",
                ("nullable@chatroom", Option::<String>::None),
            )
            .unwrap();
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let mut user_db = UserDB {
            contact: Some(Arc::new(file)),
            ..Default::default()
        };

        user_db.load_contacts().unwrap();

        let contact = user_db.contacts.get(&gen_md5("nullable@chatroom")).unwrap();
        assert_eq!(contact.name, "nullable@chatroom");
        assert_eq!(contact.get_remark().unwrap(), "");
        assert_eq!(contact.user_type, 0);
        assert!(contact
            .chatroom
            .as_ref()
            .is_none_or(|room| room.members.is_empty()));

        let openim = user_db.contacts.get(&gen_md5("nullable-openim")).unwrap();
        assert!(openim.is_openim);
        assert_eq!(openim.get_remark().unwrap(), "");
        assert_eq!(openim.user_type, 0);
    }

    #[test]
    fn ios_wechat_minimal_sqlite_backup_imports_without_session_db() {
        let dir = tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());
        let account = "account-a";
        let friend_hash = gen_md5("friend");
        let files = vec![
            (
                "Documents/account-a/WCDB_Contact.sqlite".into(),
                fixture_contact_db(),
            ),
            (
                "Documents/account-a/message_0.sqlite".into(),
                fixture_message_db(),
            ),
            (
                "Documents/account-a/mmsetting.archive".into(),
                settings_bytes("owner-wxid", "Owner Name"),
            ),
            (
                format!("Documents/{}/Img/{}/2.pic", account, friend_hash),
                png(11),
            ),
            (
                format!("Documents/{}/Img/{}/2.pic_hd", account, friend_hash),
                png(12),
            ),
            (
                format!("Documents/{}/Img/{}/2.pic_thum", account, friend_hash),
                png(13),
            ),
            (
                format!("Documents/{}/Video/{}/3.mp4", account, friend_hash),
                b"video-bytes".to_vec(),
            ),
            (
                format!("Documents/{}/Video/{}/3.video_thum", account, friend_hash),
                png(14),
            ),
            (
                format!("Documents/{}/Audio/{}/4.aud", account, friend_hash),
                b"voice-bytes".to_vec(),
            ),
            (
                format!("Documents/{}/OpenData/{}/5/file.bin", account, friend_hash),
                b"open-data".to_vec(),
            ),
        ];
        write_manifest_db(dir.path(), files);

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let matcher = Matcher::from_backup(backup, None).unwrap();
        let records = matcher.collect_records().unwrap();

        assert_eq!(records.len(), 10);
        assert!(records
            .iter()
            .any(|record| record.get_record().content == "hello"));
        assert!(records.iter().any(|record| {
            let record = record.get_record();
            record.group_id == "openim" && record.content == "openim hello"
        }));
        assert!(records.iter().any(|record| {
            let record = record.get_record();
            record.group_id == "room@chatroom"
                && record.sender_id == "member"
                && record.sender_name == "Member Name"
        }));
        assert!(records.iter().any(|record| {
            let record = record.get_record();
            record.group_id == "room@chatroom"
                && record.sender_id == "missing_member"
                && record.sender_name == "Missing Display"
                && record.content == "hello unknown"
        }));
        assert!(records.iter().any(|record| {
            record.get_record().content == "[img]" && record.attachment_count() == 3
        }));
        assert!(records.iter().any(|record| {
            record.get_record().content == "[video]" && record.attachment_count() == 2
        }));
        assert!(records.iter().any(|record| {
            record.get_record().content == "[voice]" && record.attachment_count() == 1
        }));
        let app = records
            .iter()
            .find(|record| record.get_record().content == "[link] Doc")
            .unwrap();
        assert_eq!(app.attachment_count(), 1);
        let app_metadata: IosWcMetadata =
            from_slice(app.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(app_metadata.msg_type, MsgType::CustomApp);
        assert_eq!(
            app_metadata.field("title"),
            Some(&MetadataValue::Str("Doc".into()))
        );
        assert_eq!(
            app_metadata.field("description"),
            Some(&MetadataValue::Str("Desc".into()))
        );
        assert_eq!(
            app_metadata.field("url"),
            Some(&MetadataValue::Str("https://example.test".into()))
        );
        let system = records
            .iter()
            .find(|record| record.get_record().content == "[system:plain]")
            .unwrap();
        let system_metadata: IosWcMetadata =
            from_slice(system.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(system_metadata.msg_type, MsgType::System);
        assert_eq!(
            system_metadata.field("system_type"),
            Some(&MetadataValue::Str("plain".into()))
        );
        assert_eq!(
            system_metadata.field("content"),
            Some(&MetadataValue::Str("system text".into()))
        );
        let revoke = records
            .iter()
            .find(|record| record.get_record().content == "[revoke]")
            .unwrap();
        let revoke_metadata: IosWcMetadata =
            from_slice(revoke.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(revoke_metadata.msg_type, MsgType::Revoke);
        assert_eq!(
            revoke_metadata.field("revoke"),
            Some(&MetadataValue::Str("recalled".into()))
        );
        assert!(records.iter().any(|record| {
            record
                .get_record()
                .source_message_id
                .as_deref()
                .is_some_and(|id| id == "svr:friend:101")
        }));
    }

    #[test]
    fn ios_wechat_chat_table_imports_without_friend_contact() {
        let dir = tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());
        let missing_chat = "missing_chat";
        let contact_db = sqlite_bytes("WCDB_Contact.sqlite", |conn| {
            conn.execute(
                "CREATE TABLE Friend (
                    userName TEXT NOT NULL,
                    dbContactRemark BLOB NOT NULL,
                    dbContactHeadImage BLOB NOT NULL,
                    type INTEGER NOT NULL
                )",
                [],
            )
            .unwrap();
        });
        let message_db = sqlite_bytes("message_0.sqlite", |conn| {
            let table = format!("Chat_{}", gen_md5(missing_chat));
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        MesLocalID INTEGER NOT NULL,
                        MesSvrID INTEGER NOT NULL,
                        CreateTime INTEGER NOT NULL,
                        Message TEXT NOT NULL,
                        Status INTEGER NOT NULL,
                        ImgStatus INTEGER NOT NULL,
                        Type INTEGER NOT NULL,
                        Des INTEGER NOT NULL
                    )",
                    table
                ),
                [],
            )
            .unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    table
                ),
                (1, 42, 1_598_219_157, "orphan chat", 1, 0, 1, 0),
            )
            .unwrap();
        });
        write_manifest_db(
            dir.path(),
            vec![
                ("Documents/account-a/WCDB_Contact.sqlite".into(), contact_db),
                ("Documents/account-a/message_0.sqlite".into(), message_db),
                (
                    "Documents/account-a/mmsetting.archive".into(),
                    settings_bytes("owner-wxid", "Owner Name"),
                ),
            ],
        );

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let matcher = Matcher::from_backup(backup, Some(vec![gen_md5(missing_chat)])).unwrap();
        let records = matcher.collect_records().unwrap();

        assert_eq!(records.len(), 1);
        let record = records[0].get_record();
        assert_eq!(record.group_id, gen_md5(missing_chat));
        assert_eq!(record.content, "orphan chat");
    }

    #[test]
    fn extractor_skips_user_db_with_missing_wxid_without_records() {
        let dir = tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());

        let mut settings = plist::Dictionary::new();
        settings.insert(
            "$objects".into(),
            Value::Array(vec![
                Value::String("$null".into()),
                Value::String("unused".into()),
                Value::String("".into()),
                Value::String("Display Name".into()),
            ]),
        );
        let mut settings_data = Vec::new();
        plist::to_writer_binary(&mut settings_data, &Value::Dictionary(settings)).unwrap();

        let files = [
            (
                "aa00000000000000000000000000000000000001",
                "Documents/account-a/WCDB_Contact.sqlite",
                Vec::new(),
            ),
            (
                "aa00000000000000000000000000000000000002",
                "Documents/account-a/message_0.sqlite",
                Vec::new(),
            ),
            (
                "aa00000000000000000000000000000000000003",
                "Documents/account-a/session/session.db",
                Vec::new(),
            ),
            (
                "aa00000000000000000000000000000000000004",
                "Documents/account-a/mmsetting.archive",
                settings_data,
            ),
        ];

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
        for (fileid, relative_path, bytes) in files {
            let object_dir = dir.path().join(&fileid[..2]);
            std::fs::create_dir_all(&object_dir).unwrap();
            std::fs::write(object_dir.join(fileid), bytes).unwrap();
            conn.execute(
                "INSERT INTO Files VALUES (?1, ?2, ?3, 1, X'00')",
                (fileid, DOMAIN, relative_path),
            )
            .unwrap();
        }
        drop(conn);

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let matcher = Matcher::from_backup(backup, None).unwrap();
        let records = matcher.collect_records().unwrap();
        assert!(records.is_empty());
    }

    fn write_minimal_backup_metadata(dir: &Path) {
        let mut status = plist::Dictionary::new();
        status.insert("BackupState".into(), "new".into());
        status.insert("Date".into(), "2026-05-25".into());
        status.insert("IsFullBackup".into(), true.into());
        status.insert("SnapshotState".into(), "finished".into());
        status.insert("UUID".into(), "backup-uuid".into());
        status.insert("Version".into(), "2.4".into());
        plist::to_file_xml(dir.join("Status.plist"), &Value::Dictionary(status)).unwrap();

        let mut info = plist::Dictionary::new();
        info.insert("Product Type".into(), "iPhone".into());
        info.insert("Product Version".into(), "18.0".into());
        info.insert("Target Identifier".into(), "device-id".into());
        info.insert("Target Type".into(), "Device".into());
        plist::to_file_xml(dir.join("Info.plist"), &Value::Dictionary(info)).unwrap();

        let mut lockdown = plist::Dictionary::new();
        lockdown.insert("ProductVersion".into(), "18.0".into());
        lockdown.insert("ProductType".into(), "iPhone".into());
        lockdown.insert("UniqueDeviceID".into(), "device-id".into());
        lockdown.insert("SerialNumber".into(), "serial".into());
        lockdown.insert("DeviceName".into(), "Test Phone".into());
        let mut manifest = plist::Dictionary::new();
        manifest.insert("IsEncrypted".into(), false.into());
        manifest.insert("Version".into(), "9.1".into());
        manifest.insert("Date".into(), "2026-05-25".into());
        manifest.insert("SystemDomainsVersion".into(), "20".into());
        manifest.insert("WasPasscodeSet".into(), false.into());
        manifest.insert("Lockdown".into(), Value::Dictionary(lockdown));
        plist::to_file_xml(dir.join("Manifest.plist"), &Value::Dictionary(manifest)).unwrap();
    }

    #[test]
    fn ios_wechat_second_create_time_is_converted_to_millis() {
        let timestamp = UserDB::create_time_timestamp_millis(1_598_219_157, 42);

        assert!((1_598_219_157_000..1_598_219_158_000).contains(&timestamp));
    }

    #[test]
    fn ios_wechat_millisecond_create_time_is_not_multiplied_again() {
        assert_eq!(
            UserDB::create_time_timestamp_millis(1_598_219_157_438, 42),
            1_598_219_157_438
        );
    }

    #[tokio::test]
    async fn ios_wechat_same_sender_second_collision_uses_source_message_id() {
        let backup_dir = tempdir().unwrap();
        write_minimal_backup_metadata(backup_dir.path());
        let backup = Backup::new(backup_dir.path()).unwrap();
        let user_db = UserDB {
            account: "account-a".into(),
            wxid: "owner".into(),
            name: "Owner".into(),
            ..Default::default()
        };
        let contact = Contact {
            name: "chat-a".into(),
            ..Default::default()
        };
        let first_server_id = 173_689_208_819_417_360;
        let second_server_id = 2_105_540_167_727_085_274;
        assert_eq!(
            UserDB::create_time_timestamp_millis(1_513_479_186, first_server_id),
            UserDB::create_time_timestamp_millis(1_513_479_186, second_server_id)
        );

        let first = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 988,
                    server_id: first_server_id,
                    created_time: 1_513_479_186,
                    message: "first".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::Normal,
                    is_dest: false,
                },
                &contact,
            )
            .unwrap();
        let second = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 991,
                    server_id: second_server_id,
                    created_time: 1_513_479_186,
                    message: "second".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::Normal,
                    is_dest: false,
                },
                &contact,
            )
            .unwrap();

        assert_ne!(
            first.get_record().source_message_id,
            second.get_record().source_message_id
        );
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![first, second],
            },
        )
        .await
        .unwrap();

        let stored = store.query(crate::store::Query::default()).await.unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn ios_wechat_same_server_id_across_local_ids_dedupes() {
        let backup_dir = tempdir().unwrap();
        write_minimal_backup_metadata(backup_dir.path());
        let backup = Backup::new(backup_dir.path()).unwrap();
        let user_db = UserDB {
            account: "account-a".into(),
            wxid: "owner".into(),
            name: "Owner".into(),
            ..Default::default()
        };
        let contact = Contact {
            name: "chat-a".into(),
            ..Default::default()
        };
        let first = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 1,
                    server_id: 42,
                    created_time: 1_513_479_186,
                    message: "first".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::Normal,
                    is_dest: false,
                },
                &contact,
            )
            .unwrap();
        let second = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 999,
                    server_id: 42,
                    created_time: 1_513_479_186,
                    message: "updated".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::Normal,
                    is_dest: false,
                },
                &contact,
            )
            .unwrap();
        assert_eq!(
            first.get_record().source_message_id,
            second.get_record().source_message_id
        );

        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![first, second],
            },
        )
        .await
        .unwrap();

        let stored = store.query(crate::store::Query::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "updated");
    }

    #[test]
    fn ios_wechat_source_message_id_fallback_includes_local_facts() {
        let contact = Contact {
            name: "chat-a".into(),
            ..Default::default()
        };
        let line = RecordLine {
            local_id: 7,
            server_id: 0,
            created_time: 1_598_219_157,
            message: "fallback".into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Normal,
            is_dest: false,
        };
        let first = UserDB::source_message_id(&contact, &line, "fallback", &HashMap::new());
        let second = UserDB::source_message_id(&contact, &line, "changed", &HashMap::new());
        let with_attachment = UserDB::source_message_id(
            &contact,
            &line,
            "fallback",
            &[("asset".into(), b"bytes".to_vec())]
                .iter()
                .cloned()
                .collect(),
        );

        assert!(first.starts_with("fallback:chat-a:1598219157:7:"));
        assert_ne!(first, second);
        assert_ne!(first, with_attachment);
    }

    #[test]
    fn ios_wechat_typed_metadata_serializes_media_roles() {
        let metadata = IosWcMetadata::new()
            .with_type(MsgType::Image)
            .with_hash("mid".into(), "hash-img".into())
            .with_hash("thumb".into(), "hash-thum".into())
            .with_tag("cdn".into(), "https://cdn.test/image".into());

        let encoded = to_vec(&metadata).unwrap();
        let decoded: IosWcMetadata = from_slice(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MsgType::Image);
        assert_eq!(decoded.media_hash("mid"), Some("hash-img"));
        assert_eq!(decoded.media_hash("thumb"), Some("hash-thum"));
        assert_eq!(
            decoded.field("cdn"),
            Some(&MetadataValue::Str("https://cdn.test/image".into()))
        );
    }

    #[test]
    fn ios_wechat_invalid_media_xml_records_parse_error() {
        let image_metadata = RecordLine {
            local_id: 1,
            server_id: 1,
            created_time: 1,
            message: "<!DOCTYPE msg><msg><img /></msg>".into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Image,
            is_dest: false,
        };
        let image_metadata =
            MediaResolver::image_metadata(&image_metadata).with_type(MsgType::Image);
        let voice_metadata = RecordLine {
            local_id: 2,
            server_id: 2,
            created_time: 2,
            message: "<!DOCTYPE msg><msg><voicemsg /></msg>".into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Voice,
            is_dest: false,
        };
        let voice_metadata =
            MediaResolver::audio_metadata(&voice_metadata).with_type(MsgType::Voice);

        assert_eq!(image_metadata.msg_type, MsgType::Image);
        assert_eq!(
            image_metadata.raw.parse_error.as_deref(),
            Some("invalid image xml")
        );
        assert_eq!(voice_metadata.msg_type, MsgType::Voice);
        assert_eq!(
            voice_metadata.raw.parse_error.as_deref(),
            Some("invalid voice xml")
        );
    }

    #[test]
    fn ios_wechat_invalid_non_media_xml_records_parse_error() {
        let invalid = "<!DOCTYPE msg><msg />";
        let contact_line = RecordLine {
            local_id: 1,
            server_id: 1,
            created_time: 1,
            message: invalid.into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::ContactShare,
            is_dest: false,
        };
        let contact_metadata = parse_contact_share(&contact_line).with_type(MsgType::ContactShare);
        let location_line = RecordLine {
            local_id: 2,
            server_id: 2,
            created_time: 2,
            message: invalid.into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Location,
            is_dest: false,
        };
        let location_metadata = parse_location(&location_line).with_type(MsgType::Location);
        let revoke_line = RecordLine {
            local_id: 3,
            server_id: 3,
            created_time: 3,
            message: invalid.into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Revoke,
            is_dest: false,
        };
        let (_, revoke_metadata) = parse_system_message(&revoke_line.message, MsgType::Revoke);

        assert_eq!(
            contact_metadata.raw.parse_error.as_deref(),
            Some("invalid contact xml")
        );
        assert_eq!(
            location_metadata.raw.parse_error.as_deref(),
            Some("invalid location xml")
        );
        assert_eq!(
            revoke_metadata.raw.parse_error.as_deref(),
            Some("invalid revoke xml")
        );
    }

    #[test]
    fn ios_wechat_appmsg_subtypes_emit_structured_metadata() {
        let cases = [
            (
                "file",
                r#"<msg><appmsg><title>report.pdf</title><type>6</type><fileext>pdf</fileext><totallen>42</totallen></appmsg></msg>"#,
                "[file] report.pdf",
                "fileext",
                "pdf",
            ),
            (
                "refer",
                r#"<msg><appmsg><title>reply</title><type>57</type><refermsg><displayname>Alice</displayname><content>hello</content></refermsg></appmsg></msg>"#,
                "[refer] reply",
                "refer_display_name",
                "Alice",
            ),
            (
                "transfer",
                r#"<msg><appmsg><title>transfer</title><type>2000</type><wcpayinfo><feedesc>$1.00</feedesc><pay_memo>memo</pay_memo></wcpayinfo></appmsg></msg>"#,
                "[transfer] transfer",
                "feedesc",
                "$1.00",
            ),
            (
                "red_packet",
                r#"<msg><appmsg><title>packet</title><type>2001</type></appmsg></msg>"#,
                "[red packet] packet",
                "title",
                "packet",
            ),
            (
                "mini_program",
                r#"<msg><appmsg><title>mini</title><type>33</type><weappinfo><username>gh_x</username></weappinfo></appmsg></msg>"#,
                "[mini program] mini",
                "mini_program_username",
                "gh_x",
            ),
        ];

        for (kind, xml, label, field, value) in cases {
            let metadata = parse_appmsg_metadata(xml).with_type(MsgType::CustomApp);
            assert_eq!(appmsg_label(&metadata), label);
            assert_eq!(metadata.app.as_ref().unwrap()["kind"], kind);
            assert_eq!(
                metadata.field(field),
                Some(&MetadataValue::Str(value.into()))
            );
        }
    }

    #[test]
    fn ios_wechat_appmsg_declared_subtypes_are_not_unknown() {
        let cases = [
            (1, "text", "[appmsg] item"),
            (2, "image", "[appmsg] item"),
            (3, "audio", "[appmsg] item"),
            (4, "video", "[appmsg] item"),
            (7, "text", "[appmsg] item"),
            (8, "video", "[appmsg] item"),
            (17, "realtime_location", "[realtime location] item"),
            (24, "note", "[note] item"),
            (50, "channels", "[channels] item"),
            (51, "channels", "[channels] item"),
            (62, "pat", "[pat] item"),
            (100001, "reader", "[reader] item"),
        ];

        for (appmsg_type, kind, label) in cases {
            let metadata = parse_appmsg_metadata(&format!(
                "<msg><appmsg><title>item</title><type>{}</type></appmsg></msg>",
                appmsg_type
            ));
            assert_eq!(metadata.app.as_ref().unwrap()["kind"], kind);
            assert_eq!(appmsg_label(&metadata), label);
            assert!(metadata.raw.raw_hash.is_none());
        }
    }

    #[test]
    fn ios_wechat_forwarded_and_unknown_appmsg_metadata() {
        let forwarded_xml = r#"<record><dataitem><datatitle>First</datatitle><sourcename>Alice</sourcename><datadesc>Hello</datadesc></dataitem></record>"#;
        let metadata = parse_appmsg_metadata(&format!(
            "<msg><appmsg><title>history</title><type>19</type><recorditem>{}</recorditem></appmsg></msg>",
            htmlescape::encode_minimal(forwarded_xml)
        ));
        let forwarded = metadata.app.as_ref().unwrap()["forwarded"]
            .as_array()
            .unwrap();
        assert_eq!(appmsg_label(&metadata), "[forwarded] history");
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0]["title"], "First");

        let unknown = parse_appmsg_metadata(
            "<msg><appmsg><title>mystery</title><type>40404</type></appmsg></msg>",
        );
        assert_eq!(unknown.app.as_ref().unwrap()["kind"], "unknown:40404");
        assert!(unknown.raw.raw_hash.is_some());
        assert_eq!(unknown.raw.summary.as_deref(), Some("mystery"));
    }

    #[test]
    fn ios_wechat_system_success_classifications_have_metadata() {
        let cases = [
        (
            "<sysmsg><sysmsgtemplate><content_template><template>invited</template></content_template></sysmsgtemplate></sysmsg>",
            "sysmsgtemplate",
            "invited",
        ),
        (
            "<sysmsg><editrevokecontent>edited</editrevokecontent></sysmsg>",
            "editrevokecontent",
            "edited",
        ),
        (
            "<sysmsg type=\"paymsg\"><paymsg><template>paid</template></paymsg></sysmsg>",
            "paymsg",
            "paid",
        ),
        ("Alice 邀请 Bob 加入了群聊", "room_join", "Alice 邀请 Bob 加入了群聊"),
        ("Alice 退出了群聊", "room_leave", "Alice 退出了群聊"),
        ("Alice 修改群名为 Project", "room_rename", "Alice 修改群名为 Project"),
        ("群公告 updated", "room_announcement", "群公告 updated"),
        ("Alice 拍了拍 Bob", "pat", "Alice 拍了拍 Bob"),
        ("Alice 领取了红包", "red_packet", "Alice 领取了红包"),
    ];

        for (message, system_type, content) in cases {
            let (label, metadata) = parse_system_message(message, MsgType::System);
            assert_eq!(label, format!("[system:{}]", system_type));
            assert_eq!(
                metadata.field("system_type"),
                Some(&MetadataValue::Str(system_type.into()))
            );
            assert_eq!(
                metadata.field("content"),
                Some(&MetadataValue::Str(content.into()))
            );
            assert_eq!(
                metadata.system.as_ref().unwrap()["system_type"],
                system_type
            );
        }
    }

    #[test]
    fn ios_wechat_system_parser_fallback_preserves_text() {
        let (label, metadata) =
            parse_system_message("<sysmsg><paymsg><template>paid</template>", MsgType::System);

        assert_eq!(label, "[system:plain]");
        assert_eq!(
            metadata.raw.parse_error.as_deref(),
            Some("invalid system xml")
        );
        assert_eq!(
            metadata.field("content"),
            Some(&MetadataValue::Str(
                "<sysmsg><paymsg><template>paid</template>".into()
            ))
        );
    }

    #[test]
    fn ios_wechat_group_message_without_sender_does_not_use_room_id() {
        let backup_dir = tempdir().unwrap();
        write_minimal_backup_metadata(backup_dir.path());
        write_manifest_db(backup_dir.path(), Vec::new());
        let mut backup = Backup::new(backup_dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let user_db = UserDB {
            account: "account-a".into(),
            wxid: "owner".into(),
            name: "Owner".into(),
            ..Default::default()
        };
        let contact = Contact {
            name: "room@chatroom".into(),
            remark: Some("Room".into()),
            ..Default::default()
        };
        let record = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 1,
                    server_id: 1,
                    created_time: 1,
                    message: "<msg><appmsg><title>Doc</title><type>5</type></appmsg></msg>".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::CustomApp,
                    is_dest: true,
                },
                &contact,
            )
            .unwrap();

        assert_eq!(record.get_record().group_id, "room@chatroom");
        assert_eq!(record.get_record().sender_id, "unknown");
    }

    #[test]
    fn ios_wechat_truncated_xml_preserves_record_with_fallback_content() {
        let backup_dir = tempdir().unwrap();
        write_minimal_backup_metadata(backup_dir.path());
        write_manifest_db(backup_dir.path(), Vec::new());
        let mut backup = Backup::new(backup_dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let user_db = UserDB {
            wxid: "owner".into(),
            name: "Owner".into(),
            ..Default::default()
        };
        let contact = Contact {
            name: "chat-a".into(),
            ..Default::default()
        };
        let record = user_db
            .transform_record_line(
                &backup,
                &RecordLine {
                    local_id: 1,
                    server_id: 1,
                    created_time: 1,
                    message: "<msg><location label=\"missing close\"".into(),
                    status: 0,
                    image_status: 0,
                    msg_type: MsgType::Location,
                    is_dest: true,
                },
                &contact,
            )
            .unwrap();
        let record = record.get_record();
        let metadata: IosWcMetadata = from_slice(record.metadata.as_ref().unwrap()).unwrap();

        assert_eq!(record.content, "[location]");
        assert_eq!(
            metadata.raw.parse_error.as_deref(),
            Some("invalid location xml")
        );
    }

    #[tokio::test]
    async fn ios_wechat_image_thumbnail_metadata_merge_sample() {
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let old_img = png(1);
        let old_img_hash = Hash32::sha3_256(&old_img).to_hex();
        let old_thum_hash = Hash32::sha3_256(png(2)).to_hex();
        let new_thum = png(3);
        let new_thum_hash = Hash32::sha3_256(&new_thum).to_hex();

        let old_metadata = IosWcMetadata::new()
            .with_hash("mid".into(), old_img_hash.clone())
            .with_hash("thumb".into(), old_thum_hash)
            .with_type(MsgType::Image);
        let new_metadata = IosWcMetadata::new()
            .with_hash("thumb".into(), new_thum_hash.clone())
            .with_type(MsgType::Image);

        let record = Record {
            chat_type: "WeChat".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "sender".into(),
            content: "[img]".into(),
            timestamp: 1,
            metadata: Some(to_vec(&old_metadata).unwrap()),
            ..Default::default()
        };
        store
            .insert_or_update(
                RecordType::from((
                    record,
                    [(old_img_hash.clone(), old_img)].iter().cloned().collect(),
                )),
                None,
            )
            .await
            .unwrap();

        let merger = IosWcMetadataMerger;
        let merged = merger
            .merge(
                &store,
                &[(new_thum_hash.clone(), new_thum)]
                    .iter()
                    .cloned()
                    .collect(),
                to_vec(&old_metadata).unwrap(),
                to_vec(&new_metadata).unwrap(),
            )
            .await
            .unwrap();
        let merged: IosWcMetadata = from_slice(&merged).unwrap();
        assert_eq!(merged.media_hash("mid"), Some(old_img_hash.as_str()));
        assert_eq!(merged.media_hash("thumb"), Some(new_thum_hash.as_str()));
    }

    struct TestMatcher {
        records: Vec<RecordType>,
    }

    impl MsgMatcher for TestMatcher {
        fn import_plan(&self) -> ImportPlan {
            ImportPlan {
                chats_total: Some(1),
            }
        }

        fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
            progress.chat_parsed(
                self.records.len() as u64,
                record_blob_count(&self.records) as u64,
            );
            Ok(vec![RecordBatch {
                label: "test".into(),
                records: self.records.clone(),
            }])
        }

        fn get_metadata_merger(&self) -> Option<Box<dyn MetadataMerger>> {
            Some(Box::new(IosWcMetadataMerger))
        }
    }

    #[tokio::test]
    async fn ios_wechat_importer_path_merges_image_metadata_and_assets() {
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let old_img = png(1);
        let old_img_hash = Hash32::sha3_256(&old_img).to_hex();
        let old_thum = png(2);
        let old_thum_hash = Hash32::sha3_256(&old_thum).to_hex();
        let new_thum = png(3);
        let new_thum_hash = Hash32::sha3_256(&new_thum).to_hex();

        let base_record = Record {
            chat_type: "WeChat".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "sender".into(),
            content: "[img]".into(),
            timestamp: 1,
            ..Default::default()
        };
        let old_record = Record {
            metadata: Some(
                to_vec(
                    &IosWcMetadata::new()
                        .with_hash("mid".into(), old_img_hash.clone())
                        .with_hash("thumb".into(), old_thum_hash.clone())
                        .with_type(MsgType::Image),
                )
                .unwrap(),
            ),
            ..base_record.clone()
        };
        export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![RecordType::from((
                    old_record,
                    [
                        (old_img_hash.clone(), old_img),
                        (old_thum_hash.clone(), old_thum),
                    ]
                    .iter()
                    .cloned()
                    .collect(),
                ))],
            },
        )
        .await
        .unwrap();

        let new_record = Record {
            metadata: Some(
                to_vec(
                    &IosWcMetadata::new()
                        .with_hash("thumb".into(), new_thum_hash.clone())
                        .with_type(MsgType::Image),
                )
                .unwrap(),
            ),
            ..base_record
        };
        export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![RecordType::from((
                    new_record,
                    [(new_thum_hash.clone(), new_thum.clone())]
                        .iter()
                        .cloned()
                        .collect(),
                ))],
            },
        )
        .await
        .unwrap();

        let stored = store.query(crate::store::Query::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
        let metadata: IosWcMetadata = from_slice(stored[0].metadata.as_ref().unwrap()).unwrap();
        assert_eq!(metadata.media_hash("mid"), Some(old_img_hash.as_str()));
        assert_eq!(metadata.media_hash("thumb"), Some(new_thum_hash.as_str()));
        assert_eq!(
            store
                .get_asset(Hash32::from_hex(&new_thum_hash).unwrap())
                .await
                .unwrap(),
            Some(new_thum)
        );
    }
}
