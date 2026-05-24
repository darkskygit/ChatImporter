use super::*;
use chrono::{Duration, TimeZone, Utc};
use ibackuptool2::Backup;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use std::io::Write;
use tempfile::NamedTempFile;

#[derive(Debug)]
struct RecordLine {
    id: i32,
    target: String,
    text: String,
    handle_id: i32,
    service: String,
    date: i64,
    is_from_me: bool,
    destination_caller_id: String,
    is_spam: bool,
}

#[allow(non_camel_case_types)]
struct Extractor {
    conn: Connection,
    owner: String,
}

impl Extractor {
    pub fn new<P: AsRef<Path>>(path: P, owner: String) -> SqliteResult<Self> {
        Ok(Self {
            conn: Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?,
            owner,
        })
    }

    fn get_chat_ids(&self) -> SqliteResult<Vec<i32>> {
        Ok(self
            .conn
            .prepare("SELECT DISTINCT chat_id FROM chat_message_join")?
            .query_map(params![], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect())
    }

    fn check_has_is_spam(&self) -> SqliteResult<bool> {
        Ok(self
            .conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='is_spam'")?
            .query_map(params![], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .next()
            == Some(1))
    }

    fn get_record_lines(&self, chat_id: i32) -> SqliteResult<Vec<Record>> {
        let base_date_offset = Utc.timestamp(978307200, 0);
        let has_is_spam = self.check_has_is_spam()?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT
                message.ROWID,
                handle.id as sender_name,
                message.text,
                message.handle_id,
                message.service,
                message.date,
                message.is_from_me,
                message.destination_caller_id,
                {}
            FROM chat_message_join
            INNER JOIN message
                ON message.rowid = chat_message_join.message_id
            INNER JOIN handle
                ON handle.rowid = message.handle_id
            WHERE chat_message_join.chat_id = ?
            ORDER by date asc",
            if has_is_spam { "message.is_spam" } else { "0" }
        ))?;
        let records_iter = stmt.query_map(params![chat_id], |row| {
            Ok(RecordLine {
                id: row.get(0)?,
                target: row.get(1)?,
                text: row.get(2)?,
                handle_id: row.get(3)?,
                service: row.get(4)?,
                date: row.get(5)?,
                is_from_me: row.get(6)?,
                destination_caller_id: row.get(7)?,
                is_spam: row.get(8)?,
            })
        })?;

        Ok(records_iter
            .filter_map(|r| r.ok())
            .map(|record| Record {
                chat_type: format!("iOS {}", record.service),
                owner_id: record.destination_caller_id.clone(),
                group_id: record.target.clone(),
                sender_id: if record.is_from_me {
                    record.destination_caller_id
                } else {
                    record.target.clone()
                },
                sender_name: if record.is_from_me {
                    self.owner.clone()
                } else {
                    record.target
                },
                content: record.text,
                timestamp: (base_date_offset + Duration::nanoseconds(record.date))
                    .timestamp_millis(),
                ..Default::default()
            })
            .collect())
    }
}

impl MsgMatcher for Extractor {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        self.get_chat_ids()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| {
                        self.get_record_lines(*id)
                            .map_err(|e| warn!("Failed to get sms record {}: {}", id, e))
                            .ok()
                    })
                    .flatten()
                    .map(RecordType::from)
                    .collect()
            })
            .map_err(|e| warn!("Failed to get chat ids: {}", e))
            .ok()
    }
}

#[allow(non_camel_case_types)]
pub struct Matcher {
    _smsdb: NamedTempFile,
    extractor: Extractor,
}

impl Matcher {
    pub fn new<P: AsRef<Path>>(path: P, owner: String) -> Result<Box<dyn MsgMatcher>> {
        let backup = Self::init_backup(path).map_err(|e| anyhow::anyhow!("{}", e))?;
        if let Some(sms) = backup.find_path("HomeDomain", "Library/SMS/sms.db") {
            let mut tempfile = NamedTempFile::new()?;
            tempfile.write_all(
                &backup
                    .read_file(&sms)
                    .map_err(|e| anyhow::anyhow!("{}", e))?,
            )?;
            Ok(Box::new(Self {
                extractor: Extractor::new(tempfile.path(), owner)?,
                _smsdb: tempfile,
            }) as Box<dyn MsgMatcher>)
        } else {
            Err(anyhow::anyhow!("Failed to find sms database"))
        }
    }

    fn init_backup<P: AsRef<Path>>(path: P) -> Result<Backup, Box<dyn std::error::Error>> {
        let mut backup = Backup::new(path)?;
        backup.parse_manifest()?;
        Ok(backup)
    }
}

impl MsgMatcher for Matcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        self.extractor.get_records()
    }
}

#[tokio::test]
async fn ios_sms_minimal_sample() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    conn.execute_batch(
        r#"
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
        CREATE TABLE message (
            ROWID INTEGER PRIMARY KEY,
            text TEXT,
            handle_id INTEGER,
            service TEXT,
            date INTEGER,
            is_from_me INTEGER,
            destination_caller_id TEXT,
            is_spam INTEGER
        );
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO message
            (ROWID, text, handle_id, service, date, is_from_me, destination_caller_id, is_spam)
        VALUES
            (1, 'hello sms', 1, 'iMessage', 0, 0, '+19999999999', 0);
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1);
        "#,
    )?;
    drop(conn);

    let matcher = Extractor::new(file.path(), "Owner".into())?;
    let records = matcher.get_records().unwrap();
    assert_eq!(records.len(), 1);
    let record = records[0].get_record();
    assert_eq!(record.chat_type, "iOS iMessage");
    assert_eq!(record.group_id, "+10000000000");
    assert_eq!(record.sender_name, "+10000000000");
    assert_eq!(record.content, "hello sms");

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    export_matcher(&mut store, &matcher).await.unwrap();
    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "hello sms");
    Ok(())
}
