//! The contact cache this bot keeps for itself, instead of the one inside
//! `meshcore-rs`.
//!
//! The crate replaces its map wholesale on every `EventType::Contacts` event,
//! but the radio answers a `GET_CONTACTS` with only the contacts modified since
//! the requested `lastmod`. The proxy forwards every client's frames to every
//! other client, so mc-webui's incremental poll reaches this bot as a
//! one-contact event and empties the crate's cache a few minutes after each
//! connect — after which every hop id in a `{{repeaters}}` chain falls back to
//! hex, with nothing logged to say why.
//!
//! This book is fed the same events and only ever grows, so a delta cannot
//! shrink it by more than the one contact the delta carries.

use std::collections::HashMap;
use std::sync::Arc;

use meshcore_rs::events::Contact;
use tokio::sync::RwLock;

/// Contacts keyed by their full public key.
///
/// An RF-log hop hash *is* the leading bytes of that key, so a lookup is a
/// prefix match over the keys — the same rule the crate's own
/// `get_contact_by_prefix` applies, and not a search by advertised name.
#[derive(Clone, Default)]
pub struct ContactBook {
    inner: Arc<RwLock<HashMap<[u8; 32], Contact>>>,
}

impl ContactBook {
    /// Insert or replace every contact given, leaving the others in place.
    pub async fn upsert_all(&self, contacts: impl IntoIterator<Item = Contact>) {
        let mut inner = self.inner.write().await;
        for contact in contacts {
            inner.insert(contact.public_key, contact);
        }
    }

    /// Insert or replace a single contact, leaving the others in place.
    pub async fn upsert_one(&self, contact: Contact) {
        self.inner.write().await.insert(contact.public_key, contact);
    }

    /// The contact whose public key begins with `prefix`, if the radio knows it.
    ///
    /// `None` means the hop is not a saved contact; `render_repeaters` renders
    /// that as the hop's hex id, which is the fallback this book exists to make
    /// rare rather than to remove.
    pub async fn lookup_prefix(&self, prefix: &[u8]) -> Option<Contact> {
        if prefix.is_empty() {
            return None;
        }
        self.inner
            .read()
            .await
            .values()
            .find(|contact| contact.public_key.starts_with(prefix))
            .cloned()
    }

    /// How many contacts the book holds.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether no contact has ever been recorded.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(prefix: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        key[..prefix.len()].copy_from_slice(prefix);
        key
    }

    fn contact(prefix: &[u8], name: &str) -> Contact {
        Contact {
            public_key: key(prefix),
            contact_type: 1,
            flags: 0,
            path_len: -1,
            out_path: Vec::new(),
            adv_name: name.to_string(),
            last_advert: 0,
            adv_lat: 0,
            adv_lon: 0,
            last_modification_timestamp: 0,
        }
    }

    /// The failure this book exists for: a one-contact delta arriving after a
    /// full dump must not take the earlier contacts away with it.
    #[tokio::test]
    async fn a_delta_cannot_shrink_the_book() {
        let book = ContactBook::default();
        book.upsert_all([contact(&[0xAA, 0xBB], "NODE-A"), contact(&[0xCC], "NODE-B")])
            .await;

        book.upsert_one(contact(&[0xEE], "NODE-C")).await;
        assert_eq!(book.len().await, 3);

        book.upsert_one(contact(&[0xAA, 0xBB], "NODE-A")).await;
        assert_eq!(book.len().await, 3);
    }

    #[tokio::test]
    async fn a_hop_hash_resolves_to_the_contact_it_is_a_prefix_of() {
        let book = ContactBook::default();
        book.upsert_all([
            contact(&[0xAA, 0xBB, 0xCC], "NODE-A"),
            contact(&[0xAA, 0xDD], "NODE-B"),
        ])
        .await;

        let found = book.lookup_prefix(&[0xAA, 0xBB]).await;
        assert_eq!(found.map(|c| c.adv_name), Some("NODE-A".to_string()));
    }

    #[tokio::test]
    async fn an_unknown_hop_and_an_empty_hash_stay_unresolved() {
        let book = ContactBook::default();
        book.upsert_all([contact(&[0xAA], "NODE-A")]).await;

        assert!(book.lookup_prefix(&[0xEE, 0xFF]).await.is_none());
        assert!(book.lookup_prefix(&[]).await.is_none());
    }

    #[tokio::test]
    async fn an_empty_book_is_empty() {
        let book = ContactBook::default();
        assert!(book.is_empty().await);
        book.upsert_all([contact(&[0xAA], "NODE-A")]).await;
        assert!(!book.is_empty().await);
    }
}
