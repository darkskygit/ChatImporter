use super::*;
use mailparse::{parse_mail, MailParseError};
use std::collections::HashMap;
use std::path::PathBuf;
use windows_qq_html::{Extractor, QQAttachGetter, QQMsgImage};

pub struct Matcher {
    qq_html_matcher: Extractor,
}

impl Matcher {
    pub fn new(
        data: &[u8],
        owner: String,
        file_name: String,
    ) -> Result<Box<dyn MsgMatcher>, MailParseError> {
        info!("Parsing mht...");
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
            .map(|html| Extractor::new(html, owner, file_name, AttachGetter::new(attachs.clone())))
            .map(|qq_html_matcher| Box::new(Self { qq_html_matcher }) as Box<dyn MsgMatcher>)
            .ok_or(MailParseError::Generic("test"))
    }
}

impl MsgMatcher for Matcher {
    fn get_records(&self) -> Option<Vec<RecordType>> {
        self.qq_html_matcher.get_records()
    }
}

struct AttachGetter {
    attachs: HashMap<String, Vec<u8>>,
}

impl AttachGetter {
    pub fn new(attachs: HashMap<String, Vec<u8>>) -> Self {
        Self { attachs }
    }
}

impl QQAttachGetter for AttachGetter {
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
