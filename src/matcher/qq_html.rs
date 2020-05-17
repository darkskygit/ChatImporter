use super::*;
use chrono::{NaiveDate, NaiveTime};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::to_vec;
use std::cmp::max;
use std::path::PathBuf;

#[derive(Clone)]
struct QQMsg {
    content: String,
    images: Vec<String>,
}

#[derive(Clone)]
enum QQMsgLine {
    Date(String),
    Message {
        sender: String,
        sender_id: String,
        time: NaiveTime,
        msg: QQMsg,
    },
}

pub struct QQMsgMatcher {
    html: Html,
    owner: String,
}

impl QQMsgMatcher {
    pub fn new(html: String, owner: String) -> Self {
        Self {
            html: Html::parse_document(&html),
            owner,
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

    fn first_match<'t>(captures: Option<Captures<'t>>) -> Option<String> {
        captures
            .and_then(|c| c.iter().skip(1).next().and_then(|i| i))
            .map(|i| i.as_str().trim().into())
    }

    fn get_group_id(node: Vec<&ElementRef>) -> Option<String> {
        lazy_static! {
            static ref DIV_SELECTOR: Selector = Selector::parse("tr>td>div").unwrap();
            static ref GROUP_MATCHER: Regex = Regex::new("^消息对象:(.*?)$").unwrap();
        }
        node[2]
            .select(&*DIV_SELECTOR)
            .next()
            .and_then(|elm| Self::first_match(GROUP_MATCHER.captures(&elm.inner_html())))
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
                        .filter_map(|i| i.clone())
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

    fn parse_time(name: &ElementRef) -> Option<NaiveTime> {
        name.children()
            .skip(1)
            .next()
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

    fn process_name(name: ElementRef) -> Option<(String, String, NaiveTime)> {
        lazy_static! {
            static ref INNER_DIV_SELECTOR: Selector = Selector::parse("tr>td>div>div").unwrap();
        }
        name.select(&*INNER_DIV_SELECTOR)
            .next()
            .and_then(Self::parse_sender)
            .and_then(|(sender, sender_id)| {
                Self::parse_time(&name).map(|time| (sender, sender_id, time))
            })
    }

    fn process_msg(content: ElementRef) -> Option<QQMsg> {
        lazy_static! {
            static ref FONT_REPLACER: Regex = Regex::new("<font .*?>(?P<text>.*?)</font>").unwrap();
            static ref B_REPLACER: Regex = Regex::new("<b>(?P<text>.*?)</b>").unwrap();
            static ref I_REPLACER: Regex = Regex::new("<i>(?P<text>.*?)</i>").unwrap();
            static ref U_REPLACER: Regex = Regex::new("<u>(?P<text>.*?)</u>").unwrap();
            static ref IMG_REPLACER: Regex = Regex::new(r#"<img src="(?P<img>.*?)">"#).unwrap();
        }
        let decoded = decode_html(&content.inner_html()).unwrap_or(content.inner_html());
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
                .map(|c| {
                    PathBuf::from(c["img"].trim())
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(c["img"].trim())
                        .into()
                })
                .collect(),
        })
    }

    fn transfrom_msg_line(elm: &ElementRef) -> Option<QQMsgLine> {
        lazy_static! {
            static ref DATE_MATCHER: Regex = Regex::new("^日期: (.*?)$").unwrap();
            static ref DIV_SELECTOR: Selector = Selector::parse("tr>td>div").unwrap();
        }
        let divs = elm.select(&*DIV_SELECTOR).take(2).collect::<Vec<_>>();
        if let &[name, content] = divs.as_slice() {
            Self::process_name(name).and_then(|(sender, sender_id, time)| {
                Self::process_msg(content).map(|msg| QQMsgLine::Message {
                    sender,
                    sender_id,
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
    ) -> Option<Record> {
        date.and_then(|date| {
            if let QQMsgLine::Message {
                sender,
                sender_id,
                time,
                msg,
            } = line
            {
                if msg.images.len() > 0 {
                    to_vec(&msg.images).ok().and_then(|metadata| {
                        Some(Record {
                            chat_type: "QQ".into(),
                            owner_id: self.owner.clone(),
                            group_id,
                            sender: sender_id,
                            content: msg.content,
                            timestamp: date.and_time(time).timestamp_millis(),
                            metadata: Some(metadata),
                            ..Default::default()
                        })
                    })
                } else {
                    Some(Record {
                        chat_type: "QQ".into(),
                        owner_id: self.owner.clone(),
                        group_id,
                        sender: sender_id,
                        content: msg.content,
                        timestamp: date.and_time(time).timestamp_millis(),
                        ..Default::default()
                    })
                }
            } else {
                None
            }
        })
    }
}

impl MsgMatcher for QQMsgMatcher {
    fn get_records(&self) -> Option<Vec<Record>> {
        self.get_table().and_then(|table| {
            Self::get_group_id(table.iter().take(4).collect::<Vec<_>>()).map(|group_id| {
                table
                    .iter()
                    .skip(4)
                    .map(Self::transfrom_msg_line)
                    .fold(
                        (None, Vec::<Record>::new()),
                        |(date, mut ret), curr| match curr {
                            Some(QQMsgLine::Date(date)) => (
                                Some(NaiveDate::parse_from_str(&date, "%Y-%m-%d").unwrap()),
                                ret,
                            ),
                            Some(line @ QQMsgLine::Message { .. }) => {
                                self.transfrom_record(group_id.clone(), date.clone(), line)
                                    .map(|mut record| {
                                        if let Some(some_sec) = ret
                                            .iter()
                                            .filter(|r| {
                                                i64::abs(r.timestamp - record.timestamp) < 1000
                                                    && r.sender == record.sender
                                            })
                                            .map(|r| r.timestamp)
                                            .max()
                                        {
                                            record.timestamp = max(some_sec, record.timestamp) + 1;
                                        }
                                        ret.push(record)
                                    });
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
