#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use mio::tcp::*;
use std::io::{self,Read,Write,ErrorKind};
use mio::*;
use bytes::{Buf,ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use std::collections::HashMap;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use openssl::ssl::{SslContext, SslMethod, Ssl, NonblockingSslStream, ServerNameCallback, ServerNameCallbackData};
use openssl::ssl::error::NonblockingSslError;
use openssl::x509::X509FileType;

use parser::http11::{HttpState,RequestState,ResponseState,parse_request_until_stop};
use network::buffer::Buffer;
use network::{ClientResult,ServerMessage,ConnectionError,ProxyOrder};
use network::proxy::{Server,ProxyConfiguration,ProxyClient};
use messages::{Command,TlsFront};
use network::http::{HttpProxy,Client,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult};

type BackendToken = Token;

type ClientToken = Token;

pub struct ServerConfiguration {
  listener:        TcpListener,
  address:         SocketAddr,
  instances:       HashMap<String, Vec<SocketAddr>>,
  fronts:          HashMap<String, Vec<TlsFront>>,
  default_cert:    String,
  default_context: SslContext,
  contexts:        Rc<RefCell<HashMap<String, SslContext>>>,
  tx:              mpsc::Sender<ServerMessage>,
  answers:         DefaultAnswers,
}

impl ServerConfiguration {
  pub fn new(address: SocketAddr, tx: mpsc::Sender<ServerMessage>, event_loop: &mut EventLoop<TlsServer>) -> io::Result<ServerConfiguration> {
    let contexts = HashMap::new();

    let mut context = SslContext::new(SslMethod::Tlsv1).unwrap();
    //let mut context = SslContext::new(SslMethod::Sslv3).unwrap();
    context.set_certificate_file("assets/certificate.pem", X509FileType::PEM);
    context.set_private_key_file("assets/key.pem", X509FileType::PEM);

    fn servername_callback(ssl: &mut Ssl, ad: &mut i32) -> i32 {
      trace!("GOT SERVER NAME: {:?}", ssl.get_servername());
      0
    }
    //context.set_servername_callback(Some(servername_callback as ServerNameCallback));

    fn servername_callback_s(ssl: &mut Ssl, ad: &mut i32, data: &Rc<RefCell<HashMap<String, SslContext>>>) -> i32 {
      let mut contexts = data.borrow_mut();

      if let Some(servername) = ssl.get_servername() {
        trace!("looking for context for {:?}", servername);
        //println!("contexts: {:?}", *contexts);
        let opt_ctx = contexts.remove(&servername);
        if let Some(ctx) = opt_ctx {
          let context = ssl.set_ssl_context(&ctx);
          mem::forget(ctx);
          contexts.insert(String::from(servername), context);
        }
      }
      0
    }

    let rc_ctx = Rc::new(RefCell::new(contexts));
    let store_contexts = rc_ctx.clone();
    context.set_servername_callback_with_data(
      servername_callback_s as ServerNameCallbackData<Rc<RefCell<HashMap<String, SslContext>>>>,
      store_contexts
    );

    match TcpListener::bind(&address) {
      Ok(listener) => {
        event_loop.register(&listener, Token(0), EventSet::readable(), PollOpt::level());
        Ok(ServerConfiguration {
          listener:        listener,
          address:         address,
          instances:       HashMap::new(),
          fronts:          HashMap::new(),
          default_cert:    String::from("lolcatho.st"),
          default_context: context,
          contexts:        rc_ctx,
          tx:              tx,
          answers:         DefaultAnswers {
            NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
            ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
          }
        })
      },
      Err(e) => {
        error!("could not create listener {:?}: {:?}", address, e);
        Err(e)
      }
    }
  }

  pub fn add_http_front(&mut self, http_front: TlsFront, event_loop: &mut EventLoop<TlsServer>) {
    let mut ctx = SslContext::new(SslMethod::Tlsv1).unwrap();
    ctx.set_certificate_file(&http_front.cert_path, X509FileType::PEM);
    ctx.set_private_key_file(&http_front.key_path, X509FileType::PEM);
    let hostname = http_front.hostname.clone();

    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    if self.fronts.get(&http_front.hostname).is_none() {
      self.fronts.insert(http_front.hostname, vec![front3]);
    }

    self.contexts.borrow_mut().insert(hostname, ctx);
  }

  pub fn remove_http_front(&mut self, front: TlsFront, event_loop: &mut EventLoop<TlsServer>) {
    info!("removing http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<TlsServer>) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
        addrs.push(*instance_address);
    }

    if self.instances.get(app_id).is_none() {
      self.instances.insert(String::from(app_id), vec![*instance_address]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<TlsServer>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|addr| addr != instance_address);
      } else {
        error!("Instance was already removed");
      }
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsFront> {
    if let Some(http_fronts) = self.fronts.get(host) {
      let matching_fronts = http_fronts.iter().filter(|f| uri.starts_with(&f.path_begin)); // ToDo match on uri
      let mut front = None;

      for f in matching_fronts {
        if front.is_none() {
          front = Some(f);
        }

        if let Some(ff) = front {
          if f.path_begin.len() > ff.path_begin.len() {
            front = Some(f)
          }
        }
      }
      front
    } else {
      None
    }
  }

  pub fn backend_from_request(&self, client: &mut Client<NonblockingSslStream<TcpStream>>, host: &str, uri: &str) -> Result<SocketAddr,ConnectionError> {
    trace!("looking for backend for host: {}", host);
    let real_host = if let Some(h) = host.split(":").next() {
      h
    } else {
      host
    };
    trace!("looking for backend for real host: {}", host);

    if let Some(tls_front) = self.frontend_from_request(real_host, uri) {
      // ToDo round-robin on instances
      if let Some(app_instances) = self.instances.get(&tls_front.app_id) {
        let rnd = random::<usize>();
        let idx = rnd % app_instances.len();
        info!("Connecting {} -> {:?}", host, app_instances.get(idx));
        app_instances.get(idx).map(|& addr| addr).ok_or(ConnectionError::NoBackendAvailable)
      } else {
        // FIXME: should send 503 here
        client.set_answer(&self.answers.ServiceUnavailable);
        Err(ConnectionError::NoBackendAvailable)
      }
    } else {
      // FIXME: should send 404 here
      client.set_answer(&self.answers.NotFound);
      Err(ConnectionError::HostNotFound)
    }
  }
}

impl ProxyConfiguration<TlsServer,Client<NonblockingSslStream<TcpStream>>> for ServerConfiguration {
  fn accept(&mut self, token: Token) -> Option<(Client<NonblockingSslStream<TcpStream>>,bool)> {
    if token.as_usize() == 0 {
      let accepted = self.listener.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        if let Ok(ssl) = Ssl::new(&self.default_context) {
          if let Ok(stream) = NonblockingSslStream::accept(ssl, frontend_sock) {
            if let Some(c) = Client::new(stream) {
              return Some((c, false))
            }
          } else {
            error!("could not create ssl stream");
          }
        } else {
          error!("could not create ssl context");
        }
      } else {
        error!("could not accept connection: {:?}", accepted);
      }
    }
    None
  }

  fn connect_to_backend(&mut self, client: &mut Client<NonblockingSslStream<TcpStream>>) -> Result<TcpStream,ConnectionError> {
    // FIXME: should check the host corresponds to SNI here
    let host   = try!(client.http_state().state.get_host().ok_or(ConnectionError::NoHostGiven));
    let rl     = try!(client.http_state().state.get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    let conn   = try!(client.http_state().state.get_front_keep_alive().ok_or(ConnectionError::ToBeDefined));
    let back   = try!(self.backend_from_request(client, &host, &rl.uri));
    //let socket = try!(TcpStream::connect(&back).map_err(|_| ConnectionError::ToBeDefined));

    if let Ok(socket) = TcpStream::connect(&back) {

      let position  = client.http_state().state.req_position;
      let req_state = client.http_state().state.request.clone();
      client.http_state().state = HttpState {
        req_position: position,
        res_position: 0,
        request:  req_state,
        response: ResponseState::Initial
      };

      Ok(socket)
    } else {
      // FIXME: should send 503 here
      client.set_answer(&self.answers.ServiceUnavailable);
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<TlsServer>, message: ProxyOrder) {
    trace!("notified: {:?}", message);
    match message {
      ProxyOrder::Command(Command::AddTlsFront(front)) => {
        info!("add front {:?}", front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage::AddedFront);
      },
      ProxyOrder::Command(Command::RemoveTlsFront(front)) => {
        info!("remove front {:?}", front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage::RemovedFront);
      },
      ProxyOrder::Command(Command::AddInstance(instance)) => {
        info!("add instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::AddedInstance);
        }
      },
      ProxyOrder::Command(Command::RemoveInstance(instance)) => {
        info!("remove instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::RemovedInstance);
        }
      },
      ProxyOrder::Stop                   => {
        event_loop.shutdown();
      },
      _ => {
        error!("unsupported message, ignoring");
      }
    }
  }
}

pub type TlsServer = Server<ServerConfiguration,Client<NonblockingSslStream<TcpStream>>>;

pub fn start_listener(front: SocketAddr, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let join_guard = thread::spawn(move|| {
    let configuration = ServerConfiguration::new(front, tx, &mut event_loop).unwrap();
    let mut server = TlsServer::new(1, max_connections, configuration);

    info!("starting event loop");
    event_loop.run(&mut server).unwrap();
    info!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use std::rc::{Rc,Weak};
  use std::cell::RefCell;
  use messages::{Command,TlsFront,Instance};
  use mio::util::Slab;
  use network::{ProxyOrder,ServerMessage};
  use network::http::DefaultAnswers;
  use openssl::ssl::{SslContext, SslMethod, Ssl, NonblockingSslStream, ServerNameCallback, ServerNameCallbackData};

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(ProxyOrder::Command(Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep_ms(100);
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep_ms(500);
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep_ms(300);
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 154);
        //assert!(false);
      }
    }
  }

  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(1025).build().unwrap();
      println!("starting web server");

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response);
        println!("backend web server sent response");
      }
    });
  }
*/

  use mio::tcp;
  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = HashMap::new();
    fronts.insert("lolcatho.st".to_owned(), vec![
      TlsFront {
        app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1, port: 8080,
        key_path: "".to_owned(), cert_path: "".to_owned()
      },
      TlsFront {
        app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2, port: 8080,
        key_path: "".to_owned(), cert_path: "".to_owned()
      },
      TlsFront {
        app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3, port: 8080,
        key_path: "".to_owned(), cert_path: "".to_owned()
      }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      TlsFront {
        app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned(), port: 8080,
        key_path: "".to_owned(), cert_path: "".to_owned()
      },
    ]);

    let contexts = HashMap::new();
    let rc_ctx = Rc::new(RefCell::new(contexts));

    let context = SslContext::new(SslMethod::Tlsv1).unwrap();
    let (tx,rx) = channel::<ServerMessage>();

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1030").unwrap();
    let listener = tcp::TcpListener::bind(&front).unwrap();
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      instances: HashMap::new(),
      fronts:    fronts,
      default_cert: "".to_owned(),
      default_context: context,
      contexts: rc_ctx,
      tx:        tx,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
    };

    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend1.unwrap().app_id, "app_1");
    assert_eq!(frontend2.unwrap().app_id, "app_1");
    assert_eq!(frontend3.unwrap().app_id, "app_2");
    assert_eq!(frontend4.unwrap().app_id, "app_3");
    assert_eq!(frontend5, None);
  }
}
