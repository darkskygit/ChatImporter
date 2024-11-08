use super::*;
use binread::*;
use ibackuptool2::{Backup, BackupFile};
use num_enum::TryFromPrimitive;
use plist::Value;
use rusqlite::{params, Connection, OpenFlags, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{Cursor, Error, ErrorKind, Write};
use std::iter::IntoIterator;
use std::str::{from_utf8, Utf8Error};
use std::sync::Arc;
use tempfile::NamedTempFile;

const DOMAIN: &str = "AppDomain-com.tencent.xin";

#[derive(BinRead)]
#[br(little, magic = b"\n")]
struct ContactRemark {
    _len: u8,
    #[br(count = _len)]
    remark: Vec<u8>,
}

impl ContactRemark {
    pub fn get_remark(&self) -> Result<&str, Utf8Error> {
        from_utf8(&self.remark)
    }
}

#[derive(Clone, Default)]
struct Contact {
    pub name: String,
    remark: Vec<u8>,
    head: Vec<u8>,
    pub user_type: i32,
}

impl Contact {
    pub fn from_name(name: String) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }
    pub fn get_remark(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(Cursor::new(&self.remark)
            .read_le::<ContactRemark>()?
            .get_remark()?
            .into())
    }

    #[allow(dead_code)]
    pub fn get_image(&self) -> Result<Option<String>, Utf8Error> {
        lazy_static! {
            static ref URL_MATCHER: Regex =
                Regex::new(r"(http://[a-zA-Z\./_\d]*/0)([^a-zA-Z\./_\d]|$)").unwrap();
        }
        Ok(URL_MATCHER
            .captures(from_utf8(&self.head)?)
            .and_then(|c| c.iter().nth(1).and_then(|i| i))
            .map(|i| i.as_str().trim().into()))
    }
}

enum MMType {
    Vec(Vec<u8>),
    String(String),
    SubString(String),
}

impl MMType {
    fn as_str(&self) -> &str {
        match self {
            Self::String(s) | Self::SubString(s) => s,
            _ => "",
        }
    }
}

struct MMMap {
    data: Vec<u8>,
    pos: usize,
}

impl MMMap {
    fn to_map(data: &[u8], len: Option<usize>) -> HashMap<String, MMType> {
        Self::new(data, len).into_map()
    }

    fn new(data: &[u8], len: Option<usize>) -> Self {
        Self {
            data: data[..len.unwrap_or(data.len())].into(),
            pos: 8,
        }
    }

    fn into_map(mut self) -> HashMap<String, MMType> {
        let mut map = HashMap::new();
        while let Some(k) = self.next() {
            if let MMType::String(key) = k {
                if let Some(val) = self.next() {
                    map.insert(key, val);
                }
            }
        }
        map
    }

    fn parse_pos(orig_data: &[u8], pos: usize) -> (usize, usize) {
        let size = orig_data[pos];
        if size & 0x80 == 0 {
            (size as usize, 1)
        } else {
            let splitted_data = &orig_data[pos..];
            let len = splitted_data.iter().take_while(|&u| u & 128 != 0).count() + 1;
            let len = if len >= 4 {
                4
            } else if len <= 0 {
                return (0, 0);
            } else {
                len
            };
            let splitted_size = &splitted_data[..len];
            let mut size: usize = 0;
            for (i, c) in splitted_size.iter().enumerate() {
                let shift = i * 7;
                if c & 128 != 0 {
                    // More bytes are present
                    size |= (*c as usize & 127) << shift;
                } else {
                    size |= (*c as usize) << shift;
                }
            }

            (size.into(), splitted_size.len())
        }
    }
}

impl Iterator for MMMap {
    type Item = MMType;
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.len() > self.pos {
            let (size, pos_len) = Self::parse_pos(&self.data, self.pos);
            self.pos += pos_len;
            (size > 0 && self.pos + size < self.data.len()).then(|| {
                let slice = &self.data[self.pos..self.pos + size];
                self.pos += size;

                let (sub_size, sub_pos_len) = Self::parse_pos(slice, 0);
                (sub_size > 0 && sub_pos_len + sub_size <= slice.len())
                    .then(|| {
                        from_utf8(&slice[sub_pos_len..sub_pos_len + sub_size])
                            .map(|s| MMType::SubString(s.into()))
                            .or_else(|_| from_utf8(slice).map(|s| MMType::String(s.into())))
                            .unwrap_or_else(|_| MMType::Vec(slice.into()))
                    })
                    .unwrap_or_else(|| {
                        from_utf8(slice)
                            .map(|s| MMType::String(s.into()))
                            .unwrap_or_else(|_| MMType::Vec(slice.into()))
                    })
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TryFromPrimitive)]
#[repr(u32)]
enum MsgType {
    Normal = 1,              // 文字/emoji
    Image = 3,               // 图片
    Voice = 34,              // 语音
    ContactShare = 42,       // 联系人分享
    Video = 43,              // 视频
    BigEmoji = 47,           // 大表情
    Location = 48,           // 定位
    CustomApp = 49,          // 文件、分享、转账、聊天记录批量转发
    VoipContent = 50,        // 语音/视频通话？
    ShortVideo = 62,         // 短视频？
    VoipStatus = 64,         // 语音通话状态
    WeWorkContactShare = 66, // 企业微信联系人分享
    System = 10000,          // 系统信息，入群/群改名/他人撤回信息/红包领取提醒等等
    Revoke = 10002,          // 撤回信息修改
    Unknown = u32::MAX,
}

impl Default for MsgType {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
enum MetadataType {
    Int(i64),
    Float(f64),
    Str(String),
}

impl MetadataType {
    pub fn get_hash(&self) -> i64 {
        if let Self::Int(ref i) = self {
            *i
        } else {
            0
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AttachMetadata {
    mtype: MsgType,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    hash: HashMap<String, MetadataType>,
}

impl AttachMetadata {
    pub fn new() -> Self {
        Self {
            hash: HashMap::new(),
            ..Default::default()
        }
    }

    pub fn into_map(self) -> HashMap<String, MetadataType> {
        self.hash
    }

    fn thum_checker(
        recorder: &SqliteChatRecorder,
        attaches: &Attachments,
        target: Vec<&i64>,
        thum: &i64,
    ) -> bool {
        let target = target
            .iter()
            .filter_map(|&hash| {
                recorder
                    .get_blob(*hash)
                    .and_then(|blob| {
                        Ok(blob_dhash(&blob)
                            .context(format!("Failed to decode image: {}", hash))?)
                    })
                    .ok()
            })
            .collect::<Vec<_>>();
        (target.len() > 0)
            .then(|| {
                attaches
                    .get(&thum.to_string())
                    .and_then(|blob| {
                        blob_dhash(blob)
                            .map_err(|e| warn!("Failed to decode image: {}, {}", thum, e))
                            .ok()
                    })
                    .and_then(|thum_hash| {
                        target
                            .into_iter()
                            .find(|hash| hamming_distance(*hash, thum_hash) <= 5)
                            .or_else(|| {
                                warn!("Failed to find similar image: {}", thum);
                                None
                            })
                    })
                    .is_none()
            })
            .unwrap_or(true)
    }

    fn hash_checker(
        recorder: &SqliteChatRecorder,
        attaches: &Attachments,
        old_hash: &HashMap<String, MetadataType>,
        new_hash: &HashMap<String, MetadataType>,
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
                if let ("thum", MetadataType::Int(thum)) = (key.as_str(), new) {
                    let mut target = match (new_hash.get("img"), new_hash.get("hd")) {
                        (Some(MetadataType::Int(img)), Some(MetadataType::Int(hd))) => {
                            vec![img, hd]
                        }
                        (Some(MetadataType::Int(img)), None) => vec![img],
                        (None, Some(MetadataType::Int(hd))) => vec![hd],
                        _ => vec![],
                    };
                    if let MetadataType::Int(old) = old {
                        // 迁移记录可能重新生成缩略图
                        // 因此把旧缩略图也加入对比
                        target.push(old);
                    }
                    if !Self::thum_checker(recorder, attaches, target, thum) {
                        // 存在相似高清图时跳过waring
                        continue;
                    }
                }
                warn!(r#"metadata override "{}": "{:?}" -> "{:?}""#, key, old, new);
            }
        }
    }

    pub fn merge(self, recorder: &SqliteChatRecorder, attaches: &Attachments, old: Self) -> Self {
        let old_hash = old.into_map();
        let hash = old_hash.clone().into_iter().chain(self.hash).collect();
        Self::hash_checker(recorder, attaches, &old_hash, &hash);
        Self { hash, ..self }
    }

    pub fn with_hash(mut self, name: String, hash: i64) -> Self {
        self.hash.insert(name, MetadataType::Int(hash));
        self
    }

    pub fn with_float(mut self, name: String, tag: String) -> Self {
        self.hash.insert(
            name,
            tag.parse()
                .map(|f| MetadataType::Float(f))
                .unwrap_or(MetadataType::Str(tag)),
        );
        self
    }

    pub fn with_tag(mut self, name: String, tag: String) -> Self {
        self.hash.insert(name, MetadataType::Str(tag));
        self
    }

    pub fn with_type(mut self, msg_type: MsgType) -> Self {
        self.mtype = msg_type;
        self
    }
}

#[derive(Clone, Debug)]
struct RecordLine {
    local_id: i64,
    server_id: i64,
    created_time: i64,
    message: String,
    status: u8,
    image_status: u16,
    msg_type: MsgType,
    is_dest: bool,
    skip_resource: bool,
}

impl RecordLine {
    pub fn get_attach_hashs(
        &self,
        backup: &Backup,
        account: &str,
        hashed_user: &str,
    ) -> HashMap<i64, String> {
        backup
            .find_regex_paths(
                DOMAIN,
                &format!(
                    "^Documents/{}/(Audio|Img|OpenData|Video)/{}/{}[\\./]",
                    account, hashed_user, self.local_id
                ),
            )
            .iter()
            .filter_map(|file| {
                backup
                    .read_file(file)
                    .map(|data| (data, file.relative_filename.clone()))
                    .map_err(|e| error!("Failed to read attach: {}, {}", file.relative_filename, e))
                    .ok()
            })
            .map(|(data, path)| (Blob::new(data).hash, path))
            .collect()
    }

    pub fn get_audio_metadata(&self) -> AttachMetadata {
        lazy_static! {
            static ref CLIENT_ID_MATCH: Regex =
                Regex::new(r#"clientmsgid\s*?=\s*?"(.*?)""#).unwrap();
            static ref BUFFER_ID_MATCH: Regex = Regex::new(r#"bufid\s*?=\s*?"(.*?)""#).unwrap();
        }
        [
            self.get_match_string(&*BUFFER_ID_MATCH, "bufid"),
            self.get_match_string(&*CLIENT_ID_MATCH, "clientid"),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_image_metadata(&self) -> AttachMetadata {
        lazy_static! {
            static ref CDN_THUM_URL_MATCH: Regex =
                Regex::new(r#"cdnthumburl\s*?=\s*?"(.*?)""#).unwrap();
            static ref CDN_SMALL_URL_MATCH: Regex =
                Regex::new(r#"cdnmidimgurl\s*?=\s*?"(.*?)""#).unwrap();
            static ref CDN_HD_URL_MATCH: Regex =
                Regex::new(r#"cdnbigimgurl\s*?=\s*?"(.*?)""#).unwrap();
            static ref AES_KEY_MATCH: Regex = Regex::new(r#"aeskey\s*?=\s*?"(.*?)""#).unwrap();
        }
        [
            self.get_match_string(&*CDN_THUM_URL_MATCH, "thum_cdn"),
            self.get_match_string(&*CDN_SMALL_URL_MATCH, "img_cdn"),
            self.get_match_string(&*CDN_HD_URL_MATCH, "hd_cdn"),
            self.get_match_string(&*AES_KEY_MATCH, "key"),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_video_metadata(&self) -> AttachMetadata {
        lazy_static! {
            static ref CDN_URL_MATCH: Regex = Regex::new(r#"cdnvideourl\s*?=\s*?"(.*?)""#).unwrap();
            static ref AES_KEY_MATCH: Regex = Regex::new(r#"aeskey\s*?=\s*?"(.*?)""#).unwrap();
        }
        [
            self.get_match_string(&*CDN_URL_MATCH, "cdn"),
            self.get_match_string(&*AES_KEY_MATCH, "key"),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_audio(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(AttachMetadata, Attachments)> {
        let (ftype, dir, ext) = ("voice", "Audio", "aud");
        Self::get_files(vec![self
            .get_file(backup, backups, account, hashed_user, ftype, dir, ext)
            .map(|(metadata, data)| (ftype.into(), metadata, data))])
    }

    pub fn get_contact(&self) -> AttachMetadata {
        lazy_static! {
            static ref NICKNAME_MATCH: Regex = Regex::new(r#"nickname\s*?=\s*?"(.*?)""#).unwrap();
            static ref USERNAME_MATCH: Regex = Regex::new(r#"username\s*?=\s*?"(.*?)""#).unwrap();
            static ref CITY_MATCH: Regex = Regex::new(r#"city\s*?=\s*?"(.*?)""#).unwrap();
            static ref PROVINCE_MATCH: Regex = Regex::new(r#"province\s*?=\s*?"(.*?)""#).unwrap();
            static ref OPENIMDESC_MATCH: Regex =
                Regex::new(r#"openimdesc\s*?=\s*?"(.*?)""#).unwrap();
            static ref BIG_IMG_MATCH: Regex =
                Regex::new(r#"bigheadimgurl\s*?=\s*?"(.*?)""#).unwrap();
            static ref SMALL_IMG_MATCH: Regex =
                Regex::new(r#"smallheadimgurl\s*?=\s*?"(.*?)""#).unwrap();
        }
        [
            self.get_match_string(&*NICKNAME_MATCH, "nickname"),
            self.get_match_string(&*USERNAME_MATCH, "username"),
            self.get_match_string(&*CITY_MATCH, "city"),
            self.get_match_string(&*PROVINCE_MATCH, "province"),
            self.get_match_string(&*OPENIMDESC_MATCH, "openimdesc"),
            self.get_match_string(&*BIG_IMG_MATCH, "head")
                .or_else(|| self.get_match_string(&*SMALL_IMG_MATCH, "head")),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_custom_app(
        &self,
        backup: &Backup,
        account: &str,
        hashed_user: &str,
    ) -> Option<(AttachMetadata, Attachments)> {
        lazy_static! {
            static ref TITLE_MATCH: Regex = Regex::new(r"<title>(.*?)</title>").unwrap();
            static ref TITLE_CDATA_MATCH: Regex =
                Regex::new(r"<title><!\[CDATA\[((?s).*?)]]></title>").unwrap();
            static ref DESCRIPTION_MATCH: Regex = Regex::new(r"<des>((?s).*?)</des>").unwrap();
            static ref DESCRIPTION_CDATA_MATCH: Regex =
                Regex::new(r"<des><!\[CDATA\[((?s).*?)]]></des>").unwrap();
            static ref THUM_MATCH: Regex =
                Regex::new(r"<thumburl><!\[CDATA\[((?s).*?)]]></thumburl>").unwrap();
            static ref APPNAME_MATCH: Regex = Regex::new(r"<appname>(.*?)</appname>").unwrap();
            static ref URL_MATCH: Regex = Regex::new(r"<url>(.*?)</url>").unwrap();
            static ref URL_CDATA_MATCH: Regex =
                Regex::new(r"<url><!\[CDATA\[((?s).*?)]]></url>").unwrap();
            static ref RECORD_INFO_MATCH: Regex =
                Regex::new(r"<recorditem><!\[CDATA\[((?s).*?)]]></recorditem>").unwrap();
            static ref RECORD_INFO_ESCAPE_MATCH: Regex =
                Regex::new(r"<recorditem>((?s).*?)</recorditem>").unwrap();
        }
        let path = format!(
            "Documents/{}/{}/{}/{}",
            account, "OpenData", hashed_user, self.local_id
        );
        let files = if self.skip_resource {
            HashMap::new()
        } else {
            backup
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
                                self.local_id,
                                path.name_str(),
                                e
                            )
                        })
                        .ok()
                })
                .collect::<HashMap<_, _>>()
        };
        let metadata = [
            self.get_match_string(&*TITLE_CDATA_MATCH, "title")
                .or_else(|| self.get_match_string(&*TITLE_MATCH, "title")),
            self.get_match_string(&*DESCRIPTION_CDATA_MATCH, "description")
                .or_else(|| self.get_match_string(&*DESCRIPTION_MATCH, "description")),
            self.get_match_string(&*THUM_MATCH, "thum"),
            self.get_match_string(&*RECORD_INFO_MATCH, "record")
                .or_else(|| self.get_match_string(&*RECORD_INFO_ESCAPE_MATCH, "record")),
            self.get_match_string(&*APPNAME_MATCH, "app"),
            self.get_match_string(&*URL_CDATA_MATCH, "url")
                .or_else(|| self.get_match_string(&*URL_MATCH, "url")),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        });
        Some((
            files.iter().fold(metadata, |metadata, (name, data)| {
                metadata.with_hash(format!("attach:{}", name), Blob::new(data.clone()).hash)
            }),
            files,
        ))
    }

    pub fn get_emoji(&self) -> AttachMetadata {
        lazy_static! {
            static ref MD5_MATCH: Regex = Regex::new(r#"md5\s*?=\s*?"(.*?)""#).unwrap();
            static ref CDN_URL_MATCH: Regex = Regex::new(r#"cdnurl\s*?=\s*?"(.*?)""#).unwrap();
            static ref AES_KEY_MATCH: Regex = Regex::new(r#"aeskey\s*?=\s*?"(.*?)""#).unwrap();
            static ref ENC_URL_MATCH: Regex = Regex::new(r#"encrypturl\s*?=\s*?"(.*?)""#).unwrap();
            static ref EXTERN_MATCH: Regex = Regex::new(r#"externurl\s*?=\s*?"(.*?)""#).unwrap();
        }
        [
            self.get_match_string(&*MD5_MATCH, "md5"),
            self.get_match_string(&*CDN_URL_MATCH, "cdn"),
            self.get_match_string(&*AES_KEY_MATCH, "key"),
            self.get_match_string(&*ENC_URL_MATCH, "enc"),
            self.get_match_string(&*EXTERN_MATCH, "extern"),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_image(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(AttachMetadata, Attachments)> {
        Self::get_files(vec![
            self.get_image_thum(backup, backups, account, hashed_user),
            self.get_image_small(backup, backups, account, hashed_user),
            self.get_image_hd(backup, backups, account, hashed_user),
        ])
    }

    pub fn get_location(&self) -> AttachMetadata {
        lazy_static! {
            static ref X_MATCH: Regex = Regex::new(r#" x\s*?=\s*?"(.*?)""#).unwrap();
            static ref Y_MATCH: Regex = Regex::new(r#" y\s*?=\s*?"(.*?)""#).unwrap();
            static ref LABEL_MATCH: Regex = Regex::new(r#"label\s*?=\s*?"(.*?)""#).unwrap();
            static ref NAME_MATCH: Regex = Regex::new(r#"poiname\s*?=\s*?"(.*?)""#).unwrap();
        }

        [
            self.get_match_string(&*LABEL_MATCH, "label"),
            self.get_match_string(&*NAME_MATCH, "name"),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(
            [
                self.get_match_string(&*X_MATCH, "x"),
                self.get_match_string(&*Y_MATCH, "y"),
            ]
            .iter()
            .filter_map(|e| e.as_ref())
            .fold(AttachMetadata::new(), |metadata, (k, v)| {
                metadata.with_float(k.to_string(), v.into())
            }),
            |metadata, (k, v)| metadata.with_tag(k.to_string(), v.into()),
        )
    }

    pub fn get_voip_status(&self) -> AttachMetadata {
        lazy_static! {
            static ref CONTENT_MATCH: Regex = Regex::new(r#"msgContent\s*?=\s*?"(.*?)""#).unwrap();
        }
        [self.get_match_string(&*CONTENT_MATCH, "content")]
            .iter()
            .filter_map(|e| e.as_ref())
            .fold(AttachMetadata::new(), |metadata, (k, v)| {
                metadata.with_tag(k.to_string(), v.into())
            })
    }

    pub fn get_revoke(&self) -> AttachMetadata {
        lazy_static! {
            static ref REVOKE_MATCH: Regex =
                Regex::new(r"<revokecontent>(.*?)</revokecontent>").unwrap();
            static ref REVOKE_CDATA_MATCH: Regex =
                Regex::new(r"<revokecontent><!\[CDATA\[((?s).*?)]]></revokecontent>").unwrap();
        }
        [self
            .get_match_string(&*REVOKE_CDATA_MATCH, "revoke")
            .or_else(|| self.get_match_string(&*REVOKE_MATCH, "revoke"))]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(AttachMetadata::new(), |metadata, (k, v)| {
            metadata.with_tag(k.to_string(), v.into())
        })
    }

    pub fn get_video(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(AttachMetadata, Attachments)> {
        let (ftype, dir, ext) = ("video", "Video", "mp4");
        Self::get_files(vec![self
            .get_file(backup, backups, account, hashed_user, ftype, dir, ext)
            .map(|(metadata, data)| (ftype.into(), metadata, data))])
    }

    fn get_image_small(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, AttachMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("img", "Img", "pic");
        self.get_file(backup, backups, account, hashed_user, ftype, dir, ext)
            .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn get_image_hd(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, AttachMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("hd", "Img", "pic_hd");
        self.get_file(backup, backups, account, hashed_user, ftype, dir, ext)
            .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn get_image_thum(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
    ) -> Option<(String, AttachMetadata, Vec<u8>)> {
        let (ftype, dir, ext) = ("thum", "Img", "pic_thum");
        self.get_file(backup, backups, account, hashed_user, ftype, dir, ext)
            .map(|(metadata, data)| (ftype.into(), metadata, data))
    }

    fn get_files<I>(iter: I) -> Option<(AttachMetadata, Attachments)>
    where
        I: IntoIterator<Item = Option<(String, AttachMetadata, Vec<u8>)>>,
    {
        let (metadata, map) = iter
            .into_iter()
            .filter_map(|i| i.clone())
            .filter_map(|(ftype, metadata, data)| {
                metadata
                    .hash
                    .get(&ftype)
                    .map(|hash| (ftype, hash.clone(), data))
            })
            .fold(
                (AttachMetadata::new(), HashMap::new()),
                |(metadata, mut map), (ftype, hash, data)| {
                    map.insert(hash.get_hash().to_string(), data.clone());
                    (metadata.with_hash(ftype, hash.get_hash()), map)
                },
            );
        (!map.is_empty() && !metadata.hash.is_empty()).then_some((metadata, map))
    }

    fn get_file(
        &self,
        backup: &Backup,
        backups: &HashMap<String, BackupFile>,
        account: &str,
        hashed_user: &str,
        file_type: &str,
        folder: &str,
        ext: &str,
    ) -> Option<(AttachMetadata, Vec<u8>)> {
        if self.skip_resource {
            None
        } else {
            backups
                .get(&format!(
                    "Documents/{}/{}/{}/{}.{}",
                    account, folder, hashed_user, self.local_id, ext
                ))
                .or_else(|| {
                    debug!(
                        "{} not found: {}, {}, {}",
                        file_type, account, hashed_user, self.local_id
                    );
                    None
                })
                .and_then(|file| {
                    backup
                        .read_file(file)
                        .map_err(|e| {
                            warn!(
                                "failed to read {}: {}, {}, {}, {}",
                                file_type, account, hashed_user, self.local_id, e
                            )
                        })
                        .ok()
                })
                .map(|data| {
                    (
                        AttachMetadata::new()
                            .with_hash(file_type.into(), Blob::new(data.clone()).hash),
                        data,
                    )
                })
        }
    }

    fn get_match_string<'a>(&self, regex: &Regex, key: &'a str) -> Option<(&'a str, String)> {
        regex
            .captures(&self.message)
            .filter(|c| c.len() == 2 && !c[1].is_empty())
            .map(|c| (key, hex2b64(&c[1])))
    }
}

#[derive(Clone, Default)]
struct UserDB {
    contact: Option<Arc<NamedTempFile>>,
    messages: Vec<Arc<NamedTempFile>>,
    setting: Option<BackupFile>,
    kv_setting: Option<BackupFile>,
    session: Option<Arc<NamedTempFile>>,
    account_files: HashMap<String, BackupFile>,
    chats: HashMap<String, String>,
    contacts: HashMap<String, Contact>,
    account: String,
    wxid: String,
    name: String,
    head: String,
}

impl UserDB {
    pub fn new(
        backup: &Backup,
        account: String,
        file: &BackupFile,
        account_files: Vec<BackupFile>,
    ) -> Self {
        let account_files = account_files
            .iter()
            .map(|file| (file.relative_filename.clone(), file.clone()))
            .collect();
        let user_db = Self {
            account,
            account_files,
            ..Self::default()
        };
        user_db.match_path(backup, file)
    }

    pub fn with(self, backup: &Backup, file: &BackupFile) -> Self {
        self.match_path(backup, file)
    }

    fn match_path(mut self, backup: &Backup, file: &BackupFile) -> Self {
        lazy_static! {
            static ref MESSAGES: Regex = Regex::new(r"^message_\d+.sqlite$").unwrap();
        }
        let filename = Path::new(&file.relative_filename).name_str().to_string();
        if ["WCDB_Contact.sqlite", "MM.sqlite", "session.db"].contains(&filename.as_str())
            || MESSAGES.is_match(&filename)
        {
            if let Ok(tmpfile) = NamedTempFile::new().and_then(|mut tmpfile| {
                debug!(
                    "read file: {}, {}, {}",
                    self.account, file.fileid, file.relative_filename
                );
                let data = backup
                    .read_file(file)
                    .map_err(|e| Error::new(ErrorKind::Other, format!("{}", e)))?;
                tmpfile.write_all(&data)?;
                Ok(tmpfile)
            }) {
                match filename.as_str() {
                    "WCDB_Contact.sqlite" => self.contact = Some(Arc::new(tmpfile)),
                    "MM.sqlite" => self.messages.push(Arc::new(tmpfile)),
                    "session.db" => self.session = Some(Arc::new(tmpfile)),
                    _ if filename.starts_with("message_") => self.messages.push(Arc::new(tmpfile)),
                    _ => {}
                }
            } else {
                warn!("Failed to extract file: {}", file.relative_filename);
            }
        } else if filename == "mmsetting.archive" {
            self.setting = Some(file.clone());
        } else if filename.starts_with("mmsetting.archive.") {
            self.kv_setting = Some(file.clone())
        }
        self
    }

    pub fn is_complete(&self) -> bool {
        let ret = self.contact.is_some()
            && !self.messages.is_empty()
            && (self.setting.is_some() || self.kv_setting.is_some())
            && self.session.is_some();
        if !ret {
            warn!(
                "user {} ({}, {}) db lost some metadata: {}, {}, {}, {}, {}",
                self.account,
                self.wxid,
                self.name,
                self.contact.is_some(),
                !self.messages.is_empty(),
                self.setting.is_some(),
                self.kv_setting.is_some(),
                self.session.is_some()
            );
        }
        ret
    }

    pub fn build(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        self.load_settings(backup)?;
        self.load_contacts()?;
        self.load_chats()?;
        Ok(())
    }

    fn load_settings(&mut self, backup: &Backup) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(setting) = &self.setting {
            let data = backup.read_file(setting)?;
            if let Some(array) = Value::from_reader(Cursor::new(data))
                .map_err(|e| {
                    warn!(
                        "failed to load settings: {}, {}",
                        setting.relative_filename, e
                    )
                })
                .ok()
                .and_then(|plist| {
                    plist
                        .as_dictionary()
                        .and_then(|dict| dict.get("$objects"))
                        .and_then(|obj| obj.as_array())
                        .map(|a| a.clone())
                })
            {
                if array.len() > 3 {
                    self.wxid = array[2]
                        .as_string()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    self.name = array[3]
                        .as_string()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                }
                if array.len() > 50 {
                    self.head = if let Some(head) = array
                        .iter()
                        .filter_map(|v| v.as_string())
                        .filter(|s| {
                            s.starts_with("http://")
                                && s.find("mmhead").is_some()
                                && s.find("/132").is_some()
                        })
                        .next()
                    {
                        head
                    } else {
                        ""
                    }
                    .to_string();
                }
            } else {
                warn!(
                    "failed to load settings: {}, {}",
                    setting.relative_filename, "array not exists"
                );
            }
        }
        if let Some(setting) = &self.kv_setting {
            let data = backup.read_file(setting)?;
            let map = MMMap::to_map(&data, None);
            self.wxid = if self.wxid.is_empty() {
                map.get("86").map(MMType::as_str).unwrap_or_default().into()
            } else {
                self.wxid.clone()
            };
            self.name = if self.name.is_empty() {
                map.get("88").map(MMType::as_str).unwrap_or_default().into()
            } else {
                self.name.clone()
            };
            self.head = if self.head.is_empty() {
                map.get("headimgurl")
                    .map(MMType::as_str)
                    .unwrap_or_default()
                    .into()
            } else {
                self.head.clone()
            };
        }
        if self.wxid.is_empty() || self.name.is_empty() || self.head.is_empty() {
            warn!(
                r#"lost some account info: "{}", "{}", "{}""#,
                self.wxid, self.name, self.head
            );
        }
        Ok(())
    }

    fn get_conn(file: Option<Arc<NamedTempFile>>) -> SqliteResult<Option<Connection>> {
        if let Some(file) = file {
            Ok(Some(Connection::open_with_flags(
                file.as_ref(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?))
        } else {
            Ok(None)
        }
    }

    fn load_chats(&mut self) -> SqliteResult<()> {
        for message in self.messages.iter() {
            if let Some(conn) = Self::get_conn(Some(message.clone()))? {
                let contact_keys = self.contacts.keys().map(|s| s.as_str()).collect::<Vec<_>>();
                let chats = conn
                        .prepare(r#"SELECT name FROM sqlite_master where type='table' and name like "Chat\_%" ESCAPE '\'"#)?
                        .query_map(params![], |row| {
                            let name: String = row.get(0)?;
                            let hash = &name[5..];
                            if !contact_keys.iter().any(|&i| i == hash) && hash != self.account {
                                warn!("Contact info for chat not exists: {}", hash);
                            }
                            Ok((hash.into(), name))
                        })?
                        .filter_map(|r| {
                            r.map_err(|e| warn!("failed to parse chat list: {}", e))
                                .ok()
                        })
                        .collect::<HashMap<_, _>>();
                self.chats = self.chats.clone().into_iter().chain(chats).collect();
            }
        }
        Ok(())
    }

    fn load_contacts(&mut self) -> SqliteResult<()> {
        if let Some(conn) = Self::get_conn(self.contact.clone())? {
            self.contacts = conn
                .prepare("SELECT userName, dbContactRemark, dbContactHeadImage, type FROM Friend")?
                .query_map(params![], |row| {
                    let name = row.get(0)?;
                    Ok((
                        gen_md5(&name),
                        Contact {
                            name,
                            remark: row.get(1)?,
                            head: row.get(2)?,
                            user_type: row.get(3)?,
                        },
                    ))
                })?
                .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                .collect();
        }
        Ok(())
    }

    fn get_chat_ids(&self) -> Vec<String> {
        self.chats.keys().cloned().collect::<Vec<_>>()
    }

    fn get_contacts(&self) -> Vec<String> {
        self.find_contacts("")
    }

    fn find_contacts<S: ToString>(&self, name: S) -> Vec<String> {
        let name = name.to_string();
        let chat_keys = self.chats.keys().map(|s| s.as_str()).collect::<Vec<_>>();
        self.contacts
            .iter()
            .filter_map(|(hash, c)| {
                if chat_keys.iter().any(|&i| i == hash) {
                    (name.is_empty()
                        || hash == &name
                        || c.name.find(&name).is_some()
                        || c.get_remark().ok().and_then(|r| r.find(&name)).is_some())
                    .then(|| {
                        warn!(
                            "Chat table found: {}, {}, {}",
                            hash,
                            c.name,
                            c.get_remark()
                                .unwrap_or_else(|e| format!("No Remark: {}", e))
                        );
                        hash
                    })
                } else {
                    debug!(
                        "Chat table not found: {}, {}, {}",
                        hash,
                        c.name,
                        c.get_remark()
                            .unwrap_or_else(|e| format!("No Remark: {}", e))
                    );
                    None
                }
            })
            .cloned()
            .collect()
    }

    fn find_chat_table(&self, hash: &str) -> Vec<Arc<NamedTempFile>> {
        let query = format!(
            r#"SELECT name FROM sqlite_master where type='table' and name like "Chat\_{}" ESCAPE '\'"#,
            hash
        );
        self.messages
            .iter()
            .filter(|&file| {
                Self::get_conn(Some(file.clone()))
                    .and_then(|conn| {
                        if let Some(conn) = conn {
                            conn.prepare(&query)?.exists(params![])
                        } else {
                            Err(rusqlite::Error::InvalidQuery)
                        }
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    fn load_record_lines<S: ToString>(
        &self,
        user_name: S,
        skip_resource: bool,
    ) -> SqliteResult<Vec<RecordLine>> {
        let mut lines = vec![];
        let user_name = user_name.to_string();
        let hash = self
            .chats
            .keys()
            .find(|h| h.as_str() == user_name)
            .map(|s| s.into())
            .unwrap_or_else(|| gen_md5(user_name));
        for message in self.find_chat_table(&hash) {
            if let Some(conn) = Self::get_conn(Some(message.clone()))? {
                lines.append(
                    &mut conn
                        .prepare(&format!(
                            "SELECT
                            MesLocalID,
                            MesSvrID,
                            CreateTime,
                            Message,
                            Status,
                            ImgStatus,
                            Type,
                            Des
                        FROM
                            Chat_{}",
                            hash
                        ))?
                        .query_map(params![], |row| {
                            Ok(RecordLine {
                                local_id: row.get(0)?,
                                server_id: row.get(1)?,
                                created_time: row.get(2)?,
                                message: row.get(3)?,
                                status: row.get(4)?,
                                image_status: row.get(5)?,
                                msg_type: MsgType::try_from(row.get::<_, u32>(6)?).unwrap_or_else(
                                    |t| {
                                        warn!("unknown type: {}", t);
                                        MsgType::Unknown
                                    },
                                ),
                                is_dest: row.get(7)?,
                                skip_resource,
                            })
                        })?
                        .filter_map(|r| {
                            r.map_err(|e| warn!("failed to parse chat line: {}", e))
                                .ok()
                        })
                        .collect(),
                );
            }
        }
        Ok(lines)
    }

    fn get_from_user_id(&self, content: &str) -> Option<(String, String)> {
        lazy_static! {
            static ref FROM_MATCH: Regex = Regex::new(r#"fromusername\s*?=\s*?"(.*?)""#).unwrap();
            static ref XML_FROM_MATCH: Regex =
                Regex::new(r"<fromusername>((?s).*?)</fromusername>").unwrap();
            static ref XML_CDATA_FROM_MATCH: Regex =
                Regex::new(r"<fromusername><!\[CDATA\[((?s).*?)]]></fromusername>").unwrap();
        }
        FROM_MATCH
            .captures(content)
            .filter(|c| c.len() == 2 && !c[1].is_empty())
            .or_else(|| {
                XML_FROM_MATCH
                    .captures(content)
                    .filter(|c| c.len() == 2 && !c[1].is_empty())
            })
            .or_else(|| {
                XML_CDATA_FROM_MATCH
                    .captures(content)
                    .filter(|c| c.len() == 2 && !c[1].is_empty())
            })
            .map(|c| {
                self.contacts
                    .get(&gen_md5(&c[1]))
                    .map(|c| c.clone())
                    .unwrap_or_else(|| Contact::from_name(c[1].into()))
            })
            .map(|c| (c.name.clone(), c.get_remark().unwrap_or_default()))
    }

    fn parse_user_info(&self, content: &str) -> Option<(String, String, String)> {
        lazy_static! {
            static ref FIRST_LINE_USER_ID_MATCH: Regex =
                Regex::new(r#"^\s*(.*?)\s*?:\s*?\n"#).unwrap();
        }
        FIRST_LINE_USER_ID_MATCH
            .captures(content)
            .filter(|c| c.len() == 2 && !c[1].is_empty())
            .map(|c| {
                self.contacts
                    .get(&gen_md5(&c[1]))
                    .map(|c| c.clone())
                    .unwrap_or_else(|| Contact::from_name(c[1].into()))
            })
            .map(|c| {
                (
                    c.name.clone(),
                    c.get_remark().unwrap_or_default(),
                    content.split("\n").skip(1).collect::<Vec<_>>().join("\n"),
                )
            })
            .or_else(|| {
                self.get_from_user_id(content)
                    .map(|(id, remark)| (id, remark, content.into()))
            })
    }

    fn get_microsecond(server_id: i64) -> i64 {
        use mur3::Hasher128;
        use std::hash::Hasher;
        let mut hasher = Hasher128::with_seed(42);
        hasher.write(&server_id.to_be_bytes());
        (((hasher.finish() as u128) * 1000) / u32::MAX as u128) as i64
    }

    fn transform_record_line(
        &self,
        backup: &Backup,
        line: &RecordLine,
        contact: &Contact,
    ) -> Result<RecordType, String> {
        const CHECK_ATTACHES: bool = false;
        let is_group = contact.name.ends_with("@chatroom");
        let (sender_id, sender_name, content) = {
            if line.is_dest {
                if is_group {
                    if let Some((id, remark, content)) = self.parse_user_info(&line.message) {
                        (id, remark, content)
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
                        if let Some((id, remark)) = self.get_from_user_id(&line.message) {
                            (id, remark, line.message.clone())
                        } else {
                            (
                                contact.name.clone(),
                                contact.get_remark().unwrap_or_default(),
                                line.message.clone(),
                            )
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
                self.parse_user_info(&line.message)
                    .map(|(_, _, content)| (self.wxid.clone(), self.name.clone(), content))
                    .unwrap_or_else(|| (self.wxid.clone(), self.name.clone(), line.message.clone()))
            }
        };

        let (content, metadata, attach) = match line.msg_type {
            MsgType::Normal => Some((
                content.replace("\u{2028}", " ").replace("\u{2029}", " "),
                None,
                HashMap::new(),
            )),
            MsgType::Image => line
                .get_image(
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
                        Some(line.get_image_metadata().with_type(line.msg_type.clone())),
                        HashMap::new(),
                    ))
                }),
            MsgType::Video | MsgType::ShortVideo => line
                .get_video(
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
                        Some(line.get_video_metadata().with_type(line.msg_type.clone())),
                        HashMap::new(),
                    ))
                }),
            MsgType::Voice => line
                .get_audio(
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
                        Some(line.get_audio_metadata().with_type(line.msg_type.clone())),
                        HashMap::new(),
                    ))
                }),
            MsgType::BigEmoji => Some((
                "[emoji]".into(),
                Some(line.get_emoji().with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::ContactShare | MsgType::WeWorkContactShare => Some((
                "[contact]".into(),
                Some(line.get_contact().with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::Location => Some((
                "[location]".into(),
                Some(line.get_location().with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::CustomApp => line
                .get_custom_app(backup, &self.account, &gen_md5(&contact.name))
                .map(|(metadata, map)| {
                    (
                        "[app]".into(),
                        Some(metadata.with_type(line.msg_type.clone())),
                        map,
                    )
                }),
            MsgType::VoipContent => Some((
                "[voip]".into(),
                Some(
                    AttachMetadata::new()
                        .with_tag("type".into(), line.message.clone())
                        .with_type(line.msg_type.clone()),
                ),
                HashMap::new(),
            )),
            MsgType::VoipStatus => Some((
                "[voip]".into(),
                Some(line.get_voip_status().with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            MsgType::System => Some((
                "[system]".into(),
                Some(
                    AttachMetadata::new()
                        .with_tag("content".into(), line.message.clone())
                        .with_type(line.msg_type.clone()),
                ),
                HashMap::new(),
            )),
            MsgType::Revoke => Some((
                "[revoke]".into(),
                Some(line.get_revoke().with_type(line.msg_type.clone())),
                HashMap::new(),
            )),
            _ => None,
        }
        .unwrap_or_else(|| (content, None, HashMap::new()));

        if CHECK_ATTACHES {
            use std::collections::HashSet;
            let loaded_hashs = attach
                .values()
                .map(|data| Blob::new(data.clone()).hash)
                .collect::<HashSet<_>>();
            for (hash, path) in
                line.get_attach_hashs(backup, &self.account, &gen_md5(&contact.name))
            {
                if loaded_hashs.get(&hash).is_none() && !path.ends_with(".video_thum") {
                    warn!(
                        "Hash {} not exists: {}, {} | {} | {:?}",
                        hash, path, line.local_id, line.created_time, line.msg_type
                    )
                }
            }
        }

        let record = Record {
            chat_type: "WeChat".into(),
            owner_id: self.wxid.clone(),
            group_id: contact.name.clone(),
            sender_id,
            sender_name,
            content,
            timestamp: line.created_time * 1000 + Self::get_microsecond(line.server_id),
            metadata: metadata.as_ref().and_then(|m| {
                to_vec(m)
                    .map_err(|e| warn!("failed to serialization metadata: {}", e))
                    .ok()
            }),
            ..Default::default()
        };

        Ok(if metadata.is_some() {
            RecordType::from((record, attach))
        } else {
            RecordType::from(record)
        })
    }

    fn transform_record_lines(
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

    fn load_records<S: ToString>(
        &self,
        backup: &Backup,
        chat_id: S,
        skip_resource: bool,
    ) -> Option<Vec<RecordType>> {
        let chat_id = chat_id.to_string();
        self.contacts
            .get(&chat_id)
            .or_else(|| {
                warn!("failed to get chat contact: {}", chat_id);
                None
            })
            .and_then(|contact| {
                self.load_record_lines(&chat_id, skip_resource)
                    .map(|lines| self.transform_record_lines(backup, contact, lines))
                    .map_err(|e| warn!("failed to get chat line: {}", e))
                    .ok()
            })
    }

    pub fn get_record_names(&self, names: Option<Vec<String>>) -> Vec<String> {
        match names {
            None => self.get_chat_ids(),
            Some(names) if names.is_empty() => self.get_contacts(),
            Some(names) => names,
        }
    }

    pub fn get_records(
        &self,
        backup: &Backup,
        name: String,
        skip_resource: bool,
    ) -> Vec<RecordType> {
        self.find_contacts(&name)
            .iter()
            .filter_map(|chat_id| {
                info!("Extracting: {} => {}", name, chat_id);
                self.load_records(backup, chat_id, skip_resource)
            })
            .flatten()
            .collect::<Vec<_>>()
    }
}

#[allow(non_camel_case_types)]
struct Extractor {
    backup: Backup,
    user_info: HashMap<String, UserDB>,
}

impl Extractor {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let mut backup = Backup::new(path)?;
        if backup.manifest.is_encrypted {
            backup.parse_keybag()?;
            debug!("trying decrypt of backup keybag");
            if let Some(ref mut kb) = backup.manifest.keybag.as_mut() {
                let pass = rpassword::prompt_password("Backup Password: ")?;
                kb.unlock_with_passcode(&pass);
            }
            backup.manifest.unlock_manifest();
            backup.parse_manifest()?;
            backup.unwrap_file_keys()?;
        } else {
            backup.parse_manifest()?;
        }
        let user_info = Self::get_user_info(&backup);
        Ok(Self { backup, user_info })
    }

    fn get_user_info(backup: &Backup) -> HashMap<String, UserDB> {
        const MATCHED_NAME: [&str; 5] = [
            "WCDB_Contact.sqlite",
            "MM.sqlite",
            "message_",
            "mmsetting.archive",
            "session.db",
        ];
        let mut user_map = HashMap::new();
        let paths = vec![
            backup.find_wildcard_paths(DOMAIN, "*/WCDB_Contact.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/MM.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/message_*.sqlite"),
            backup.find_wildcard_paths(DOMAIN, "*/mmsetting.archive"),
            backup.find_wildcard_paths(DOMAIN, "*/mmsetting.archive.*"),
            backup.find_wildcard_paths(DOMAIN, "*/session/session.db"),
        ];
        for file in paths.iter().flatten() {
            let path = Path::new(&file.relative_filename);
            if MATCHED_NAME.contains(&path.name_str())
                || path.name_str().starts_with(MATCHED_NAME[2])
                || path.name_str().starts_with(MATCHED_NAME[3])
            {
                if let Some(mut user_id) = path
                    .strip_prefix("Documents")
                    .ok()
                    .and_then(|p| p.components().next())
                    .map(|user_id| user_id.name_str().to_string())
                {
                    if user_id == "MMappedKV" {
                        user_id = if path.ext_str() == "crc" {
                            gen_md5(path.with_extension("").ext_str())
                        } else {
                            gen_md5(path.ext_str())
                        };
                        if user_id == "d41d8cd98f00b204e9800998ecf8427e" {
                            continue;
                        }
                    }
                    if let Some(user) = user_map.remove(&user_id) {
                        let user: UserDB = user;
                        user_map.insert(user_id, user.with(&backup, file));
                    } else {
                        user_map.insert(
                            user_id.clone(),
                            UserDB::new(
                                &backup,
                                user_id.clone(),
                                file,
                                backup.find_wildcard_paths(
                                    DOMAIN,
                                    &format!("Documents/{}/*", user_id),
                                ),
                            ),
                        );
                    }
                } else {
                    warn!("Unmatched path: {}", path.display());
                }
            } else {
                warn!("Unknown file name: {}", path.display());
            }
        }
        user_map
            .iter()
            .filter(|(_, user_db)| user_db.is_complete())
            .filter_map(|(user_id, user_db)| {
                let mut user = user_db.clone();
                user.build(&backup)
                    .map(|_| (user_id.clone(), user))
                    .map_err(|e| warn!("failed to init user: {}", e))
                    .ok()
            })
            .collect()
    }

    pub fn get_users(&self) -> Vec<String> {
        self.user_info.keys().cloned().collect()
    }

    pub fn get_user_db(&self, user: &str) -> Option<(&UserDB, &Backup)> {
        self.user_info.get(user).map(|db| (db, &self.backup))
    }
}

#[allow(non_camel_case_types)]
pub struct Matcher {
    extractor: Extractor,
    extract_ids: Vec<String>,
    names: Option<Vec<String>>,
    skip_resource: bool,
}

impl Matcher {
    pub fn new<P: AsRef<Path>>(path: P, names: Option<Vec<String>>) -> Result<Box<dyn MsgMatcher>> {
        let extractor = Extractor::new(path).map_err(|e| anyhow::anyhow!("{}", e))?;
        let extract_ids = extractor.get_users();
        Ok(Box::new(Self {
            extractor,
            extract_ids,
            names,
            skip_resource: false,
        }) as Box<dyn MsgMatcher>)
    }
}

fn merge_metadata(
    recorder: &SqliteChatRecorder,
    attaches: &Attachments,
    old: Vec<u8>,
    new: Vec<u8>,
) -> Option<Vec<u8>> {
    if let Ok((old, new)) = from_slice(&old)
        .map_err(|e| error!("Failed to parse old metadata: {}", e))
        .and_then(|old| {
            // 调用前已做判断，metadata必为非空
            from_slice::<AttachMetadata>(&new)
                // 新元数据是即时生成的，不应该解析错误
                .map_err(|e| panic!("Failed to parse new metadata: {}", e))
                .map(|new| (old, new))
        })
    {
        to_vec(&new.merge(recorder, attaches, old))
            .map_err(|e| error!("Failed to serialize metadata: {}", e))
            .ok()
    } else {
        Some(new)
    }
}

impl MsgMatcher for Matcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        Some(
            self.extract_ids
                .iter()
                .filter_map(|u| self.extractor.get_user_db(u))
                .flat_map(|(user_db, backup)| {
                    user_db
                        .get_record_names(self.names.clone())
                        .iter()
                        .flat_map(|name| {
                            user_db.get_records(backup, name.clone(), self.skip_resource)
                        })
                        .collect::<Vec<_>>()
                })
                .collect(),
        )
    }

    fn get_metadata_merger(&self) -> Option<SqliteMetadataMerger> {
        Some(merge_metadata)
    }
}
