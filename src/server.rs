#[cfg(target_os = "macos")]
use crate::ipc::ConnectionTmpl;
use crate::ipc::Data;
use connection::{ConnInner, Connection};
use hbb_common::{
    allow_err,
    anyhow::{anyhow, Context},
    bail,
    config::{Config, CONNECT_TIMEOUT, RELAY_PORT},
    log,
    message_proto::*,
    protobuf::{Message as _, ProtobufEnum},
    rendezvous_proto::*,
    sleep, socket_client,
    sodiumoxide::crypto::{box_, secretbox, sign},
    timeout, tokio, ResultType, Stream,
};
#[cfg(target_os = "macos")]
use notify::{watcher, RecursiveMode, Watcher};
#[cfg(target_os = "macos")]
use parity_tokio_ipc::ConnectionClient;
use service::{GenericService, Service, ServiceTmpl, Subscriber};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

pub mod audio_service;
mod clipboard_service;
#[cfg(windows)]
pub mod clipboard_file_service;
mod connection;
pub mod input_service;
mod service;
mod video_service;

use hbb_common::tcp::new_listener;

pub type Childs = Arc<Mutex<Vec<std::process::Child>>>;
type ConnMap = HashMap<i32, ConnInner>;

lazy_static::lazy_static! {
    pub static ref CHILD_PROCESS: Childs = Default::default();
}

pub struct Server {
    connections: ConnMap,
    services: HashMap<&'static str, Box<dyn Service>>,
    id_count: i32,
}

pub type ServerPtr = Arc<RwLock<Server>>;
pub type ServerPtrWeak = Weak<RwLock<Server>>;

pub fn new() -> ServerPtr {
    let mut server = Server {
        connections: HashMap::new(),
        services: HashMap::new(),
        id_count: 0,
    };
    server.add_service(Box::new(audio_service::new()));
    server.add_service(Box::new(video_service::new()));
    server.add_service(Box::new(clipboard_service::new()));
    #[cfg(windows)]
    server.add_service(Box::new(clipboard_file_service::new()));
    server.add_service(Box::new(input_service::new_cursor()));
    server.add_service(Box::new(input_service::new_pos()));
    Arc::new(RwLock::new(server))
}

async fn accept_connection_(server: ServerPtr, socket: Stream, secure: bool) -> ResultType<()> {
    let local_addr = socket.local_addr();
    drop(socket);
    // even we drop socket, below still may fail if not use reuse_addr,
    // there is TIME_WAIT before socket really released, so sometimes we
    // see “Only one usage of each socket address is normally permitted” on windows sometimes,
    let listener = new_listener(local_addr, true).await?;
    log::info!("Server listening on: {}", &listener.local_addr()?);
    if let Ok((stream, addr)) = timeout(CONNECT_TIMEOUT, listener.accept()).await? {
        stream.set_nodelay(true).ok();
        let stream_addr = stream.local_addr()?;
        create_tcp_connection(server, Stream::from(stream, stream_addr), addr, secure).await?;
    }
    Ok(())
}

pub async fn create_tcp_connection(
    server: ServerPtr,
    stream: Stream,
    addr: SocketAddr,
    secure: bool,
) -> ResultType<()> {
    let mut stream = stream;
    let id = {
        let mut w = server.write().unwrap();
        w.id_count += 1;
        w.id_count
    };
    let (sk, pk) = Config::get_key_pair();
    if secure && pk.len() == sign::PUBLICKEYBYTES && sk.len() == sign::SECRETKEYBYTES {
        let mut sk_ = [0u8; sign::SECRETKEYBYTES];
        sk_[..].copy_from_slice(&sk);
        let sk = sign::SecretKey(sk_);
        let mut msg_out = Message::new();
        let (our_pk_b, our_sk_b) = box_::gen_keypair();
        let signed_id = sign::sign(
            format!("{}\0{}", Config::get_id(), base64::encode(our_pk_b.0)).as_bytes(),
            &sk,
        );
        msg_out.set_signed_id(SignedId {
            id: signed_id,
            ..Default::default()
        });
        timeout(CONNECT_TIMEOUT, stream.send(&msg_out)).await??;
        match timeout(CONNECT_TIMEOUT, stream.next()).await? {
            Some(res) => {
                let bytes = res?;
                if let Ok(msg_in) = Message::parse_from_bytes(&bytes) {
                    if let Some(message::Union::public_key(pk)) = msg_in.union {
                        if pk.asymmetric_value.len() == box_::PUBLICKEYBYTES {
                            let nonce = box_::Nonce([0u8; box_::NONCEBYTES]);
                            let mut pk_ = [0u8; box_::PUBLICKEYBYTES];
                            pk_[..].copy_from_slice(&pk.asymmetric_value);
                            let their_pk_b = box_::PublicKey(pk_);
                            let symmetric_key =
                                box_::open(&pk.symmetric_value, &nonce, &their_pk_b, &our_sk_b)
                                    .map_err(|_| {
                                        anyhow!("Handshake failed: box decryption failure")
                                    })?;
                            if symmetric_key.len() != secretbox::KEYBYTES {
                                bail!("Handshake failed: invalid secret key length from peer");
                            }
                            let mut key = [0u8; secretbox::KEYBYTES];
                            key[..].copy_from_slice(&symmetric_key);
                            stream.set_key(secretbox::Key(key));
                        } else if pk.asymmetric_value.is_empty() {
                            Config::set_key_confirmed(false);
                            log::info!("Force to update pk");
                        } else {
                            bail!("Handshake failed: invalid public sign key length from peer");
                        }
                    } else {
                        log::error!("Handshake failed: invalid message type");
                    }
                } else {
                    bail!("Handshake failed: invalid message format");
                }
            }
            None => {
                bail!("Failed to receive public key");
            }
        }
    }

    Connection::start(addr, stream, id, Arc::downgrade(&server)).await;
    Ok(())
}

pub async fn accept_connection(
    server: ServerPtr,
    socket: Stream,
    peer_addr: SocketAddr,
    secure: bool,
) {
    if let Err(err) = accept_connection_(server, socket, secure).await {
        log::error!("Failed to accept connection from {}: {}", peer_addr, err);
    }
}

pub async fn create_relay_connection(
    server: ServerPtr,
    relay_server: String,
    uuid: String,
    peer_addr: SocketAddr,
    secure: bool,
) {
    if let Err(err) =
        create_relay_connection_(server, relay_server, uuid.clone(), peer_addr, secure).await
    {
        log::error!(
            "Failed to create relay connection for {} with uuid {}: {}",
            peer_addr,
            uuid,
            err
        );
    }
}

async fn create_relay_connection_(
    server: ServerPtr,
    relay_server: String,
    uuid: String,
    peer_addr: SocketAddr,
    secure: bool,
) -> ResultType<()> {
    let mut stream = socket_client::connect_tcp(
        crate::check_port(relay_server, RELAY_PORT),
        Config::get_any_listen_addr(),
        CONNECT_TIMEOUT,
    )
    .await?;
    let mut msg_out = RendezvousMessage::new();
    msg_out.set_request_relay(RequestRelay {
        uuid,
        ..Default::default()
    });
    stream.send(&msg_out).await?;
    create_tcp_connection(server, stream, peer_addr, secure).await?;
    Ok(())
}

impl Server {
    pub fn add_connection(&mut self, conn: ConnInner, noperms: &Vec<&'static str>) {
        for s in self.services.values() {
            if !noperms.contains(&s.name()) {
                s.on_subscribe(conn.clone());
            }
        }
        self.connections.insert(conn.id(), conn);
    }

    pub fn remove_connection(&mut self, conn: &ConnInner) {
        for s in self.services.values() {
            s.on_unsubscribe(conn.id());
        }
        self.connections.remove(&conn.id());
    }

    fn add_service(&mut self, service: Box<dyn Service>) {
        let name = service.name();
        self.services.insert(name, service);
    }

    pub fn subscribe(&mut self, name: &str, conn: ConnInner, sub: bool) {
        if let Some(s) = self.services.get(&name) {
            if s.is_subed(conn.id()) == sub {
                return;
            }
            if sub {
                s.on_subscribe(conn.clone());
            } else {
                s.on_unsubscribe(conn.id());
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for s in self.services.values() {
            s.join();
        }
    }
}

pub fn check_zombie() {
    std::thread::spawn(|| loop {
        let mut lock = CHILD_PROCESS.lock().unwrap();
        let mut i = 0;
        while i != lock.len() {
            let c = &mut (*lock)[i];
            if let Ok(Some(_)) = c.try_wait() {
                lock.remove(i);
            } else {
                i += 1;
            }
        }
        drop(lock);
        std::thread::sleep(Duration::from_millis(100));
    });
}

#[tokio::main]
pub async fn start_server(is_server: bool, _tray: bool) {
    #[cfg(target_os = "linux")]
    {
        log::info!("DISPLAY={:?}", std::env::var("DISPLAY"));
        log::info!("XAUTHORITY={:?}", std::env::var("XAUTHORITY"));
    }

    if is_server {
        std::thread::spawn(move || {
            if let Err(err) = crate::ipc::start("") {
                log::error!("Failed to start ipc: {}", err);
                std::process::exit(-1);
            }
        });
        input_service::fix_key_down_timeout_loop();
        #[cfg(target_os = "macos")]
        tokio::spawn(async { sync_and_watch_config_dir().await });
        crate::RendezvousMediator::start_all().await;
    } else {
        match crate::ipc::connect(1000, "").await {
            Ok(mut conn) => {
                allow_err!(conn.send(&Data::SystemInfo(None)).await);
                if let Ok(Some(data)) = conn.next_timeout(1000).await {
                    log::info!("server info: {:?}", data);
                }
                // sync key pair
                let mut n = 0;
                loop {
                    if Config::get_key_confirmed() {
                        // check ipc::get_id(), key_confirmed may change, so give some chance to correct
                        n += 1;
                        if n > 3 {
                            break;
                        } else {
                            sleep(1.).await;
                        }
                    } else {
                        allow_err!(conn.send(&Data::ConfirmedKey(None)).await);
                        if let Ok(Some(Data::ConfirmedKey(Some(pair)))) =
                            conn.next_timeout(1000).await
                        {
                            Config::set_key_pair(pair);
                            Config::set_key_confirmed(true);
                            log::info!("key pair synced");
                            break;
                        } else {
                            sleep(1.).await;
                        }
                    }
                }
            }
            Err(err) => {
                log::info!("server not started (will try to start): {}", err);
                std::thread::spawn(|| start_server(true, false));
            }
        }
    }
}

#[cfg(target_os = "macos")]
async fn sync_and_watch_config_dir() {
    if crate::username() == "root" {
        return;
    }

    match crate::ipc::connect(1000, "_service").await {
        Ok(mut conn) => {
            match sync_config_to_user(&mut conn).await {
                Err(e) => log::error!("sync config to user failed:{}", e),
                _ => {}
            }

            tokio::spawn(async move {
                log::info!(
                    "watching config dir: {}",
                    Config::path("").to_str().unwrap().to_string()
                );

                let (tx, rx) = std::sync::mpsc::channel();
                let mut watcher = watcher(tx, Duration::from_secs(2)).unwrap();
                watcher
                    .watch(Config::path("").as_path(), RecursiveMode::Recursive)
                    .unwrap();

                loop {
                    let ev = rx.recv();
                    match ev {
                        Ok(event) => match event {
                            notify::DebouncedEvent::Write(path) => {
                                log::info!(
                                    "config file changed, call ipc_service to sync: {}",
                                    path.to_str().unwrap().to_string()
                                );

                                match sync_config_to_root(&mut conn, path).await {
                                    Err(e) => log::error!("sync config to root failed: {}", e),
                                    _ => {}
                                }
                            }
                            x => {
                                log::debug!("another {:?}", x)
                            }
                        },
                        Err(e) => println!("watch error: {:?}", e),
                    }
                }
            });
        }
        Err(_) => {
            log::info!("connect ipc_service failed, skip config sync");
            return;
        }
    }
}

#[cfg(target_os = "macos")]
async fn sync_config_to_user(conn: &mut ConnectionTmpl<ConnectionClient>) -> ResultType<()> {
    allow_err!(
        conn.send(&Data::SyncConfigToUserReq {
            username: crate::username(),
            to: Config::path("").to_str().unwrap().to_string(),
        })
        .await
    );

    if let Some(data) = conn.next_timeout(2000).await? {
        match data {
            Data::SyncConfigToUserResp(success) => {
                log::info!("copy and reload config dir success: {:?}", success);
            }
            _ => {}
        };
    };

    Ok(())
}

#[cfg(target_os = "macos")]
async fn sync_config_to_root(
    conn: &mut ConnectionTmpl<ConnectionClient>,
    from: std::path::PathBuf,
) -> ResultType<()> {
    allow_err!(
        conn.send(&Data::SyncConfigToRootReq {
            from: from.to_str().unwrap().to_string()
        })
        .await
    );

    // todo: this code will block outer loop, resolve it later.
    // if let Some(data) = conn.next_timeout(2000).await? {
    //     match data {
    //         Data::SyncConfigToRootResp(success) => {
    //             log::info!("copy config to root dir success: {:?}", success);
    //         }
    //         x => {
    //             log::info!("receive another {:?}", x)
    //         }
    //     };
    // };

    Ok(())
}
