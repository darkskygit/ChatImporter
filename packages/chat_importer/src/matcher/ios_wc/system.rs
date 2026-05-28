use super::message::MsgType;
use super::xml::{xml_attr, xml_text, SafeXml};
use super::*;
use serde_json::json;

pub(super) fn system_label(metadata: &IosWcMetadata) -> String {
    metadata
        .system
        .as_ref()
        .and_then(|system| system["system_type"].as_str())
        .or_else(|| metadata.field_str("system_type"))
        .map(|kind| format!("[system:{kind}]"))
        .unwrap_or_else(|| "[system]".into())
}

pub(super) fn parse_system_message(message: &str, msg_type: MsgType) -> (String, IosWcMetadata) {
    let trimmed = message.trim_start();
    let expects_xml_document = trimmed.starts_with("<sysmsg")
        || trimmed.starts_with("<msg")
        || trimmed.starts_with("<revokecontent")
        || trimmed.starts_with("<?xml")
        || trimmed.starts_with("<!DOCTYPE");
    let mut metadata = if expects_xml_document && SafeXml::parse(message).is_err() {
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
    metadata = metadata.without_fields(&["system_type", "content"]);

    let label = if msg_type == MsgType::Revoke {
        "[revoke]".into()
    } else {
        system_label(&metadata)
    };
    (label, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_success_classifications_have_metadata() {
        let cases = [
            (
                "<sysmsg><sysmsgtemplate><content_template><template>invited</template></content_template></sysmsgtemplate></sysmsg>",
                "sysmsgtemplate",
                "invited",
            ),
            (
                "<sysmsg><editrevokecontent>edited</editrevokecontent></sysmsg>",
                "editrevokecontent",
                "edited",
            ),
            (
                "<sysmsg type=\"paymsg\"><paymsg><template>paid</template></paymsg></sysmsg>",
                "paymsg",
                "paid",
            ),
            ("Alice 邀请 Bob 加入了群聊", "room_join", "Alice 邀请 Bob 加入了群聊"),
            ("Alice 退出了群聊", "room_leave", "Alice 退出了群聊"),
            ("Alice 修改群名为 Project", "room_rename", "Alice 修改群名为 Project"),
            ("群公告 updated", "room_announcement", "群公告 updated"),
            ("Alice 拍了拍 Bob", "pat", "Alice 拍了拍 Bob"),
            ("Alice 领取了红包", "red_packet", "Alice 领取了红包"),
        ];

        for (message, system_type, content) in cases {
            let (label, metadata) = parse_system_message(message, MsgType::System);
            assert_eq!(label, format!("[system:{}]", system_type));
            assert_eq!(metadata.field("system_type"), None);
            assert_eq!(metadata.field("content"), None);
            assert_eq!(
                metadata.system.as_ref().unwrap()["system_type"],
                system_type
            );
            assert_eq!(
                metadata.system.as_ref().unwrap()["content"],
                serde_json::json!(content)
            );
        }
    }

    #[test]
    fn system_parser_fallback_preserves_text() {
        let (label, metadata) =
            parse_system_message("<sysmsg><paymsg><template>paid</template>", MsgType::System);

        assert_eq!(label, "[system:plain]");
        assert_eq!(
            metadata.raw.parse_error.as_deref(),
            Some("invalid system xml")
        );
        assert_eq!(metadata.field("content"), None);
        assert_eq!(
            metadata.system.as_ref().unwrap()["content"],
            serde_json::json!("<sysmsg><paymsg><template>paid</template>")
        );
    }

    #[test]
    fn system_rich_text_fragment_is_not_invalid_xml() {
        let (_, metadata) = parse_system_message(
            r#"<img src="SystemMessages_HongbaoIcon.png"/> Alice领取了<_wc_custom_link_ href="weixin://weixinhongbao/opendetail">红包</_wc_custom_link_>"#,
            MsgType::System,
        );

        assert_eq!(metadata.raw.parse_error.as_deref(), None);
        assert_eq!(
            metadata.system.as_ref().unwrap()["system_type"],
            serde_json::json!("red_packet")
        );
    }

    #[test]
    fn invalid_revoke_xml_records_parse_error() {
        let (_, metadata) = parse_system_message("<!DOCTYPE msg><msg />", MsgType::Revoke);

        assert_eq!(
            metadata.raw.parse_error.as_deref(),
            Some("invalid revoke xml")
        );
    }
}
