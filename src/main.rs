use std::borrow::Cow;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::Context;
use body::CacheBody;
use body::IncomingTeeSink;
use cache::bytes_to_parts;
use cache::response_to_bytes;
use cache::MemCache;
use cache::ResponseCache;
use cache::CACHE_HEADER;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::header::CACHE_CONTROL;
use hyper::http::response::Parts;
use hyper::Method;
use hyper::{server::conn::http1, service::Service};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use lru::LruCache;
use mobc::Pool;
use pool::ClientConnectionManager;
use tokio::net::TcpListener;

mod body;
mod cache;
mod pool;

#[derive(Clone)]
struct Config<'a> {
    target_host: Cow<'a, str>,
    target_port: u16,
}

#[derive(Clone)]
struct CachingProxy<'config> {
    lru: MemCache,
    config: Config<'config>,
    http_client: Pool<ClientConnectionManager>,
}

struct RequestCacheControl {
    read_cache: bool,
    write_cache: bool,
}

impl<'config> CachingProxy<'config> {
    fn configure_caching(
        &self,
        req: &Request<Incoming>,
    ) -> Result<RequestCacheControl, anyhow::Error> {
        let cache_control = match req.headers().get(CACHE_CONTROL) {
            Some(cache_control) => cache_control
                .to_str()
                .context("invalid cache-control request header value")?,
            None => "",
        };
        let check_cache = req.method() == Method::GET && !cache_control.contains("no-cache");
        let cache_response = !cache_control.contains("no-store");
        Ok(RequestCacheControl {
            read_cache: check_cache,
            write_cache: cache_response,
        })
    }

    // Can't take &self because of the need to be called inside a moved closure
    fn cache_response(cfg: &RequestCacheControl, res: &Parts) -> Result<bool, anyhow::Error> {
        if cfg.write_cache {
            match res.headers.get(CACHE_CONTROL) {
                Some(cache_control) => Ok(cache_control
                    .to_str()
                    .context("invalid cache-control response header value")?
                    .to_string()
                    .contains("no-store")),
                None => Ok(true),
            }
        } else {
            Ok(false)
        }
    }

    fn read_cache(
        &self,
        config: &RequestCacheControl,
        key: &str,
    ) -> Result<Option<Vec<u8>>, anyhow::Error> {
        if config.read_cache {
            self.lru.get_key(key)
        } else {
            Ok(None)
        }
    }
}

impl<'config> Service<Request<Incoming>> for CachingProxy<'config> {
    type Response = Response<CacheBody<MemCache>>;

    type Error = anyhow::Error;

    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let key = format!("{}::{}", req.uri().path(), req.uri().query().unwrap_or(""));
        let handle_error = |err: anyhow::Error| Box::pin(async move { Err(err) });
        let cache_config = match self.configure_caching(&req) {
            Ok(config) => config,
            Err(err) => return handle_error(err),
        };
        // TODO check VARY
        // TODO nix this unwrap
        match self.read_cache(&cache_config, &key).unwrap() {
            Some(response) => Box::pin(async move {
                let (status, headers, body) = bytes_to_parts(&response);
                let mut builder = Response::builder().status(status);
                for (k, v) in headers.into_iter() {
                    builder = builder.header(k.unwrap(), v);
                }
                Ok(builder
                    .body(CacheBody::Source(Full::new(Bytes::from(Vec::from(body)))))
                    .unwrap())
            }),
            None => {
                let (mut parts, body) = req.into_parts();
                let host = format!("{}:{}", self.config.target_host, self.config.target_port);
                let new_uri = format!(
                    "http://{}/{}?{}",
                    host,
                    parts.uri.path(),
                    parts.uri.query().unwrap_or("")
                );
                parts.uri = new_uri.parse().unwrap();
                parts
                    .headers
                    .insert(hyper::header::HOST, HeaderValue::from_str(&host).unwrap());
                let new_req = Request::from_parts(parts, body);
                let http_client = self.http_client.clone();
                let cache = self.lru.clone();
                Box::pin(async move {
                    let mut conn = match http_client.get().await {
                        Ok(conn) => conn,
                        Err(err) => return Err(err.into()),
                    };
                    match conn.send_request(new_req).await {
                        Ok(response) => {
                            let (mut head, body) = response.into_parts();
                            match Self::cache_response(&cache_config, &head) {
                                Ok(should_cache) => {
                                    if should_cache {
                                        let response_bytes = response_to_bytes(&head);
                                        let sink_body = CacheBody::Sink(IncomingTeeSink::new(
                                            body,
                                            response_bytes,
                                            cache,
                                            key,
                                        ));
                                        head.headers
                                            .insert(CACHE_HEADER, HeaderValue::from_static("miss"));
                                        let response = Response::from_parts(head, sink_body);
                                        Ok(response)
                                    } else {
                                        head.headers
                                            .insert(CACHE_HEADER, HeaderValue::from_static("skip"));
                                        let response =
                                            Response::from_parts(head, CacheBody::Skip(body));
                                        Ok(response)
                                    }
                                }
                                Err(err) => Err(err.into()),
                            }
                        }
                        Err(err) => Err(err.into()),
                    }
                })
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let target_host = "neverssl.com";
    let target_port = 80;

    let http_client = ClientConnectionManager::new(format!("{}:{}", target_host, target_port));
    let pool = Pool::builder().max_open(50).build(http_client);
    // We create a TcpListener and bind it to 127.0.0.1:3000
    let listener = TcpListener::bind(addr).await?;
    let svc = CachingProxy {
        config: Config {
            target_host: Cow::from(target_host),
            target_port,
        },
        lru: Arc::new(StdMutex::new(LruCache::new(NonZeroUsize::new(2).unwrap()))),
        http_client: pool,
    };

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);
        let copy = svc.clone();

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new().serve_connection(io, copy).await {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
