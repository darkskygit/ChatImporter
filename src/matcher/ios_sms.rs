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

#[test]
fn test_ios_sms_db() -> SqliteResult<()> {
    let matcher = Extractor::new("sms.db", "".into())?;
    println!(
        "{}",
        matcher
            .get_chat_ids()?
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    for recoder in matcher.get_record_lines(0)? {
        println!("{:?}", recoder);
    }
    Ok(())
}
