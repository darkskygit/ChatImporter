use super::message::MsgType;
use super::xml::{xml_attr, xml_text, SafeXml};
use super::*;
use serde_json::json;

pub(super) fn system_label(metadata: &IosWcMetadata) -> String {
    metadata
        .field_str("system_type")
        .map(|kind| format!("[system:{}]", kind))
        .unwrap_or_else(|| "[system]".into())
}

pub(super) fn parse_system_message(message: &str, msg_type: MsgType) -> (String, IosWcMetadata) {
    let mut metadata = if message.trim_start().starts_with('<') && SafeXml::parse(message).is_err()
    {
        let error = if msg_type == MsgType::Revoke {
            "invalid revoke xml"
        } else {
            "invalid system xml"
        };
        IosWcMetadata::new().with_parse_error(error)
    } else {
        IosWcMetadata::new()
    }
    .with_type(msg_type.clone());

    let system_type = if msg_type == MsgType::Revoke {
        "revoke"
    } else if xml_text(
        message,
        &["sysmsg", "sysmsgtemplate", "content_template", "template"],
    )
    .is_some()
    {
        "sysmsgtemplate"
    } else if xml_text(message, &["sysmsg", "editrevokecontent"]).is_some() {
        "editrevokecontent"
    } else if xml_text(message, &["sysmsg", "paymsg", "template"]).is_some()
        || xml_attr(message, &["sysmsg"], "type").as_deref() == Some("paymsg")
    {
        "paymsg"
    } else if message.contains("拍了拍") {
        "pat"
    } else if message.contains("邀请") || message.contains("加入") {
        "room_join"
    } else if message.contains("退出") || message.contains("移出") {
        "room_leave"
    } else if message.contains("修改群名") || message.contains("群名") {
        "room_rename"
    } else if message.contains("群公告") {
        "room_announcement"
    } else if message.contains("红包") {
        "red_packet"
    } else {
        "plain"
    };

    metadata = metadata.with_tag("system_type".into(), system_type.into());
    let parsed_text = xml_text(message, &["sysmsg", "revokemsg", "revokecontent"])
        .or_else(|| xml_text(message, &["revokecontent"]))
        .or_else(|| {
            xml_text(
                message,
                &["sysmsg", "sysmsgtemplate", "content_template", "template"],
            )
        })
        .or_else(|| xml_text(message, &["sysmsg", "editrevokecontent"]))
        .or_else(|| xml_text(message, &["sysmsg", "paymsg", "template"]))
        .unwrap_or_else(|| message.to_string());
    metadata = metadata.with_tag("content".into(), parsed_text.clone());
    if msg_type == MsgType::Revoke {
        metadata = metadata.with_tag("revoke".into(), parsed_text.clone());
    }
    metadata.system = Some(json!({
        "system_type": system_type,
        "content": parsed_text,
    }));

    let label = if msg_type == MsgType::Revoke {
        "[revoke]".into()
    } else {
        system_label(&metadata)
    };
    (label, metadata)
}
