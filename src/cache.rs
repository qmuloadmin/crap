use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;

use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use fred::bytes::Bytes;
use fred::prelude::*;
use futures::future::BoxFuture;
use hyper::header::HeaderValue;
use hyper::header::CONTENT_LENGTH;
use hyper::http::header::HeaderName;
use hyper::http::response::Parts;
use hyper::HeaderMap;
use hyper::StatusCode;
use lru::LruCache;
use phf::{phf_set, Set};

static UNCACHED_HEADERS: Set<&'static str> = phf_set! {"content-length", "connection"};
pub(crate) static CACHE_HEADER: &'static str = "X-CRAP-CACHE";

//ResponseCache impls should be cheaply cloneable so they can be cloned to move across threads
pub(crate) trait ResponseCache: Clone + Send + Sync + Unpin + 'static {
    type Error: Send + Sync + Debug + 'static;
    // store_key is called in Drop and thus cannot be async
    // errors should be logged for advisory purposes
    fn store_key(self, key: String, content: Vec<u8>);
    fn get_key(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send;
}

// A crappy cache implementation suitable for unit tests.
// TODO move it to a test module
pub(crate) type MemCache = Arc<Mutex<LruCache<String, Vec<u8>>>>;

impl ResponseCache for MemCache {
    type Error = anyhow::Error;

    fn store_key(self, key: String, content: Vec<u8>) {
        self.lock().unwrap().put(key, content);
    }

    async fn get_key(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.lock().unwrap().get(key).cloned())
    }
}

#[derive(Clone)]
pub(crate) struct RedisCache {
    client: fred::clients::RedisPool,
    sender: Sender<BoxFuture<'static, Result<(), RedisError>>>,
}

impl RedisCache {
    pub(crate) fn new(pool: fred::clients::RedisPool) -> Self {
        let (send, rec) = crossbeam::channel::unbounded();
        tokio::task::spawn(async move {
            loop {
                let rec: Receiver<BoxFuture<'static, Result<(), RedisError>>> = rec.clone();
                // TODO log this instead of unwraping.
                let _ = tokio::task::spawn_blocking(move || rec.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .await
                    .unwrap();
            }
        });
        RedisCache {
            client: pool,
            sender: send,
        }
    }

    async fn cache_by_key(self, key: String, content: Vec<u8>) -> Result<(), RedisError> {
        self.client
            .set::<(), String, Bytes>(key, content.into(), None, None, false)
            .await
    }
}

impl ResponseCache for RedisCache {
    type Error = fred::error::RedisError;
    fn store_key(self, key: String, content: Vec<u8>) {
        let _ = self
            .sender
            .clone()
            .send(Box::pin(self.cache_by_key(key, content)));
    }
    async fn get_key(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        self.client.get(key).await
    }
}

pub(crate) fn response_to_bytes(parts: &Parts) -> Vec<u8> {
    let body_size: usize = parts
        .headers
        .get(CONTENT_LENGTH)
        .unwrap_or(&HeaderValue::from(0))
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let status_line = format!("{}\n", parts.status.as_str()).into_bytes();
    let headers = parts
        .headers
        .iter()
        .filter(|hdr| !UNCACHED_HEADERS.contains(hdr.0.as_str()))
        .fold(
            // TODO how to get a better capacity here?
            Vec::<u8>::with_capacity(parts.headers.len()),
            |mut hbytes, (k, v)| {
                let kbytes = k.as_str().as_bytes();
                let vbytes = v.as_bytes();
                hbytes.extend(kbytes);
                hbytes.push(b':');
                hbytes.extend(vbytes);
                hbytes.push(b'\n');
                hbytes
            },
        );
    let mut response = Vec::with_capacity(body_size + status_line.len() + headers.len() + 2);
    response.extend(status_line);
    response.extend(headers);
    response.extend(b"-\n"); // TODO this dash thing is a stupid hack to get to a PoC
    response
}

enum BytesStage {
    Status,
    Headers,
}

pub(crate) fn bytes_to_parts<'a>(bs: &'a [u8]) -> (StatusCode, HeaderMap<HeaderValue>, &'a [u8]) {
    let mut status = Vec::new();
    let mut headers: HeaderMap<HeaderValue> = HeaderMap::new();
    headers.insert(CACHE_HEADER, HeaderValue::from_str("hit").unwrap());
    let mut stage = BytesStage::Status;
    let mut body_start = bs.len();
    for line in bs.split(|b| *b == b'\n') {
        if &line == &b"-" {
            body_start += 2;
            break;
        }
        match stage {
            BytesStage::Status => {
                body_start = line.len() + 1;
                status = line.into();
                stage = BytesStage::Headers;
            }
            BytesStage::Headers => {
                body_start += line.len() + 1;
                let (k, v) = line.split_at(line.iter().position(|b| *b == b':').unwrap());
                // TODO while we shouldn't be getting a lot of invalid values here
                // we should still handle them -- refuse to serve the cached value and fetch a raw response instead
                let header = HeaderName::from_bytes(k).expect(&String::from_utf8_lossy(&k));
                headers.insert(header, HeaderValue::from_bytes(&v[1..v.len()]).unwrap());
            }
        }
    }
    (
        StatusCode::from_bytes(&status).unwrap(),
        headers,
        &bs[body_start..],
    )
}
