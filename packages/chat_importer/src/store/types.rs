use async_trait::async_trait;
use std::collections::HashMap;

use super::ChatStore;

pub type Attachments = HashMap<String, Vec<u8>>;

#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct Record {
    pub id: Option<i64>,
    pub chat_type: String,
    pub owner_id: String,
    pub group_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: i64,
    pub metadata: Option<Vec<u8>>,
}

impl Record {
    pub fn display(&self) -> String {
        format!(
            "{} ({}): {}",
            self.sender_name, self.sender_id, self.content
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecordType {
    Record(Record),
    RecordWithAttachments {
        record: Record,
        attachments: Attachments,
    },
}

impl RecordType {
    pub fn into_parts(self) -> (Record, Attachments) {
        match self {
            Self::Record(record) => (record, Attachments::new()),
            Self::RecordWithAttachments {
                record,
                attachments,
            } => (record, attachments),
        }
    }

    pub fn get_record(&self) -> &Record {
        match self {
            Self::Record(record) | Self::RecordWithAttachments { record, .. } => record,
        }
    }

    pub fn display(&self) -> String {
        self.get_record().display()
    }
}

impl From<Record> for RecordType {
    fn from(record: Record) -> Self {
        Self::Record(record)
    }
}

impl From<(Record, Attachments)> for RecordType {
    fn from((record, attachments): (Record, Attachments)) -> Self {
        Self::RecordWithAttachments {
            record,
            attachments,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub chat_type: Option<String>,
    pub owner_id: Option<String>,
    pub group_id: Option<String>,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    pub keyword: Option<String>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub offset: Option<u64>,
    pub limit: Option<u32>,
}

impl Query {
    pub fn offset(&self) -> u64 {
        self.offset.unwrap_or(0)
    }

    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(100)
    }
}

#[async_trait]
pub trait MetadataMerger: Send + Sync {
    async fn merge(
        &self,
        store: &ChatStore,
        new_attachments: &Attachments,
        old_metadata: Vec<u8>,
        new_metadata: Vec<u8>,
    ) -> Option<Vec<u8>>;
}
