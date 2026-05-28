use super::*;
use chrono::{NaiveDate, NaiveTime};
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use serde_json::to_vec;
use std::path::PathBuf;

#[derive(Clone, Serialize)]
pub enum QQMsgImage {
    Attach { name: String, data: Vec<u8> },
    Hash(String),
    UnmatchName(String),
}

#[derive(Clone)]
struct QQMsg {
    content: String,
    images: Vec<QQMsgImage>,
}

#[derive(Clone)]
enum QQMsgLine {
    Date(String),
    Message {
        sender_id: String,
        sender_name: String,
        time: NaiveTime,
        msg: QQMsg,
    },
}

pub trait QQAttachGetter {
    fn get_attach(&self, path: &str) -> QQMsgImage {
        QQMsgImage::UnmatchName(
            PathBuf::from(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .into(),
        )
    }
}

pub struct Extractor {
    html: Html,
    owner: String,
    file_name: String,
    attach_getter: Box<dyn QQAttachGetter>,
}

impl Extractor {
    pub fn new<A>(html: String, owner: String, file_name: String, attach_getter: A) -> Self
    where
        A: 'static + QQAttachGetter,
    {
        Self {
            html: Html::parse_document(&html),
            owner,
            file_name,
            attach_getter: Box::new(attach_getter),
        }
    }

    fn get_table(&self) -> Option<Vec<ElementRef<'_>>> {
        lazy_static! {
            static ref TABLE_SELECTOR: Selector = Selector::parse("body>table>tbody").unwrap();
            static ref TR_TD_SELECTOR: Selector = Selector::parse("tr>td").unwrap();
        }
        self.html
            .select(&TABLE_SELECTOR)
            .next()
            .map(|elm| elm.select(&TR_TD_SELECTOR).collect())
    }

    fn first_match(captures: Option<Captures<'_>>) -> Option<String> {
        captures
            .and_then(|c| c.iter().nth(1).and_then(|i| i))
            .map(|i| i.as_str().trim().into())
    }

    fn get_group_id(node: Vec<&ElementRef>) -> Option<(bool, String)> {
        lazy_static! {
            static ref DIV_SELECTOR: Selector = Selector::parse("tr>td>div").unwrap();
            static ref TYPE_MATCHER: Regex = Regex::new("^消息分组:(.*?)$").unwrap();
            static ref GROUP_MATCHER: Regex = Regex::new("^消息对象:(.*?)$").unwrap();
        }
        node[2]
            .select(&DIV_SELECTOR)
            .next()
            .and_then(|elm| Self::first_match(GROUP_MATCHER.captures(&elm.inner_html())))
            .and_then(|group_id| decode_html(&group_id).ok())
            .and_then(|group_id| {
                node[1]
                    .select(&DIV_SELECTOR)
                    .next()
                    .and_then(|elm| Self::first_match(TYPE_MATCHER.captures(&elm.inner_html())))
                    .map(|group_type| {
                        (
                            group_type.contains("联系人") || group_type.contains("临时会话"),
                            group_id,
                        )
                    })
            })
    }

    fn parse_sender(elm: ElementRef) -> Option<(String, String)> {
        lazy_static! {
            static ref NAME_MATCHER: Regex = Regex::new(r"^(.*?)[<\(](.*?)[>\)]$").unwrap();
        }
        match decode_html(&elm.inner_html()) {
            Ok(decoded) => NAME_MATCHER
                .captures(&decoded.replace("&get;", ">"))
                .and_then(|c| {
                    match c
                        .iter()
                        .skip(1)
                        .take(2)
                        .flatten()
                        .map(|i| i.as_str().trim().to_string())
                        .collect::<Vec<_>>()
                        .as_slice()
                    {
                        [sender, sender_id] => Some((sender.clone(), sender_id.clone())),
                        _ => None,
                    }
                })
                .or_else(|| {
                    error!("Failed to parse name line: {}", decoded);
                    None
                }),
            Err(e) => {
                warn!("Failed to decode Html: {:?}", e);
                None
            }
        }
    }

    fn parse_sender_pm(&self, elm: ElementRef, is_self: bool) -> Option<(String, String)> {
        lazy_static! {
            static ref NAME_MATCHER: Regex = Regex::new(r"^(.*?)[<\(](.*?)[>\)]$").unwrap();
        }
        match decode_html(&elm.inner_html()) {
            Ok(decoded) => {
                if is_self {
                    Some((decoded, self.owner.clone()))
                } else {
                    NAME_MATCHER
                        .captures(&self.file_name)
                        .and_then(|c| {
                            match c
                                .iter()
                                .skip(1)
                                .take(2)
                                .flatten()
                                .map(|i| i.as_str().trim().to_string())
                                .collect::<Vec<_>>()
                                .as_slice()
                            {
                                [_, sender_id] => Some((decoded.clone(), sender_id.clone())),
                                _ => None,
                            }
                        })
                        .or_else(|| {
                            error!("Failed to parse name line: {}", decoded);
                            None
                        })
                }
            }
            Err(e) => {
                warn!("Failed to decode Html: {:?}", e);
                None
            }
        }
    }

    fn parse_time(name: &ElementRef) -> Option<NaiveTime> {
        name.children()
            .nth(1)
            .and_then(|nodereef| match nodereef.value() {
                Node::Text(text) => Some(text),
                _ => None,
            })
            .and_then(|time| match NaiveTime::parse_from_str(time, "%H:%M:%S") {
                Ok(time) => Some(time),
                Err(e) => {
                    warn!("Failed to parse time: {}", e);
                    None
                }
            })
    }

    fn process_name(&self, name: ElementRef, is_pm: bool) -> Option<(String, String, NaiveTime)> {
        lazy_static! {
            static ref INNER_DIV_SELECTOR: Selector = Selector::parse("tr>td>div>div").unwrap();
            static ref DIV_SELECTOR: Selector = Selector::parse("tr>td").unwrap();
            static ref DIV_STYLE: Regex = Regex::new("(#.*?);").unwrap();
        }
        if is_pm {
            let is_self = name
                .parent()
                .and_then(ElementRef::wrap)
                .and_then(|elm| Self::first_match(DIV_STYLE.captures(&elm.inner_html())))
                .map(|color| color == "#42B475")
                .unwrap_or(false);
            name.select(&INNER_DIV_SELECTOR)
                .next()
                .and_then(|elm| self.parse_sender_pm(elm, is_self))
                .and_then(|(sender, sender_id)| {
                    Self::parse_time(&name).map(|time| (sender, sender_id, time))
                })
        } else {
            name.select(&INNER_DIV_SELECTOR)
                .next()
                .and_then(Self::parse_sender)
                .and_then(|(sender, sender_id)| {
                    Self::parse_time(&name).map(|time| (sender, sender_id, time))
                })
        }
    }

    fn convert_image(&self, path: &str) -> QQMsgImage {
        self.attach_getter.get_attach(path)
    }

    fn process_msg(&self, content: ElementRef) -> Option<QQMsg> {
        lazy_static! {
            static ref FONT_REPLACER: Regex = Regex::new("<font .*?>(?P<text>.*?)</font>").unwrap();
            static ref B_REPLACER: Regex = Regex::new("<b>(?P<text>.*?)</b>").unwrap();
            static ref I_REPLACER: Regex = Regex::new("<i>(?P<text>.*?)</i>").unwrap();
            static ref U_REPLACER: Regex = Regex::new("<u>(?P<text>.*?)</u>").unwrap();
            static ref IMG_REPLACER: Regex = Regex::new(r#"<img src="(?P<img>.*?)">"#).unwrap();
        }
        let decoded = decode_html(&content.inner_html()).unwrap_or_else(|_| content.inner_html());
        Some(QQMsg {
            content: [
                (&*FONT_REPLACER, "$text"),
                (&*B_REPLACER, "$text"),
                (&*I_REPLACER, "$text"),
                (&*U_REPLACER, "$text"),
                (&*IMG_REPLACER, "<img>"),
            ]
            .iter()
            .fold(decoded.clone(), |content, (matcher, rep)| {
                matcher.replace_all(&content, *rep).into()
            }),
            images: IMG_REPLACER
                .captures_iter(&decoded)
                .map(|c| self.convert_image(c["img"].trim()))
                .collect(),
        })
    }

    fn transfrom_msg_line(&self, elm: &ElementRef, is_pm: bool) -> Option<QQMsgLine> {
        lazy_static! {
            static ref DATE_MATCHER: Regex = Regex::new("^日期: (.*?)$").unwrap();
            static ref DIV_SELECTOR: Selector = Selector::parse("tr>td>div").unwrap();
        }
        let divs = elm.select(&DIV_SELECTOR).take(2).collect::<Vec<_>>();
        if let [name, content] = *divs.as_slice() {
            self.process_name(name, is_pm)
                .and_then(|(sender_name, sender_id, time)| {
                    self.process_msg(content).map(|msg| QQMsgLine::Message {
                        sender_id,
                        sender_name,
                        time,
                        msg,
                    })
                })
        } else {
            Self::first_match(DATE_MATCHER.captures(&elm.inner_html())).map(QQMsgLine::Date)
        }
    }

    fn transfrom_record(
        &self,
        group_id: String,
        date: Option<NaiveDate>,
        duplicate_index: usize,
        line: QQMsgLine,
    ) -> Option<RecordType> {
        date.and_then(|date| {
            if let QQMsgLine::Message {
                sender_id,
                sender_name,
                time,
                msg,
            } = line
            {
                let timestamp = date.and_time(time).and_utc().timestamp_millis();
                let source_group_id = group_id.clone();
                let source_message_id = qq_source_message_id(
                    &group_id,
                    &sender_id,
                    timestamp,
                    &msg.content,
                    &msg.images,
                    duplicate_index,
                );
                if !msg.images.is_empty() {
                    to_vec(
                        &msg.images
                            .iter()
                            .map(|image| match image {
                                QQMsgImage::Attach { data, .. } => {
                                    QQMsgImage::Hash(Hash32::sha3_256(data).to_hex())
                                }
                                other => other.clone(),
                            })
                            .collect::<Vec<_>>(),
                    )
                    .ok()
                    .map(|metadata| {
                        RecordType::from((
                            Record {
                                chat_type: "QQ".into(),
                                owner_id: self.owner.clone(),
                                group_id,
                                sender_id,
                                sender_name,
                                content: msg.content,
                                timestamp,
                                metadata: Some(metadata),
                                source_kind: Some("windows-qq-mht".into()),
                                source_group_id: Some(source_group_id),
                                source_message_id: Some(source_message_id),
                                source_backup_id: Some(self.file_name.clone()),
                                ..Default::default()
                            },
                            msg.images
                                .iter()
                                .filter_map(|image| match image.clone() {
                                    QQMsgImage::Attach { data, name } => {
                                        Some((name, Attachment::from_bytes(data)))
                                    }
                                    _ => None,
                                })
                                .collect(),
                        ))
                    })
                } else {
                    Some(RecordType::from(Record {
                        chat_type: "QQ".into(),
                        owner_id: self.owner.clone(),
                        group_id,
                        sender_id,
                        sender_name,
                        content: msg.content,
                        timestamp,
                        source_kind: Some("windows-qq-mht".into()),
                        source_group_id: Some(source_group_id),
                        source_message_id: Some(source_message_id),
                        source_backup_id: Some(self.file_name.clone()),
                        ..Default::default()
                    }))
                }
            } else {
                None
            }
        })
    }
}

impl MsgMatcher for Extractor {
    fn import_plan(&self) -> ImportPlan {
        ImportPlan {
            chats_total: Some(1),
        }
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
        let records = self
            .get_table()
            .and_then(|table| {
                Self::get_group_id(table.iter().take(4).collect::<Vec<_>>()).map(
                    |(is_pm, group_id)| {
                        let group_id = if is_pm || group_id != "0" {
                            group_id
                        } else {
                            self.file_name.clone()
                        };
                        table
                            .iter()
                            .skip(4)
                            .map(|elm| self.transfrom_msg_line(elm, is_pm))
                            .fold(
                                (None, Vec::<String>::new(), Vec::<RecordType>::new()),
                                |(date, mut seen_source_ids, mut ret), curr| match curr {
                                    Some(QQMsgLine::Date(date)) => (
                                        Some(NaiveDate::parse_from_str(&date, "%Y-%m-%d").unwrap()),
                                        seen_source_ids,
                                        ret,
                                    ),
                                    Some(line @ QQMsgLine::Message { .. }) => {
                                        let duplicate_index = qq_duplicate_index(
                                            &group_id,
                                            date,
                                            &line,
                                            &seen_source_ids,
                                        );
                                        self.transfrom_record(
                                            group_id.clone(),
                                            date,
                                            duplicate_index,
                                            line,
                                        )
                                        .map(
                                            |record_type| {
                                                let current = record_type.get_record();
                                                if let Some(source_message_id) =
                                                    current.source_message_id.clone()
                                                {
                                                    seen_source_ids.push(source_message_id);
                                                }
                                                let record = modify_timestamp(
                                                    record_type.clone(),
                                                    ret.iter()
                                                        .map(|r| r.get_record())
                                                        .filter(|r| {
                                                            i64::abs(
                                                                r.timestamp - current.timestamp,
                                                            ) < 1000
                                                                && r.sender_id == current.sender_id
                                                        })
                                                        .map(|r| r.timestamp)
                                                        .max(),
                                                );
                                                record.map(|record| ret.push(record))
                                            },
                                        );
                                        (date, seen_source_ids, ret)
                                    }
                                    None => (date, seen_source_ids, ret),
                                },
                            )
                            .2
                    },
                )
            })
            .context("Cannot transform QQ records")?;
        progress.chat_parsed(records.len() as u64, record_blob_count(&records) as u64);
        Ok(vec![RecordBatch { records }])
    }
}

fn qq_duplicate_index(
    group_id: &str,
    date: Option<NaiveDate>,
    line: &QQMsgLine,
    seen_source_ids: &[String],
) -> usize {
    let Some(date) = date else {
        return 0;
    };
    let QQMsgLine::Message {
        sender_id,
        time,
        msg,
        ..
    } = line
    else {
        return 0;
    };
    let timestamp = date.and_time(*time).and_utc().timestamp_millis();
    let mut duplicate_index = 0;
    loop {
        let candidate = qq_source_message_id(
            group_id,
            sender_id,
            timestamp,
            &msg.content,
            &msg.images,
            duplicate_index,
        );
        if !seen_source_ids.iter().any(|seen| seen == &candidate) {
            return duplicate_index;
        }
        duplicate_index += 1;
    }
}

fn qq_source_message_id(
    group_id: &str,
    sender_id: &str,
    timestamp: i64,
    content: &str,
    images: &[QQMsgImage],
    duplicate_index: usize,
) -> String {
    // Most QQ MHT exports do not expose a durable message id. The duplicate index is only
    // used to distinguish exact same-content repeats inside one export; it is not based on
    // the file name or raw line number, so exporting the same chat again still de-dupes.
    let content_hash = Hash32::sha3_256(content.as_bytes()).to_hex();
    let attachment_hash = qq_attachment_fingerprint(images);
    format!(
        "qq:{}:{}:{}:{}:{}:{}",
        group_id, sender_id, timestamp, content_hash, attachment_hash, duplicate_index
    )
}

fn qq_attachment_fingerprint(images: &[QQMsgImage]) -> String {
    let mut hashes = images
        .iter()
        .map(|image| match image {
            QQMsgImage::Attach { data, .. } => Hash32::sha3_256(data).to_hex(),
            QQMsgImage::Hash(hash) | QQMsgImage::UnmatchName(hash) => hash.clone(),
        })
        .collect::<Vec<_>>();
    hashes.sort();
    Hash32::sha3_256(hashes.join("\n").as_bytes()).to_hex()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChatStore, Query, RecordType};
    use tempfile::tempdir;

    struct TestAttachGetter;

    impl QQAttachGetter for TestAttachGetter {
        fn get_attach(&self, path: &str) -> QQMsgImage {
            QQMsgImage::Attach {
                name: path.into(),
                data: b"qq-image".to_vec(),
            }
        }
    }

    #[tokio::test]
    async fn windows_qq_html_minimal_sample() {
        let html = r#"
        <html><body><table><tbody>
            <tr><td></td></tr>
            <tr><td><div>消息分组:联系人</div></td></tr>
            <tr><td><div>消息对象:Bob(456)</div></td></tr>
            <tr><td></td></tr>
            <tr><td>日期: 2024-01-01</td></tr>
            <tr><td>
                <div><div>Alice(123)</div> 12:34:56</div>
                <div>hello<img src="pic.png"></div>
            </td></tr>
        </tbody></table></body></html>
        "#;
        let extractor = Extractor::new(
            html.into(),
            "owner".into(),
            "Bob(456)".into(),
            TestAttachGetter,
        );
        let records = extractor.collect_records().unwrap();
        assert_eq!(records.len(), 1);
        let record = records[0].get_record();
        assert_eq!(record.chat_type, "QQ");
        assert_eq!(record.group_id, "Bob(456)");
        assert_eq!(record.sender_id, "456");
        assert_eq!(record.content, "hello<img>");
        assert_eq!(record.source_kind.as_deref(), Some("windows-qq-mht"));
        assert_eq!(record.source_group_id.as_deref(), Some("Bob(456)"));
        assert!(record
            .source_message_id
            .as_deref()
            .unwrap()
            .starts_with("qq:Bob(456):456:1704112496000:"));
        let metadata = String::from_utf8(record.metadata.clone().unwrap()).unwrap();
        assert!(metadata.contains(&Hash32::sha3_256(b"qq-image").to_hex()));
        match &records[0] {
            RecordType::RecordWithAttachments { attachments, .. } => {
                assert_eq!(attachments.get("pic.png").unwrap().bytes(), b"qq-image");
            }
            _ => panic!("expected attachment record"),
        }

        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        crate::matcher::export_matcher(&mut store, &test_progress(), &extractor)
            .await
            .unwrap();
        let stored = store.query(Query::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].chat_type, "QQ");
        assert_eq!(stored[0].group_id, "Bob(456)");
        assert_eq!(stored[0].content, "hello<img>");
        match &records[0] {
            RecordType::RecordWithAttachments { attachments, .. } => {
                let hash = Hash32::sha3_256(attachments.get("pic.png").unwrap().bytes());
                assert_eq!(
                    store.get_asset(hash).await.unwrap().unwrap(),
                    b"qq-image".to_vec()
                );
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn windows_qq_same_sender_second_collision_uses_source_message_id() {
        let html = r#"
        <html><body><table><tbody>
            <tr><td></td></tr>
            <tr><td><div>消息分组:联系人</div></td></tr>
            <tr><td><div>消息对象:Bob(456)</div></td></tr>
            <tr><td></td></tr>
            <tr><td>日期: 2024-01-01</td></tr>
            <tr><td><div><div>Alice(123)</div> 12:34:56</div><div>first</div></td></tr>
            <tr><td><div><div>Alice(123)</div> 12:34:56</div><div>second</div></td></tr>
        </tbody></table></body></html>
        "#;
        let extractor = Extractor::new(
            html.into(),
            "owner".into(),
            "Bob(456)".into(),
            TestAttachGetter,
        );
        let records = extractor.collect_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_ne!(
            records[0].get_record().source_message_id,
            records[1].get_record().source_message_id
        );

        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        crate::matcher::export_matcher(&mut store, &test_progress(), &extractor)
            .await
            .unwrap();
        let stored = store.query(Query::default()).await.unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn windows_qq_same_export_different_file_name_dedupes() {
        let html = r#"
        <html><body><table><tbody>
            <tr><td></td></tr>
            <tr><td><div>消息分组:联系人</div></td></tr>
            <tr><td><div>消息对象:Bob(456)</div></td></tr>
            <tr><td></td></tr>
            <tr><td>日期: 2024-01-01</td></tr>
            <tr><td>
                <div><div>Alice(123)</div> 12:34:56</div>
                <div>hello<img src="pic.png"></div>
            </td></tr>
        </tbody></table></body></html>
        "#;
        let first = Extractor::new(
            html.into(),
            "owner".into(),
            "Bob(456)".into(),
            TestAttachGetter,
        );
        let second = Extractor::new(
            html.into(),
            "owner".into(),
            "OtherName(456)".into(),
            TestAttachGetter,
        );
        assert_eq!(
            first.collect_records().unwrap()[0]
                .get_record()
                .source_message_id,
            second.collect_records().unwrap()[0]
                .get_record()
                .source_message_id
        );

        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        crate::matcher::export_matcher(&mut store, &test_progress(), &first)
            .await
            .unwrap();
        crate::matcher::export_matcher(&mut store, &test_progress(), &second)
            .await
            .unwrap();
        let stored = store.query(Query::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
    }
}
