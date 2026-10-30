use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::protocol::{CacheStats, FetchResponse, SearchRequest, SearchResponse};

#[derive(Debug, Clone)]
enum CachedValue {
    Search(SearchResponse),
    Fetch(FetchResponse),
}

#[derive(Debug)]
struct Entry {
    value: CachedValue,
    expires_at: Instant,
    inserted_at: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    hits: u64,
    misses: u64,
}

#[derive(Debug)]
pub struct Cache {
    inner: Mutex<Inner>,
    search_ttl: Duration,
    fetch_ttl: Duration,
    max_entries: usize,
}

impl Cache {
    pub fn new(search_ttl: Duration, fetch_ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            search_ttl,
            fetch_ttl,
            max_entries,
        }
    }

    pub fn get_search(&self, request: &SearchRequest) -> Option<SearchResponse> {
        let key = search_key(request);
        self.get(&key).and_then(|value| match value {
            CachedValue::Search(response) => Some(response),
            CachedValue::Fetch(_) => None,
        })
    }

    pub fn get_fetch(&self, url: &str) -> Option<FetchResponse> {
        let key = fetch_key(url);
        self.get(&key).and_then(|value| match value {
            CachedValue::Fetch(response) => Some(response),
            CachedValue::Search(_) => None,
        })
    }

    pub fn insert_search(&self, request: &SearchRequest, response: SearchResponse) {
        self.insert(
            search_key(request),
            CachedValue::Search(response),
            self.search_ttl,
        );
    }

    pub fn insert_fetch(&self, url: &str, response: FetchResponse) {
        self.insert(fetch_key(url), CachedValue::Fetch(response), self.fetch_ttl);
    }

    pub fn stats(&self) -> CacheStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        CacheStats {
            entries: inner.entries.len(),
            hits: inner.hits,
            misses: inner.misses,
        }
    }

    fn get(&self, key: &str) -> Option<CachedValue> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if inner
            .entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= now)
        {
            inner.entries.remove(key);
        }
        let value = inner.entries.get(key).map(|entry| entry.value.clone());
        match value {
            Some(value) => {
                inner.hits += 1;
                Some(value)
            }
            None => {
                inner.misses += 1;
                None
            }
        }
    }

    fn insert(&self, key: String, value: CachedValue, ttl: Duration) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        if inner.entries.len() >= self.max_entries {
            inner.entries.retain(|_, entry| entry.expires_at > now);
            while inner.entries.len() >= self.max_entries {
                let Some(oldest) = inner
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.inserted_at)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                inner.entries.remove(&oldest);
            }
        }

        inner.entries.insert(
            key,
            Entry {
                value,
                expires_at: now + ttl,
                inserted_at: now,
            },
        );
    }
}

fn search_key(request: &SearchRequest) -> String {
    format!(
        "search:{}",
        serde_json::to_string(request).unwrap_or_else(|_| request.query.clone())
    )
}

fn fetch_key(url: &str) -> String {
    format!("fetch:{url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            count: 3,
            need_content: false,
            ori_question: None,
            localization: None,
        }
    }

    #[test]
    fn caches_search_and_fetch() {
        let cache = Cache::new(Duration::from_secs(60), Duration::from_secs(60), 10);
        cache.insert_search(&request("rust"), SearchResponse { results: vec![] });
        assert!(cache.get_search(&request("rust")).is_some());
        assert!(cache.get_search(&request("other")).is_none());

        cache.insert_fetch(
            "https://example.com",
            FetchResponse {
                title: "t".into(),
                content: "c".into(),
            },
        );
        assert!(cache.get_fetch("https://example.com").is_some());
        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn evicts_when_full() {
        let cache = Cache::new(Duration::from_secs(60), Duration::from_secs(60), 2);
        cache.insert_search(&request("a"), SearchResponse { results: vec![] });
        cache.insert_search(&request("b"), SearchResponse { results: vec![] });
        cache.insert_search(&request("c"), SearchResponse { results: vec![] });
        assert!(cache.stats().entries <= 2);
    }
}
