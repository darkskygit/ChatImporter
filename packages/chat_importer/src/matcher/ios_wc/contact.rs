use super::*;
use prost::Message;

#[derive(Clone, PartialEq, prost::Message)]
struct ContactRemarkProto {
    #[prost(string, tag = "1")]
    nickname: String,
    #[prost(string, tag = "2")]
    alias: String,
    #[prost(string, tag = "3")]
    remark: String,
    #[prost(string, tag = "4")]
    wechat: String,
    #[prost(string, repeated, tag = "5")]
    tags: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ContactProfileProto {
    #[prost(int32, tag = "1")]
    gender: i32,
    #[prost(string, tag = "2")]
    country: String,
    #[prost(string, tag = "3")]
    state: String,
    #[prost(string, tag = "4")]
    city: String,
    #[prost(string, tag = "5")]
    signature: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Contact {
    pub name: String,
    pub(super) nickname: Option<String>,
    pub(super) alias: Option<String>,
    pub(super) remark: Option<String>,
    pub(super) wechat: Option<String>,
    pub(super) tags: Vec<String>,
    pub(super) profile: ContactProfile,
    pub(super) head_image_url: Option<String>,
    pub(super) chatroom: Option<ChatRoomInfo>,
    // contact type is useful for future official/openim/group filtering.
    #[allow(dead_code)]
    pub user_type: i32,
    #[allow(dead_code)]
    pub(super) is_openim: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ContactProfile {
    pub(super) gender: Option<i32>,
    pub(super) country: Option<String>,
    pub(super) state: Option<String>,
    pub(super) city: Option<String>,
    pub(super) signature: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ContactRemark {
    pub(super) nickname: Option<String>,
    pub(super) alias: Option<String>,
    pub(super) remark: Option<String>,
    pub(super) wechat: Option<String>,
    pub(super) tags: Vec<String>,
}

impl Contact {
    pub fn get_remark(&self) -> Result<String, Box<dyn std::error::Error>> {
        let remark = self
            .remark
            .clone()
            .or_else(|| self.nickname.clone())
            .or_else(|| self.alias.clone())
            .or_else(|| self.wechat.clone())
            .or_else(|| (!self.tags.is_empty()).then(|| self.tags.join(",")))
            .unwrap_or_default();
        Ok(remark)
    }

    #[allow(dead_code)]
    pub(super) fn profile_metadata(&self) -> HashMap<String, String> {
        vec![
            self.profile
                .gender
                .map(|gender| ("gender".to_string(), gender.to_string())),
            self.profile
                .country
                .clone()
                .map(|value| ("country".to_string(), value)),
            self.profile
                .state
                .clone()
                .map(|value| ("state".to_string(), value)),
            self.profile
                .city
                .clone()
                .map(|value| ("city".to_string(), value)),
            self.profile
                .signature
                .clone()
                .map(|value| ("signature".to_string(), value)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    // Kept for future avatar export or contact enrichment from head image metadata.
    #[allow(dead_code)]
    pub fn get_image(&self) -> Result<Option<String>, Utf8Error> {
        Ok(self.head_image_url.clone())
    }

    pub(super) fn room_member_display(&self, username: &str) -> Option<String> {
        self.chatroom
            .as_ref()
            .and_then(|room| room.members.get(username))
            .and_then(|member| member.display_name.clone())
    }
}

pub(super) fn parse_contact_remark(data: &[u8]) -> ContactRemark {
    ContactRemarkProto::decode(data)
        .ok()
        .map(|remark| ContactRemark {
            nickname: (!remark.nickname.is_empty()).then_some(remark.nickname),
            alias: (!remark.alias.is_empty()).then_some(remark.alias),
            remark: (!remark.remark.is_empty()).then_some(remark.remark),
            wechat: (!remark.wechat.is_empty()).then_some(remark.wechat),
            tags: remark.tags,
        })
        .or_else(|| {
            from_utf8(data).ok().map(|value| ContactRemark {
                nickname: (!value.is_empty()).then(|| value.to_string()),
                ..Default::default()
            })
        })
        .unwrap_or_default()
}

pub(super) fn parse_contact_profile(data: &[u8]) -> ContactProfile {
    ContactProfileProto::decode(data)
        .ok()
        .map(|profile| ContactProfile {
            gender: (profile.gender != 0).then_some(profile.gender),
            country: (!profile.country.is_empty()).then_some(profile.country),
            state: (!profile.state.is_empty()).then_some(profile.state),
            city: (!profile.city.is_empty()).then_some(profile.city),
            signature: (!profile.signature.is_empty()).then_some(profile.signature),
        })
        .unwrap_or_default()
}

pub(super) fn parse_contact_head_image(data: &[u8]) -> Option<String> {
    lazy_static! {
        static ref URL_MATCHER: Regex =
            Regex::new(r"(http://[a-zA-Z\./_\d]*/0)([^a-zA-Z\./_\d]|$)").unwrap();
    }
    from_utf8(data).ok().and_then(|text| {
        URL_MATCHER
            .captures(text)
            .and_then(|c| c.iter().nth(1).flatten())
            .map(|i| i.as_str().trim().into())
    })
}

pub(super) fn contact_from_parts(
    name: String,
    remark_data: Vec<u8>,
    profile_data: Vec<u8>,
    head_data: Vec<u8>,
    user_type: i32,
    is_openim: bool,
    chatroom: Option<ChatRoomInfo>,
) -> Contact {
    let remark = parse_contact_remark(&remark_data);
    Contact {
        name,
        nickname: remark.nickname,
        alias: remark.alias,
        remark: remark.remark,
        wechat: remark.wechat,
        tags: remark.tags,
        profile: parse_contact_profile(&profile_data),
        head_image_url: parse_contact_head_image(&head_data),
        chatroom,
        user_type,
        is_openim,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ChatRoomInfo {
    pub(super) members: HashMap<String, RoomMember>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RoomMember {
    pub(super) username: String,
    pub(super) display_name: Option<String>,
    pub(super) inviter_username: Option<String>,
}

impl ChatRoomInfo {
    pub(super) fn merge(mut self, other: Self) -> Self {
        self.members.extend(other.members);
        self
    }
}

pub(super) fn parse_room_members(data: &[u8]) -> ChatRoomInfo {
    let Ok(text) = from_utf8(data) else {
        return ChatRoomInfo::default();
    };
    parse_room_info_xml(text)
}

#[derive(Clone, PartialEq, prost::Message)]
struct ChatroomProto {
    #[prost(string, tag = "6")]
    room_info_xml: String,
}

pub(super) fn parse_db_contact_chatroom(data: &[u8]) -> ChatRoomInfo {
    ChatroomProto::decode(data)
        .ok()
        .map(|room| parse_room_info_xml(&room.room_info_xml))
        .unwrap_or_default()
}

fn parse_room_info_xml(text: &str) -> ChatRoomInfo {
    let members = super::xml::SafeXml::parse(text)
        .ok()
        .map(|xml| {
            xml.children_attrs_and_text(
                &["RoomData"],
                "Member",
                &["UserName", "DisplayName", "InviterUserName"],
                &["DisplayName", "InviterUserName"],
            )
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(attrs, children)| {
            let username = attrs.get("UserName")?.to_string();
            let display_name = children
                .get("DisplayName")
                .or_else(|| attrs.get("DisplayName"))
                .filter(|value| !value.is_empty())
                .cloned();
            let inviter_username = children
                .get("InviterUserName")
                .or_else(|| attrs.get("InviterUserName"))
                .filter(|value| !value.is_empty())
                .cloned();
            Some((
                username.clone(),
                RoomMember {
                    username,
                    display_name,
                    inviter_username,
                },
            ))
        })
        .collect();
    ChatRoomInfo { members }
}

pub(super) fn has_column(conn: &Connection, table: &str, column: &str) -> SqliteResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let columns = stmt
        .query_map(params![], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    Ok(columns.iter().any(|name| name == column))
}

fn optional_blob(row: &rusqlite::Row<'_>, index: usize) -> SqliteResult<Vec<u8>> {
    Ok(row.get::<_, Option<Vec<u8>>>(index)?.unwrap_or_default())
}

fn optional_i32(row: &rusqlite::Row<'_>, index: usize) -> SqliteResult<i32> {
    Ok(row.get::<_, Option<i32>>(index)?.unwrap_or_default())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ResolvedSender {
    Known { id: String, display_name: String },
    Unknown { id: String, display_name: String },
}

impl ResolvedSender {
    pub(super) fn into_parts(self) -> (String, String) {
        match self {
            Self::Known { id, display_name } => (id, display_name),
            Self::Unknown { id, display_name } => (id, display_name),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ContactBook {
    pub(super) contacts: HashMap<String, Contact>,
}

impl ContactBook {
    pub(super) fn resolve_message_sender(
        &self,
        room: Option<&Contact>,
        content: &str,
    ) -> Option<(String, String, String)> {
        self.prefixed_user_id(content)
            .map(|(id, _)| {
                let (id, display_name) = self.resolve_sender(room, &id).into_parts();
                (id, display_name, Self::strip_prefixed_content(content))
            })
            .or_else(|| {
                self.sender_from_xml(content)
                    .map(|(id, display_name)| (id, display_name, content.into()))
            })
    }

    pub(super) fn sender_from_xml(&self, content: &str) -> Option<(String, String)> {
        super::xml::xml_attr(content, &["msg"], "fromusername")
            .or_else(|| super::xml::xml_text(content, &["msg", "fromusername"]))
            .or_else(|| super::xml::xml_text(content, &["fromusername"]))
            .map(|id| self.resolve_sender(None, &id).into_parts())
    }

    pub(super) fn prefixed_user_id<'a>(&self, content: &'a str) -> Option<(String, &'a str)> {
        content
            .split_once(":\n")
            .filter(|(id, _)| !id.trim().is_empty())
            .map(|(id, _)| (id.trim().to_string(), content))
    }

    pub(super) fn strip_prefixed_content(content: &str) -> String {
        content.split("\n").skip(1).collect::<Vec<_>>().join("\n")
    }

    pub(super) fn resolve_sender(
        &self,
        room: Option<&Contact>,
        raw_sender_id: &str,
    ) -> ResolvedSender {
        let contact = self.contacts.get(&gen_md5(raw_sender_id));
        let display_name = room
            .and_then(|room| room.room_member_display(raw_sender_id))
            .or_else(|| contact.and_then(|contact| contact.get_remark().ok()))
            .unwrap_or_default();
        match contact {
            Some(contact) => ResolvedSender::Known {
                id: contact.name.clone(),
                display_name,
            },
            None => ResolvedSender::Unknown {
                id: raw_sender_id.to_string(),
                display_name,
            },
        }
    }
}

impl super::account::UserDB {
    pub(super) fn load_contacts(&mut self) -> SqliteResult<()> {
        if let Some(conn) = Self::get_conn(self.contact.clone())? {
            let has_chatroom = has_column(&conn, "Friend", "dbContactChatRoom")?;
            let has_profile = has_column(&conn, "Friend", "dbContactProfile")?;
            self.contacts = match (has_chatroom, has_profile) {
                (true, true) => conn
                    .prepare("SELECT userName, dbContactRemark, dbContactChatRoom, dbContactHeadImage, type, dbContactProfile FROM Friend")?
                    .query_map(params![], |row| {
                        let name: String = row.get(0)?;
                        let room_data = optional_blob(row, 2)?;
                        Ok((
                            gen_md5(&name),
                            contact_from_parts(
                                name,
                                optional_blob(row, 1)?,
                                optional_blob(row, 5)?,
                                optional_blob(row, 3)?,
                                optional_i32(row, 4)?,
                                false,
                                Some(parse_db_contact_chatroom(&room_data)),
                            ),
                        ))
                    })?
                    .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                    .collect(),
                (true, false) => conn
                    .prepare("SELECT userName, dbContactRemark, dbContactChatRoom, dbContactHeadImage, type FROM Friend")?
                    .query_map(params![], |row| {
                        let name: String = row.get(0)?;
                        let room_data = optional_blob(row, 2)?;
                        Ok((
                            gen_md5(&name),
                            contact_from_parts(
                                name,
                                optional_blob(row, 1)?,
                                Vec::new(),
                                optional_blob(row, 3)?,
                                optional_i32(row, 4)?,
                                false,
                                Some(parse_db_contact_chatroom(&room_data)),
                            ),
                        ))
                    })?
                    .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                    .collect(),
                (false, true) => conn
                    .prepare("SELECT userName, dbContactRemark, dbContactHeadImage, type, dbContactProfile FROM Friend")?
                    .query_map(params![], |row| {
                        let name: String = row.get(0)?;
                        Ok((
                            gen_md5(&name),
                            contact_from_parts(
                                name,
                                optional_blob(row, 1)?,
                                optional_blob(row, 4)?,
                                optional_blob(row, 2)?,
                                optional_i32(row, 3)?,
                                false,
                                None,
                            ),
                        ))
                    })?
                    .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                    .collect(),
                (false, false) => conn
                    .prepare(
                        "SELECT userName, dbContactRemark, dbContactHeadImage, type FROM Friend",
                    )?
                    .query_map(params![], |row| {
                        let name: String = row.get(0)?;
                        Ok((
                            gen_md5(&name),
                            contact_from_parts(
                                name,
                                optional_blob(row, 1)?,
                                Vec::new(),
                                optional_blob(row, 2)?,
                                optional_i32(row, 3)?,
                                false,
                                None,
                            ),
                        ))
                    })?
                    .filter_map(|r| r.map_err(|e| warn!("failed to parse contact: {}", e)).ok())
                    .collect(),
            };
            if let Ok(mut stmt) = conn.prepare(
                "SELECT userName, dbContactRemark, dbContactHeadImage, type FROM OpenIMContact",
            ) {
                let openim_contacts = stmt
                    .query_map(params![], |row| {
                        let name: String = row.get(0)?;
                        Ok((
                            gen_md5(&name),
                            contact_from_parts(
                                name,
                                optional_blob(row, 1)?,
                                Vec::new(),
                                optional_blob(row, 2)?,
                                optional_i32(row, 3)?,
                                true,
                                None,
                            ),
                        ))
                    })?
                    .filter_map(|r| {
                        r.map_err(|e| warn!("failed to parse OpenIM contact: {}", e))
                            .ok()
                    })
                    .collect::<HashMap<_, _>>();
                self.contacts = self
                    .contacts
                    .clone()
                    .into_iter()
                    .chain(openim_contacts)
                    .collect();
            }
            if let Ok(mut stmt) = conn.prepare("SELECT chatRoomName, roomInfoXml FROM ChatRoom") {
                let room_members = stmt
                    .query_map(params![], |row| {
                        let room_name: String = row.get(0)?;
                        let room_info_xml = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                        Ok((
                            gen_md5(room_name),
                            parse_room_members(room_info_xml.as_bytes()),
                        ))
                    })?
                    .filter_map(|r| r.map_err(|e| warn!("failed to parse ChatRoom: {}", e)).ok())
                    .collect::<HashMap<_, _>>();
                for (hash, members) in room_members {
                    if let Some(contact) = self.contacts.get_mut(&hash) {
                        contact.chatroom =
                            Some(contact.chatroom.clone().unwrap_or_default().merge(members));
                    }
                }
            }
        }
        Ok(())
    }
}
