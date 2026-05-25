use super::*;
use chrono::{Duration, TimeZone, Utc};
use ibackuptool2::Backup;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use serde::Serialize;
use std::io::Write;
use tempfile::NamedTempFile;

#[derive(Debug)]
struct Conversation {
    rowid: i64,
    guid: Option<String>,
    chat_identifier: Option<String>,
    _display_name: Option<String>,
    group_id: Option<String>,
    account_login: Option<String>,
    last_addressed_sim_id: Option<String>,
    participants: Vec<String>,
}

impl Conversation {
    fn source_group_id(&self, service: &str) -> String {
        if let Some(guid) = non_empty(self.guid.as_deref()) {
            return guid.to_string();
        }

        let mut participants = self
            .participants
            .iter()
            .filter_map(|participant| non_empty(Some(participant)))
            .map(str::to_string)
            .collect::<Vec<_>>();
        participants.sort();
        participants.dedup();

        let identifier = non_empty(self.chat_identifier.as_deref())
            .or_else(|| non_empty(self.group_id.as_deref()))
            .unwrap_or("unknown-chat");
        let service = non_empty(Some(service)).unwrap_or("unknown-service");
        format!("{}:{}:{}", service, participants.join(","), identifier)
    }
}

#[derive(Debug)]
struct RecordLine {
    rowid: i64,
    guid: Option<String>,
    text: String,
    sender_handle: Option<String>,
    service: String,
    date: i64,
    is_from_me: bool,
    destination_caller_id: Option<String>,
    account: Option<String>,
    account_guid: Option<String>,
}

#[derive(Serialize)]
struct SmsMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    message_guid: Option<String>,
    message_service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_account_guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_destination_caller_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_account_login: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_last_addressed_sim_id: Option<String>,
}

#[allow(non_camel_case_types)]
struct Extractor {
    conn: Connection,
    owner_name: String,
    owner_id: String,
    source_backup_id: String,
}

impl Extractor {
    pub fn new<P: AsRef<Path>>(
        path: P,
        owner_name: String,
        owner_id: String,
        source_backup_id: String,
    ) -> SqliteResult<Self> {
        Ok(Self {
            conn: Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?,
            owner_name,
            owner_id,
            source_backup_id,
        })
    }

    fn table_has_column(&self, table: &str, column: &str) -> SqliteResult<bool> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = ?1",
            table
        ))?;
        let count: i64 = stmt.query_row(params![column], |row| row.get(0))?;
        Ok(count > 0)
    }

    fn optional_column_expr(&self, table: &str, column: &str) -> SqliteResult<String> {
        Ok(if self.table_has_column(table, column)? {
            column.to_string()
        } else {
            "NULL".to_string()
        })
    }

    fn get_conversations(&self) -> SqliteResult<Vec<Conversation>> {
        let account_login_expr = self.optional_column_expr("chat", "account_login")?;
        let last_addressed_sim_id_expr =
            self.optional_column_expr("chat", "last_addressed_sim_id")?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT ROWID, guid, chat_identifier, display_name, group_id, {account_login_expr}, {last_addressed_sim_id_expr}
             FROM chat
             ORDER BY ROWID"
        ))?;
        let conversations = stmt
            .query_map(params![], |row| {
                Ok(Conversation {
                    rowid: row.get(0)?,
                    guid: row.get(1)?,
                    chat_identifier: row.get(2)?,
                    _display_name: row.get(3)?,
                    group_id: row.get(4)?,
                    account_login: row.get(5)?,
                    last_addressed_sim_id: row.get(6)?,
                    participants: Vec::new(),
                })
            })?
            .filter_map(|row| row.ok())
            .collect::<Vec<_>>();

        conversations
            .into_iter()
            .map(|mut conversation| {
                conversation.participants = self.get_participants(conversation.rowid)?;
                Ok(conversation)
            })
            .collect()
    }

    fn get_participants(&self, chat_id: i64) -> SqliteResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT handle.id
             FROM chat_handle_join
             INNER JOIN handle ON handle.ROWID = chat_handle_join.handle_id
             WHERE chat_handle_join.chat_id = ?
             ORDER BY handle.id",
        )?;
        let participants = stmt
            .query_map(params![chat_id], |row| row.get::<_, String>(0))?
            .filter_map(|row| row.ok())
            .collect();
        Ok(participants)
    }

    fn get_record_lines(&self, conversation: &Conversation) -> SqliteResult<Vec<Record>> {
        let account_expr = self.optional_column_expr("message", "account")?;
        let account_guid_expr = self.optional_column_expr("message", "account_guid")?;
        let guid_expr = self.optional_column_expr("message", "guid")?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT
                message.ROWID,
                {guid_expr},
                COALESCE(message.text, ''),
                handle.id,
                COALESCE(message.service, ''),
                message.date,
                message.is_from_me,
                message.destination_caller_id,
                {account_expr},
                {account_guid_expr}
            FROM chat_message_join
            INNER JOIN message
                ON message.ROWID = chat_message_join.message_id
            LEFT JOIN handle
                ON handle.ROWID = message.handle_id
            WHERE chat_message_join.chat_id = ?
            ORDER BY message.date ASC, message.ROWID ASC",
        ))?;
        let records_iter = stmt.query_map(params![conversation.rowid], |row| {
            Ok(RecordLine {
                rowid: row.get(0)?,
                guid: row.get(1)?,
                text: row.get(2)?,
                sender_handle: row.get(3)?,
                service: row.get(4)?,
                date: row.get(5)?,
                is_from_me: row.get(6)?,
                destination_caller_id: row.get(7)?,
                account: row.get(8)?,
                account_guid: row.get(9)?,
            })
        })?;

        Ok(records_iter
            .filter_map(|record| record.ok())
            .filter_map(|record| self.record_from_line(conversation, record))
            .collect())
    }

    fn record_from_line(&self, conversation: &Conversation, record: RecordLine) -> Option<Record> {
        let source_group_id = conversation.source_group_id(&record.service);
        let sender_handle = record
            .sender_handle
            .as_deref()
            .and_then(|value| non_empty(Some(value)));
        let sender_id = if record.is_from_me {
            self.owner_id.clone()
        } else {
            sender_handle?.to_string()
        };
        let sender_name = if record.is_from_me {
            self.owner_name.clone()
        } else {
            sender_id.clone()
        };
        let source_message_id = record
            .guid
            .as_deref()
            .and_then(|value| non_empty(Some(value)))
            .map(str::to_string)
            // Older or partial SMS exports may miss message guid. Rowid is scoped by the chat
            // source group here so two guid-less messages in the same import are not merged.
            .unwrap_or_else(|| format!("{}:rowid:{}", source_group_id, record.rowid));
        let metadata = SmsMetadata {
            message_guid: record
                .guid
                .as_deref()
                .and_then(|value| non_empty(Some(value)))
                .map(str::to_string),
            message_service: record.service.clone(),
            message_account: record.account.clone(),
            message_account_guid: record.account_guid.clone(),
            message_destination_caller_id: record.destination_caller_id.clone(),
            chat_guid: conversation.guid.clone(),
            chat_account_login: conversation.account_login.clone(),
            chat_last_addressed_sim_id: conversation.last_addressed_sim_id.clone(),
        };

        Some(Record {
            chat_type: format!("iOS {}", record.service),
            owner_id: self.owner_id.clone(),
            group_id: source_group_id.clone(),
            sender_id,
            sender_name,
            content: record.text,
            timestamp: (base_date_offset() + Duration::nanoseconds(record.date)).timestamp_millis(),
            metadata: serde_json::to_vec(&metadata).ok(),
            source_kind: Some("ios-sms".into()),
            source_group_id: Some(source_group_id),
            source_message_id: Some(source_message_id),
            source_backup_id: Some(self.source_backup_id.clone()),
            ..Default::default()
        })
    }
}

impl MsgMatcher for Extractor {
    fn import_plan(&self) -> ImportPlan {
        ImportPlan {
            chats_total: self
                .get_conversations()
                .ok()
                .map(|conversations| conversations.len() as u64),
        }
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
        let conversations = self.get_conversations()?;
        progress.chat_planned(conversations.len() as u64);
        Ok(conversations
            .iter()
            .filter_map(|conversation| {
                self.get_record_lines(conversation)
                    .map_err(|e| {
                        warn!(
                            "Failed to get sms conversation {}: {}",
                            conversation.rowid, e
                        )
                    })
                    .ok()
                    .map(|records| {
                        let records = records
                            .into_iter()
                            .map(RecordType::from)
                            .collect::<Vec<_>>();
                        progress
                            .chat_parsed(records.len() as u64, record_blob_count(&records) as u64);
                        RecordBatch {
                            label: conversation.rowid.to_string(),
                            records,
                        }
                    })
            })
            .collect())
    }
}

#[allow(non_camel_case_types)]
pub struct Matcher {
    _smsdb: NamedTempFile,
    extractor: Extractor,
}

impl Matcher {
    pub fn from_backup(
        backup: &Backup,
        owner_name: String,
        owner_id: Option<String>,
        source_backup_id: String,
    ) -> Result<Option<Box<dyn MsgMatcher>>> {
        let owner_id = match resolve_owner_id(backup, owner_id) {
            Some(owner_id) => owner_id,
            None => {
                warn!("skipping SMS backup without explicit owner id or backup device id");
                return Ok(None);
            }
        };

        if let Some(sms) = backup.find_path("HomeDomain", "Library/SMS/sms.db") {
            let mut tempfile = NamedTempFile::new()?;
            tempfile.write_all(
                &backup
                    .read_file(&sms)
                    .map_err(|e| anyhow::anyhow!("{}", e))?,
            )?;
            Ok(Some(Box::new(Self {
                extractor: Extractor::new(tempfile.path(), owner_name, owner_id, source_backup_id)?,
                _smsdb: tempfile,
            }) as Box<dyn MsgMatcher>))
        } else {
            Err(anyhow::anyhow!("Failed to find sms database"))
        }
    }
}

impl MsgMatcher for Matcher {
    fn import_plan(&self) -> ImportPlan {
        self.extractor.import_plan()
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
        self.extractor.get_record_batches(progress)
    }
}

fn resolve_owner_id(backup: &Backup, explicit_owner_id: Option<String>) -> Option<String> {
    resolve_sms_owner_id(
        explicit_owner_id.as_deref(),
        &backup.info.target_identifier,
        &backup.manifest.lockdown.unique_device_id,
    )
}

fn resolve_sms_owner_id(
    explicit_owner_id: Option<&str>,
    target_identifier: &str,
    unique_device_id: &str,
) -> Option<String> {
    explicit_owner_id
        .and_then(|owner_id| non_empty(Some(owner_id)).map(str::to_string))
        .or_else(|| {
            non_empty(Some(target_identifier))
                .or_else(|| non_empty(Some(unique_device_id)))
                .map(|device_id| {
                    warn!(
                        "SMS import has no explicit owner id; using backup device fallback {}",
                        device_id
                    );
                    format!("sms-device:{}", device_id)
                })
        })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn base_date_offset() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(978307200, 0).single().unwrap()
}

#[tokio::test]
async fn ios_sms_minimal_sample() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-1', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-1', 'hello sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1);
        "#,
    )?;
    drop(conn);

    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    let records = matcher.collect_records().unwrap();
    assert_eq!(records.len(), 1);
    let record = records[0].get_record();
    assert_eq!(record.chat_type, "iOS iMessage");
    assert_eq!(record.owner_id, "owner-id");
    assert_eq!(record.group_id, "chat-guid-1");
    assert_eq!(record.sender_id, "+10000000000");
    assert_eq!(record.sender_name, "+10000000000");
    assert_eq!(record.content, "hello sms");
    assert_eq!(record.source_kind.as_deref(), Some("ios-sms"));
    assert_eq!(record.source_group_id.as_deref(), Some("chat-guid-1"));
    assert_eq!(record.source_message_id.as_deref(), Some("msg-guid-1"));

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();
    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "hello sms");
    Ok(())
}

#[tokio::test]
async fn ios_sms_group_chat_keeps_all_senders_in_one_group() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-group', 'chat-id', 'Group', NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000001'), (2, '+10000000002');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1), (7, 2);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-1', 'from one', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1'),
            (2, 'msg-guid-2', 'from two', 2, 'iMessage', 1, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1), (7, 2);
        "#,
    )?;
    drop(conn);

    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    let records = matcher.collect_records().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.get_record().group_id == "chat-guid-group"));
    assert_eq!(records[0].get_record().sender_id, "+10000000001");
    assert_eq!(records[1].get_record().sender_id, "+10000000002");
    Ok(())
}

#[tokio::test]
async fn ios_sms_same_source_group_sender_timestamp_isolated_by_owner() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-shared', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-shared', 'hello sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1);
        "#,
    )?;
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    let first = Extractor::new(
        file.path(),
        "Owner A".into(),
        "owner-a".into(),
        "backup-a".into(),
    )?;
    let second = Extractor::new(
        file.path(),
        "Owner B".into(),
        "owner-b".into(),
        "backup-b".into(),
    )?;
    export_matcher(&mut store, &test_progress(), &first)
        .await
        .unwrap();
    export_matcher(&mut store, &test_progress(), &second)
        .await
        .unwrap();

    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|record| record.owner_id == "owner-a"));
    assert!(stored.iter().any(|record| record.owner_id == "owner-b"));
    Ok(())
}

#[test]
fn ios_sms_owner_fallback_uses_device_id_not_line_metadata() {
    assert_eq!(
        resolve_sms_owner_id(None, "device-a", "lockdown-a").as_deref(),
        Some("sms-device:device-a")
    );
    assert_eq!(
        resolve_sms_owner_id(None, "", "lockdown-a").as_deref(),
        Some("sms-device:lockdown-a")
    );
    assert_eq!(resolve_sms_owner_id(None, "", ""), None);
    assert_eq!(
        resolve_sms_owner_id(Some("explicit-owner"), "device-a", "lockdown-a").as_deref(),
        Some("explicit-owner")
    );
}

#[tokio::test]
async fn ios_sms_line_changes_only_update_metadata_with_explicit_owner() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-line', '+10000000000', NULL, NULL, 'P:+11111111111', 'sim-2');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-1', 'first line', 1, 'SMS', 0, 1, '+11111111111', 'P:+11111111111', 'account-guid-1'),
            (2, 'msg-guid-2', 'second line', 1, 'SMS', 1, 1, '+22222222222', 'P:+22222222222', 'account-guid-2');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1), (7, 2);
        "#,
    )?;
    drop(conn);

    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    let records = matcher.collect_records().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.get_record().owner_id == "owner-id"));
    assert!(records
        .iter()
        .all(|record| record.get_record().group_id == "chat-guid-line"));
    assert!(records
        .iter()
        .all(|record| record.get_record().sender_id == "owner-id"));

    let metadata: serde_json::Value =
        serde_json::from_slice(records[1].get_record().metadata.as_ref().unwrap()).unwrap();
    assert_eq!(
        metadata["message_destination_caller_id"].as_str(),
        Some("+22222222222")
    );
    assert_eq!(metadata["message_account"].as_str(), Some("P:+22222222222"));
    assert_eq!(
        metadata["chat_last_addressed_sim_id"].as_str(),
        Some("sim-2")
    );
    Ok(())
}

#[tokio::test]
async fn ios_sms_duplicate_message_guid_updates_existing_record() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-1', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-1', 'hello sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1);
        "#,
    )?;
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();

    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].source_message_id.as_deref(), Some("msg-guid-1"));
    Ok(())
}

#[tokio::test]
async fn ios_sms_distinct_message_guids_with_same_legacy_key_both_insert() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-1', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, 'msg-guid-1', 'first sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1'),
            (2, 'msg-guid-2', 'second sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1), (7, 2);
        "#,
    )?;
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();

    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored
        .iter()
        .any(|record| record.source_message_id.as_deref() == Some("msg-guid-1")));
    assert!(stored
        .iter()
        .any(|record| record.source_message_id.as_deref() == Some("msg-guid-2")));
    Ok(())
}

#[tokio::test]
async fn ios_sms_missing_message_guid_reimport_uses_rowid_source_message_id() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-1', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, NULL, 'hello sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1);
        "#,
    )?;
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();

    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].source_kind.as_deref(), Some("ios-sms"));
    assert_eq!(
        stored[0].source_message_id.as_deref(),
        Some("chat-guid-1:rowid:1")
    );
    Ok(())
}

#[tokio::test]
async fn ios_sms_missing_message_guid_same_legacy_key_both_insert() -> SqliteResult<()> {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(file.path())?;
    create_sms_schema(&conn)?;
    conn.execute_batch(
        r#"
        INSERT INTO chat
            (ROWID, guid, chat_identifier, display_name, group_id, account_login, last_addressed_sim_id)
        VALUES
            (7, 'chat-guid-1', '+10000000000', NULL, NULL, 'P:+19999999999', 'sim-1');
        INSERT INTO handle (ROWID, id) VALUES (1, '+10000000000');
        INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (7, 1);
        INSERT INTO message
            (ROWID, guid, text, handle_id, service, date, is_from_me, destination_caller_id, account, account_guid)
        VALUES
            (1, NULL, 'first sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1'),
            (2, NULL, 'second sms', 1, 'iMessage', 0, 0, '+19999999999', 'P:+19999999999', 'account-guid-1');
        INSERT INTO chat_message_join (chat_id, message_id) VALUES (7, 1), (7, 2);
        "#,
    )?;
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
    let matcher = Extractor::new(
        file.path(),
        "Owner".into(),
        "owner-id".into(),
        "backup-1".into(),
    )?;
    export_matcher(&mut store, &test_progress(), &matcher)
        .await
        .unwrap();

    let stored = store.query(crate::store::Query::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored
        .iter()
        .any(|record| record.source_message_id.as_deref() == Some("chat-guid-1:rowid:1")));
    assert!(stored
        .iter()
        .any(|record| record.source_message_id.as_deref() == Some("chat-guid-1:rowid:2")));
    Ok(())
}

#[cfg(test)]
fn create_sms_schema(conn: &Connection) -> SqliteResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE chat (
            ROWID INTEGER PRIMARY KEY,
            guid TEXT,
            chat_identifier TEXT,
            display_name TEXT,
            group_id TEXT,
            account_login TEXT,
            last_addressed_sim_id TEXT
        );
        CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
        CREATE TABLE message (
            ROWID INTEGER PRIMARY KEY,
            guid TEXT,
            text TEXT,
            handle_id INTEGER,
            service TEXT,
            date INTEGER,
            is_from_me INTEGER,
            destination_caller_id TEXT,
            account TEXT,
            account_guid TEXT
        );
        "#,
    )
}
