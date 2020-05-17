mod qq_html;

use super::*;
use htmlescape::decode_html;
use lazy_static::lazy_static;
use regex::{Captures, Regex};

pub use qq_html::QQMsgMatcher;

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<Record>>;
}
