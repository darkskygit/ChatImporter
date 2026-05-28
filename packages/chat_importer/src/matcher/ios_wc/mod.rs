use super::*;
use ibackuptool2::{Backup, BackupFile};
use num_enum::TryFromPrimitive;
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::io::{Cursor, Error, ErrorKind, Write};
use std::iter::IntoIterator;
use std::str::{from_utf8, Utf8Error};
use std::sync::{Arc, Condvar, Mutex};
use tempfile::NamedTempFile;

const DOMAIN: &str = "AppDomain-com.tencent.xin";
const RECORD_LINE_CHUNK_SIZE: usize = 1024;

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
mod wxgf;
mod xml;

#[cfg(test)]
use account::*;
use backup::*;
#[cfg(test)]
use contact::*;
#[cfg(test)]
use message::*;
use metadata::*;

struct ChatParseJob<'a> {
    index: usize,
    user_db: &'a account::UserDB,
    backup: &'a Backup,
    selection: account::ChatSelection,
}

struct RecordLineChunk<'a> {
    chat_index: usize,
    chunk_index: usize,
    user_db: &'a account::UserDB,
    backup: &'a Backup,
    contact: contact::Contact,
    lines: Vec<message::RecordLine>,
}

#[derive(Default)]
struct ChunkQueue<'a> {
    chunks: std::collections::VecDeque<RecordLineChunk<'a>>,
    active_readers: usize,
}

enum RecordChunkEvent {
    Chunk {
        chat_index: usize,
        chunk_index: usize,
        records: Vec<RecordType>,
    },
    ChatDone {
        chat_index: usize,
        chunks: usize,
    },
}

struct IosWcMetadataMerger;

struct IosWcCollectSink {
    records: Arc<Mutex<Vec<RecordType>>>,
}

impl RecordSink for IosWcCollectSink {
    fn push(&self, record: RecordType) -> Result<()> {
        self.records
            .lock()
            .map_err(|_| anyhow::anyhow!("ios wc collect sink poisoned"))?
            .push(record);
        Ok(())
    }
}

#[async_trait::async_trait]
impl MetadataMerger for IosWcMetadataMerger {
    async fn merge(
        &self,
        store: &ChatStore,
        attaches: &Attachments,
        context: &MetadataMergeContext,
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
            to_vec(&new.merge(store, attaches, context, old).await)
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
        let records = Arc::new(Mutex::new(Vec::<RecordType>::new()));
        self.stream_records(
            progress,
            &IosWcCollectSink {
                records: Arc::clone(&records),
            },
        )?;
        let records = Arc::try_unwrap(records)
            .map_err(|_| anyhow::anyhow!("ios wc collect sink still shared"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("ios wc collect sink poisoned"))?;
        Ok(vec![RecordBatch { records }])
    }

    fn stream_records(&self, progress: &PipelineProgress, sink: &dyn RecordSink) -> Result<()> {
        let jobs = self
            .extract_ids
            .iter()
            .filter_map(|u| self.extractor.get_user_db(u))
            .flat_map(|(user_db, backup)| {
                user_db
                    .get_record_chats(self.names.clone())
                    .into_iter()
                    .map(move |selection| (user_db, backup, selection))
            })
            .enumerate()
            .map(|(index, (user_db, backup, selection))| ChatParseJob {
                index,
                user_db,
                backup,
                selection,
            })
            .collect::<Vec<_>>();
        progress.chat_planned(jobs.len() as u64);
        if jobs.is_empty() {
            return Ok(());
        }

        let parallelism = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4);
        let reader_count = jobs.len().min(parallelism.clamp(1, 4));
        let worker_count = parallelism;
        let jobs = Arc::new(Mutex::new(
            jobs.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));
        let queue = Arc::new((
            Mutex::new(ChunkQueue {
                active_readers: reader_count,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let (result_tx, result_rx) =
            std::sync::mpsc::sync_channel::<RecordChunkEvent>(worker_count * 2);
        let errors = Arc::new(Mutex::new(Vec::<anyhow::Error>::new()));
        std::thread::scope(|scope| {
            for _ in 0..reader_count {
                let jobs = Arc::clone(&jobs);
                let queue = Arc::clone(&queue);
                let result_tx = result_tx.clone();
                scope.spawn(move || {
                    loop {
                        let Some(job) = jobs.lock().expect("ios wc job queue poisoned").pop_front()
                        else {
                            break;
                        };
                        info!(
                            "Extracting: {} => {}",
                            job.selection.selector, job.selection.chat_id
                        );
                        let mut chunks = 0_usize;
                        if let Err(error) = job.user_db.load_record_line_chunks(
                            &job.selection.chat_id,
                            RECORD_LINE_CHUNK_SIZE,
                            |chunk_index, lines| {
                                chunks += 1;
                                let chunk = RecordLineChunk {
                                    chat_index: job.index,
                                    chunk_index,
                                    user_db: job.user_db,
                                    backup: job.backup,
                                    contact: job.selection.contact.clone(),
                                    lines,
                                };
                                let (queue, available) = &*queue;
                                queue
                                    .lock()
                                    .expect("ios wc chunk queue poisoned")
                                    .chunks
                                    .push_back(chunk);
                                progress.chat_chunk_planned();
                                available.notify_one();
                            },
                        ) {
                            warn!("failed to get chat line: {}", error);
                        }
                        let _ = result_tx.send(RecordChunkEvent::ChatDone {
                            chat_index: job.index,
                            chunks,
                        });
                        progress.chat_parsed(0, 0);
                    }
                    let (queue, available) = &*queue;
                    let mut queue = queue.lock().expect("ios wc chunk queue poisoned");
                    queue.active_readers -= 1;
                    available.notify_all();
                });
            }
            for _ in 0..worker_count {
                let queue = Arc::clone(&queue);
                let result_tx = result_tx.clone();
                scope.spawn(move || loop {
                    let chunk = {
                        let (queue, available) = &*queue;
                        let mut queue = queue.lock().expect("ios wc chunk queue poisoned");
                        loop {
                            if let Some(chunk) = queue.chunks.pop_front() {
                                break Some(chunk);
                            }
                            if queue.active_readers == 0 {
                                break None;
                            }
                            queue = available.wait(queue).expect("ios wc chunk queue poisoned");
                        }
                    };
                    let Some(chunk) = chunk else {
                        break;
                    };
                    let records = chunk.user_db.transform_record_lines(
                        chunk.backup,
                        &chunk.contact,
                        chunk.lines,
                    );
                    progress.chat_chunk_parsed(
                        records.len() as u64,
                        record_blob_count(&records) as u64,
                    );
                    if result_tx
                        .send(RecordChunkEvent::Chunk {
                            chat_index: chunk.chat_index,
                            chunk_index: chunk.chunk_index,
                            records,
                        })
                        .is_err()
                    {
                        break;
                    }
                });
            }
            drop(result_tx);
            let errors = Arc::clone(&errors);
            scope.spawn(move || {
                let mut chunks = BTreeMap::<(usize, usize), Vec<RecordType>>::new();
                let mut chat_chunks = BTreeMap::<usize, usize>::new();
                let mut expected_chat = 0_usize;
                let mut expected_chunk = 0_usize;
                for event in result_rx {
                    match event {
                        RecordChunkEvent::Chunk {
                            chat_index,
                            chunk_index,
                            records,
                        } => {
                            chunks.insert((chat_index, chunk_index), records);
                        }
                        RecordChunkEvent::ChatDone { chat_index, chunks } => {
                            chat_chunks.insert(chat_index, chunks);
                        }
                    }
                    loop {
                        if let Some(records) = chunks.remove(&(expected_chat, expected_chunk)) {
                            for record in records {
                                if let Err(error) = sink.push(record) {
                                    errors
                                        .lock()
                                        .expect("ios wc error queue poisoned")
                                        .push(error);
                                    return;
                                }
                            }
                            expected_chunk += 1;
                            continue;
                        }
                        if chat_chunks
                            .get(&expected_chat)
                            .is_some_and(|chunks| *chunks == expected_chunk)
                        {
                            chat_chunks.remove(&expected_chat);
                            expected_chat += 1;
                            expected_chunk = 0;
                            continue;
                        }
                        break;
                    }
                }
            });
        });
        let mut errors = Arc::try_unwrap(errors)
            .map_err(|_| anyhow::anyhow!("ios wc error queue still shared"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("ios wc error queue poisoned"))?;
        if let Some(error) = errors.pop() {
            return Err(error);
        }
        Ok(())
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
            let friend_ext = format!("ChatExt2_{}", gen_md5("friend"));
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
                r#"<msg><videomsg cdnvideourl="video" cdnrawvideourl="raw-video" aeskey="key" cdnrawvideoaeskey="raw-key" md5="video-md5" rawmd5="raw-md5" rawlength="42"/></msg>"#,
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
            (
                11,
                111,
                1_598_219_167,
                r#"<msg username="openim-contact@kefu.openim" nickname="OpenIM Support" openimdesc="OpenIM Service" smallheadimgurl="https://example.test/openim.png" />"#,
                1,
                0,
                67,
                0,
            ),
            (
                12,
                112,
                1_598_219_168,
                r#"<msg><img cdnthumburl="thumb-v2" cdnmidimgurl="mid-v2" cdnbigimgurl="hd-v2" aeskey="key-v2"/></msg>"#,
                1,
                0,
                3,
                0,
            ),
            (
                13,
                113,
                1_598_219_169,
                r#"<msg><videomsg cdnvideourl="temp-video" aeskey="temp-key"/></msg>"#,
                1,
                0,
                43,
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
                    "CREATE TABLE {} (
                        MesLocalID INTEGER NOT NULL,
                        MsgSource TEXT,
                        WCDB_CT_MsgSource INTEGER
                    )",
                    friend_ext
                ),
                [],
            )
            .unwrap();
            conn.execute(
                &format!("INSERT INTO {} VALUES (?1, ?2, ?3)", friend_ext),
                (
                    1,
                    "<msgsource><sequence_id>seq-1</sequence_id><strid>str-1</strid><silence>1</silence><membercount>2</membercount><signature>sig-1</signature></msgsource>",
                    0,
                ),
            )
            .unwrap();
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

    fn chunked_message_db(rows: usize) -> Vec<u8> {
        sqlite_bytes("message_0.sqlite", |conn| {
            let chat = gen_md5("friend");
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
            for index in 0..rows {
                conn.execute(
                    &format!(
                        "INSERT INTO Chat_{} (
                            MesLocalID, MesSvrID, CreateTime, Message, Status, ImgStatus, Type, Des
                        ) VALUES (?1, ?2, ?3, ?4, 0, 0, 1, 0)",
                        chat
                    ),
                    params![
                        index as i64 + 1,
                        index as i64 + 10_000,
                        1_598_219_157 + index as i64,
                        format!("line-{index:04}")
                    ],
                )
                .unwrap();
            }
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
                format!("Documents/{}/ImgV2/{}/12.pic_hd", account, friend_hash),
                png(15),
            ),
            (
                format!("Documents/{}/Video/{}/3.mp4", account, friend_hash),
                b"video-bytes".to_vec(),
            ),
            (
                format!("Documents/{}/Video/{}/3_raw.mp4", account, friend_hash),
                b"raw-video-bytes".to_vec(),
            ),
            (
                format!("Documents/{}/Video/{}/3.video_thum", account, friend_hash),
                png(14),
            ),
            (
                format!("Documents/{}/Video/{}/13_temp.mp4", account, friend_hash),
                b"temp-video-bytes".to_vec(),
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

        assert_eq!(records.len(), 13);
        let hello = records
            .iter()
            .find(|record| record.get_record().content == "hello")
            .unwrap();
        let hello_metadata: IosWcMetadata =
            from_slice(hello.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(hello_metadata.msg_type, MsgType::Normal);
        assert_eq!(
            hello_metadata.field("msgsource_sequence_id"),
            Some(&MetadataValue::Str("seq-1".into()))
        );
        assert_eq!(
            hello_metadata.field("msgsource_strid"),
            Some(&MetadataValue::Str("str-1".into()))
        );
        assert_eq!(
            hello_metadata.field("msgsource_silence"),
            Some(&MetadataValue::Str("1".into()))
        );
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
            record.get_record().source_message_id.as_deref() == Some("svr:friend:112")
                && record.get_record().content == "[img]"
                && record.attachment_count() == 1
        }));
        assert!(records.iter().any(|record| {
            record.get_record().content == "[video]" && record.attachment_count() == 3
        }));
        assert!(records.iter().any(|record| {
            record.get_record().content == "[video]" && record.attachment_count() == 1
        }));
        let video = records
            .iter()
            .find(|record| {
                record.get_record().content == "[video]" && record.attachment_count() == 3
            })
            .unwrap();
        let video_metadata: IosWcMetadata =
            from_slice(video.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(
            video_metadata.field("raw_cdn"),
            Some(&MetadataValue::Str("raw-video".into()))
        );
        assert_eq!(
            video_metadata.field("raw_md5"),
            Some(&MetadataValue::Str("raw-md5".into()))
        );
        assert!(video_metadata.media_hash("video_raw").is_some());
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
            app_metadata.app.as_ref().unwrap()["title"],
            serde_json::json!("Doc")
        );
        assert_eq!(
            app_metadata.app.as_ref().unwrap()["description"],
            serde_json::json!("Desc")
        );
        assert_eq!(
            app_metadata.app.as_ref().unwrap()["url"],
            serde_json::json!("https://example.test")
        );
        let system = records
            .iter()
            .find(|record| record.get_record().content == "[system:plain]")
            .unwrap();
        let system_metadata: IosWcMetadata =
            from_slice(system.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(system_metadata.msg_type, MsgType::System);
        assert_eq!(
            system_metadata.system.as_ref().unwrap()["system_type"],
            serde_json::json!("plain")
        );
        assert_eq!(
            system_metadata.system.as_ref().unwrap()["content"],
            serde_json::json!("system text")
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
        let openim_contact = records
            .iter()
            .find(|record| {
                let record = record.get_record();
                record.content == "[contact]"
                    && record
                        .metadata
                        .as_ref()
                        .and_then(|metadata| from_slice::<IosWcMetadata>(metadata).ok())
                        .is_some_and(|metadata| metadata.msg_type == MsgType::OpenIMContactShare)
            })
            .unwrap();
        let openim_metadata: IosWcMetadata =
            from_slice(openim_contact.get_record().metadata.as_ref().unwrap()).unwrap();
        assert_eq!(
            openim_metadata.field("username"),
            Some(&MetadataValue::Str("openim-contact@kefu.openim".into()))
        );
        assert_eq!(
            openim_metadata.field("openimdesc"),
            Some(&MetadataValue::Str("OpenIM Service".into()))
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
    fn ios_wechat_chunked_chat_import_keeps_record_order() {
        let dir = tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());
        let account = "account-a";
        write_manifest_db(
            dir.path(),
            vec![
                (
                    "Documents/account-a/WCDB_Contact.sqlite".into(),
                    fixture_contact_db(),
                ),
                (
                    "Documents/account-a/message_0.sqlite".into(),
                    chunked_message_db(RECORD_LINE_CHUNK_SIZE + 3),
                ),
                (
                    format!("Documents/{}/mmsetting.archive", account),
                    settings_bytes("owner-wxid", "Owner"),
                ),
            ],
        );

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let matcher = Matcher::from_backup(backup, None).unwrap();
        let records = matcher.collect_records().unwrap();

        assert_eq!(records.len(), RECORD_LINE_CHUNK_SIZE + 3);
        assert_eq!(records[0].get_record().content, "line-0000");
        assert_eq!(
            records[RECORD_LINE_CHUNK_SIZE].get_record().content,
            "line-1024"
        );
        assert_eq!(records.last().unwrap().get_record().content, "line-1026");
    }

    #[test]
    fn ios_wechat_uses_mmappedkv_setting_for_db_folder_account() {
        let dir = tempdir().unwrap();
        write_minimal_backup_metadata(dir.path());
        let wxid = "owner-wxid";
        let account = gen_md5(wxid);
        write_manifest_db(
            dir.path(),
            vec![
                (
                    format!("Documents/{}/DB/WCDB_Contact.sqlite", account),
                    fixture_contact_db(),
                ),
                (
                    format!("Documents/{}/DB/message_0.sqlite", account),
                    fixture_message_db(),
                ),
                (
                    format!("Documents/MMappedKV/mmsetting.archive.{}", wxid),
                    Vec::new(),
                ),
                (
                    format!("Documents/MMappedKV/mmsetting.archive.{}.crc", wxid),
                    b"crc".to_vec(),
                ),
            ],
        );

        let mut backup = Backup::new(dir.path()).unwrap();
        backup.parse_manifest().unwrap();
        let extractor = Extractor::from_backup(backup).unwrap();
        let users = extractor.get_users();
        assert_eq!(users, vec![account.clone()]);
        let (user_db, _) = extractor.get_user_db(&account).unwrap();
        assert_eq!(user_db.wxid, wxid);
        assert_eq!(
            user_db
                .kv_setting
                .as_ref()
                .unwrap()
                .relative_filename
                .as_str(),
            "Documents/MMappedKV/mmsetting.archive.owner-wxid"
        );
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
                    msg_source: None,
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
                    msg_source: None,
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
                    msg_source: None,
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
                    msg_source: None,
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
                    msg_source: None,
                },
                &contact,
            )
            .unwrap();

        assert_eq!(record.get_record().group_id, "room@chatroom");
        assert_eq!(record.get_record().sender_id, "unknown");
    }

    #[test]
    fn ios_wechat_group_image_metadata_uses_stripped_xml_body() {
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
                    local_id: 8,
                    server_id: 5073122080177807510,
                    created_time: 1548479192,
                    message: r#"wxid_sender:
<?xml version="1.0"?>
<msg><img aeskey="key-a" cdnmidimgurl="cdn-a" md5="md5-a" /></msg>"#
                        .into(),
                    status: 4,
                    image_status: 2,
                    msg_type: MsgType::Image,
                    is_dest: true,
                    msg_source: None,
                },
                &contact,
            )
            .unwrap();
        let record = record.get_record();
        let metadata: IosWcMetadata = from_slice(record.metadata.as_ref().unwrap()).unwrap();

        assert_eq!(record.content, "[img]");
        assert_eq!(record.sender_id, "wxid_sender");
        assert_eq!(metadata.raw.parse_error, None);
        assert_eq!(
            metadata.field("key"),
            Some(&MetadataValue::Str("key-a".into()))
        );
        assert_eq!(
            metadata.field("img_cdn"),
            Some(&MetadataValue::Str("cdn-a".into()))
        );
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
                    msg_source: None,
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
                RecordType::from((record, attachments([(old_img_hash.clone(), old_img)]))),
                None,
            )
            .await
            .unwrap();

        let merger = IosWcMetadataMerger;
        let merged = merger
            .merge(
                &store,
                &attachments([(new_thum_hash.clone(), new_thum)]),
                &MetadataMergeContext::default(),
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
                    attachments([
                        (old_img_hash.clone(), old_img),
                        (old_thum_hash.clone(), old_thum),
                    ]),
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
                    attachments([(new_thum_hash.clone(), new_thum.clone())]),
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

    fn attachments<const N: usize>(items: [(String, Vec<u8>); N]) -> Attachments {
        Vec::from(items)
            .into_iter()
            .map(|(name, bytes)| (name, Attachment::from_bytes(bytes)))
            .collect()
    }
}
