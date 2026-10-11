//! Tolerant wire models for the documented inbox and archive operations.

use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MailboxKind {
    Private,
    Business,
}

impl MailboxKind {
    pub fn as_bool(self) -> bool {
        matches!(self, Self::Business)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Mailbox {
    pub tenant_id: String,
    pub company_id: i64,
    pub kind: MailboxKind,
}

/// Discovery never chooses a tenant or infers its mailbox kind.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tenant {
    pub tenant_id: String,
    pub company_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company_name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Letter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e_letter_registered: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_name: Option<String>,
    #[serde(default, deserialize_with = "nullable_strings")]
    pub parent_directory_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_of_documents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_sub_directories: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn nullable_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Content is preserved as bytes, including successful JSON content.
#[derive(Clone, Debug)]
pub struct Content {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}
