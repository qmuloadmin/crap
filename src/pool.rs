use std::{io::Error, sync::Arc};

use async_trait::async_trait;
use hyper::{body::Incoming, client::conn::http1::SendRequest};
use hyper_util::rt::TokioIo;
use mobc::Manager;
use tokio::net::TcpStream;

#[derive(Clone)]
pub(crate) struct ClientConnectionManager {
    host: Arc<String>,
}

impl ClientConnectionManager {
    pub(crate) fn new(host: String) -> Self {
        Self {
            host: Arc::new(host),
        }
    }
}

#[derive(Debug)]
pub struct ClientPoolError {
    context: String,
    cause: Box<(dyn std::error::Error + 'static + Send + Sync)>,
}

impl std::fmt::Display for ClientPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.cause)
    }
}

impl std::error::Error for ClientPoolError {
    fn cause(&self) -> Option<&dyn std::error::Error> {
        Some(&**&self.cause)
    }
}

impl From<Error> for ClientPoolError {
    fn from(value: Error) -> Self {
        ClientPoolError {
            context: format!("I/O Error"),
            cause: Box::new(value),
        }
    }
}

impl From<hyper::Error> for ClientPoolError {
    fn from(value: hyper::Error) -> Self {
        ClientPoolError {
            context: format!("HTTP Error"),
            cause: Box::new(value),
        }
    }
}

#[async_trait]
impl Manager for ClientConnectionManager {
    type Connection = SendRequest<Incoming>;

    type Error = ClientPoolError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let stream = TcpStream::connect(self.host.as_str()).await?;
        let io = TokioIo::new(stream);
        let (sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("connection error: {}", err)
            }
        });
        Ok(sender)
    }

    fn validate(&self, conn: &mut Self::Connection) -> bool {
        !conn.is_closed()
    }

    async fn check(&self, mut conn: Self::Connection) -> Result<Self::Connection, Self::Error> {
        conn.ready().await?;
        Ok(conn)
    }
}
