use super::message::MsgType;
use super::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct MediaMetadata {
    pub(super) role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) logical_name: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) hints: HashMap<String, MetadataValue>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct SenderMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) display_name: Option<String>,
}

impl SenderMetadata {
    fn is_empty(&self) -> bool {
        self.id.is_none() && self.display_name.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct RawMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) parse_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) raw_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<String>,
}

impl RawMetadata {
    fn is_empty(&self) -> bool {
        self.parse_error.is_none() && self.raw_hash.is_none() && self.summary.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub(super) enum MetadataValue {
    Float(f64),
    Str(String),
}

impl MetadataValue {
    #[allow(dead_code)]
    pub fn get_hash(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Float(value) if value.is_finite() && *value >= 0.0 => Some(*value as u64),
            Self::Str(value) => value.parse().ok(),
            _ => None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct IosWcMetadata {
    pub(super) msg_type: MsgType,
    #[serde(default, skip_serializing_if = "SenderMetadata::is_empty")]
    pub(super) sender: SenderMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) app: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) system: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) contact: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) location: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) media: Vec<MediaMetadata>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) fields: HashMap<String, MetadataValue>,
    #[serde(default, skip_serializing_if = "RawMetadata::is_empty")]
    pub(super) raw: RawMetadata,
}

impl IosWcMetadata {
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
            sender: SenderMetadata::default(),
            app: None,
            system: None,
            contact: None,
            location: None,
            media: Vec::new(),
            raw: RawMetadata::default(),
            ..Default::default()
        }
    }

    pub fn media_hashes(&self) -> HashMap<String, MetadataValue> {
        self.media
            .iter()
            .filter_map(|media| {
                media
                    .hash
                    .as_ref()
                    .map(|hash| (media.role.clone(), MetadataValue::Str(hash.clone())))
            })
            .collect()
    }

    pub fn field(&self, name: &str) -> Option<&MetadataValue> {
        self.fields.get(name)
    }

    pub fn field_str(&self, name: &str) -> Option<&str> {
        self.field(name).and_then(|value| match value {
            MetadataValue::Str(value) => Some(value.as_str()),
            _ => None,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn media_hash(&self, role: &str) -> Option<&str> {
        self.media
            .iter()
            .find(|media| media.role == role)
            .and_then(|media| media.hash.as_deref())
    }

    async fn log_media_overrides(
        store: &ChatStore,
        attaches: &Attachments,
        resolved_video_conflicts: &HashMap<String, ResolvedVideoConflict>,
        old_hash: &HashMap<String, MetadataValue>,
        new_hash: &HashMap<String, MetadataValue>,
    ) {
        const CHECK_DIFFERENCE: bool = true;
        if CHECK_DIFFERENCE {
            for (key, (old, new)) in old_hash.keys().filter_map(|key| {
                old_hash.get(key).and_then(|val| {
                    new_hash
                        .get(key)
                        .and_then(|new_val| (val != new_val).then_some((key, (val, new_val))))
                })
            }) {
                if let Some(conflict) = resolved_video_conflicts.get(key) {
                    log_resolved_video_conflict(key, old, new, *conflict);
                    continue;
                }
                if media_override_is_perceptual_match(store, attaches, old, new).await {
                    debug!(
                        r#"metadata override "{}" uses matching media content: "{:?}" -> "{:?}""#,
                        key, old, new
                    );
                } else {
                    warn!(r#"metadata override "{}": "{:?}" -> "{:?}""#, key, old, new);
                }
            }
        }
    }

    pub async fn merge(
        mut self,
        store: &ChatStore,
        attaches: &Attachments,
        context: &MetadataMergeContext,
        old: Self,
    ) -> Self {
        let resolved_video_conflicts = self
            .resolve_video_parse_conflicts(store, attaches, context, &old)
            .await;
        let old_hash = old.media_hashes();
        let new_hash = self.media_hashes();
        let merged_hash = old_hash.clone().into_iter().chain(new_hash).collect();
        Self::log_media_overrides(
            store,
            attaches,
            &resolved_video_conflicts,
            &old_hash,
            &merged_hash,
        )
        .await;
        for old_media in old.media {
            if !self.media.iter().any(|media| media.role == old_media.role) {
                self.media.push(old_media);
            }
        }
        self
    }

    async fn resolve_video_parse_conflicts(
        &mut self,
        store: &ChatStore,
        attaches: &Attachments,
        context: &MetadataMergeContext,
        old: &Self,
    ) -> HashMap<String, ResolvedVideoConflict> {
        let mut resolved = HashMap::new();
        for old_media in &old.media {
            let Some(new_media) = self
                .media
                .iter_mut()
                .find(|media| media.role == old_media.role)
            else {
                continue;
            };
            if !media_role_is_video(&old_media.role) {
                continue;
            }
            let (Some(old_hash), Some(new_hash)) =
                (old_media.hash.as_deref(), new_media.hash.as_deref())
            else {
                continue;
            };
            if old_hash == new_hash {
                continue;
            }
            let choice =
                choose_video_conflict_media(store, attaches, context, old_media, new_media).await;
            match choice {
                VideoConflictChoice {
                    selected: VideoConflictSelection::Old,
                    ..
                } => *new_media = old_media.clone(),
                VideoConflictChoice {
                    selected: VideoConflictSelection::New | VideoConflictSelection::Fallback,
                    ..
                } => {}
            }
            if choice.reason != VideoConflictReason::Fallback
                || choice.conflicting_recency
                || choice.conflicting_mtime
            {
                resolved.insert(old_media.role.clone(), ResolvedVideoConflict(choice));
            }
        }
        resolved
    }

    pub fn with_hash(mut self, name: String, hash: String) -> Self {
        if let Some(media) = self.media.iter_mut().find(|media| media.role == name) {
            media.hash = Some(hash);
        } else {
            self.media.push(MediaMetadata {
                role: name,
                hash: Some(hash),
                ..Default::default()
            });
        }
        self
    }

    pub fn with_media_modified_at(mut self, role: &str, modified_at: Option<u64>) -> Self {
        let Some(modified_at) = modified_at else {
            return self;
        };
        if let Some(media) = self.media.iter_mut().find(|media| media.role == role) {
            media.hints.insert(
                "modified_at".into(),
                MetadataValue::Float(modified_at as f64),
            );
        }
        self
    }

    pub fn with_float(mut self, name: String, tag: String) -> Self {
        self.fields.insert(
            name,
            tag.parse()
                .map(MetadataValue::Float)
                .unwrap_or(MetadataValue::Str(tag)),
        );
        self
    }

    pub fn with_tag(mut self, name: String, tag: String) -> Self {
        self.fields.insert(name, MetadataValue::Str(tag));
        self
    }

    pub fn with_type(mut self, msg_type: MsgType) -> Self {
        self.msg_type = msg_type;
        self
    }

    pub fn with_parse_error(mut self, error: impl Into<String>) -> Self {
        self.raw.parse_error = Some(error.into());
        self
    }

    pub fn without_parse_error(mut self) -> Self {
        self.raw.parse_error = None;
        self
    }

    pub fn without_parse_error_when(self, condition: bool) -> Self {
        if condition {
            self.without_parse_error()
        } else {
            self
        }
    }

    pub fn without_fields(mut self, fields: &[&str]) -> Self {
        for field in fields {
            self.fields.remove(*field);
        }
        self
    }

    pub fn with_raw_hash(mut self, raw: &str) -> Self {
        self.raw.raw_hash = Some(Hash32::sha3_256(raw.as_bytes()).to_hex());
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.raw.summary = Some(summary.into());
        self
    }

    pub fn merge_fields_from(mut self, other: Self) -> Self {
        self.fields = other.fields.into_iter().chain(self.fields).collect();
        self.raw = if self.raw.is_empty() {
            other.raw
        } else {
            self.raw
        };
        self
    }

    pub fn merge_media_from(mut self, other: Self) -> Self {
        for media in other.media {
            if let Some(existing) = self
                .media
                .iter_mut()
                .find(|existing| existing.role == media.role)
            {
                *existing = media;
            } else {
                self.media.push(media);
            }
        }
        self
    }
}

async fn media_override_is_perceptual_match(
    store: &ChatStore,
    attaches: &Attachments,
    old: &MetadataValue,
    new: &MetadataValue,
) -> bool {
    let (Some(old), Some(new)) = (old.get_hash(), new.get_hash()) else {
        return false;
    };
    if let Some(attachment) = attaches.get(new) {
        let bytes = attachment
            .analysis_bytes()
            .unwrap_or_else(|| attachment.bytes());
        match store.asset_content_matches_bytes(old, bytes).await {
            Ok(Some(matches)) => return matches,
            Ok(None) => {}
            Err(error) => {
                debug!("failed to compare media metadata override bytes: {error}");
            }
        }
    }
    match store.assets_content_match(old, new).await {
        Ok(Some(matches)) => matches,
        Ok(None) => false,
        Err(error) => {
            debug!("failed to compare media metadata override hashes: {error}");
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoConflictSelection {
    Old,
    New,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoConflictReason {
    ParseableSide,
    ConversationRecency,
    AttachmentMtime,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VideoConflictChoice {
    selected: VideoConflictSelection,
    reason: VideoConflictReason,
    conflicting_recency: bool,
    conflicting_mtime: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedVideoConflict(VideoConflictChoice);

fn log_resolved_video_conflict(
    role: &str,
    old: &MetadataValue,
    new: &MetadataValue,
    conflict: ResolvedVideoConflict,
) {
    let choice = conflict.0;
    if choice.conflicting_recency || choice.conflicting_mtime {
        warn!(
            r#"metadata override "{}" resolved by {:?} with conflicting hints ({:?} -> {:?}): "{:?}" -> "{:?}""#,
            role, choice.reason, choice.conflicting_recency, choice.conflicting_mtime, old, new
        );
        return;
    }
    match choice.reason {
        VideoConflictReason::ParseableSide
        | VideoConflictReason::ConversationRecency
        | VideoConflictReason::AttachmentMtime => {
            debug!(
                r#"metadata override "{}" resolved by {:?}: "{:?}" -> "{:?}""#,
                role, choice.reason, old, new
            );
        }
        VideoConflictReason::Fallback => {
            warn!(
                r#"metadata override "{}" unresolved video conflict: "{:?}" -> "{:?}""#,
                role, old, new
            );
        }
    }
}

async fn choose_video_conflict_media(
    store: &ChatStore,
    attaches: &Attachments,
    context: &MetadataMergeContext,
    old_media: &MediaMetadata,
    new_media: &MediaMetadata,
) -> VideoConflictChoice {
    let (Some(old_hash), Some(new_hash)) = (old_media.hash.as_deref(), new_media.hash.as_deref())
    else {
        return VideoConflictChoice {
            selected: VideoConflictSelection::Fallback,
            reason: VideoConflictReason::Fallback,
            conflicting_recency: false,
            conflicting_mtime: false,
        };
    };
    let old_status = match store.asset_video_parse_status(old_hash).await {
        Ok(status) => status,
        Err(error) => {
            debug!("failed to inspect old video metadata override asset: {error}");
            None
        }
    };
    let new_status = match store.asset_video_parse_status(new_hash).await {
        Ok(status) => status,
        Err(error) => {
            debug!("failed to inspect new video metadata override asset: {error}");
            None
        }
    };
    let recency = conversation_recency_choice(context);
    let mtime = attachment_mtime_choice(attaches, old_media, new_media);
    if let Some(selected) = parse_status_choice(old_status, new_status) {
        return VideoConflictChoice {
            selected,
            reason: VideoConflictReason::ParseableSide,
            conflicting_recency: recency.is_some_and(|choice| choice != selected),
            conflicting_mtime: mtime.is_some_and(|choice| choice != selected),
        };
    }
    if let Some(selected) = recency {
        return VideoConflictChoice {
            selected,
            reason: VideoConflictReason::ConversationRecency,
            conflicting_recency: false,
            conflicting_mtime: mtime.is_some_and(|choice| choice != selected),
        };
    }
    if let Some(selected) = mtime {
        return VideoConflictChoice {
            selected,
            reason: VideoConflictReason::AttachmentMtime,
            conflicting_recency: false,
            conflicting_mtime: false,
        };
    }
    VideoConflictChoice {
        selected: VideoConflictSelection::Fallback,
        reason: VideoConflictReason::Fallback,
        conflicting_recency: false,
        conflicting_mtime: false,
    }
}

fn parse_status_choice(
    old_status: Option<bool>,
    new_status: Option<bool>,
) -> Option<VideoConflictSelection> {
    match (old_status, new_status) {
        (Some(true), Some(false)) => Some(VideoConflictSelection::Old),
        (Some(false), Some(true)) => Some(VideoConflictSelection::New),
        _ => None,
    }
}

fn conversation_recency_choice(context: &MetadataMergeContext) -> Option<VideoConflictSelection> {
    match (
        context.old_conversation_latest_timestamp,
        context.new_conversation_latest_timestamp,
    ) {
        (Some(old), Some(new)) if old > new => Some(VideoConflictSelection::Old),
        (Some(old), Some(new)) if new > old => Some(VideoConflictSelection::New),
        _ => None,
    }
}

fn attachment_mtime_choice(
    attaches: &Attachments,
    old_media: &MediaMetadata,
    new_media: &MediaMetadata,
) -> Option<VideoConflictSelection> {
    let old = media_modified_at(old_media);
    let new = new_media
        .hash
        .as_deref()
        .and_then(|hash| attaches.get(hash))
        .and_then(Attachment::modified_at)
        .or_else(|| media_modified_at(new_media));
    match (old, new) {
        (Some(old), Some(new)) if old > new => Some(VideoConflictSelection::Old),
        (Some(old), Some(new)) if new > old => Some(VideoConflictSelection::New),
        _ => None,
    }
}

fn media_modified_at(media: &MediaMetadata) -> Option<u64> {
    media
        .hints
        .get("modified_at")
        .and_then(MetadataValue::as_u64)
}

fn media_role_is_video(role: &str) -> bool {
    role == "video" || role == "video_raw"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_metadata_serializes_media_roles() {
        let metadata = IosWcMetadata::new()
            .with_type(MsgType::Image)
            .with_hash("mid".into(), "hash-img".into())
            .with_hash("thumb".into(), "hash-thum".into())
            .with_tag("cdn".into(), "https://cdn.test/image".into());

        let encoded = serde_json::to_vec(&metadata).unwrap();
        let decoded: IosWcMetadata = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MsgType::Image);
        assert_eq!(decoded.media_hash("mid"), Some("hash-img"));
        assert_eq!(decoded.media_hash("thumb"), Some("hash-thum"));
        assert_eq!(
            decoded.field("cdn"),
            Some(&MetadataValue::Str("https://cdn.test/image".into()))
        );
    }

    #[test]
    fn video_conflict_prefers_parseable_side() {
        assert_eq!(
            parse_status_choice(Some(true), Some(false)),
            Some(VideoConflictSelection::Old)
        );
        assert_eq!(
            parse_status_choice(Some(false), Some(true)),
            Some(VideoConflictSelection::New)
        );
        assert_eq!(parse_status_choice(Some(false), Some(false)), None);
    }

    #[test]
    fn video_conflict_uses_visible_conversation_recency() {
        let context = MetadataMergeContext {
            old_conversation_latest_timestamp: Some(10),
            new_conversation_latest_timestamp: Some(20),
        };
        assert_eq!(
            conversation_recency_choice(&context),
            Some(VideoConflictSelection::New)
        );

        let context = MetadataMergeContext {
            old_conversation_latest_timestamp: Some(30),
            new_conversation_latest_timestamp: Some(20),
        };
        assert_eq!(
            conversation_recency_choice(&context),
            Some(VideoConflictSelection::Old)
        );
    }

    #[test]
    fn video_conflict_uses_attachment_mtime() {
        let old = IosWcMetadata::new()
            .with_hash("video".into(), "old".into())
            .with_media_modified_at("video", Some(10));
        let new = IosWcMetadata::new()
            .with_hash("video".into(), "new".into())
            .with_media_modified_at("video", Some(20));
        let mut attaches = Attachments::new();
        attaches.insert(
            "new".into(),
            Attachment::from_bytes(b"new".to_vec()).with_modified_at(Some(30)),
        );

        assert_eq!(
            attachment_mtime_choice(&attaches, &old.media[0], &new.media[0]),
            Some(VideoConflictSelection::New)
        );
    }
}
