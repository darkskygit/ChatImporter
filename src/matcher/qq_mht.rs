use super::*;
use mailparse::{parse_mail, MailParseError};
use std::collections::HashMap;
use std::path::PathBuf;

pub struct QQMhtMsgMatcher {
    qq_html_matcher: QQMsgMatcher,
}

impl QQMhtMsgMatcher {
    pub fn new(data: &[u8], owner: String) -> Result<Self, MailParseError> {
        let mht = parse_mail(data)?;
        let attachs = mht
            .subparts
            .iter()
            .filter_map(|part| {
                let headers = part
                    .headers
                    .iter()
                    .map(|h| (h.get_key(), h.get_value()))
                    .collect::<HashMap<_, _>>();
                headers
                    .get("Content-Location")
                    .or(Some(&"__main__".into()))
                    .and_then(|name| part.get_body_raw().map(|data| (name.clone(), data)).ok())
            })
            .collect::<HashMap<_, _>>();
        attachs
            .get("__main__")
            .and_then(|data| String::from_utf8(data.clone()).ok())
            .map(|html| QQMsgMatcher::new(html, owner, QQMhtAttachGetter::new(attachs.clone())))
            .map(|qq_html_matcher| Self { qq_html_matcher })
            .ok_or(MailParseError::Generic("test"))
    }
}

impl MsgMatcher for QQMhtMsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        self.qq_html_matcher.get_records()
    }
}

struct QQMhtAttachGetter {
    attachs: HashMap<String, Vec<u8>>,
}

impl QQMhtAttachGetter {
    pub fn new(attachs: HashMap<String, Vec<u8>>) -> Self {
        Self { attachs }
    }
}

impl QQAttachGetter for QQMhtAttachGetter {
    fn get_attach(&self, path: &str) -> QQMsgImage {
        let name = PathBuf::from(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        self.attachs
            .get(&name)
            .map(|data| QQMsgImage::Attach {
                data: data.clone(),
                name: name.clone(),
            })
            .unwrap_or_else(|| QQMsgImage::UnmatchName(name))
    }
}
