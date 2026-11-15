//! Cursor-paginated lists.

use serde::{Deserialize, Serialize};

use crate::types::PageCursor;

/// One page of a history or list, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Page<T> {
    /// The items.
    pub data: Vec<T>,
    /// The cursor of the next (older) page, with the same filters; `None` on the last page.
    #[serde(deserialize_with = "super::required")]
    pub next_cursor: Option<PageCursor>,
    /// Whether another page follows.
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cursor_is_required_but_nullable() {
        let last: Page<u8> =
            serde_json::from_str(r#"{"data":[1],"next_cursor":null,"has_more":false}"#).unwrap();
        assert_eq!(last.next_cursor, None);
        let page: Page<u8> =
            serde_json::from_str(r#"{"data":[],"next_cursor":"c","has_more":true}"#).unwrap();
        assert_eq!(page.next_cursor.as_ref().map(PageCursor::as_str), Some("c"));
        assert!(serde_json::from_str::<Page<u8>>(r#"{"data":[],"has_more":false}"#).is_err());
    }
}
