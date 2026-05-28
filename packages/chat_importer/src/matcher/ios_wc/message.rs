use super::*;
use rusqlite::types::ValueRef;

const WCDB_MSG_DICT: &[u8] = include_bytes!("MsgDict.dict");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TryFromPrimitive)]
#[repr(u32)]
#[derive(Default)]
pub(super) enum MsgType {
    Normal = 1,              // 文字/emoji
    Image = 3,               // 图片
    Voice = 34,              // 语音
    ContactShare = 42,       // 联系人分享
    Video = 43,              // 视频
    BigEmoji = 47,           // 大表情
    Location = 48,           // 定位
    CustomApp = 49,          // 文件、分享、转账、聊天记录批量转发
    VoipContent = 50,        // 语音/视频通话？
    ShortVideo = 62,         // 短视频？
    VoipStatus = 64,         // 语音通话状态
    WeWorkContactShare = 66, // 企业微信联系人分享
    OpenIMContactShare = 67, // OpenIM 联系人分享
    System = 10000,          // 系统信息，入群/群改名/他人撤回信息/红包领取提醒等等
    Revoke = 10002,          // 撤回信息修改
    #[default]
    Unknown = u32::MAX,
}

#[derive(Clone, Debug)]
pub(super) struct RecordLine {
    pub(super) local_id: i64,
    pub(super) server_id: i64,
    pub(super) created_time: i64,
    pub(super) message: String,
    // message status can be useful for future sent/failed/deleted state filtering.
    #[allow(dead_code)]
    pub(super) status: u8,
    // Image status can help future attachment recovery distinguish pending thumbnails.
    #[allow(dead_code)]
    pub(super) image_status: u16,
    pub(super) msg_type: MsgType,
    pub(super) is_dest: bool,
    pub(super) msg_source: Option<String>,
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

pub(super) fn find_chat_table(
    messages: &[Arc<NamedTempFile>],
    hash: &str,
) -> Vec<Arc<NamedTempFile>> {
    let query = format!(
        r#"SELECT name FROM sqlite_master where type='table' and name like "Chat\_{}" ESCAPE '\'"#,
        hash
    );
    messages
        .iter()
        .filter(|&file| {
            get_conn(Some(file.clone()))
                .and_then(|conn| {
                    if let Some(conn) = conn {
                        conn.prepare(&query)?.exists(params![])
                    } else {
                        Err(rusqlite::Error::InvalidQuery)
                    }
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
pub(super) fn load_record_lines<S: ToString>(
    messages: &[Arc<NamedTempFile>],
    chats: &HashMap<String, String>,
    user_name: S,
) -> SqliteResult<Vec<RecordLine>> {
    let mut lines = vec![];
    load_record_line_chunks(messages, chats, user_name, usize::MAX, |_, chunk| {
        lines.extend(chunk);
    })?;
    Ok(lines)
}

pub(super) fn load_record_line_chunks<S, F>(
    messages: &[Arc<NamedTempFile>],
    chats: &HashMap<String, String>,
    user_name: S,
    chunk_size: usize,
    mut emit: F,
) -> SqliteResult<usize>
where
    S: ToString,
    F: FnMut(usize, Vec<RecordLine>),
{
    let chunk_size = chunk_size.max(1);
    let mut chunk = Vec::with_capacity(chunk_size.min(1024));
    let mut chunk_index = 0;
    let mut line_count = 0;
    let user_name = user_name.to_string();
    let hash = chats
        .keys()
        .find(|h| h.as_str() == user_name)
        .map(|s| s.into())
        .unwrap_or_else(|| gen_md5(user_name));
    for message in find_chat_table(messages, &hash) {
        if let Some(conn) = get_conn(Some(message.clone()))? {
            let table = format!("Chat_{}", hash);
            let compression_column = if has_column(&conn, &table, "WCDB_CT_Message")? {
                "WCDB_CT_Message"
            } else {
                "0"
            };
            let mut msg_sources = load_msg_sources(&conn, &hash)?;
            let mut compressed_skipped = HashMap::<(u32, i64), usize>::new();
            let mut stmt = conn.prepare(&format!(
                "SELECT
                            MesLocalID,
                            MesSvrID,
                            CreateTime,
                            Message,
                            Status,
                            ImgStatus,
                            Type,
                            Des,
                            {}
                        FROM
                            {}",
                compression_column, table
            ))?;
            let mut rows = stmt.query(params![])?;
            while let Some(row) = rows.next()? {
                let line = (|| -> SqliteResult<Option<RecordLine>> {
                    let local_id = row.get(0)?;
                    let msg_type = row.get::<_, u32>(6)?;
                    let compression_type = row.get::<_, i64>(8)?;
                    let message = match decode_message(row.get_ref(3)?, compression_type) {
                        Ok(Some(message)) => message,
                        Ok(None) => {
                            *compressed_skipped
                                .entry((msg_type, compression_type))
                                .or_default() += 1;
                            return Ok(None);
                        }
                        Err(error) => {
                            warn!("failed to parse chat line: {}", error);
                            return Ok(None);
                        }
                    };
                    Ok(Some(RecordLine {
                        local_id,
                        server_id: row.get(1)?,
                        created_time: row.get(2)?,
                        message,
                        status: row.get(4)?,
                        image_status: row.get(5)?,
                        msg_type: MsgType::try_from(msg_type).unwrap_or_else(|t| {
                            warn!("unknown type: {}", t);
                            MsgType::Unknown
                        }),
                        is_dest: row.get(7)?,
                        msg_source: msg_sources.remove(&local_id),
                    }))
                })();
                let Some(line) = line
                    .map_err(|error| warn!("failed to parse chat line: {}", error))
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                chunk.push(line);
                line_count += 1;
                if chunk.len() >= chunk_size {
                    emit(chunk_index, std::mem::take(&mut chunk));
                    chunk_index += 1;
                }
            }
            if !compressed_skipped.is_empty() {
                let mut skipped = compressed_skipped
                    .iter()
                    .map(|((msg_type, compression_type), count)| {
                        format!(
                            "type={}, compression={}, count={}",
                            msg_type, compression_type, count
                        )
                    })
                    .collect::<Vec<_>>();
                skipped.sort();
                warn!(
                    "unsupported compressed WeChat message content: table={}, {}",
                    table,
                    skipped.join("; ")
                );
            }
        }
    }
    if !chunk.is_empty() {
        emit(chunk_index, chunk);
    }
    Ok(line_count)
}

fn load_msg_sources(conn: &Connection, hash: &str) -> SqliteResult<HashMap<i64, String>> {
    let table = format!("ChatExt2_{}", hash);
    if !table_exists(conn, &table)? {
        return Ok(HashMap::new());
    }
    let compression_column = if has_column(conn, &table, "WCDB_CT_MsgSource")? {
        "WCDB_CT_MsgSource"
    } else {
        "0"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT MesLocalID, MsgSource, {} FROM {}",
        compression_column, table
    ))?;
    let mut rows = stmt.query(params![])?;
    let mut sources = HashMap::new();
    while let Some(row) = rows.next()? {
        let local_id = row.get::<_, i64>(0)?;
        let compression_type = row.get::<_, i64>(2)?;
        match decode_message(row.get_ref(1)?, compression_type) {
            Ok(Some(source)) if !source.is_empty() => {
                sources.insert(local_id, source);
            }
            Ok(_) => {}
            Err(error) => {
                warn!(
                    "failed to parse WeChat message source: table={}, local_id={}, {}",
                    table, local_id, error
                );
            }
        }
    }
    Ok(sources)
}

fn table_exists(conn: &Connection, table: &str) -> SqliteResult<bool> {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")?
        .exists(params![table])
}

fn has_column(conn: &Connection, table: &str, column: &str) -> SqliteResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let columns = stmt
        .query_map(params![], |row| row.get::<_, String>(1))?
        .collect::<SqliteResult<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn decode_message(value: ValueRef<'_>, compression_type: i64) -> SqliteResult<Option<String>> {
    match value {
        ValueRef::Text(bytes) => from_utf8(bytes)
            .map(|message| Some(message.to_string()))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    bytes.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            }),
        ValueRef::Blob(bytes) if compression_type == 4 => zstd::decode_all(bytes)
            .map(|message| Some(String::from_utf8_lossy(&message).into_owned()))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    bytes.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            }),
        ValueRef::Blob(bytes) if compression_type == 2 => {
            zstd::bulk::Decompressor::with_dictionary(WCDB_MSG_DICT)
                .and_then(|mut decompressor| decompressor.decompress(bytes, 64 * 1024 * 1024))
                .map(|message| Some(String::from_utf8_lossy(&message).into_owned()))
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        bytes.len(),
                        rusqlite::types::Type::Blob,
                        Box::new(error),
                    )
                })
        }
        ValueRef::Blob(bytes) if compression_type == 0 => {
            Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
        }
        ValueRef::Blob(_) => Ok(None),
        ValueRef::Null => Ok(Some(String::new())),
        _ => Ok(Some(value.as_i64()?.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn sqlite_bytes(name: &str, build: impl FnOnce(&Connection)) -> Vec<u8> {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        let conn = Connection::open(&path).unwrap();
        build(&conn);
        drop(conn);
        fs::read(path).unwrap()
    }

    fn chat_db(chat_id: &str, messages: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        let hash = gen_md5(chat_id);
        {
            let conn = Connection::open(file.path()).unwrap();
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
                    hash
                ),
                params![],
            )
            .unwrap();
            for (index, message) in messages.iter().enumerate() {
                conn.execute(
                    &format!(
                        "INSERT INTO Chat_{} (
                            MesLocalID, MesSvrID, CreateTime, Message, Status, ImgStatus, Type, Des
                        ) VALUES (?1, ?2, ?3, ?4, 0, 0, 1, 0)",
                        hash
                    ),
                    params![
                        index as i64 + 1,
                        index as i64 + 10,
                        1000 + index as i64,
                        message
                    ],
                )
                .unwrap();
            }
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn load_record_line_chunks_splits_rows_in_order() {
        let file = Arc::new(chat_db("chat-a", &["first", "second", "third"]));
        let mut chunks = Vec::new();
        let total =
            load_record_line_chunks(&[file], &HashMap::new(), "chat-a", 2, |index, lines| {
                chunks.push((
                    index,
                    lines
                        .into_iter()
                        .map(|line| (line.local_id, line.message))
                        .collect::<Vec<_>>(),
                ));
            })
            .unwrap();

        assert_eq!(total, 3);
        assert_eq!(
            chunks,
            vec![
                (0, vec![(1, "first".into()), (2, "second".into())]),
                (1, vec![(3, "third".into())]),
            ]
        );
    }

    #[test]
    fn load_record_lines_decodes_zstd_normal_message_blob() {
        let chat = "friend";
        let chat_hash = gen_md5(chat);
        let bytes = sqlite_bytes("message_0.sqlite", |conn| {
            let table = format!("Chat_{}", chat_hash);
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        MesLocalID INTEGER NOT NULL,
                        MesSvrID INTEGER NOT NULL,
                        CreateTime INTEGER NOT NULL,
                        Message BLOB NOT NULL,
                        Status INTEGER NOT NULL,
                        ImgStatus INTEGER NOT NULL,
                        Type INTEGER NOT NULL,
                        Des INTEGER NOT NULL,
                        WCDB_CT_Message INTEGER NOT NULL
                    )",
                    table
                ),
                [],
            )
            .unwrap();
            let message = zstd::encode_all("compressed hello".as_bytes(), 0).unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table
                ),
                (1, 42, 1_598_219_157, message, 1, 0, 1, 0, 4),
            )
            .unwrap();
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();

        let lines = load_record_lines(&[Arc::new(file)], &HashMap::new(), chat).unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "compressed hello");
    }

    #[test]
    fn load_record_lines_decodes_wcdb_dict_compressed_message_blob() {
        let chat = "friend";
        let chat_hash = gen_md5(chat);
        let bytes = sqlite_bytes("message_0.sqlite", |conn| {
            let table = format!("Chat_{}", chat_hash);
            let ext_table = format!("ChatExt2_{}", chat_hash);
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        MesLocalID INTEGER NOT NULL,
                        MesSvrID INTEGER NOT NULL,
                        CreateTime INTEGER NOT NULL,
                        Message BLOB NOT NULL,
                        Status INTEGER NOT NULL,
                        ImgStatus INTEGER NOT NULL,
                        Type INTEGER NOT NULL,
                        Des INTEGER NOT NULL,
                        WCDB_CT_Message INTEGER NOT NULL
                    )",
                    table
                ),
                [],
            )
            .unwrap();
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        MesLocalID INTEGER NOT NULL,
                        MsgSource BLOB,
                        WCDB_CT_MsgSource INTEGER NOT NULL
                    )",
                    ext_table
                ),
                [],
            )
            .unwrap();
            let message =
                zstd::bulk::Compressor::with_dictionary(0, include_bytes!("MsgDict.dict"))
                    .and_then(|mut compressor| compressor.compress("dict hello".as_bytes()))
                    .unwrap();
            let source = zstd::bulk::Compressor::with_dictionary(0, include_bytes!("MsgDict.dict"))
                .and_then(|mut compressor| {
                    compressor.compress(
                        "<msgsource><sequence_id>seq-compressed</sequence_id></msgsource>"
                            .as_bytes(),
                    )
                })
                .unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table
                ),
                (1, 42, 1_598_219_157, message, 1, 0, 1, 0, 2),
            )
            .unwrap();
            conn.execute(
                &format!("INSERT INTO {} VALUES (?1, ?2, ?3)", ext_table),
                (1, source, 2),
            )
            .unwrap();
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();

        let lines = load_record_lines(&[Arc::new(file)], &HashMap::new(), chat).unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "dict hello");
        assert_eq!(
            lines[0].msg_source.as_deref(),
            Some("<msgsource><sequence_id>seq-compressed</sequence_id></msgsource>")
        );
    }
}
