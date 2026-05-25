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

    pub fn media_hash(&self, role: &str) -> Option<&str> {
        self.media
            .iter()
            .find(|media| media.role == role)
            .and_then(|media| media.hash.as_deref())
    }

    async fn log_media_overrides(
        _store: &ChatStore,
        _attaches: &Attachments,
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
                warn!(r#"metadata override "{}": "{:?}" -> "{:?}""#, key, old, new);
            }
        }
    }

    pub async fn merge(mut self, store: &ChatStore, attaches: &Attachments, old: Self) -> Self {
        let old_hash = old.media_hashes();
        let new_hash = self.media_hashes();
        let merged_hash = old_hash.clone().into_iter().chain(new_hash).collect();
        Self::log_media_overrides(store, attaches, &old_hash, &merged_hash).await;
        for old_media in old.media {
            if !self.media.iter().any(|media| media.role == old_media.role) {
                self.media.push(old_media);
            }
        }
        self
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
}
