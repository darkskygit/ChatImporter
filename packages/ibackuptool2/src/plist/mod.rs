use plist::Value;

pub fn decode_nskeyedarchiver(
    value: plist::Value,
) -> Result<plist::Value, Box<dyn std::error::Error>> {
    let mut rot = plist::Dictionary::new();

    // First, ensure the top-level is a dictionary.
    if let Value::Dictionary(root) = value {
        // Next, ensure that this item is actually created by NSKeyedArchiver
        if let Some(Value::String(string)) = root.get("$archiver") {
            if string != "NSKeyedArchiver" {
                return Err(invalid_archive("not built by NSKeyedArchiver"));
            }
        }

        // Resolve the top-level id as a usize index
        let top_uid = match root.get("$top") {
            Some(Value::Dictionary(dict)) => match dict.get("root") {
                Some(Value::Uid(val)) => Some(val.get() as usize),
                _ => None,
            },
            _ => None,
        };

        // Try to get the object container
        let objects = match root.get("$objects") {
            Some(Value::Array(objs)) => objs,
            _ => return Err(invalid_archive("no objects")),
        };

        // If we have a root uuid try to get it
        if let Some(root_uid) = top_uid {
            let root = objects
                .get(root_uid)
                .ok_or_else(|| invalid_archive("root uid out of bounds"))?;
            // read referenced object as dict
            let dict = root
                .as_dictionary()
                .ok_or_else(|| invalid_archive("root object is not a dictionary"))?;

            // for each key, unwrap it into it's referenced uid object or self.
            for (k, v) in dict.iter() {
                match v {
                    Value::Uid(uid) => {
                        let uid = uid.get() as usize;
                        let referenced = objects
                            .get(uid)
                            .ok_or_else(|| invalid_archive("referenced uid out of bounds"))?;
                        rot.insert(k.to_string(), referenced.clone());
                    }
                    _ => {
                        rot.insert(k.to_string(), v.clone());
                    }
                }
            }
        } else {
            return Err(invalid_archive("no root uid specified"));
        }
    } else {
        return Err(invalid_archive("malformed keyedarchiver root"));
    }

    Ok(Value::Dictionary(rot))
}

fn invalid_archive(message: &'static str) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}
