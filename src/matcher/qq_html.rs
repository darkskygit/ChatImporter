use super::*;
use scraper::{element_ref::Select, ElementRef, Html, Node, Selector};

enum QQMsgLine {}

pub struct QQMsgMatcher(Html);

impl QQMsgMatcher {
    pub fn new(html: String) -> Self {
        Self(Html::parse_document(&html))
    }

    fn get_table(&self) -> Option<Vec<ElementRef>> {
        Selector::parse("body>table>tbody").ok().and_then(|table| {
            Selector::parse("tr>td").ok().and_then(|selector| {
                self.0
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
}

impl MsgMatcher for QQMsgMatcher {
    fn get_records(&self) -> Vec<Record> {
        let table = self.get_table().unwrap();

        println!(
            "{:?}",
            Self::get_group_id(table.iter().take(4).collect::<Vec<_>>())
        );
        todo!()
    }
}
