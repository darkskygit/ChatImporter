use super::*;

const XML_BYTE_LIMIT: usize = 512 * 1024;
const XML_NODE_LIMIT: u32 = 4096;

#[derive(Debug)]
pub(super) struct SafeXml<'a> {
    doc: roxmltree::Document<'a>,
}

impl<'a> SafeXml<'a> {
    pub(super) fn parse(input: &'a str) -> Result<Self> {
        if input.len() > XML_BYTE_LIMIT {
            anyhow::bail!("xml exceeds byte limit");
        }
        let upper = input.to_ascii_uppercase();
        if upper.contains("<!DOCTYPE") || upper.contains("<!ENTITY") {
            anyhow::bail!("xml doctype/entity is not allowed");
        }
        let options = roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: XML_NODE_LIMIT,
            entity_resolver: None,
        };
        let doc = roxmltree::Document::parse_with_options(input, options)?;
        Ok(Self { doc })
    }

    pub(super) fn text(&self, path: &[&str]) -> Option<String> {
        self.raw_text(path).map(|text| hex2b64(&text))
    }

    pub(super) fn raw_text(&self, path: &[&str]) -> Option<String> {
        self.node(path)
            .and_then(|node| node.text())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    pub(super) fn attr(&self, path: &[&str], name: &str) -> Option<String> {
        self.node(path)
            .and_then(|node| node.attribute(name))
            .filter(|text| !text.is_empty())
            .map(hex2b64)
    }

    #[allow(dead_code)]
    pub(super) fn nested_xml_text(&self, path: &[&str]) -> Option<String> {
        self.text(path)
            .and_then(|text| htmlescape::decode_html(&text).ok())
    }

    pub(super) fn children_attrs_and_text(
        &self,
        parent_path: &[&str],
        child_name: &str,
        attrs: &[&str],
        text_children: &[&str],
    ) -> Vec<(HashMap<String, String>, HashMap<String, String>)> {
        self.node(parent_path)
            .map(|parent| {
                parent
                    .children()
                    .filter(|node| node.is_element() && node.tag_name().name() == child_name)
                    .map(|node| {
                        let attr_values = attrs
                            .iter()
                            .filter_map(|name| {
                                node.attribute(*name)
                                    .filter(|value| !value.is_empty())
                                    .map(|value| ((*name).to_string(), hex2b64(value)))
                            })
                            .collect::<HashMap<_, _>>();
                        let text_values = text_children
                            .iter()
                            .filter_map(|name| {
                                node.children()
                                    .find(|child| {
                                        child.is_element() && child.tag_name().name() == *name
                                    })
                                    .and_then(|child| child.text())
                                    .filter(|value| !value.is_empty())
                                    .map(|value| ((*name).to_string(), hex2b64(value)))
                            })
                            .collect::<HashMap<_, _>>();
                        (attr_values, text_values)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn node(&self, path: &[&str]) -> Option<roxmltree::Node<'_, '_>> {
        let mut current = self.doc.root_element();
        let mut parts = path.iter().copied();
        if let Some(first) = parts.next() {
            if current.tag_name().name() != first {
                current = current
                    .children()
                    .find(|node| node.is_element() && node.tag_name().name() == first)?;
            }
        }
        for part in parts {
            current = current
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == part)?;
        }
        Some(current)
    }
}

pub(super) fn xml_attr(input: &str, path: &[&str], name: &str) -> Option<String> {
    SafeXml::parse(input)
        .ok()
        .and_then(|xml| xml.attr(path, name))
}

pub(super) fn xml_text(input: &str, path: &[&str]) -> Option<String> {
    SafeXml::parse(input).ok().and_then(|xml| xml.text(path))
}

pub(super) fn xml_raw_text(input: &str, path: &[&str]) -> Option<String> {
    SafeXml::parse(input)
        .ok()
        .and_then(|xml| xml.raw_text(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_xml_reads_cdata_text_and_attrs() {
        let xml = SafeXml::parse(
            r#"<msg><app title="attr"><title><![CDATA[hello]]></title></app></msg>"#,
        )
        .unwrap();

        assert_eq!(xml.text(&["msg", "app", "title"]), Some("hello".into()));
        assert_eq!(xml.attr(&["msg", "app"], "title"), Some("attr".into()));
    }

    #[test]
    fn safe_xml_rejects_doctype_and_entity() {
        assert!(SafeXml::parse("<!DOCTYPE msg><msg/>").is_err());
        assert!(SafeXml::parse("<!ENTITY x 'y'><msg/>").is_err());
    }

    #[test]
    fn safe_xml_rejects_oversized_input() {
        let body = "x".repeat(XML_BYTE_LIMIT + 1);
        assert!(SafeXml::parse(&format!("<msg>{}</msg>", body)).is_err());
    }

    #[test]
    fn safe_xml_extracts_nested_xml_text() {
        let xml =
            SafeXml::parse("<msg><recorditem>&lt;record&gt;ok&lt;/record&gt;</recorditem></msg>")
                .unwrap();

        assert_eq!(
            xml.nested_xml_text(&["msg", "recorditem"]),
            Some("<record>ok</record>".into())
        );
    }
}
