use adler32::adler32;
use async_stream::try_stream;
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http::HeaderMap;
use native_tls::TlsConnector;
use reqwest::{Client, ClientBuilder};
use std::fmt::Debug;
use std::sync::Mutex;
use std::time::SystemTime;
use std::{pin::Pin, sync::Arc};
use tokio::sync::RwLock;
use url::Url;
use ureq::{Agent, AgentBuilder,Request, MiddlewareNext,Response, Middleware, Error as UreqError};

use crate::asynchronous::generator::StreamGenerator;
use crate::{asynchronous::transport::AsyncTransport, error::Result, Error};
use crate::transports::{HeadersMiddleware, MiddlewareState} ;
use std::io::Read;
/// An asynchronous polling type. Makes use of the nonblocking reqwest types and
/// methods.
#[derive(Clone)]
pub struct PollingTransport {
    client: Arc<Mutex<Agent>>,
    base_url: Arc<RwLock<Url>>,
    generator: StreamGenerator<Bytes>,
}

impl PollingTransport {
    pub fn new(
        base_url: Url,
        tls_config: Option<TlsConnector>,
        opening_headers: Option<HeaderMap>,
    ) -> Self {
        // let client = match (tls_config, opening_headers) {
        //     (Some(config), Some(map)) => ClientBuilder::new()
        //         .use_preconfigured_tls(config)
        //         .default_headers(map)
        //         .build()
        //         .unwrap(),
        //     (Some(config), None) => ClientBuilder::new()
        //         .use_preconfigured_tls(config)
        //         .build()
        //         .unwrap(),
        //     (None, Some(map)) => ClientBuilder::new().default_headers(map).build().unwrap(),
        //     (None, None) => Client::new(),
        // };

        // let mut url = base_url;
        // url.query_pairs_mut().append_pair("transport", "polling");

        // PollingTransport {
        //     client: client.clone(),
        //     base_url: Arc::new(RwLock::new(url.clone())),
        //     generator: StreamGenerator::new(Self::stream(url, client)),
        // }

        let agent : Agent =  match (tls_config, opening_headers) {
            (Some(config), Some(map)) =>{
                let headers = Arc::new(Mutex::new(MiddlewareState{
                    headers: map.clone()
                }));
                let connector = Arc::new(config);
                let agent = AgentBuilder::new()
                .tls_connector(connector)
                .middleware(HeadersMiddleware(headers.clone()))
                .build();
                agent
            },
            (Some(config), None) =>{ 
                let connector = Arc::new(config);
                AgentBuilder::new()
                    .tls_connector(connector)
                    .build()
            },
            (None, Some(map)) => {
                let headers = Arc::new(Mutex::new(MiddlewareState{
                    headers: map.clone()
                }));
                AgentBuilder::new().middleware(HeadersMiddleware(headers.clone())).build()
            },
            (None, None) => Agent::new(),
        };

        let mut url = base_url;
        url.query_pairs_mut().append_pair("transport", "polling");

        PollingTransport {
            client: Arc::new(Mutex::new(agent.clone())),
            base_url: Arc::new(RwLock::new(url.clone())),
            generator: StreamGenerator::new(Self::stream(url.clone(), agent.clone()))
        }
    }

    fn address(mut url: Url) -> Result<Url> {
        let reader = format!("{:#?}", SystemTime::now());
        let hash = adler32(reader.as_bytes()).unwrap();
        url.query_pairs_mut().append_pair("t", &hash.to_string());
        Ok(url)
    }

    async fn client_call(client: Agent, url: &str) -> Result<Response> {
        client
            .get(url)
            .call()
            .map_err(|e|{crate::Error::IncompletePacket() } )
    }
    fn send_request(url: Url, client: Agent) -> impl Stream<Item = Result<Response>> {
        try_stream! {
            let address = Self::address(url);
            yield  Self::client_call(client.clone(), address?.as_str()).await?;
        }
    }

    fn stream(
        url: Url,
        client: Agent,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + 'static + Send>> {
        Box::pin(try_stream! {
            loop {
                for await elem in Self::send_request(url.clone(), client.clone()) {
                    let response = elem?;
                    let len = response.header("Content-Length")
                        .and_then(|s| s.parse::<usize>().ok()).unwrap();

                    let mut bytes: Vec<u8> = Vec::with_capacity(len);
                    response.into_reader().read_to_end(&mut bytes).unwrap();
                    let bytes = bytes::Bytes::from(bytes);
                    yield bytes;
                    // for await bytes in elem?.bytes_stream() {
                    //     yield bytes?;
                    // }
                }
            }
        })
    }
}

impl Stream for PollingTransport {
    type Item = Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.generator.poll_next_unpin(cx)
    }
}

#[async_trait]
impl AsyncTransport for PollingTransport {
    async fn emit(&self, data: Bytes, is_binary_att: bool) -> Result<()> {
        let data_to_send = if is_binary_att {
            // the binary attachment gets `base64` encoded
            let mut packet_bytes = BytesMut::with_capacity(data.len() + 1);
            packet_bytes.put_u8(b'b');

            let encoded_data = base64::encode(data.clone());
            packet_bytes.put(encoded_data.as_bytes());

            packet_bytes.freeze()
        } else {
            data.clone()
        };

        let client = self.client.lock().unwrap().clone();
        let status = client
            .post(self.address().await?.as_str())
            .send_bytes(&data_to_send)
            .unwrap()
            .status();
        if status != 200 {
            let error = Error::IncompleteHttp(status);
            return Err(error);
        }

        Ok(())
    }

    async fn base_url(&self) -> Result<Url> {
        Ok(self.base_url.read().await.clone())
    }

    async fn set_base_url(&self, base_url: Url) -> Result<()> {
        let mut url = base_url;
        if !url
            .query_pairs()
            .any(|(k, v)| k == "transport" && v == "polling")
        {
            url.query_pairs_mut().append_pair("transport", "polling");
        }
        *self.base_url.write().await = url;
        Ok(())
    }
}

impl Debug for PollingTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollingTransport")
            .field("client", &self.client)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::asynchronous::transport::AsyncTransport;

    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn polling_transport_base_url() -> Result<()> {
        let url = crate::test::engine_io_server()?.to_string();
        let transport = PollingTransport::new(Url::from_str(&url[..]).unwrap(), None, None);
        assert_eq!(
            transport.base_url().await?.to_string(),
            url.clone() + "?transport=polling"
        );
        transport
            .set_base_url(Url::parse("https://127.0.0.1")?)
            .await?;
        assert_eq!(
            transport.base_url().await?.to_string(),
            "https://127.0.0.1/?transport=polling"
        );
        assert_ne!(transport.base_url().await?.to_string(), url);

        transport
            .set_base_url(Url::parse("http://127.0.0.1/?transport=polling")?)
            .await?;
        assert_eq!(
            transport.base_url().await?.to_string(),
            "http://127.0.0.1/?transport=polling"
        );
        assert_ne!(transport.base_url().await?.to_string(), url);
        Ok(())
    }
}
