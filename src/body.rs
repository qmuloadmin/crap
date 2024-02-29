use http_body_util::Full;
use hyper::body::Body;
use hyper::body::Bytes;
use hyper::body::Frame;
use hyper::body::Incoming;
use std::pin::Pin;
use std::task::Poll;

use crate::cache;
use crate::cache::ResponseCache;

macro_rules! poll_anyhow {
	// Converts a hyper Body::poll_frame returning errors of any type into errors of anyhow type
    ($body:ident, $cx:ident) => {
        match Body::poll_frame(Pin::new($body), $cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(opt) => Poll::Ready(match opt {
                Some(result) => Some(match result {
                    Ok(frame) => Ok(frame),
                    Err(err) => Err(err.into()),
                }),
                None => None,
            }),
        }
    };
}

pub(crate) enum CacheBody<Cache: ResponseCache> {
    //Sink indicates the response is coming from a server and it is being cached (we are the sink of the response)
    Sink(IncomingTeeSink<Cache>),
    //Source indicates the response is coming from our cache (we are the source of the response)
    Source(Full<Bytes>),
    //Skip indicates the response is not being cached due to various rules, e.g. Cache-Control, or configuration
    Skip(Incoming),
}

impl<Cache: cache::ResponseCache> Body for CacheBody<Cache> {
    type Data = Bytes;

    type Error = anyhow::Error;

    fn is_end_stream(&self) -> bool {
        match &self {
            &CacheBody::Sink(sink) => sink.is_end_stream(),
            &CacheBody::Source(source) => source.is_end_stream(),
            &CacheBody::Skip(inc) => inc.is_end_stream(),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match &self {
            &CacheBody::Sink(sink) => sink.size_hint(),
            &CacheBody::Source(source) => source.size_hint(),
            &CacheBody::Skip(inc) => inc.size_hint(),
        }
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            CacheBody::Skip(inc) => poll_anyhow!(inc, cx),
            CacheBody::Sink(sink) => Body::poll_frame(Pin::new(sink), cx),
            CacheBody::Source(source) => poll_anyhow!(source, cx)
        }
    }
}

pub(crate) struct IncomingTeeSink<Cache: ResponseCache> {
    inc: Incoming,
    sink: Option<Vec<u8>>,
    frame_data: Vec<Bytes>,
    cache: Option<Cache>,
    key: String,
}

impl<Cache: cache::ResponseCache> IncomingTeeSink<Cache> {
    pub fn new(inc: Incoming, sink: Vec<u8>, cache: Cache, key: String) -> Self {
        Self {
            inc,
            sink: Some(sink),
            frame_data: Vec::with_capacity(5),
            cache: Some(cache),
            key,
        }
    }
}

impl<Cache: cache::ResponseCache> Body for IncomingTeeSink<Cache> {
    type Data = Bytes;

    type Error = anyhow::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let unpinned = Pin::get_mut(self);
        // TODO specify some max content length where we stop caching
        match Body::poll_frame(Pin::new(&mut unpinned.inc), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(opt) => Poll::Ready(match opt {
                None => None,
                Some(result) => match result {
                    Err(err) => Some(Err(err.into())),
                    Ok(frame) => {
                        if frame.is_data() {
                            let data = frame.into_data().unwrap();
                            unpinned.frame_data.push(data.clone());
                            Some(Ok(Frame::data(data)))
                        } else {
                            Some(Ok(frame))
                        }
                    }
                },
            }),
        }
    }

    fn is_end_stream(&self) -> bool {
        Body::is_end_stream(&self.inc)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        Body::size_hint(&self.inc)
    }
}

impl<Cache: cache::ResponseCache> Drop for IncomingTeeSink<Cache> {
    fn drop(&mut self) {
		let mut sink = self.sink.take().unwrap();
		let cache = self.cache.take().unwrap();
        if self.inc.is_end_stream() {
            for frame in self.frame_data.iter() {
                for byte in frame.into_iter() {
                    sink.push(*byte);
                }
            }
            cache.store_key(self.key.clone(), sink);
        }
    }
}
