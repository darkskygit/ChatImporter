use super::*;
use chrono::{NaiveDate, NaiveTime, TimeZone};
use scraper::{element_ref::Select, ElementRef, Html, Node, Selector};
use std::path::PathBuf;

#[derive(Clone)]
enum QQMsgType {
    Content(String),
    Image(PathBuf),
}

#[derive(Clone)]
enum QQMsgLine {
    Date(String),
    Message {
        sender: String,
        sender_id: String,
        time: NaiveTime,
        msg: QQMsgType,
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
        Selector::parse("body>table>tbody").ok().and_then(|table| {
            Selector::parse("tr>td").ok().and_then(|selector| {
                self.html
                    .select(&table)
                    .next()
                    .map(|elm| elm.select(&selector).collect())
            })
        })
    }

    fn get_group_id(node: Vec<&ElementRef>) -> Option<String> {
        Regex::new("^消息对象:(.*?)$")
            .ok()
            .and_then(|regex| {
                Selector::parse("tr>td>div")
                    .map(|selector| (regex, selector))
                    .ok()
            })
            .and_then(|(regex, selector)| {
                node[2].select(&selector).next().and_then(|elm| {
                    regex
                        .captures(&elm.inner_html())
                        .and_then(|c| c.iter().skip(1).next().and_then(|i| i))
                        .map(|i| i.as_str().trim().into())
                })
            })
    }

    fn transfrom_msg_line(elm: &ElementRef) -> Option<QQMsgLine> {
        None
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
                Some(Record {
                    chat_type: "QQ".into(),
                    owner_id: self.owner.clone(),
                    group_id,
                    sender: sender_id,
                    content: match msg {
                        QQMsgType::Content(content) => content,
                        QQMsgType::Image(p) => p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .into(),
                    },
                    timestamp: date.and_time(time).timestamp_millis(),
                    ..Default::default()
                })
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
                    .fold((None, vec![]), |(date, mut ret), curr| match curr {
                        Some(QQMsgLine::Date(date)) => (
                            Some(NaiveDate::parse_from_str(&date, "2015-09-05").unwrap()),
                            ret,
                        ),
                        Some(line @ QQMsgLine::Message { .. }) => {
                            self.transfrom_record(group_id.clone(), date.clone(), line)
                                .map(|record| ret.push(record));
                            (date, ret)
                        }
                        None => (date, ret),
                    })
                    .1
            })
        })
    }
}
