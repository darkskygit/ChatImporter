use super::*;
use chrono::{NaiveDate, NaiveTime};
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use serde_json::to_vec;
use std::cmp::max;
use std::path::PathBuf;

#[derive(Clone, Serialize)]
pub enum QQMsgImage {
    Attach { name: String, data: Vec<u8> },
    Hash(i64),
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

#[allow(dead_code)]
pub struct QQPathAttachGetter;

impl QQAttachGetter for QQPathAttachGetter {}

pub struct QQMsgMatcher {
    html: Html,
    owner: String,
    file_name: String,
    attach_getter: Box<dyn QQAttachGetter>,
}

impl QQMsgMatcher {
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

    fn get_table(&self) -> Option<Vec<ElementRef>> {
        lazy_static! {
            static ref TABLE_SELECTOR: Selector = Selector::parse("body>table>tbody").unwrap();
            static ref TR_TD_SELECTOR: Selector = Selector::parse("tr>td").unwrap();
        }
        self.html
            .select(&*TABLE_SELECTOR)
            .next()
            .map(|elm| elm.select(&*TR_TD_SELECTOR).collect())
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
            .select(&*DIV_SELECTOR)
            .next()
            .and_then(|elm| Self::first_match(GROUP_MATCHER.captures(&elm.inner_html())))
            .and_then(|group_id| decode_html(&group_id).ok())
            .and_then(|group_id| {
                node[1]
                    .select(&*DIV_SELECTOR)
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
            name.select(&*INNER_DIV_SELECTOR)
                .next()
                .and_then(|elm| self.parse_sender_pm(elm, is_self))
                .and_then(|(sender, sender_id)| {
                    Self::parse_time(&name).map(|time| (sender, sender_id, time))
                })
        } else {
            name.select(&*INNER_DIV_SELECTOR)
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
        let divs = elm.select(&*DIV_SELECTOR).take(2).collect::<Vec<_>>();
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
                if !msg.images.is_empty() {
                    to_vec(
                        &msg.images
                            .iter()
                            .map(|image| match image {
                                QQMsgImage::Attach { data, .. } => {
                                    QQMsgImage::Hash(Blob::new(data.clone()).hash)
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
                                timestamp: date.and_time(time).timestamp_millis(),
                                metadata: Some(metadata),
                                ..Default::default()
                            },
                            msg.images
                                .iter()
                                .filter_map(|image| match image.clone() {
                                    QQMsgImage::Attach { data, name } => Some((name, data)),
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
                        timestamp: date.and_time(time).timestamp_millis(),
                        ..Default::default()
                    }))
                }
            } else {
                None
            }
        })
    }

    fn modify_timestamp(record_type: RecordType, near_sec: Option<i64>) -> Option<RecordType> {
        if let Some(near_sec) = near_sec {
            match record_type {
                RecordType::Record(record) => Some(RecordType::from(Record {
                    timestamp: max(near_sec, record.timestamp) + 1,
                    ..record
                })),
                RecordType::RecordRef(record) => Some(RecordType::from(Record {
                    timestamp: max(near_sec, record.timestamp) + 1,
                    ..record.clone()
                })),
                RecordType::RecordWithAttachs { record, attachs } => Some(RecordType::from((
                    Record {
                        timestamp: max(near_sec, record.timestamp) + 1,
                        ..record
                    },
                    attachs,
                ))),
                RecordType::RecordRefWithAttachs { record, attachs } => Some(RecordType::from((
                    Record {
                        timestamp: max(near_sec, record.timestamp) + 1,
                        ..record.clone()
                    },
                    attachs,
                ))),
                _ => None,
            }
        } else {
            Some(record_type)
        }
    }
}

impl MsgMatcher for QQMsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        self.get_table().and_then(|table| {
            Self::get_group_id(table.iter().take(4).collect::<Vec<_>>()).map(|(is_pm, group_id)| {
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
                        (None, Vec::<RecordType>::new()),
                        |(date, mut ret), curr| match curr {
                            Some(QQMsgLine::Date(date)) => (
                                Some(NaiveDate::parse_from_str(&date, "%Y-%m-%d").unwrap()),
                                ret,
                            ),
                            Some(line @ QQMsgLine::Message { .. }) => {
                                self.transfrom_record(group_id.clone(), date, line).map(
                                    |record_type| {
                                        record_type
                                            .get_record()
                                            .and_then(|record| {
                                                Self::modify_timestamp(
                                                    record_type.clone(),
                                                    ret.iter()
                                                        .filter_map(|r| r.get_record())
                                                        .filter(|r| {
                                                            i64::abs(r.timestamp - record.timestamp)
                                                                < 1000
                                                                && r.sender_id == record.sender_id
                                                        })
                                                        .map(|r| r.timestamp)
                                                        .max(),
                                                )
                                            })
                                            .map(|record| ret.push(record))
                                    },
                                );
                                (date, ret)
                            }
                            None => (date, ret),
                        },
                    )
                    .1
            })
        })
    }
}
