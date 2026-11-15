//! Versioned journal records of the flows: JSON objects carrying `"v": 1`; a record of another
//! version is refused, never ignored.

use std::io;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Error, Result, error::StoreError, store::Journal};

/// The record format this release writes and reads.
const VERSION: u64 = 1;

/// The record under `key`, if any. A failed read or an unreadable record is an
/// [`Error::Store`] with `submitted == false`.
pub(crate) async fn load<T: DeserializeOwned>(journal: &Journal, key: &str) -> Result<Option<T>> {
    let Some(bytes) = journal.load(key).await.map_err(|error| store_error(key, false, error))?
    else {
        return Ok(None);
    };
    decode(&bytes).map(Some).map_err(|error| store_error(key, false, error))
}

/// Saves `record` under `key`; `submitted` tells, on failure, whether the operation the record
/// serves already reached the server.
pub(crate) async fn save<T: Serialize>(
    journal: &Journal,
    key: &str,
    record: &T,
    submitted: bool,
) -> Result<()> {
    let bytes = encode(record).map_err(|error| store_error(key, submitted, error))?;
    journal.save(key, &bytes).await.map_err(|error| store_error(key, submitted, error))
}

/// Deletes `key`; `submitted` tells, on failure, whether the operation the record served already
/// reached the server.
pub(crate) async fn delete(journal: &Journal, key: &str, submitted: bool) -> Result<()> {
    journal.delete(key).await.map_err(|error| store_error(key, submitted, error))
}

fn store_error(key: &str, submitted: bool, source: io::Error) -> Error {
    Error::Store(Box::new(StoreError::new(key, submitted, source)))
}

fn encode<T: Serialize>(record: &T) -> io::Result<Vec<u8>> {
    let Value::Object(mut fields) = serde_json::to_value(record).map_err(io::Error::other)? else {
        return Err(io::Error::other("a record is a JSON object"));
    };
    fields.insert("v".to_owned(), VERSION.into());
    serde_json::to_vec(&fields).map_err(io::Error::other)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let value: Value = serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
    match value.get("v").and_then(Value::as_u64) {
        Some(VERSION) => serde_json::from_value(value).map_err(|error| invalid(error.to_string())),
        version => Err(invalid(format!("record version {version:?} is not {VERSION}"))),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::store::MemoryStore;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Note {
        id: String,
    }

    fn note() -> Note {
        Note { id: "n-1".to_owned() }
    }

    #[tokio::test]
    async fn records_round_trip_with_their_version() {
        let journal = Journal::new(MemoryStore::new());
        save(&journal, "k", &note(), false).await.unwrap();
        let raw: Value =
            serde_json::from_slice(&journal.load("k").await.unwrap().unwrap()).unwrap();
        assert_eq!(raw["v"], 1);
        assert_eq!(load::<Note>(&journal, "k").await.unwrap(), Some(note()));
        delete(&journal, "k", true).await.unwrap();
        assert_eq!(load::<Note>(&journal, "k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn another_version_or_shape_is_refused() {
        let journal = Journal::new(MemoryStore::new());
        let mut raw = serde_json::to_value(note()).unwrap();
        for version in [Value::from(2), Value::Null] {
            raw["v"] = version;
            journal.save("k", &serde_json::to_vec(&raw).unwrap()).await.unwrap();
            let Err(Error::Store(error)) = load::<Note>(&journal, "k").await else {
                panic!("a record of another version is refused");
            };
            assert!(!error.submitted());
            assert_eq!(error.key(), "k");
        }
        journal.save("k", br#"{"v":1,"id":5}"#).await.unwrap();
        assert!(matches!(load::<Note>(&journal, "k").await, Err(Error::Store(_))));
    }
}
