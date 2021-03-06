//! Test client and server TLS connected with TLS.

extern crate bytes;
extern crate futures;
extern crate tls_api;
extern crate tls_api_native_tls;
extern crate tokio_core;
extern crate tokio_tls_api;
extern crate httpbis;
extern crate log;
extern crate env_logger;

use bytes::Bytes;

use std::sync::Arc;
use std::net::SocketAddr;

use futures::future::Future;

use httpbis::solicit::header::Headers;
use httpbis::*;
use httpbis::message::SimpleHttpMessage;

use tls_api::Certificate;
use tls_api_native_tls::TlsAcceptor;
use tls_api_native_tls::TlsAcceptorBuilder;
use tls_api_native_tls::TlsConnector;
use tls_api::TlsAcceptorBuilder as tls_api_TlsAcceptorBuilder;
use tls_api::TlsConnector as tls_api_TlsConnector;
use tls_api::TlsConnectorBuilder;


fn test_tls_acceptor() -> TlsAcceptor {
    let pkcs12 = include_bytes!("identity.p12");
    let builder = TlsAcceptorBuilder::from_pkcs12(pkcs12, "mypass").unwrap();
    builder.build().unwrap()
}

fn test_tls_connector() -> TlsConnector {
    let root_ca = include_bytes!("root-ca.der");
    let root_ca = Certificate::from_der(root_ca.to_vec());

    let mut builder = TlsConnector::builder().unwrap();
    builder.add_root_certificate(root_ca).expect("add_root_certificate");
    builder.build().unwrap()
}


#[test]
fn tls() {
    struct ServiceImpl {
    }

    impl Service for ServiceImpl {
        fn start_request(&self, _headers: Headers, _req: HttpPartStream) -> Response {
            Response::headers_and_bytes(Headers::ok_200(), Bytes::from("hello"))
        }
    }

    let server = Server::new_tls_single_thread(
        "[::1]:0".parse::<SocketAddr>().unwrap(),
        test_tls_acceptor(),
        Default::default(),
        ServiceImpl {}).expect("server");

    let client: Client = Client::new_expl(
        server.local_addr(),
        ClientTlsOption::Tls("foobar.com".to_owned(), Arc::new(test_tls_connector())),
        Default::default())
            .expect("http client");

    let resp: SimpleHttpMessage = client.start_get("/hi", "localhost").collect().wait().unwrap();
    assert_eq!(200, resp.headers.status());
    assert_eq!(&b"hello"[..], &resp.body[..]);
}
