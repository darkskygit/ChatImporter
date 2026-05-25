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
    ext: &'a str,
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
            xml_attr(&line.message, &["msg", "img"], "aeskey").map(|v| ("key", v)),
            xml_attr(&line.message, &["msg", "img"], "md5").map(|v| ("md5", v)),
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
            xml_attr(&line.message, &["msg", "videomsg"], "aeskey").map(|v| ("key", v)),
            xml_attr(&line.message, &["msg", "videomsg"], "md5").map(|v| ("md5", v)),
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
        let (ftype, dir, ext) = ("voice", "Audio", "aud");
        Self::collect_media_files(vec![Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                ext,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))])
        .map(|(metadata, attachments)| {
            (
                metadata.merge_fields_from(Self::audio_metadata(line)),
                attachments,
            )
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
            .collect::<HashMap<_, _>>();
        let metadata = parse_appmsg_metadata(&line.message);
        Some((
            files.iter().fold(metadata, |metadata, (name, data)| {
                metadata.with_hash(
                    format!("open_data:{}", name),
                    Hash32::sha3_256(data).to_hex(),
                )
            }),
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
            (
                metadata.merge_fields_from(Self::image_metadata(line)),
                attachments,
            )
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
            Self::media_file(
                line,
                backup,
                backups,
                AttachmentFile {
                    account,
                    hashed_user,
                    file_type: "video",
                    folder: "Video",
                    ext: "mp4",
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
                    ext: "video_thum",
                },
            ),
        ])
        .map(|(metadata, attachments)| {
            (
                metadata.merge_fields_from(Self::video_metadata(line)),
                attachments,
            )
        })
    }

    fn media_file(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        file: AttachmentFile<'_>,
    ) -> Option<(String, IosWcMetadata, Vec<u8>)> {
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
    ) -> Option<(String, IosWcMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("mid", "Img", "pic");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                ext,
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
    ) -> Option<(String, IosWcMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("hd", "Img", "pic_hd");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                ext,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn image_thum(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, IosWcMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("thumb", "Img", "pic_thum");
        Self::get_file(
            line,
            backup,
            backups,
            AttachmentFile {
                account,
                hashed_user,
                file_type: ftype,
                folder: dir,
                ext,
            },
        )
        .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn collect_media_files<I>(iter: I) -> Option<(IosWcMetadata, Attachments)>
    where
        I: IntoIterator<Item = Option<(String, IosWcMetadata, Vec<u8>)>>,
    {
        let (metadata, map) = iter
            .into_iter()
            .filter_map(|i| i.clone())
            .filter_map(|(ftype, metadata, data)| {
                metadata
                    .media_hash(&ftype)
                    .map(|hash| (ftype, hash.to_string(), data))
            })
            .try_fold(
                (IosWcMetadata::new(), HashMap::new()),
                |(metadata, mut map), (ftype, hash, data)| {
                    map.insert(hash.to_string(), data.clone());
                    Some((metadata.with_hash(ftype, hash.to_string()), map))
                },
            )?;
        (!map.is_empty() && !metadata.media.is_empty()).then_some((metadata, map))
    }

    fn get_file(
        line: &RecordLine,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        file: AttachmentFile<'_>,
    ) -> Option<(IosWcMetadata, Vec<u8>)> {
        backups
            .get(&format!(
                "Documents/{}/{}/{}/{}.{}",
                file.account, file.folder, file.hashed_user, line.local_id, file.ext
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
                    .map_err(|e| {
                        warn!(
                            "failed to read {}: {}, {}, {}, {}",
                            file.file_type, file.account, file.hashed_user, line.local_id, e
                        )
                    })
                    .ok()
            })
            .map(|data| {
                (
                    IosWcMetadata::new()
                        .with_hash(file.file_type.into(), Hash32::sha3_256(&data).to_hex()),
                    data,
                )
            })
    }
}
