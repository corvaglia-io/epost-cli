use crate::{
    api::ApiClient,
    error::{Error, Result},
    models::Letter,
};
use serde::Serialize;
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "directory_id", rename_all = "snake_case")]
pub enum Source {
    Inbox,
    ArchiveRoot,
    ArchiveDirectory(String),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::Inbox => "inbox".into(),
            Self::ArchiveRoot => "archive:root".into(),
            Self::ArchiveDirectory(id) => format!("archive:directory:{id}"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub source: Source,
    pub letters: Vec<Letter>,
    /// True only when pagination ended normally; this is not a snapshot guarantee.
    pub pagination_complete: bool,
    pub pages: u32,
    pub issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_exit_code: Option<u8>,
}

pub async fn page(
    client: &mut ApiClient,
    source: &Source,
    size: u32,
    offset: u32,
) -> Result<Vec<Letter>> {
    match source {
        Source::Inbox => client.inbox_page(size, offset).await,
        Source::ArchiveRoot => client.archive_page(None, size, offset).await,
        Source::ArchiveDirectory(id) => client.archive_page(Some(id), size, offset).await,
    }
}

/// Bounded offset traversal. Overlapping pages are preserved as an uncertainty.
pub async fn collect(
    client: &mut ApiClient,
    source: Source,
    size: u32,
    max_pages: u32,
) -> Result<Inventory> {
    if !(1..=1000).contains(&size) || !(1..=100_000).contains(&max_pages) {
        return Err(Error::Config(
            "page size must be 1..1000 and max pages 1..100000",
        ));
    }
    let mut result = Inventory {
        source,
        letters: Vec::new(),
        pagination_complete: false,
        pages: 0,
        issues: Vec::new(),
        failure_exit_code: None,
    };
    let mut seen = HashSet::new();
    let mut offset = 0u32;
    for _ in 0..max_pages {
        let items = match page(client, &result.source, size, offset).await {
            Ok(items) => items,
            Err(error) if result.pages == 0 => return Err(error),
            Err(error) => {
                result.failure_exit_code = Some(error.exit_code());
                result.issues.push(error.to_string());
                return Ok(result);
            }
        };
        result.pages += 1;
        let count = items.len();
        if count > size as usize {
            result
                .issues
                .push("provider returned more entries than requested".into());
            return Ok(result);
        }
        let mut new_count = 0;
        for letter in items {
            if result.letters.len() >= 100_000 {
                result
                    .issues
                    .push("inventory letter safety limit reached".into());
                return Ok(result);
            }
            match letter.id.as_deref().filter(|id| !id.is_empty()) {
                Some(id) if seen.insert(id.to_owned()) => {
                    new_count += 1;
                    result.letters.push(letter);
                }
                Some(_) => {
                    if !result
                        .issues
                        .iter()
                        .any(|s| s == "duplicate IDs observed across pages")
                    {
                        result
                            .issues
                            .push("duplicate IDs observed across pages".into());
                    }
                }
                None => {
                    result.issues.push("letter without usable ID".into());
                    result.letters.push(letter);
                }
            }
        }
        if count < size as usize {
            result.pagination_complete = result.issues.is_empty();
            return Ok(result);
        }
        if new_count == 0 {
            result.issues.push("pagination made no progress".into());
            return Ok(result);
        }
        match offset
            .checked_add(size)
            .filter(|offset| *offset <= i32::MAX as u32)
        {
            Some(next) => offset = next,
            None => {
                result
                    .issues
                    .push("pagination offset exceeded API range".into());
                return Ok(result);
            }
        }
    }
    result.issues.push("maximum page count reached".into());
    Ok(result)
}
