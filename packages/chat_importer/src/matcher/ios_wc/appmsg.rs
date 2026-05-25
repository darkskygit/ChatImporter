use super::xml::{xml_raw_text, xml_text};
use super::*;
use serde_json::json;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum AppMsgKind {
    Text,
    Image,
    Audio,
    Video,
    Link,
    File,
    RealtimeLocation,
    ForwardedRecords,
    Note,
    MiniProgram,
    Channels,
    Refer,
    Pat,
    Transfer,
    RedPacket,
    Reader,
    Unknown(i32),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct ForwardedRecord {
    pub(super) title: Option<String>,
    pub(super) sender: Option<String>,
    pub(super) content: Option<String>,
}

pub(super) fn appmsg_kind(type_value: Option<&str>) -> AppMsgKind {
    match type_value.and_then(|value| value.parse::<i32>().ok()) {
        Some(1 | 7) => AppMsgKind::Text,
        Some(2) => AppMsgKind::Image,
        Some(3) => AppMsgKind::Audio,
        Some(4 | 8) => AppMsgKind::Video,
        Some(5) => AppMsgKind::Link,
        Some(6) => AppMsgKind::File,
        Some(17) => AppMsgKind::RealtimeLocation,
        Some(19) => AppMsgKind::ForwardedRecords,
        Some(24) => AppMsgKind::Note,
        Some(33 | 36 | 44) => AppMsgKind::MiniProgram,
        Some(50 | 51) => AppMsgKind::Channels,
        Some(57) => AppMsgKind::Refer,
        Some(62) => AppMsgKind::Pat,
        Some(2000) => AppMsgKind::Transfer,
        Some(2001) => AppMsgKind::RedPacket,
        Some(100001) => AppMsgKind::Reader,
        Some(value) => AppMsgKind::Unknown(value),
        None => AppMsgKind::Unknown(0),
    }
}

impl AppMsgKind {
    fn as_str(&self) -> String {
        match self {
            Self::Text => "text".into(),
            Self::Image => "image".into(),
            Self::Audio => "audio".into(),
            Self::Video => "video".into(),
            Self::Link => "link".into(),
            Self::File => "file".into(),
            Self::RealtimeLocation => "realtime_location".into(),
            Self::ForwardedRecords => "forwarded_records".into(),
            Self::Note => "note".into(),
            Self::MiniProgram => "mini_program".into(),
            Self::Channels => "channels".into(),
            Self::Refer => "refer".into(),
            Self::Pat => "pat".into(),
            Self::Transfer => "transfer".into(),
            Self::RedPacket => "red_packet".into(),
            Self::Reader => "reader".into(),
            Self::Unknown(value) => format!("unknown:{}", value),
        }
    }
}

pub(super) fn parse_appmsg_metadata(message: &str) -> IosWcMetadata {
    let base_metadata = if super::xml::SafeXml::parse(message).is_err() {
        IosWcMetadata::new().with_parse_error("invalid appmsg xml")
    } else {
        IosWcMetadata::new()
    };
    let fields = [
        xml_text(message, &["msg", "appmsg", "title"]).map(|v| ("title", v)),
        xml_text(message, &["msg", "appmsg", "des"]).map(|v| ("description", v)),
        xml_text(message, &["msg", "appmsg", "thumburl"]).map(|v| ("thumb", v)),
        xml_raw_text(message, &["msg", "appmsg", "type"]).map(|v| ("appmsg_type", v)),
        xml_text(message, &["msg", "appmsg", "fileext"]).map(|v| ("fileext", v)),
        xml_text(message, &["msg", "appmsg", "totallen"]).map(|v| ("totallen", v)),
        xml_text(message, &["msg", "appmsg", "appname"]).map(|v| ("app", v)),
        xml_text(message, &["msg", "appmsg", "url"]).map(|v| ("url", v)),
        xml_text(message, &["msg", "appmsg", "refermsg", "displayname"])
            .map(|v| ("refer_display_name", v)),
        xml_text(message, &["msg", "appmsg", "refermsg", "content"]).map(|v| ("refer_content", v)),
        xml_text(message, &["msg", "appmsg", "wcpayinfo", "paysubtype"]).map(|v| ("paysubtype", v)),
        xml_text(message, &["msg", "appmsg", "wcpayinfo", "feedesc"]).map(|v| ("feedesc", v)),
        xml_text(message, &["msg", "appmsg", "wcpayinfo", "pay_memo"]).map(|v| ("pay_memo", v)),
        xml_text(message, &["msg", "appmsg", "wcpayinfo", "payer_username"]).map(|v| ("payer", v)),
        xml_text(
            message,
            &["msg", "appmsg", "wcpayinfo", "receiver_username"],
        )
        .map(|v| ("receiver", v)),
        xml_text(message, &["msg", "appmsg", "finderFeed", "nickname"])
            .map(|v| ("channels_nickname", v)),
        xml_text(message, &["msg", "appmsg", "weappinfo", "username"])
            .map(|v| ("mini_program_username", v)),
        xml_text(message, &["msg", "appmsg", "weappinfo", "pagepath"])
            .map(|v| ("mini_program_pagepath", v)),
    ];
    let mut metadata = fields
        .iter()
        .filter_map(|entry| entry.as_ref())
        .fold(base_metadata, |metadata, (key, value)| {
            metadata.with_tag((*key).into(), value.into())
        });
    let kind = appmsg_kind(metadata.field_str("appmsg_type"));
    let forwarded = parse_forwarded_records(message);
    let title = metadata.field_str("title").unwrap_or_default().to_string();
    metadata.app = Some(json!({
        "kind": kind.as_str(),
        "title": title,
        "description": metadata.field_str("description"),
        "url": metadata.field_str("url"),
        "forwarded": forwarded,
    }));
    if matches!(kind, AppMsgKind::Unknown(_)) {
        metadata = metadata.with_raw_hash(message).with_summary(title);
    }
    metadata
}

fn parse_forwarded_records(message: &str) -> Vec<ForwardedRecord> {
    xml_text(message, &["msg", "appmsg", "recorditem"])
        .and_then(|record| htmlescape::decode_html(&record).ok())
        .map(|record| {
            super::xml::SafeXml::parse(&record)
                .ok()
                .map(|xml| {
                    xml.children_attrs_and_text(
                        &["record"],
                        "dataitem",
                        &["datatype"],
                        &["datatitle", "sourcename", "sourceusername", "datadesc"],
                    )
                    .into_iter()
                    .map(|(_, text)| ForwardedRecord {
                        title: text.get("datatitle").cloned(),
                        sender: text
                            .get("sourcename")
                            .or_else(|| text.get("sourceusername"))
                            .cloned(),
                        content: text.get("datadesc").cloned(),
                    })
                    .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub(super) fn appmsg_label(metadata: &IosWcMetadata) -> String {
    let title = metadata
        .field("title")
        .and_then(|value| match value {
            MetadataValue::Str(value) if !value.is_empty() => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or("appmsg");
    let kind = appmsg_kind(metadata.field_str("appmsg_type"));
    match kind {
        AppMsgKind::Link => format!("[link] {}", title),
        AppMsgKind::File => format!("[file] {}", title),
        AppMsgKind::ForwardedRecords => format!("[forwarded] {}", title),
        AppMsgKind::Note => format!("[note] {}", title),
        AppMsgKind::Transfer => format!("[transfer] {}", title),
        AppMsgKind::RedPacket => format!("[red packet] {}", title),
        AppMsgKind::MiniProgram => format!("[mini program] {}", title),
        AppMsgKind::RealtimeLocation => format!("[realtime location] {}", title),
        AppMsgKind::Reader => format!("[reader] {}", title),
        AppMsgKind::Channels => format!("[channels] {}", title),
        AppMsgKind::Refer => format!("[refer] {}", title),
        AppMsgKind::Pat => format!("[pat] {}", title),
        _ => format!("[appmsg] {}", title),
    }
}
