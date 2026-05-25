use super::message::RecordLine;
use super::xml::{xml_attr, SafeXml};
use super::*;

fn xml_metadata(message: &str, error: &'static str) -> IosWcMetadata {
    if SafeXml::parse(message).is_err() {
        IosWcMetadata::new().with_parse_error(error)
    } else {
        IosWcMetadata::new()
    }
}

pub(super) fn parse_contact_share(line: &RecordLine) -> IosWcMetadata {
    [
        xml_attr(&line.message, &["msg"], "nickname").map(|v| ("nickname", v)),
        xml_attr(&line.message, &["msg"], "username").map(|v| ("username", v)),
        xml_attr(&line.message, &["msg"], "city").map(|v| ("city", v)),
        xml_attr(&line.message, &["msg"], "province").map(|v| ("province", v)),
        xml_attr(&line.message, &["msg"], "openimdesc").map(|v| ("openimdesc", v)),
        xml_attr(&line.message, &["msg"], "bigheadimgurl")
            .or_else(|| xml_attr(&line.message, &["msg"], "smallheadimgurl"))
            .map(|v| ("head", v)),
    ]
    .iter()
    .filter_map(|e| e.as_ref())
    .fold(
        xml_metadata(&line.message, "invalid contact xml"),
        |metadata, (k, v)| metadata.with_tag(k.to_string(), v.into()),
    )
}

pub(super) fn parse_emoji(line: &RecordLine) -> IosWcMetadata {
    [
        xml_attr(&line.message, &["msg", "emoji"], "md5").map(|v| ("md5", v)),
        xml_attr(&line.message, &["msg", "emoji"], "cdnurl").map(|v| ("cdn", v)),
        xml_attr(&line.message, &["msg", "emoji"], "aeskey").map(|v| ("key", v)),
        xml_attr(&line.message, &["msg", "emoji"], "encrypturl").map(|v| ("enc", v)),
        xml_attr(&line.message, &["msg", "emoji"], "externurl").map(|v| ("extern", v)),
    ]
    .iter()
    .filter_map(|e| e.as_ref())
    .fold(
        xml_metadata(&line.message, "invalid emoji xml"),
        |metadata, (k, v)| metadata.with_tag(k.to_string(), v.into()),
    )
}

pub(super) fn parse_location(line: &RecordLine) -> IosWcMetadata {
    [
        xml_attr(&line.message, &["msg", "location"], "label").map(|v| ("label", v)),
        xml_attr(&line.message, &["msg", "location"], "poiname").map(|v| ("name", v)),
    ]
    .iter()
    .filter_map(|e| e.as_ref())
    .fold(
        [
            xml_attr(&line.message, &["msg", "location"], "x").map(|v| ("x", v)),
            xml_attr(&line.message, &["msg", "location"], "y").map(|v| ("y", v)),
        ]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(
            xml_metadata(&line.message, "invalid location xml"),
            |metadata, (k, v)| metadata.with_float(k.to_string(), v.into()),
        ),
        |metadata, (k, v)| metadata.with_tag(k.to_string(), v.into()),
    )
}

pub(super) fn parse_voip_status(line: &RecordLine) -> IosWcMetadata {
    [xml_attr(&line.message, &["msg"], "msgContent").map(|v| ("content", v))]
        .iter()
        .filter_map(|e| e.as_ref())
        .fold(
            xml_metadata(&line.message, "invalid voip xml"),
            |metadata, (k, v)| metadata.with_tag(k.to_string(), v.into()),
        )
}
