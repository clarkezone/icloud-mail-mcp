use crate::error::{ErrorCode, MailError, Result};
use chrono::NaiveDate;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
pub const RESPONSE_LIMIT: usize = 128 * 1024;
pub const TEXT_LIMIT: usize = 8 * 1024 * 1024;
pub const CACHE_LIMIT: usize = 32 * 1024 * 1024;
pub const SNAPSHOT_LIMIT: usize = 10_000;
pub const LIFETIME: std::time::Duration = std::time::Duration::from_secs(600);
pub fn page_default() -> usize {
    50
}
pub fn chunk_default() -> usize {
    16384
}
#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListFolders {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub include_counts: bool,
    #[serde(default = "page_default")]
    pub page_size: usize,
    pub cursor: Option<String>,
}
#[derive(Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListMessages {
    pub folder_handle: String,
    #[serde(default = "page_default")]
    pub page_size: usize,
    pub cursor: Option<String>,
}
#[derive(Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchMessages {
    pub folder_handle: String,
    #[serde(default)]
    pub filters: Filters,
    #[serde(default = "page_default")]
    pub page_size: usize,
    pub cursor: Option<String>,
}
#[derive(Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMessage {
    pub message_handle: String,
    #[serde(default = "chunk_default")]
    pub body_chunk_bytes: usize,
    pub body_cursor: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    pub from_contains: Option<String>,
    pub to_contains: Option<String>,
    pub subject_contains: Option<String>,
    pub text_contains: Option<String>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    pub received_on_or_after: Option<String>,
    pub received_before: Option<String>,
}
impl Filters {
    pub fn valid(&self) -> bool {
        let strings = [
            &self.from_contains,
            &self.to_contains,
            &self.subject_contains,
            &self.text_contains,
        ];
        if strings
            .into_iter()
            .flatten()
            .any(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
        {
            return false;
        }
        let date = |s: &Option<String>| -> std::result::Result<Option<NaiveDate>, ()> {
            s.as_ref()
                .map(|s| {
                    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| ())?;
                    if d.format("%Y-%m-%d").to_string() != *s {
                        return Err(());
                    }
                    Ok(d)
                })
                .transpose()
        };
        let (Ok(a), Ok(b)) = (
            date(&self.received_on_or_after),
            date(&self.received_before),
        ) else {
            return false;
        };
        !matches!((a,b), (Some(a),Some(b)) if a >= b)
    }
}
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Address {
    pub name: Option<String>,
    pub address: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Headers {
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: Option<String>,
    pub header_date: Option<String>,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct Folder {
    pub folder_handle: String,
    pub name: String,
    pub delimiter: Option<String>,
    pub selectable: bool,
    pub roles: Vec<String>,
    pub message_count: Option<u32>,
    pub unread_count: Option<u32>,
    pub counts_observed_at: Option<String>,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct Summary {
    pub message_handle: String,
    #[serde(flatten)]
    pub headers: Headers,
    pub received_at: String,
    pub size_bytes: u32,
    pub flags: Vec<String>,
    pub has_attachments: Option<bool>,
    pub truncated_fields: Vec<String>,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct Attachment {
    pub part_ref: String,
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub size_bytes: Option<u32>,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct FolderPage {
    pub folders: Vec<Folder>,
    pub snapshot_created_at: String,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct MessagePage {
    pub messages: Vec<Summary>,
    pub snapshot_created_at: String,
    pub skipped_unavailable: usize,
    pub next_cursor: Option<String>,
    pub consistency: String,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct Body {
    pub text: String,
    pub representation: String,
    pub next_cursor: Option<String>,
    pub snapshot_created_at: String,
    pub complete: bool,
}
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct Message {
    pub message_handle: String,
    pub headers: Headers,
    pub flags: Vec<String>,
    pub attachments: Vec<Attachment>,
    pub metadata_truncated: bool,
    pub truncated_fields: Vec<String>,
    pub warnings: Vec<String>,
    pub body: Body,
}
#[derive(Clone)]
pub enum Operation {
    Folders(ListFolders),
    List(ListMessages),
    Search(SearchMessages),
    Get(GetMessage),
}
impl Operation {
    pub fn valid(&self) -> bool {
        let token = |s: &str| !s.is_empty() && s.len() <= 2048;
        let page = |n: usize| (1..=100).contains(&n);
        match self {
            Self::Folders(p) => {
                p.prefix.len() <= 1024 && page(p.page_size) && p.cursor.as_deref().is_none_or(token)
            }
            Self::List(p) => {
                token(&p.folder_handle)
                    && page(p.page_size)
                    && p.cursor.as_deref().is_none_or(token)
            }
            Self::Search(p) => {
                token(&p.folder_handle)
                    && page(p.page_size)
                    && p.filters.valid()
                    && p.cursor.as_deref().is_none_or(token)
            }
            Self::Get(p) => {
                token(&p.message_handle)
                    && (256..=65536).contains(&p.body_chunk_bytes)
                    && p.body_cursor.as_deref().is_none_or(token)
            }
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Folders(_) => "list_folders",
            Self::List(_) => "list_messages",
            Self::Search(_) => "search_messages",
            Self::Get(_) => "get_message",
        }
    }
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub fn limited(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].into()
}
pub fn value<T: Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|_| MailError(ErrorCode::InternalError))
}
