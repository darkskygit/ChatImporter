mod qq_html;
mod qq_mht;

use super::*;
use gchdb::{Blob, Record, RecordType};
use htmlescape::decode_html;
use lazy_static::lazy_static;
use regex::{Captures, Regex};

use qq_html::{QQAttachGetter, QQMsgImage};

pub use qq_html::{QQMsgMatcher, QQPathAttachGetter};
pub use qq_mht::QQMhtMsgMatcher;

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>>;
}
