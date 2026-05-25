use super::*;
use mailparse::{parse_mail, MailParseError};
use std::collections::HashMap;
use std::path::PathBuf;
use win_qq_html::{Extractor, QQAttachGetter, QQMsgImage};

pub struct Matcher {
    qq_html_matcher: Extractor,
}

impl Matcher {
    pub fn from_mht(
        data: &[u8],
        owner: String,
        file_name: String,
    ) -> Result<Box<dyn MsgMatcher>, MailParseError> {
        info!("Parsing mht...");
        let mht = parse_mail(data)?;
        let attaches = mht
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
        attaches
            .get("__main__")
            .and_then(|data| String::from_utf8(data.clone()).ok())
            .map(|html| Extractor::new(html, owner, file_name, AttachGetter::new(attaches.clone())))
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
    attaches: HashMap<String, Vec<u8>>,
}

impl AttachGetter {
    pub fn new(attaches: HashMap<String, Vec<u8>>) -> Self {
        Self { attaches }
    }
}

impl QQAttachGetter for AttachGetter {
    fn get_attach(&self, path: &str) -> QQMsgImage {
        let name = PathBuf::from(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        self.attaches
            .get(&name)
            .map(|data| QQMsgImage::Attach {
                data: data.clone(),
                name: name.clone(),
            })
            .unwrap_or_else(|| QQMsgImage::UnmatchName(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn windows_qq_mht_minimal_sample() {
        let html = r#"<html><body><table><tbody>
            <tr><td></td></tr>
            <tr><td><div>消息分组:联系人</div></td></tr>
            <tr><td><div>消息对象:Bob(456)</div></td></tr>
            <tr><td></td></tr>
            <tr><td>日期: 2024-01-01</td></tr>
            <tr><td><div><div>Alice(123)</div> 12:34:56</div><div>hello<img src="pic.png"></div></td></tr>
        </tbody></table></body></html>"#;
        let mht = format!(
            "Content-Type: multipart/related; boundary=\"BOUNDARY\"\r\n\r\n\
             --BOUNDARY\r\nContent-Location: __main__\r\n\r\n{}\r\n\
             --BOUNDARY\r\nContent-Location: pic.png\r\n\r\nqq-image\r\n\
             --BOUNDARY--\r\n",
            html
        );
        let matcher = Matcher::from_mht(mht.as_bytes(), "owner".into(), "Bob(456)".into()).unwrap();
        let records = matcher.get_records().unwrap();
        assert_eq!(records.len(), 1);
        let record = records[0].get_record();
        assert_eq!(record.content, "hello<img>");
        match &records[0] {
            RecordType::RecordWithAttachments { attachments, .. } => {
                assert_eq!(attachments.get("pic.png").unwrap(), b"qq-image\r\n");
            }
            _ => panic!("expected attachment record"),
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Bob(456).mht");
        std::fs::write(&file, mht).unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        exporter(&mut store, ExportType::WindowsQQ(&file, "owner".into()))
            .await
            .unwrap();
        let stored = store.query(crate::store::Query::default()).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "hello<img>");
        assert!(stored[0].metadata.is_some());
    }
}
