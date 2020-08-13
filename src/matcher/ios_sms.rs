use super::*;
use chrono::{Duration, TimeZone, Utc};
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};

#[derive(Debug)]
struct SMSRecord {
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
pub struct iOSSMSMatcher {
    conn: Connection,
    my_name: String,
}

impl iOSSMSMatcher {
    pub fn new<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        Ok(Self {
            conn: Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?,
            my_name: "".into(),
        })
    }

    fn get_sms_records(&self, chat_id: i32) -> SqliteResult<Vec<Record>> {
        let base_date_offset = Utc.timestamp(978307200, 0);
        let mut stmt = self.conn.prepare(
            "SELECT
                message.ROWID,
                handle.id as sender_name,
                message.text,
                message.handle_id,
                message.service,
                message.date,
                message.is_from_me,
                message.destination_caller_id,
                message.is_spam
            FROM chat_message_join
            INNER JOIN message
                ON message.rowid = chat_message_join.message_id
            INNER JOIN handle
                ON handle.rowid = message.handle_id
            WHERE chat_message_join.chat_id = ?
            ORDER by date asc",
        )?;
        let records_iter = stmt.query_map(params![chat_id], |row| {
            Ok(SMSRecord {
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
                    self.my_name.clone()
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

#[test]
fn test_ios_sms_db() -> SqliteResult<()> {
    let matcher = iOSSMSMatcher::new("sms.db")?;
    for recoder in matcher.get_sms_records(10)? {
        println!("{:?}", recoder);
    }
    Ok(())
}
