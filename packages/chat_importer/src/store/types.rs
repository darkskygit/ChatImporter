use async_trait::async_trait;
use std::collections::HashMap;

use super::ChatStore;

pub type Attachments = HashMap<String, Attachment>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    bytes: Vec<u8>,
    analysis_bytes: Option<Vec<u8>>,
    modified_at: Option<u64>,
}

impl Attachment {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            analysis_bytes: None,
            modified_at: None,
        }
    }

    pub fn with_analysis_bytes(bytes: Vec<u8>, analysis_bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            analysis_bytes: Some(analysis_bytes),
            modified_at: None,
        }
    }

    pub fn with_modified_at(mut self, modified_at: Option<u64>) -> Self {
        self.modified_at = modified_at;
        self
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn analysis_bytes(&self) -> Option<&[u8]> {
        self.analysis_bytes.as_deref()
    }

    pub fn modified_at(&self) -> Option<u64> {
        self.modified_at
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl From<Vec<u8>> for Attachment {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}

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
    pub source_kind: Option<String>,
    pub source_group_id: Option<String>,
    pub source_message_id: Option<String>,
    pub source_backup_id: Option<String>,
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
    pub fn attachment_count(&self) -> usize {
        match self {
            Self::Record(_) => 0,
            Self::RecordWithAttachments { attachments, .. } => attachments.len(),
        }
    }

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
// Planned query DTO for browse/search UI and CLI repair commands.
#[allow(dead_code)]
pub struct Query {
    pub chat_type: Option<String>,
    pub owner_id: Option<String>,
    pub group_id: Option<String>,
    pub conversation_key: Option<String>,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    pub keyword: Option<String>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub offset: Option<u64>,
    pub limit: Option<u32>,
    pub include_duplicates: bool,
}

// Helper defaults used by the planned query API.
#[allow(dead_code)]
impl Query {
    pub fn offset(&self) -> u64 {
        self.offset.unwrap_or(0)
    }

    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(100)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
// Request DTO for manually merging source conversations after multi-device imports.
#[allow(dead_code)]
pub struct ConversationMergeRequest {
    pub chat_type: String,
    pub owner_id: String,
    pub source_group_ids: Vec<String>,
    pub target_conversation_key: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
// Preview DTO for merge tools to show duplicates/conflicts before applying changes.
#[allow(dead_code)]
pub struct ConversationMergePreview {
    pub source_group_ids: Vec<String>,
    pub duplicate_candidates: Vec<RecordDuplicateCandidate>,
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
// Candidate DTO for future duplicate-review UI/CLI flows.
#[allow(dead_code)]
pub struct RecordDuplicateCandidate {
    pub canonical_record_id: i64,
    pub duplicate_record_id: i64,
    pub reason: String,
    pub confidence: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetWriteOutcome {
    pub asset_hash: Vec<u8>,
    pub canonical_asset_hash: Vec<u8>,
    pub original_bytes: u64,
    pub estimated_stored_bytes: u64,
    pub new_stored_bytes: u64,
    pub new_objects: usize,
    pub exact_asset_new: bool,
    pub canonical_asset_new: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteOutcome {
    pub record_id: i64,
    pub record_inserted: bool,
    pub record_updated: bool,
    pub attachments_seen: usize,
    pub attachment_original_bytes: u64,
    pub assets: Vec<AssetWriteOutcome>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataMergeContext {
    pub old_conversation_latest_timestamp: Option<i64>,
    pub new_conversation_latest_timestamp: Option<i64>,
}

#[async_trait]
pub trait MetadataMerger: Send + Sync {
    async fn merge(
        &self,
        store: &ChatStore,
        new_attachments: &Attachments,
        context: &MetadataMergeContext,
        old_metadata: Vec<u8>,
        new_metadata: Vec<u8>,
    ) -> Option<Vec<u8>>;
}
