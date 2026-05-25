use super::*;

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

pub(super) fn load_record_lines<S: ToString>(
    messages: &[Arc<NamedTempFile>],
    chats: &HashMap<String, String>,
    user_name: S,
) -> SqliteResult<Vec<RecordLine>> {
    let mut lines = vec![];
    let user_name = user_name.to_string();
    let hash = chats
        .keys()
        .find(|h| h.as_str() == user_name)
        .map(|s| s.into())
        .unwrap_or_else(|| gen_md5(user_name));
    for message in find_chat_table(messages, &hash) {
        if let Some(conn) = get_conn(Some(message.clone()))? {
            lines.append(
                &mut conn
                    .prepare(&format!(
                        "SELECT
                            MesLocalID,
                            MesSvrID,
                            CreateTime,
                            Message,
                            Status,
                            ImgStatus,
                            Type,
                            Des
                        FROM
                            Chat_{}",
                        hash
                    ))?
                    .query_map(params![], |row| {
                        Ok(RecordLine {
                            local_id: row.get(0)?,
                            server_id: row.get(1)?,
                            created_time: row.get(2)?,
                            message: row.get(3)?,
                            status: row.get(4)?,
                            image_status: row.get(5)?,
                            msg_type: MsgType::try_from(row.get::<_, u32>(6)?).unwrap_or_else(
                                |t| {
                                    warn!("unknown type: {}", t);
                                    MsgType::Unknown
                                },
                            ),
                            is_dest: row.get(7)?,
                        })
                    })?
                    .filter_map(|r| {
                        r.map_err(|e| warn!("failed to parse chat line: {}", e))
                            .ok()
                    })
                    .collect(),
            );
        }
    }
    Ok(lines)
}
