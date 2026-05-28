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
        Some(50 | 51 | 63) => AppMsgKind::Channels,
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
            .or_else(|| xml_text(message, &["msg", "appmsg", "finderLive", "nickname"]))
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
    if base_parse_failed_but_recovered(&metadata, forwarded.is_empty()) {
        metadata = metadata.without_parse_error();
    }
    metadata.app = Some(json!({
        "kind": kind.as_str(),
        "title": title,
        "description": metadata.field_str("description"),
        "url": metadata.field_str("url"),
        "forwarded": forwarded,
    }));
    metadata = metadata.without_fields(&["title", "description", "url", "appmsg_type"]);
    if matches!(kind, AppMsgKind::Unknown(_)) {
        metadata = metadata.with_raw_hash(message).with_summary(title);
    }
    metadata
}

fn base_parse_failed_but_recovered(metadata: &IosWcMetadata, forwarded_empty: bool) -> bool {
    metadata.raw.parse_error.as_deref() == Some("invalid appmsg xml")
        && (!metadata.fields.is_empty() || !forwarded_empty)
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
        .app
        .as_ref()
        .and_then(|app| app["title"].as_str())
        .filter(|title| !title.is_empty())
        .or_else(|| {
            metadata.field("title").and_then(|value| match value {
                MetadataValue::Str(value) if !value.is_empty() => Some(value.as_str()),
                _ => None,
            })
        })
        .unwrap_or("appmsg");
    let kind = metadata
        .app
        .as_ref()
        .and_then(|app| app["kind"].as_str())
        .map(appmsg_kind_from_str)
        .unwrap_or_else(|| appmsg_kind(metadata.field_str("appmsg_type")));
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

fn appmsg_kind_from_str(kind: &str) -> AppMsgKind {
    match kind {
        "text" => AppMsgKind::Text,
        "image" => AppMsgKind::Image,
        "audio" => AppMsgKind::Audio,
        "video" => AppMsgKind::Video,
        "link" => AppMsgKind::Link,
        "file" => AppMsgKind::File,
        "realtime_location" => AppMsgKind::RealtimeLocation,
        "forwarded_records" => AppMsgKind::ForwardedRecords,
        "note" => AppMsgKind::Note,
        "mini_program" => AppMsgKind::MiniProgram,
        "channels" => AppMsgKind::Channels,
        "refer" => AppMsgKind::Refer,
        "pat" => AppMsgKind::Pat,
        "transfer" => AppMsgKind::Transfer,
        "red_packet" => AppMsgKind::RedPacket,
        "reader" => AppMsgKind::Reader,
        value if value.starts_with("unknown:") => value
            .trim_start_matches("unknown:")
            .parse()
            .map(AppMsgKind::Unknown)
            .unwrap_or(AppMsgKind::Unknown(0)),
        _ => AppMsgKind::Unknown(0),
    }
}

#[cfg(test)]
mod tests {
    use super::super::message::MsgType;
    use super::super::metadata::MetadataValue;
    use super::*;

    #[test]
    fn appmsg_subtypes_emit_structured_metadata() {
        let cases = [
            (
                "file",
                r#"<msg><appmsg><title>report.pdf</title><type>6</type><fileext>pdf</fileext><totallen>42</totallen></appmsg></msg>"#,
                "[file] report.pdf",
                "fileext",
                "pdf",
            ),
            (
                "refer",
                r#"<msg><appmsg><title>reply</title><type>57</type><refermsg><displayname>Alice</displayname><content>hello</content></refermsg></appmsg></msg>"#,
                "[refer] reply",
                "refer_display_name",
                "Alice",
            ),
            (
                "transfer",
                r#"<msg><appmsg><title>transfer</title><type>2000</type><wcpayinfo><feedesc>$1.00</feedesc><pay_memo>memo</pay_memo></wcpayinfo></appmsg></msg>"#,
                "[transfer] transfer",
                "feedesc",
                "$1.00",
            ),
            (
                "red_packet",
                r#"<msg><appmsg><title>packet</title><type>2001</type></appmsg></msg>"#,
                "[red packet] packet",
                "title",
                "packet",
            ),
            (
                "mini_program",
                r#"<msg><appmsg><title>mini</title><type>33</type><weappinfo><username>gh_x</username></weappinfo></appmsg></msg>"#,
                "[mini program] mini",
                "mini_program_username",
                "gh_x",
            ),
            (
                "channels",
                r#"<msg><appmsg><title>live</title><type>63</type><finderLive><nickname>host</nickname></finderLive></appmsg></msg>"#,
                "[channels] live",
                "channels_nickname",
                "host",
            ),
        ];

        for (kind, xml, label, field, value) in cases {
            let metadata = parse_appmsg_metadata(xml).with_type(MsgType::CustomApp);
            assert_eq!(appmsg_label(&metadata), label);
            assert_eq!(metadata.app.as_ref().unwrap()["kind"], kind);
            if field == "title" {
                assert_eq!(
                    metadata.app.as_ref().unwrap()["title"],
                    serde_json::json!(value)
                );
            } else {
                assert_eq!(
                    metadata.field(field),
                    Some(&MetadataValue::Str(value.into()))
                );
            }
        }
    }

    #[test]
    fn appmsg_declared_subtypes_are_not_unknown() {
        let cases = [
            (1, "text", "[appmsg] item"),
            (2, "image", "[appmsg] item"),
            (3, "audio", "[appmsg] item"),
            (4, "video", "[appmsg] item"),
            (7, "text", "[appmsg] item"),
            (8, "video", "[appmsg] item"),
            (17, "realtime_location", "[realtime location] item"),
            (24, "note", "[note] item"),
            (50, "channels", "[channels] item"),
            (51, "channels", "[channels] item"),
            (63, "channels", "[channels] item"),
            (62, "pat", "[pat] item"),
            (100001, "reader", "[reader] item"),
        ];

        for (appmsg_type, kind, label) in cases {
            let metadata = parse_appmsg_metadata(&format!(
                "<msg><appmsg><title>item</title><type>{}</type></appmsg></msg>",
                appmsg_type
            ));
            assert_eq!(metadata.app.as_ref().unwrap()["kind"], kind);
            assert_eq!(appmsg_label(&metadata), label);
            assert!(metadata.raw.raw_hash.is_none());
        }
    }

    #[test]
    fn forwarded_and_unknown_appmsg_metadata() {
        let forwarded_xml = r#"<record><dataitem><datatitle>First</datatitle><sourcename>Alice</sourcename><datadesc>Hello</datadesc></dataitem></record>"#;
        let metadata = parse_appmsg_metadata(&format!(
            "<msg><appmsg><title>history</title><type>19</type><recorditem>{}</recorditem></appmsg></msg>",
            htmlescape::encode_minimal(forwarded_xml)
        ));
        let forwarded = metadata.app.as_ref().unwrap()["forwarded"]
            .as_array()
            .unwrap();
        assert_eq!(appmsg_label(&metadata), "[forwarded] history");
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0]["title"], "First");

        let unknown = parse_appmsg_metadata(
            "<msg><appmsg><title>mystery</title><type>40404</type></appmsg></msg>",
        );
        assert_eq!(unknown.app.as_ref().unwrap()["kind"], "unknown:40404");
        assert!(unknown.raw.raw_hash.is_some());
        assert_eq!(unknown.raw.summary.as_deref(), Some("mystery"));
    }
}
