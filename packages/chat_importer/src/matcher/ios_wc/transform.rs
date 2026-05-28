use super::account::UserDB;
use super::appmsg::appmsg_label;
use super::basic::{
    parse_contact_share, parse_emoji, parse_location, parse_msg_source, parse_voip_status,
};
use super::contact::*;
use super::media::MediaResolver;
use super::message::{MsgType, RecordLine};
use super::system::parse_system_message;
use super::*;

impl UserDB {
    fn get_microsecond(server_id: i64) -> i64 {
        use mur3::Hasher128;
        use std::hash::Hasher;
        let mut hasher = Hasher128::with_seed(42);
        hasher.write(&server_id.to_be_bytes());
        (((hasher.finish() as u128) * 1000) / u64::MAX as u128) as i64
    }

    pub(super) fn create_time_timestamp_millis(created_time: i64, server_id: i64) -> i64 {
        if created_time.abs() >= 100_000_000_000 {
            created_time
        } else {
            created_time * 1000 + Self::get_microsecond(server_id)
        }
    }

    pub(super) fn transform_record_line(
        &self,
        backup: &Backup,
        line: &RecordLine,
        contact: &Contact,
    ) -> Result<RecordType, String> {
        let is_group = contact.name.ends_with("@chatroom");
        let contact_book = ContactBook {
            contacts: self.contacts.clone(),
        };
        let (sender_id, sender_name, content) = {
            if line.is_dest {
                if is_group {
                    if let Some((id, raw_content)) = contact_book.prefixed_user_id(&line.message) {
                        let (sender_id, sender_name) =
                            contact_book.resolve_sender(Some(contact), &id).into_parts();
                        (
                            sender_id,
                            sender_name,
                            ContactBook::strip_prefixed_content(raw_content),
                        )
                    } else if [
                        MsgType::BigEmoji,
                        MsgType::CustomApp,
                        MsgType::Video,
                        MsgType::VoipStatus,
                        MsgType::System,
                        MsgType::Revoke,
                    ]
                    .contains(&line.msg_type)
                    {
                        if let Some((id, remark)) = contact_book.sender_from_xml(&line.message) {
                            (id, remark, line.message.clone())
                        } else {
                            ("unknown".into(), String::new(), line.message.clone())
                        }
                    } else {
                        return Err(format!(
                            "new line not exists in a group line: {}, {}, {}, {:?}",
                            gen_md5(&contact.name),
                            line.local_id,
                            line.created_time,
                            line.msg_type
                        ));
                    }
                } else {
                    (
                        contact.name.clone(),
                        contact.get_remark().unwrap_or_default(),
                        line.message.clone(),
                    )
                }
            } else {
                contact_book
                    .resolve_message_sender(None, &line.message)
                    .map(|(_, _, content)| (self.wxid.clone(), self.name.clone(), content))
                    .unwrap_or_else(|| (self.wxid.clone(), self.name.clone(), line.message.clone()))
            }
        };

        let content_line = RecordLine {
            message: content.clone(),
            ..line.clone()
        };

        let (content, metadata, attach) = match line.msg_type {
            MsgType::Normal => Some((
                content.replace("\u{2028}", " ").replace("\u{2029}", " "),
                None,
                HashMap::new(),
            )),
            MsgType::Image => MediaResolver::image(
                &content_line,
                backup,
                &self.account_files,
                &self.account,
                &gen_md5(&contact.name),
            )
            .map(|(metadata, map)| {
                (
                    "[img]".into(),
                    Some(metadata.with_type(line.msg_type.clone())),
                    map,
                )
            })
            .or_else(|| {
                Some((
                    "[img]".into(),
                    Some(
                        MediaResolver::image_metadata(&content_line)
                            .with_type(line.msg_type.clone()),
                    ),
                    HashMap::new(),
                ))
            }),
            MsgType::Video | MsgType::ShortVideo => MediaResolver::video(
                &content_line,
                backup,
                &self.account_files,
                &self.account,
                &gen_md5(&contact.name),
            )
            .map(|(metadata, map)| {
                (
                    "[video]".into(),
                    Some(metadata.with_type(line.msg_type.clone())),
                    map,
                )
            })
            .or_else(|| {
                Some((
                    "[video]".into(),
                    Some(
                        MediaResolver::video_metadata(&content_line)
                            .with_type(line.msg_type.clone()),
                    ),
                    HashMap::new(),
                ))
            }),
            MsgType::Voice => MediaResolver::audio(
                &content_line,
                backup,
                &self.account_files,
                &self.account,
                &gen_md5(&contact.name),
            )
            .map(|(metadata, map)| {
                (
                    "[voice]".into(),
                    Some(metadata.with_type(line.msg_type.clone())),
                    map,
                )
            })
            .or_else(|| {
                Some((
                    "[voice]".into(),
                    Some(
                        MediaResolver::audio_metadata(&content_line)
                            .with_type(line.msg_type.clone()),
                    ),
                    HashMap::new(),
                ))
            }),
            MsgType::BigEmoji => Some((
                "[emoji]".into(),
                Some(parse_emoji(&content_line).with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::ContactShare | MsgType::WeWorkContactShare | MsgType::OpenIMContactShare => {
                Some((
                    "[contact]".into(),
                    Some(parse_contact_share(&content_line).with_type(line.msg_type.clone())),
                    HashMap::new(),
                ))
            }
            MsgType::Location => Some((
                "[location]".into(),
                Some(parse_location(&content_line).with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::CustomApp => MediaResolver::custom_app(
                &content_line,
                backup,
                &self.account,
                &gen_md5(&contact.name),
            )
            .map(|(metadata, map)| {
                let label = appmsg_label(&metadata);
                (label, Some(metadata.with_type(line.msg_type.clone())), map)
            }),
            MsgType::VoipContent => Some((
                "[voip]".into(),
                Some(
                    IosWcMetadata::new()
                        .with_tag("type".into(), line.message.clone())
                        .with_type(line.msg_type.clone()),
                ),
                HashMap::new(),
            )),
            MsgType::VoipStatus => Some((
                "[voip]".into(),
                Some(parse_voip_status(&content_line).with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::System | MsgType::Revoke => {
                let (label, metadata) =
                    parse_system_message(&content_line.message, line.msg_type.clone());
                Some((label, Some(metadata), HashMap::new()))
            }
            _ => None,
        }
        .unwrap_or_else(|| (content, None, HashMap::new()));
        let metadata = if let Some(msg_source) = line.msg_source.as_deref() {
            let source_metadata = parse_msg_source(msg_source).with_type(line.msg_type.clone());
            Some(match metadata {
                Some(metadata) => metadata.merge_fields_from(source_metadata),
                None => source_metadata,
            })
        } else {
            metadata
        };

        let source_message_id = Self::source_message_id(contact, line, &content, &attach);
        let record = Record {
            chat_type: "WeChat".into(),
            owner_id: self.wxid.clone(),
            group_id: contact.name.clone(),
            sender_id,
            sender_name,
            content,
            timestamp: Self::create_time_timestamp_millis(line.created_time, line.server_id),
            metadata: metadata.as_ref().and_then(|m| {
                to_vec(m)
                    .map_err(|e| warn!("failed to serialization metadata: {}", e))
                    .ok()
            }),
            source_kind: Some("ios-wechat".into()),
            source_group_id: Some(contact.name.clone()),
            source_message_id: Some(source_message_id),
            ..Default::default()
        };

        Ok(if metadata.is_some() {
            RecordType::from((record, attach))
        } else {
            RecordType::from(record)
        })
    }

    pub(super) fn source_message_id(
        contact: &Contact,
        line: &RecordLine,
        content: &str,
        attach: &Attachments,
    ) -> String {
        if line.server_id > 0 {
            format!("svr:{}:{}", contact.name, line.server_id)
        } else {
            let content_hash = Hash32::sha3_256(content.as_bytes()).to_hex();
            let attachment_hash = attachment_fingerprint(attach);
            format!(
                "fallback:{}:{}:{}:{}:{}",
                contact.name, line.created_time, line.local_id, content_hash, attachment_hash
            )
        }
    }

    pub(super) fn transform_record_lines(
        &self,
        backup: &Backup,
        contact: &Contact,
        lines: Vec<RecordLine>,
    ) -> Vec<RecordType> {
        lines
            .iter()
            .fold(Vec::<RecordType>::new(), |mut ret, curr| {
                match self.transform_record_line(backup, curr, contact) {
                    Ok(record_type) => ret.push(record_type),
                    Err(e) => error!("failed to transform record line: {}", e),
                };
                ret
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::contact::Contact;
    use super::super::message::{MsgType, RecordLine};
    use super::*;

    #[test]
    fn second_create_time_is_converted_to_millis() {
        let timestamp = UserDB::create_time_timestamp_millis(1_598_219_157, 42);

        assert!((1_598_219_157_000..1_598_219_158_000).contains(&timestamp));
    }

    #[test]
    fn millisecond_create_time_is_not_multiplied_again() {
        assert_eq!(
            UserDB::create_time_timestamp_millis(1_598_219_157_438, 42),
            1_598_219_157_438
        );
    }

    #[test]
    fn source_message_id_fallback_includes_local_facts() {
        let contact = Contact {
            name: "chat-a".into(),
            ..Default::default()
        };
        let line = RecordLine {
            local_id: 7,
            server_id: 0,
            created_time: 1_598_219_157,
            message: "fallback".into(),
            status: 0,
            image_status: 0,
            msg_type: MsgType::Normal,
            is_dest: false,
            msg_source: None,
        };
        let first = UserDB::source_message_id(&contact, &line, "fallback", &HashMap::new());
        let second = UserDB::source_message_id(&contact, &line, "changed", &HashMap::new());
        let with_attachment = UserDB::source_message_id(
            &contact,
            &line,
            "fallback",
            &vec![("asset".into(), Attachment::from_bytes(b"bytes".to_vec()))]
                .into_iter()
                .collect(),
        );

        assert!(first.starts_with("fallback:chat-a:1598219157:7:"));
        assert_ne!(first, second);
        assert_ne!(first, with_attachment);
    }
}
