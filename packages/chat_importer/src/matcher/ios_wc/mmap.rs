use super::*;

pub(super) enum MMType {
    Vec(()),
    String(String),
    SubString(String),
}

impl MMType {
    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::String(s) | Self::SubString(s) => s,
            _ => "",
        }
    }
}

pub(super) struct MMMap {
    data: Vec<u8>,
    pos: usize,
}

impl MMMap {
    pub(super) fn to_map(data: &[u8], len: Option<usize>) -> HashMap<String, MMType> {
        Self::new(data, len).into_map()
    }

    fn new(data: &[u8], len: Option<usize>) -> Self {
        Self {
            data: data[..len.unwrap_or(data.len())].into(),
            pos: 8,
        }
    }

    fn into_map(mut self) -> HashMap<String, MMType> {
        let mut map = HashMap::new();
        while let Some(k) = self.next() {
            if let MMType::String(key) = k {
                if let Some(val) = self.next() {
                    map.insert(key, val);
                }
            }
        }
        map
    }

    fn parse_pos(orig_data: &[u8], pos: usize) -> (usize, usize) {
        let size = orig_data[pos];
        if size & 0x80 == 0 {
            (size as usize, 1)
        } else {
            let splitted_data = &orig_data[pos..];
            let len = splitted_data.iter().take_while(|&u| u & 128 != 0).count() + 1;
            let len = if len >= 4 {
                4
            } else if len == 0 {
                return (0, 0);
            } else {
                len
            };
            let splitted_size = &splitted_data[..len];
            let mut size: usize = 0;
            for (i, c) in splitted_size.iter().enumerate() {
                let shift = i * 7;
                if c & 128 != 0 {
                    // More bytes are present
                    size |= (*c as usize & 127) << shift;
                } else {
                    size |= (*c as usize) << shift;
                }
            }

            (size, splitted_size.len())
        }
    }
}

impl Iterator for MMMap {
    type Item = MMType;
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.len() > self.pos {
            let (size, pos_len) = Self::parse_pos(&self.data, self.pos);
            self.pos += pos_len;
            (size > 0 && self.pos + size < self.data.len()).then(|| {
                let slice = &self.data[self.pos..self.pos + size];
                self.pos += size;

                let (sub_size, sub_pos_len) = Self::parse_pos(slice, 0);
                if sub_size > 0 && sub_pos_len + sub_size <= slice.len() {
                    from_utf8(&slice[sub_pos_len..sub_pos_len + sub_size])
                        .map(|s| MMType::SubString(s.into()))
                        .or_else(|_| from_utf8(slice).map(|s| MMType::String(s.into())))
                        .unwrap_or(MMType::Vec(()))
                } else {
                    from_utf8(slice)
                        .map(|s| MMType::String(s.into()))
                        .unwrap_or(MMType::Vec(()))
                }
            })
        } else {
            None
        }
    }
}
