use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;

use bytes::Bytes;

use futures::future;
use futures::future::Future;
use futures::stream::Stream;
use futures::sync::mpsc::unbounded;
use futures::sync::mpsc::UnboundedSender;
use futures::sync::mpsc::UnboundedReceiver;
use futures::sync::oneshot;

use tokio_core::reactor;

use tls_api::TlsConnector;
use tls_api::TlsConnectorBuilder;
use tls_api_stub;

use futures_misc::*;

use error;
use error::Error;
use result::Result;

use solicit::header::*;
use solicit::HttpScheme;
use solicit::StreamId;

use solicit_async::*;

use client_conn::*;
use client_conf::*;
use common::*;
use stream_part::*;
use service::Service;

pub use client_tls::ClientTlsOption;


// Data sent from event loop to Http2Client
struct LoopToClient {
    // used only once to send shutdown signal
    shutdown: ShutdownSignal,
    _loop_handle: reactor::Remote,
    controller_tx: UnboundedSender<ControllerCommand>,
}

pub struct Client {
    loop_to_client: LoopToClient,
    thread_join_handle: Option<thread::JoinHandle<()>>,
    http_scheme: HttpScheme,
}

impl Client {

    pub fn new_plain(host: &str, port: u16, conf: ClientConf) -> Result<Client> {
        // TODO: sync
        // TODO: try connect to all addrs
        let socket_addr = (host, port).to_socket_addrs()?.next().expect("resolve host/port");

        let tls_enabled: ClientTlsOption<tls_api_stub::TlsConnector> = ClientTlsOption::Plain;

        Client::new_expl(&socket_addr, tls_enabled, conf)
    }

    pub fn new_tls<C : TlsConnector>(host: &str, port: u16, conf: ClientConf) -> Result<Client> {
        // TODO: sync
        // TODO: try connect to all addrs
        let socket_addr = (host, port).to_socket_addrs()?.next().expect("resolve host/port");

        let tls_enabled = {
            let mut tls_connector = C::builder()?;

            if C::supports_alpn() {
                // TODO: check negotiated protocol after connect
                tls_connector.set_alpn_protocols(&[b"h2"])?;
            }

            let tls_connector = tls_connector.build()?;

            let tls_connector = Arc::new(tls_connector);
            ClientTlsOption::Tls(host.to_owned(), tls_connector)
        };

        Client::new_expl(&socket_addr, tls_enabled, conf)
    }

    pub fn new_with_cert<C, R>(host: &str, port: u16, mut cert: R, mut private_key: R, conf: ClientConf) -> Result<Client>
        where C : TlsConnector, R : io::Read {

        let socket_addr = (host, port).to_socket_addrs()?.next().expect("resolve host/port");

        let tls_enabled = {
            let mut tls_connector = C::builder()?;

            if C::supports_alpn() {
                // TODO: check negotiated protocol after connect
                tls_connector.set_alpn_protocols(&[b"h2"])?;
            }

            tls_connector.set_certificate(&mut cert)?;
            tls_connector.set_private_key(&mut private_key)?;

            let tls_connector = tls_connector.build()?;

            let tls_connector = Arc::new(tls_connector);
            ClientTlsOption::Tls(host.to_owned(), tls_connector)
        };

        Client::new_expl(&socket_addr, tls_enabled, conf)
    }

    pub fn new_expl<C : TlsConnector>(addr: &SocketAddr, tls: ClientTlsOption<C>, conf: ClientConf) -> Result<Client> {
        // We need some data back from event loop.
        // This channel is used to exchange that data
        let (get_from_loop_tx, get_from_loop_rx) = mpsc::channel();

        let addr = addr.clone();
        let http_scheme = tls.http_scheme();

        // Start event loop.
        let join_handle = thread::Builder::new()
            .name(conf.thread_name.clone().unwrap_or_else(|| "http2-client-loop".to_owned()).to_string())
            .spawn(move || {
                run_client_event_loop(addr, tls, conf, get_from_loop_tx);
            })
            .expect("spawn");

        // Get back call channel and shutdown channel.
        let loop_to_client = get_from_loop_rx.recv()
            .map_err(|_| Error::IoError(io::Error::new(io::ErrorKind::Other, "get response from loop")))?;

        Ok(Client {
            loop_to_client: loop_to_client,
            thread_join_handle: Some(join_handle),
            http_scheme: http_scheme,
        })
    }

    pub fn start_request_simple(
        &self,
        headers: Headers,
        body: Bytes)
            -> Response
    {
        self.start_request(
            headers,
            HttpPartStream::once_bytes(body))
    }

    pub fn start_get(
        &self,
        path: &str,
        authority: &str)
            -> Response
    {
        let headers = Headers(vec![
            Header::new(":method", "GET"),
            Header::new(":path", path.to_owned()),
            Header::new(":authority", authority.to_owned()),
            Header::new(":scheme", self.http_scheme.as_bytes()),
        ]);
        self.start_request_simple(headers, Bytes::new())
    }

    pub fn start_post(
        &self,
        path: &str,
        authority: &str,
        body: Bytes)
            -> Response
    {
        let headers = Headers(vec![
            Header::new(":method", "POST"),
            Header::new(":path", path.to_owned()),
            Header::new(":authority", authority.to_owned()),
            Header::new(":scheme", self.http_scheme.as_bytes()),
        ]);
        self.start_request_simple(headers, body)
    }

    pub fn dump_state(&self) -> HttpFutureSend<ConnectionStateSnapshot> {
        let (tx, rx) = oneshot::channel();
        // ignore error
        drop(self.loop_to_client.controller_tx.send(ControllerCommand::DumpState(tx)));
        Box::new(rx.map_err(|_| error::Error::Other("conn died")))
    }

    pub fn wait_for_connect(&self) -> HttpFutureSend<()> {
        let (tx, rx) = oneshot::channel();
        // ignore error
        drop(self.loop_to_client.controller_tx.send(ControllerCommand::WaitForConnect(tx)));
        Box::new(rx.map_err(|_| error::Error::Other("conn died")).and_then(|r| r))
    }
}

impl Service for Client {
    // TODO: copy-paste with ClientConnection::start_request
    fn start_request(
        &self,
        headers: Headers,
        body: HttpPartStream)
            -> Response
    {
        let (resp_tx, resp_rx) = oneshot::channel();

        let start = StartRequestMessage {
            headers: headers,
            body: body,
            resp_tx: resp_tx,
        };

        if let Err(_) = self.loop_to_client.controller_tx.send(ControllerCommand::StartRequest(start)) {
            return Response::err(error::Error::Other("client controller died"));
        }

        let resp_rx = resp_rx.map_err(|oneshot::Canceled| error::Error::Other("client likely died"));

        let resp_rx = resp_rx.map(|r| r.into_stream_flag());

        let resp_rx = resp_rx.flatten_stream();

        Response::from_stream(resp_rx)
    }}

enum ControllerCommand {
    GoAway,
    StartRequest(StartRequestMessage),
    WaitForConnect(oneshot::Sender<Result<()>>),
    DumpState(oneshot::Sender<ConnectionStateSnapshot>),
}

struct ControllerState<C : TlsConnector> {
    handle: reactor::Handle,
    socket_addr: SocketAddr,
    tls: ClientTlsOption<C>,
    conf: ClientConf,
    // current connection
    conn: Arc<ClientConnection>,
    tx: UnboundedSender<ControllerCommand>,
}

impl<C : TlsConnector> ControllerState<C> {
    fn init_conn(&mut self) {
        let (conn, future) = ClientConnection::new(
            self.handle.clone(),
            &self.socket_addr,
            self.tls.clone(),
            self.conf.clone(),
            CallbacksImpl {
                tx: self.tx.clone(),
            });

        self.handle.spawn(future.map_err(|e| { warn!("client error: {:?}", e); () }));

        self.conn = Arc::new(conn);
    }

    fn iter(mut self, cmd: ControllerCommand) -> ControllerState<C> {
        match cmd {
            ControllerCommand::GoAway => {
                self.init_conn();
            },
            ControllerCommand::StartRequest(start) => {
                if let Err(start) = self.conn.start_request_with_resp_sender(start) {
                    self.init_conn();
                    if let Err(start) = self.conn.start_request_with_resp_sender(start) {
                        let err = error::Error::Other("client died and reconnect failed");
                        // ignore error
                        if let Err(_) = start.resp_tx.send(Response::err(err)) {
                            debug!("called likely died");
                        }
                    }
                }
            }
            ControllerCommand::WaitForConnect(tx) => {
                if let Err(tx) = self.conn.wait_for_connect_with_resp_sender(tx) {
                    self.init_conn();
                    if let Err(tx) = self.conn.wait_for_connect_with_resp_sender(tx) {
                        let err = error::Error::Other("client died and reconnect failed");
                        // ignore error
                        drop(tx.send(Err(err)));
                    }
                }
            }
            ControllerCommand::DumpState(tx) => {
                self.conn.dump_state_with_resp_sender(tx);
            }
        }
        self
    }

    fn run(self, rx: UnboundedReceiver<ControllerCommand>)
        -> HttpFuture<()>
    {
        let rx = rx.map_err(|_| error::Error::Other("channel died"));
        let r = rx.fold(self, |state, cmd| {
            Ok::<_, error::Error>(state.iter(cmd))
        });
        let r = r.map(|_| ());
        Box::new(r)
    }
}

struct CallbacksImpl {
    tx: UnboundedSender<ControllerCommand>,
}

impl ClientConnectionCallbacks for CallbacksImpl {
    fn goaway(&self, _stream_id: StreamId, _error_code: u32) {
        drop(self.tx.send(ControllerCommand::GoAway));
    }
}

// Event loop entry point
fn run_client_event_loop<C : TlsConnector>(
    socket_addr: SocketAddr,
    tls: ClientTlsOption<C>,
    conf: ClientConf,
    send_to_back: mpsc::Sender<LoopToClient>)
{
    // Create an event loop.
    let mut lp: reactor::Core = reactor::Core::new().expect("Core::new");

    // Create a channel to receive shutdown signal.
    let (shutdown_signal, shutdown_future) = shutdown_signal();

    let (controller_tx, controller_rx) = unbounded();

    let (http_conn, conn_future) =
        ClientConnection::new(lp.handle(), &socket_addr, tls.clone(), conf.clone(), CallbacksImpl {
            tx: controller_tx.clone(),
        });

    lp.handle().spawn(conn_future.map_err(|e| { warn!("client error: {:?}", e); () }));

    let init = ControllerState {
        handle: lp.handle(),
        socket_addr: socket_addr.clone(),
        tls: tls,
        conf: conf,
        conn: Arc::new(http_conn),
        tx: controller_tx.clone(),
    };

    let controller_future = init.run(controller_rx);

    // Send channels back to Http2Client
    send_to_back
        .send(LoopToClient {
            shutdown: shutdown_signal,
            _loop_handle: lp.remote(),
            controller_tx: controller_tx,
        })
        .expect("send back");

    let shutdown_future = shutdown_future
        .then(move |_| {
            // Must complete with error,
            // so `join` with this future cancels another future.
            future::failed::<(), _>(Error::Shutdown)
        });

    // Wait for either completion of connection (i. e. error)
    // or shutdown signal.
    let done = controller_future.join(shutdown_future);

    match lp.run(done) {
        Ok(_) => {}
        Err(Error::Shutdown) => {}
        Err(e) => {
            error!("Core::run failed: {:?}", e);
        }
    }
}

// We shutdown the client in the destructor.
impl Drop for Client {
    fn drop(&mut self) {
        self.loop_to_client.shutdown.shutdown();

        // do not ignore errors because we own event loop thread
        self.thread_join_handle.take().expect("handle.take")
            .join().expect("join thread");
    }
}
