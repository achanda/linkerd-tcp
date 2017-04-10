use futures::{Async, Future, Poll, Sink, Stream};
use futures::sync::{BiLock, mpsc};
use hyper::Client;
use hyper::server::Http;
use rustls;
use rustls::ResolvesServerCert;
use std::boxed::Box;
use std::cell::RefCell;
use std::collections::{VecDeque, HashMap};
use std::fs::File;
use std::io::{self, BufReader};
use std::net::{self, SocketAddr};
use std::rc::Rc;
use std::time::Duration;
use tacho::{self, Tacho};
use tokio_core::net::TcpListener;
use tokio_core::reactor::{Core, Handle};
use tokio_timer::Timer as TokioTimer;

mod admin_http;
mod sni;
pub mod config;

use WeightedAddr;
use lb::{Balancer, Acceptor, Connector, PlainAcceptor, PlainConnector, SecureAcceptor,
         SecureConnector};
use namerd;
use self::config::*;
use self::sni::Sni;

const DEFAULT_BUFFER_SIZE: usize = 8 * 1024;
const DEFAULT_MAX_WAITERS: usize = 8;
const DEFAULT_NAMERD_SECONDS: u64 = 60;
const DEFAULT_METRICS_SECONDS: u64 = 10;

fn default_admin_addr() -> net::SocketAddr {
    "0.0.0.0:9989".parse().unwrap()
}

/// Creates two reactor-aware runners from a configuration.
///
/// Each runner takes a Handle and produces a `Future`, which should be passed to `run`
/// which completes when the thread should stop running.
pub fn configure(app: AppConfig) -> (Admin, Proxies) {
    let transfer_buf = {
        let sz = app.buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE);
        Rc::new(RefCell::new(vec![0;sz]))
    };

    let Tacho { metrics, aggregator, report } = Tacho::default();

    let mut namerds = VecDeque::new();
    let mut proxies = VecDeque::new();
    let mut proxy_configs = app.proxies;
    for _ in 0..proxy_configs.len() {
        let ProxyConfig { label, namerd, servers, client, max_waiters, .. } = proxy_configs.pop()
            .unwrap();
        let (addrs_tx, addrs_rx) = mpsc::channel(1);
        namerds.push_back(Namerd {
            config: namerd,
            sender: addrs_tx,
            metrics: metrics.clone(),
        });
        proxies.push_back(Proxy {
            client: client,
            server: ProxyServer {
                label: label,
                addrs: Box::new(addrs_rx.fuse()),
                servers: servers,
                buf: transfer_buf.clone(),
                max_waiters: max_waiters.unwrap_or(DEFAULT_MAX_WAITERS),
                metrics: metrics.clone(),
            },
        });
    }

    let addr = app.admin
        .as_ref()
        .and_then(|a| a.addr)
        .unwrap_or_else(default_admin_addr);
    let interval_s = app.admin
        .as_ref()
        .and_then(|a| a.metrics_interval_secs)
        .unwrap_or(DEFAULT_METRICS_SECONDS);
    let admin = Admin {
        addr: addr,
        metrics_interval: Duration::from_secs(interval_s),
        namerds: namerds,
        aggregator: aggregator,
        report: report,
    };
    let proxies = Proxies { proxies: proxies };
    (admin, proxies)
}

pub trait Loader: Sized {
    type Run: Future<Item = (), Error = io::Error>;
    fn load(self, handle: Handle) -> io::Result<(SocketAddr, Self::Run)>;
}
pub trait Runner: Sized {
    fn run(self) -> io::Result<()>;
}

impl<L: Loader> Runner for L {
    fn run(self) -> io::Result<()> {
        let mut core = Core::new()?;
        let (_, fut) = self.load(core.handle())?;
        core.run(fut)
    }
}

pub struct Admin {
    addr: net::SocketAddr,
    metrics_interval: Duration,
    namerds: VecDeque<Namerd>,
    aggregator: tacho::Aggregator,
    report: BiLock<tacho::Report>,
}
impl Loader for Admin {
    type Run = Running;
    fn load(self, handle: Handle) -> io::Result<(SocketAddr, Running)> {
        let mut running = Running::new();
        {
            let mut namerds = self.namerds;
            for _ in 0..namerds.len() {
                let (_, f) = namerds.pop_front().unwrap().load(handle.clone())?;
                running.register(f.map_err(|_| io::ErrorKind::Other.into()));
            }
        }
        let metrics_export = Rc::new(RefCell::new(String::new()));
        {
            let metrics_export = metrics_export.clone();
            running.register(self.aggregator.map_err(|_| io::ErrorKind::Other.into()));

            let reporting = TokioTimer::default()
                .interval(self.metrics_interval)
                .map_err(|_| {})
                .fold(self.report, move |m, _| {
                    let metrics_export = metrics_export.clone();
                    m.lock().map(move |mut m| {
                        let mut export = metrics_export.borrow_mut();
                        *export = tacho::prometheus::format(&m);
                        m.reset();
                        m.unlock()
                    })
                })
                .map(|_| {})
                .map_err(|_| io::ErrorKind::Other.into());
            running.register(reporting);
        }
        {
            // TODO make this addr configurable.
            let listener = {
                println!("Listening on http://{}.", self.addr);
                TcpListener::bind(&self.addr, &handle).expect("unable to listen")
            };

            let http = Http::new();
            let srv = listener.incoming().for_each(move |(socket, addr)| {
                let server = admin_http::Server::new(metrics_export.clone());
                http.bind_connection(&handle, socket, addr, server);
                Ok(())
            });
            running.register(srv);
        }
        Ok((self.addr, running))
    }
}


pub struct Namerd {
    pub config: NamerdConfig,
    pub sender: mpsc::Sender<Vec<WeightedAddr>>,
    pub metrics: tacho::Metrics,
}
impl Loader for Namerd {
    type Run = Box<Future<Item = (), Error = io::Error>>;
    fn load(self, handle: Handle) -> io::Result<(SocketAddr, Self::Run)> {
        let path = self.config.path;
        let addr = self.config.addr;
        let interval_secs = self.config.interval_secs.unwrap_or(DEFAULT_NAMERD_SECONDS);
        let interval = Duration::from_secs(interval_secs);
        let ns = self.config.namespace.clone().unwrap_or_else(|| "default".into());
        info!("Updating {} in {} from {} every {}s",
              path,
              ns,
              addr,
              interval_secs);
        let addrs = {
            let client = Client::new(&handle);
            namerd::resolve(self.config.addr, client, interval, &ns, &path, self.metrics)
        };
        let driver = {
            let sink = self.sender.sink_map_err(|_| error!("sink error"));
            addrs.forward(sink).map_err(|_| io::ErrorKind::Other.into()).map(|_| {})
        };
        Ok((addr, Box::new(driver)))
    }
}

pub struct Proxies {
    proxies: VecDeque<Proxy>,
}
impl Loader for Proxies {
    type Run = Running;
    fn load(self, handle: Handle) -> io::Result<(SocketAddr, Running)> {
        let mut running = Running::new();
        let mut proxies = self.proxies;
        let mut addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        for _ in 0..proxies.len() {
            let p = proxies.pop_front().unwrap();
            let (_addr, f) = p.load(handle.clone())?;
            addr = _addr;
            running.register(f);
        }
        Ok((addr, running))
    }
}

pub struct Proxy {
    pub client: Option<ClientConfig>,
    pub server: ProxyServer,
}
impl Loader for Proxy {
    type Run = Running;
    fn load(self, handle: Handle) -> io::Result<(SocketAddr, Running)> {
        match self.client.and_then(|c| c.tls) {
            None => {
                let conn = PlainConnector::new(handle.clone());
                let f = self.server.load(&handle, conn).expect("b");
                Ok(f)
            }
            Some(ref c) => {
                let mut tls = rustls::ClientConfig::new();
                if let Some(ref certs) = c.trust_certs {
                    for p in certs {
                        let f = File::open(p).expect("cannot open certificate file");
                        tls.root_store
                            .add_pem_file(&mut BufReader::new(f))
                            .expect("certificate error");
                    }
                };
                let conn = SecureConnector::new(c.dns_name.clone(), tls, handle.clone());
                let f = self.server.load(&handle, conn).expect("a");
                Ok(f)
            }
        }
    }
}

pub struct ProxyServer {
    pub label: String,
    pub servers: Vec<ServerConfig>,
    pub addrs: Box<Stream<Item = Vec<WeightedAddr>, Error = ()>>,
    pub buf: Rc<RefCell<Vec<u8>>>,
    pub max_waiters: usize,
    pub metrics: tacho::Metrics,
}
impl ProxyServer {
    fn load<C>(self, handle: &Handle, conn: C) -> io::Result<(SocketAddr, Running)>
        where C: Connector + 'static
    {
        let addrs = self.addrs.map_err(|_| io::ErrorKind::Other.into());
        let metrics = self.metrics.clone().labeled("proxy".into(), self.label.into());
        let bal = Balancer::new(addrs, conn, self.buf.clone(), metrics.clone())
            .into_shared(self.max_waiters, handle.clone());

        // Placeholder for our local listening SocketAddr.
        let mut local_addr: SocketAddr = "127.0.0.1:0".parse().expect("unable to parse addr");

        // TODO scope/tag stats for servers.

        let mut running = Running::new();
        for s in &self.servers {
            let handle = handle.clone();
            let bal = bal.clone();
            match *s {
                ServerConfig::Tcp { ref addr } => {
                    let metrics = metrics.clone().labeled("srv".into(), format!("{}", addr));
                    let acceptor = PlainAcceptor::new(handle, metrics);
                    let (bound_addr, forwarder) = acceptor.accept(addr);
                    local_addr = bound_addr;
                    let f = forwarder.forward(bal).map(|_| {});
                    running.register(f);
                }
                ServerConfig::Tls { ref addr,
                                    ref alpn_protocols,
                                    ref default_identity,
                                    ref identities,
                                    .. } => {
                    let mut tls = rustls::ServerConfig::new();
                    tls.cert_resolver = load_cert_resolver(identities, default_identity);
                    if let Some(ref protos) = *alpn_protocols {
                        tls.set_protocols(protos);
                    }

                    let metrics = metrics.clone().labeled("srv".into(), format!("{}", addr));
                    let acceptor = SecureAcceptor::new(handle, tls, metrics);
                    let (bound_addr, forwarder) = acceptor.accept(addr);
                    local_addr = bound_addr;
                    let f = forwarder.forward(bal).map(|_| {});
                    running.register(f);
                }
            }
        }
        Ok((local_addr, running))
    }
}

fn load_cert_resolver(ids: &Option<HashMap<String, TlsServerIdentity>>,
                      def: &Option<TlsServerIdentity>)
                      -> Box<ResolvesServerCert> {
    let mut is_empty = def.is_some();
    if let Some(ref ids) = *ids {
        is_empty = is_empty && ids.is_empty();
    }
    if is_empty {
        panic!("No TLS server identities specified");
    }

    Box::new(Sni::new(ids, def))
}

/// Tracks a list of `F`-typed `Future`s until are complete.
pub struct Running(VecDeque<Box<Future<Item = (), Error = io::Error>>>);
impl Running {
    fn new() -> Running {
        Running(VecDeque::new())
    }

    fn register<F>(&mut self, f: F)
        where F: Future<Item = (), Error = io::Error> + 'static
    {
        self.0.push_back(Box::new(f))
    }
}
impl Future for Running {
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<(), io::Error> {
        let sz = self.0.len();
        trace!("polling {} running", sz);
        for i in 0..sz {
            let mut f = self.0.pop_front().unwrap();
            trace!("polling runner {}", i);
            if f.poll()? == Async::NotReady {
                trace!("runner {} not ready", i);
                self.0.push_back(f);
            } else {
                trace!("runner {} finished", i);
            }
        }
        if self.0.is_empty() {
            trace!("runner finished");
            Ok(Async::Ready(()))
        } else {
            trace!("runner not finished");
            Ok(Async::NotReady)
        }
    }
}
