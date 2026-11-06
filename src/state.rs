use crate::{
    error::{ErrorCode as E, MailError, Result},
    mail::RawFolder,
    model::*,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc, time::Instant};

#[derive(Serialize, Deserialize)]
pub(crate) enum Token {
    Folder(String),
    Message {
        folder: String,
        validity: u32,
        uid: u32,
    },
    Cursor {
        snapshot: String,
        index: usize,
    },
}
pub(crate) struct Tokens(hmac::Key);
impl Tokens {
    pub fn new() -> Self {
        let mut key = Vec::from(uuid::Uuid::new_v4().as_bytes());
        key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(hmac::Key::new(hmac::HMAC_SHA256, &key))
    }
    pub fn seal(&self, token: &Token) -> Result<String> {
        let data = serde_json::to_vec(token).map_err(|_| MailError(E::InternalError))?;
        let result = format!(
            "{}.{}",
            B64.encode(&data),
            B64.encode(hmac::sign(&self.0, &data).as_ref())
        );
        if result.len() > 2048 {
            return Err(MailError(E::SizeLimit));
        }
        Ok(result)
    }
    pub fn open(&self, s: &str, cursor: bool) -> Result<Token> {
        let error = || {
            MailError(if cursor {
                E::CursorExpired
            } else {
                E::StaleReference
            })
        };
        if s.len() > 2048 {
            return Err(error());
        }
        let (data, mac) = s.split_once('.').ok_or_else(error)?;
        let data = B64.decode(data).map_err(|_| error())?;
        let mac = B64.decode(mac).map_err(|_| error())?;
        hmac::verify(&self.0, &data, &mac).map_err(|_| error())?;
        serde_json::from_slice(&data).map_err(|_| error())
    }
}
pub(crate) enum Snapshot {
    Folders {
        prefix: String,
        counts: bool,
        folders: Vec<RawFolder>,
    },
    Messages {
        folder: String,
        validity: u32,
        filters: Filters,
        uids: Vec<u32>,
    },
    Body {
        folder: String,
        validity: u32,
        uid: u32,
        section: String,
        text: String,
        representation: String,
        warnings: Vec<String>,
    },
}
impl Snapshot {
    fn weight(&self) -> usize {
        512 + match self {
            Self::Folders {
                prefix, folders, ..
            } => {
                prefix.capacity()
                    + folders.capacity() * std::mem::size_of::<RawFolder>()
                    + folders
                        .iter()
                        .map(|f| {
                            f.name.capacity()
                                + f.delimiter.as_ref().map_or(0, String::capacity)
                                + f.roles.capacity() * 24
                                + f.roles.iter().map(String::capacity).sum::<usize>()
                        })
                        .sum::<usize>()
            }
            Self::Messages {
                folder,
                filters,
                uids,
                ..
            } => {
                folder.capacity()
                    + uids.capacity() * 4
                    + serde_json::to_vec(filters).map_or(8192, |s| s.len())
            }
            Self::Body {
                folder,
                section,
                text,
                representation,
                warnings,
                ..
            } => {
                folder.capacity()
                    + section.capacity()
                    + text.capacity()
                    + representation.capacity()
                    + warnings.iter().map(String::capacity).sum::<usize>()
            }
        }
    }
}
pub(crate) struct Entry {
    pub id: String,
    pub created: String,
    pub deadline: Instant,
    pub snapshot: Snapshot,
    pub weight: usize,
}
pub(crate) struct Cache {
    entries: VecDeque<Arc<Entry>>,
    bytes: usize,
}
impl Cache {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }
    pub fn reserve(&mut self, bytes: usize) -> Result<()> {
        if bytes > CACHE_LIMIT {
            return Err(MailError(E::SizeLimit));
        }
        while self.entries.len() >= 32
            || self.bytes + bytes > CACHE_LIMIT
            || self
                .entries
                .front()
                .is_some_and(|e| e.deadline <= Instant::now())
        {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= old.weight;
            } else {
                break;
            }
        }
        Ok(())
    }
    pub fn insert(&mut self, snapshot: Snapshot) -> Result<Arc<Entry>> {
        let weight = snapshot.weight();
        self.reserve(weight)?;
        let entry = Arc::new(Entry {
            id: uuid::Uuid::new_v4().to_string(),
            created: now(),
            deadline: Instant::now() + LIFETIME,
            snapshot,
            weight,
        });
        self.bytes += weight;
        self.entries.push_back(entry.clone());
        Ok(entry)
    }
    pub fn get(&mut self, id: &str) -> Result<Arc<Entry>> {
        while self
            .entries
            .front()
            .is_some_and(|e| e.deadline <= Instant::now())
        {
            let old = self.entries.pop_front().unwrap();
            self.bytes -= old.weight;
        }
        self.entries
            .iter()
            .find(|e| e.id == id && e.deadline > Instant::now())
            .cloned()
            .ok_or(MailError(E::CursorExpired))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Snapshot {
        Snapshot::Folders {
            prefix: String::new(),
            counts: false,
            folders: Vec::new(),
        }
    }
    #[test]
    fn full_cache_lookup_does_not_evict_and_expiry_is_fixed() {
        let mut cache = Cache::new();
        let mut ids = Vec::new();
        for _ in 0..32 {
            ids.push(cache.insert(snapshot()).unwrap().id.clone());
        }
        assert!(cache.get(&ids[0]).is_ok());
        assert!(cache.get(&ids[31]).is_ok());
        Arc::get_mut(cache.entries.front_mut().unwrap())
            .unwrap()
            .deadline = Instant::now() - std::time::Duration::from_secs(1);
        assert!(matches!(
            cache.get(&ids[0]),
            Err(MailError(E::CursorExpired))
        ));
        let next = cache.insert(snapshot()).unwrap();
        assert!(cache.get(&next.id).is_ok());
    }
    #[test]
    fn cache_accounts_for_reserved_string_capacity() {
        let mut cache = Cache::new();
        let huge = Snapshot::Body {
            folder: String::new(),
            validity: 1,
            uid: 1,
            section: "1".into(),
            text: String::with_capacity(CACHE_LIMIT + 1),
            representation: "plain_text".into(),
            warnings: Vec::new(),
        };
        assert!(matches!(cache.insert(huge), Err(MailError(E::SizeLimit))));
    }
}
