use super::message::get_conn;
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ConversationSummary {
    pub(super) chat_id: String,
    pub(super) display_name: Option<String>,
    pub(super) abstract_text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SessionIndex {
    pub(super) summaries: HashMap<String, ConversationSummary>,
}

impl SessionIndex {
    pub(super) fn load(file: Option<Arc<NamedTempFile>>) -> SqliteResult<Self> {
        let mut index = Self::default();
        let Some(conn) = get_conn(file)? else {
            return Ok(index);
        };
        if let Ok(mut stmt) =
            conn.prepare("SELECT strUsrName, strNickName, strContent FROM SessionAbstract")
        {
            let summaries = stmt
                .query_map(params![], |row| {
                    let chat_id: String = row.get(0)?;
                    Ok((
                        chat_id.clone(),
                        ConversationSummary {
                            chat_id,
                            display_name: row.get(1).ok(),
                            abstract_text: row.get(2).ok(),
                        },
                    ))
                })?
                .filter_map(|row| {
                    row.map_err(|err| warn!("failed to parse session row: {}", err))
                        .ok()
                })
                .collect::<HashMap<_, _>>();
            index.summaries = summaries;
        }
        if let Ok(mut stmt) =
            conn.prepare("SELECT strUsrName, strNickName, strContent FROM UniversalSession")
        {
            let summaries = stmt
                .query_map(params![], |row| {
                    let chat_id: String = row.get(0)?;
                    Ok((
                        chat_id.clone(),
                        ConversationSummary {
                            chat_id,
                            display_name: row.get(1).ok(),
                            abstract_text: row.get(2).ok(),
                        },
                    ))
                })?
                .filter_map(|row| {
                    row.map_err(|err| warn!("failed to parse universal session row: {}", err))
                        .ok()
                })
                .collect::<HashMap<_, _>>();
            index.summaries.extend(summaries);
        }
        Ok(index)
    }
}
