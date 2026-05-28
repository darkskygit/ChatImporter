use super::appmsg::parse_appmsg_metadata;
use super::message::RecordLine;
use super::xml::{xml_attr, SafeXml};
use super::*;

pub(super) struct MediaResolver;

struct AttachmentFile<'a> {
    pub(super) account: &'a str,
    hashed_user: &'a str,
    file_type: &'a str,
    folder: &'a str,
    suffix: &'a str,
    normalize_image: bool,
}

fn clear_parse_error_when_media_found(metadata: IosWcMetadata, has_media: bool) -> IosWcMetadata {
    metadata.without_parse_error_when(has_media)
}

impl MediaResolver {
    pub fn audio_metadata(line: &RecordLine) -> IosWcMetadata {
        if SafeXml::parse(&line.message).is_err() {
            return IosWcMetadata::new().with_parse_error("invalid voice xml");
        }
        [
            xml_attr(&line.message, &["msg", "voicemsg"], "bufid").map(|v| ("bufid", v)),
            xml_attr(&line.message, &["msg", "voicemsg"], "clientmsgid").map(|v| ("clientid", v)),
            xml_attr(&line.message, &["msg", "voicemsg"], "length").map(|v| ("duration", v)),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(IosWcMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn image_metadata(line: &RecordLine) -> IosWcMetadata {
        if SafeXml::parse(&line.message).is_err() {
            return IosWcMetadata::new().with_parse_error("invalid image xml");
        }
        [
            xml_attr(&line.message, &["msg", "img"], "cdnthumburl").map(|v| ("thum_cdn", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnmidimgurl").map(|v| ("img_cdn", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnbigimgurl").map(|v| ("hd_cdn", v)),
            xml_attr(&line.message, &["msg", "img"], "imgname").map(|v| ("img_name", v)),
            xml_attr(&line.message, &["msg", "img"], "aeskey").map(|v| ("key", v)),
            xml_attr(&line.message, &["msg", "img"], "md5").map(|v| ("md5", v)),
            xml_attr(&line.message, &["msg", "img"], "length").map(|v| ("length", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnmidwidth").map(|v| ("mid_width", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnmidheight").map(|v| ("mid_height", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnthumbwidth").map(|v| ("thumb_width", v)),
            xml_attr(&line.message, &["msg", "img"], "cdnthumbheight").map(|v| ("thumb_height", v)),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(IosWcMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn video_metadata(line: &RecordLine) -> IosWcMetadata {
        if SafeXml::parse(&line.message).is_err() {
            return IosWcMetadata::new().with_parse_error("invalid video xml");
        }
        [
            xml_attr(&line.message, &["msg", "videomsg"], "cdnvideourl").map(|v| ("cdn", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "cdnrawvideourl").map(|v| ("raw_cdn", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "aeskey").map(|v| ("key", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "cdnrawvideoaeskey")
                .map(|v| ("raw_key", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "md5").map(|v| ("md5", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "newmd5").map(|v| ("new_md5", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "rawmd5").map(|v| ("raw_md5", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "length").map(|v| ("length", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "rawlength").map(|v| ("raw_length", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "playlength").map(|v| ("play_length", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "offset").map(|v| ("offset", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "rawoffset").map(|v| ("raw_offset", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "cdnthumbwidth")
                .map(|v| ("thumb_width", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "cdnthumbheight")
                .map(|v| ("thumb_height", v)),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(IosWcMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn audio(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(IosWcMetadata, Attachments)> {
        let (ftype, dir) = ("voice", "Audio");
        Self::collect_media_files(vec![Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                suffix: ".aud",
                normalize_image: false,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))])
        .map(|(metadata, attachments)| {
            let line_metadata = clear_parse_error_when_media_found(
                Self::audio_metadata(line),
                !attachments.is_empty(),
            );
            (metadata.merge_fields_from(line_metadata), attachments)
        })
    }

    pub fn custom_app(
        line: &RecordLine,
        backup: &Backup,
        account: &str,
        hashed_user: &str,
    ) -> Option<(IosWcMetadata, Attachments)> {
        let path = format!(
            "Documents/{}/{}/{}/{}",
            account, "OpenData", hashed_user, line.local_id
        );
        let files = backup
            .find_regex_paths(DOMAIN, &format!("{}[\\./]", path))
            .iter()
            .filter_map(|file| {
                use std::path::PathBuf;
                let path = PathBuf::from(&file.relative_filename);
                backup
                    .read_file(file)
                    .map(|data| (path.name_str().to_string(), data))
                    .map_err(|e| {
                        warn!(
                            "failed to read attach: {}, {}, {}, {}, {}",
                            account,
                            hashed_user,
                            line.local_id,
                            path.name_str(),
                            e
                        )
                    })
                    .ok()
            })
            .map(|(name, data)| (name, Attachment::from_bytes(data)))
            .collect::<HashMap<_, _>>();
        let metadata = parse_appmsg_metadata(&line.message);
        let has_files = !files.is_empty();
        Some((
            files
                .iter()
                .fold(metadata, |metadata, (name, data)| {
                    metadata.with_hash(
                        format!("open_data:{}", name),
                        Hash32::sha3_256(data.bytes()).to_hex(),
                    )
                })
                .without_parse_error_when(has_files),
            files,
        ))
    }

    pub fn image(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(IosWcMetadata, Attachments)> {
        Self::collect_media_files(vec![
            Self::image_thum(line, backup, backups, account, hashed_user),
            Self::image_small(line, backup, backups, account, hashed_user),
            Self::image_hd(line, backup, backups, account, hashed_user),
        ])
        .map(|(metadata, attachments)| {
            let line_metadata = clear_parse_error_when_media_found(
                Self::image_metadata(line),
                !attachments.is_empty(),
            );
            (metadata.merge_fields_from(line_metadata), attachments)
        })
    }

    pub fn video(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(IosWcMetadata, Attachments)> {
        Self::collect_media_files(vec![
            Self::video_regular(line, backup, backups, account, hashed_user),
            Self::media_file(
                line,
                backup,
                backups,
                AttachmentFile {
                    account,
                    hashed_user,
                    file_type: "video_raw",
                    folder: "Video",
                    suffix: "_raw.mp4",
                    normalize_image: false,
                },
            ),
            Self::media_file(
                line,
                backup,
                backups,
                AttachmentFile {
                    account,
                    hashed_user,
                    file_type: "video_thum",
                    folder: "Video",
                    suffix: ".video_thum",
                    normalize_image: true,
                },
            ),
        ])
        .map(|(metadata, attachments)| {
            let line_metadata = clear_parse_error_when_media_found(
                Self::video_metadata(line),
                !attachments.is_empty(),
            );
            (metadata.merge_fields_from(line_metadata), attachments)
        })
    }

    fn video_regular(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, IosWcMetadata, Attachment)> {
        let ftype = "video";
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: "Video",
                suffix: ".mp4",
                normalize_image: false,
            },
        )
        .or_else(|| {
            Self::get_file(
                line,
                backup,
                backups,
                AttachmentFile {
                    account,
                    hashed_user,
                    file_type: ftype,
                    folder: "Video",
                    suffix: "_temp.mp4",
                    normalize_image: false,
                },
            )
        })
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn media_file(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        file: AttachmentFile<'_>,
    ) -> Option<(String, IosWcMetadata, Attachment)> {
        let file_type = file.file_type.to_string();
        Self::get_file(line, backup, backups, file)
            .map(|(metadata, data)| (file_type, metadata, data))
    }

    fn image_small(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, IosWcMetadata, Attachment)> {
        let (ftype, dir, suffix) = ("mid", "Img", ".pic");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                suffix,
                normalize_image: true,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn image_hd(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, IosWcMetadata, Attachment)> {
        let (ftype, dir, suffix) = ("hd", "Img", ".pic_hd");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                suffix,
                normalize_image: true,
            },
        )
        .or_else(|| {
            Self::get_file(
                line,
                backup,
                backups,
                AttachmentFile {
                    account,
                    hashed_user,
                    file_type: ftype,
                    folder: "ImgV2",
                    suffix,
                    normalize_image: true,
                },
            )
        })
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn image_thum(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, IosWcMetadata, Attachment)> {
        let (ftype, dir, suffix) = ("thumb", "Img", ".pic_thum");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                suffix,
                normalize_image: true,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn collect_media_files<I>(iter: I) -> Option<(IosWcMetadata, Attachments)>
    where
        I: IntoIterator<Item = Option<(String, IosWcMetadata, Attachment)>>,
    {
        let (metadata, map) = iter.into_iter().filter_map(|i| i.clone()).try_fold(
            (IosWcMetadata::new(), HashMap::new()),
            |(metadata, mut map), (ftype, file_metadata, attachment)| {
                let hash = Hash32::sha3_256(attachment.bytes()).to_hex();
                map.insert(hash.clone(), attachment);
                Some((
                    metadata.merge_media_from(file_metadata.with_hash(ftype, hash.to_string())),
                    map,
                ))
            },
        )?;
        (!map.is_empty() && !metadata.media.is_empty()).then_some((metadata, map))
    }

    fn get_file(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        file: AttachmentFile<'_>,
    ) -> Option<(IosWcMetadata, Attachment)> {
        backups
            .get(&format!(
                "Documents/{}/{}/{}/{}{}",
                file.account, file.folder, file.hashed_user, line.local_id, file.suffix
            ))
            .or_else(|| {
                debug!(
                    "{} not found: {}, {}, {}",
                    file.file_type, file.account, file.hashed_user, line.local_id
                );
                None
            })
            .and_then(|backup_file| {
                backup
                    .read_file(backup_file)
                    .map(|data| (backup_file, data))
                    .map_err(|e| {
                        warn!(
                            "failed to read {}: {}, {}, {}, {}",
                            file.file_type, file.account, file.hashed_user, line.local_id, e
                        )
                    })
                    .ok()
            })
            .map(|(backup_file, data)| {
                let modified_at = backup_file
                    .fileinfo
                    .as_ref()
                    .and_then(|info| (info.last_modified > 0).then_some(info.last_modified));
                let attachment = if file.normalize_image {
                    wxgf::normalize_image_attachment(data)
                } else {
                    Attachment::from_bytes(data)
                }
                .with_modified_at(modified_at);
                (
                    IosWcMetadata::new()
                        .with_hash(
                            file.file_type.into(),
                            Hash32::sha3_256(attachment.bytes()).to_hex(),
                        )
                        .with_media_modified_at(file.file_type, modified_at),
                    attachment,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::message::MsgType;
    use super::*;

    fn line(message: &str, msg_type: MsgType) -> RecordLine {
        RecordLine {
            local_id: 1,
            server_id: 1,
            created_time: 1,
            message: message.into(),
            status: 0,
            image_status: 0,
            msg_type,
            is_dest: false,
            msg_source: None,
        }
    }

    #[test]
    fn invalid_media_xml_records_parse_error() {
        let image_metadata = MediaResolver::image_metadata(&line(
            "<!DOCTYPE msg><msg><img /></msg>",
            MsgType::Image,
        ))
        .with_type(MsgType::Image);
        let voice_metadata = MediaResolver::audio_metadata(&line(
            "<!DOCTYPE msg><msg><voicemsg /></msg>",
            MsgType::Voice,
        ))
        .with_type(MsgType::Voice);

        assert_eq!(image_metadata.msg_type, MsgType::Image);
        assert_eq!(
            image_metadata.raw.parse_error.as_deref(),
            Some("invalid image xml")
        );
        assert_eq!(voice_metadata.msg_type, MsgType::Voice);
        assert_eq!(
            voice_metadata.raw.parse_error.as_deref(),
            Some("invalid voice xml")
        );
    }
}
