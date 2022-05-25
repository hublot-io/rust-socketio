use crate::error::{Error, Result};
use crate::transport::Transport;
use bytes::{BufMut, Bytes, BytesMut};
use native_tls::TlsConnector;
use reqwest::{
    blocking::{Client, ClientBuilder},
    header::HeaderMap,
};
use ureq::{Agent, AgentBuilder,Request, MiddlewareNext,Response, Middleware, Error as UreqError};
use std::time::Duration;
use std::io::Read;
use std::sync::{Arc, Mutex, RwLock};
use url::Url;

pub struct MiddlewareState {
    pub headers: HeaderMap
}
pub struct HeadersMiddleware(pub Arc<Mutex<MiddlewareState>>);

 impl Middleware for HeadersMiddleware {
    fn handle(&self, mut request: Request, next: MiddlewareNext) -> std::result::Result<Response, UreqError>{
            let req = request.clone();
        // inner scope so we release the lock asap
            let state = self.0.lock().unwrap();
            let map = &state.headers;
            // TODO: implem custom headers
            
            // for h in map.clone() {

            //     if let Some(head) = h.0 {
            //         match h.1.to_str(){
            //             Ok(val_as_str) =>{ 
            //                 req.set(head.as_str(),val_as_str);
            //             },
            //             Err(_e) => {
            //                 println!("Cannot set heander {:?} into this request, value might be invalid", head);
            //             }
            //         };
            //     };
            // }; 
        next.handle(req)
    }
}

#[derive(Debug, Clone)]
pub struct PollingTransport {
    client: Arc<Mutex<Agent>>,
    base_url: Arc<RwLock<Url>>,
}

impl PollingTransport {
    /// Creates an instance of `PollingTransport`.
    pub fn new(
        base_url: Url,
        tls_config: Option<TlsConnector>,
        opening_headers: Option<HeaderMap>,
    ) -> Self {
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
            client: Arc::new(Mutex::new(agent)),
            base_url: Arc::new(RwLock::new(url)),
        }
    }
}

impl Transport for PollingTransport {
    fn emit(&self, data: Bytes, is_binary_att: bool) -> Result<()> {
        let data_to_send = if is_binary_att {
            // the binary attachment gets `base64` encoded
            let mut packet_bytes = BytesMut::with_capacity(data.len() + 1);
            packet_bytes.put_u8(b'b');

            let encoded_data = base64::encode(data);
            packet_bytes.put(encoded_data.as_bytes());

            packet_bytes.freeze()
        } else {
            data
        };
        let client = self.client.lock()?;
        let status = client
            .post(self.address()?.as_str()) 
            .send_bytes(&data_to_send)
            .expect("[Transport] cannot emit byers")
            .status();

        drop(client);

        if status != 200 {
            let error = Error::IncompleteHttp(status);
            return Err(error);
        }

        Ok(())
    }

    fn poll(&self) -> Result<Bytes> {
        let response = self.client.lock()?.get(self.address()?.as_str()).call().expect("[Transport] polling error");
        let len = response.header("Content-Length")
            .and_then(|s| s.parse::<usize>().ok()).unwrap();

        let mut bytes: Vec<u8> = Vec::with_capacity(len);
        response.into_reader().read_to_end(&mut bytes)?;
        let resp:  bytes::Bytes = bytes::Bytes::from(bytes); 
        Ok(resp)
    }

    fn base_url(&self) -> Result<Url> {
        Ok(self.base_url.read()?.clone())
    }

    fn set_base_url(&self, base_url: Url) -> Result<()> {
        let mut url = base_url;
        if !url
            .query_pairs()
            .any(|(k, v)| k == "transport" && v == "polling")
        {
            url.query_pairs_mut().append_pair("transport", "polling");
        }
        *self.base_url.write()? = url;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::str::FromStr;
    #[test]
    fn polling_transport_base_url() -> Result<()> {
        let url = crate::test::engine_io_server()?.to_string();
        let transport = PollingTransport::new(Url::from_str(&url[..]).unwrap(), None, None);
        assert_eq!(
            transport.base_url()?.to_string(),
            url.clone() + "?transport=polling"
        );
        transport.set_base_url(Url::parse("https://127.0.0.1")?)?;
        assert_eq!(
            transport.base_url()?.to_string(),
            "https://127.0.0.1/?transport=polling"
        );
        assert_ne!(transport.base_url()?.to_string(), url);

        transport.set_base_url(Url::parse("http://127.0.0.1/?transport=polling")?)?;
        assert_eq!(
            transport.base_url()?.to_string(),
            "http://127.0.0.1/?transport=polling"
        );
        assert_ne!(transport.base_url()?.to_string(), url);
        Ok(())
    }

    #[test]
    fn transport_debug() -> Result<()> {
        let mut url = crate::test::engine_io_server()?;
        let transport =
            PollingTransport::new(Url::from_str(&url.to_string()[..]).unwrap(), None, None);
        url.query_pairs_mut().append_pair("transport", "polling");
        assert_eq!(format!("PollingTransport {{ client: {:?}, base_url: RwLock {{ data: {:?}, poisoned: false, .. }} }}", transport.client, url), format!("{:?}", transport));
        let test: Box<dyn Transport> = Box::new(transport);
        assert_eq!(
            format!("Transport(base_url: Ok({:?}))", url),
            format!("{:?}", test)
        );
        Ok(())
    }
}
