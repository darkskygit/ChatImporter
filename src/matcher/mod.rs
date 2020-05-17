mod qq_html;

use super::*;
use regex::Regex;

pub use qq_html::QQMsgMatcher;

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<Record>>;
}
