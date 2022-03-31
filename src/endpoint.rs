use crate::cli::Action;
use crate::msg::{AddrPair, Msg, MsgCtx, MsgDirection, MsgType};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt::Display;
use std::net::SocketAddr;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Sender},
};

#[derive(Serialize, Deserialize)]
pub enum Ctl {
    Act(Action),
}

#[derive(Serialize, Deserialize)]
pub enum CtlRsp {
    Msg(i32, String),
    MapLs(Vec<(SocketAddr, SocketAddr)>),
}

impl Display for CtlRsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlRsp::Msg(_, msg) => {
                write!(f, "msg: {}", msg)
            }
            CtlRsp::MapLs(l) => {
                for (lst_addr, dst_addr) in l {
                    write!(f, "{} => {}", lst_addr, dst_addr)?;
                }
                Ok(())
            }
        }
    }
}

type CtlChanMsg = (Ctl, oneshot::Sender<CtlRsp>);

pub async fn endpoint(addr: SocketAddr, cli_addr: SocketAddr) {
    let (ctl_tx, ctl_rx) = mpsc::channel::<CtlChanMsg>(1024);
    tokio::spawn(route(addr, ctl_rx));
    handle_cli(ctl_tx, cli_addr).await;
}

async fn handle_cli(tx: Sender<CtlChanMsg>, cli_addr: SocketAddr) {
    // Listen to cli addr
    let mut msg_ctx = MsgCtx::<Ctl, CtlRsp>::new();
    let mut listener = match TcpListener::bind(&cli_addr).await {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to bind to {}: {}", cli_addr, e);
        }
    };
    let (conn, _) = accept(&mut listener).await;
    let (mut readhalf, mut writehalf) = conn.into_split();

    loop {
        tokio::select! {
            // Read from cli
            _ = readhalf.readable() => {
                if let Err(e) = msg_ctx.handle_read(&mut readhalf) {
                    eprintln!("Failed to handle read: {}", e);
                    let (conn, _) = accept(&mut listener).await;
                    let (rh, wh) = conn.into_split();
                    readhalf = rh;
                    writehalf = wh;
                }
                // Send Ctl to router and wait for CtlRsp
                loop {
                    if let Some(ctl) = msg_ctx.pop_rx_msg() {
                        let (oneshot_tx, oneshot_rx) = oneshot::channel();
                        if let Err(e) = tx.send((ctl, oneshot_tx)).await {
                            panic!("Ctl receive closed: {}", e);
                        }
                        let rsp = match oneshot_rx.await {
                            Ok(rsp) => rsp,
                            Err(e) => {
                                panic!("Expect a CtlRsp: {}", e);
                            }
                        };
                        msg_ctx.queue_tx_msg(rsp);
                    } else {
                        break;
                    }
                }
            }
            // Write to cli
            _ = writehalf.writable(), if msg_ctx.need_to_write() => {
                if let Err(e) = msg_ctx.handle_write(&mut writehalf) {
                    eprintln!("Failed to handle write: {}", e);
                    let (conn, _) = accept(&mut listener).await;
                    let (rh, wh) = conn.into_split();
                    readhalf = rh;
                    writehalf = wh;
                }
            }
        }
    }
}

struct Endpoint {
    msg_ctx: MsgCtx<Msg, Msg>,
    // lst ports, key: (lst_addr, dst_addr)
    port_mappers: HashMap<(SocketAddr, SocketAddr), Sender<Msg>>,
    // dst ports, key: (lst_addr, dst_addr, local_addr)
    dst_ports: HashMap<(SocketAddr, SocketAddr, SocketAddr), Sender<Msg>>,
    // Mapper -> router
    router_tx: mpsc::Sender<Msg>,
}

impl Endpoint {
    fn new(router_tx: mpsc::Sender<Msg>) -> Self {
        Endpoint {
            msg_ctx: MsgCtx::new(),
            port_mappers: HashMap::new(),
            dst_ports: HashMap::new(),
            router_tx,
        }
    }

    async fn handle_msgs(&mut self) {
        loop {
            let msg = self.msg_ctx.pop_rx_msg();
            if msg.is_none() {
                break;
            }
            let Msg { addr, dir, typ } = msg.unwrap();
            match typ {
                MsgType::MapConnecting => {
                    // Try to connect to dst_addr.
                    let key = (
                        addr.lst_addr.clone(),
                        addr.dst_addr.clone(),
                        addr.local_addr.clone(),
                    );
                    let (tx, rx) = mpsc::channel(1024);
                    tokio::spawn(dst_port(addr, self.router_tx.clone(), rx));
                    let _ = self.dst_ports.insert(key, tx);
                }
                typ => {
                    match dir {
                        MsgDirection::D2L => {
                            let key = (addr.lst_addr.clone(), addr.dst_addr.clone());
                            if let Some(tx) = self.port_mappers.get(&key) {
                                if let Err(_) = tx
                                    .send(Msg {
                                        addr: addr.clone(),
                                        dir: MsgDirection::L2D,
                                        typ,
                                    })
                                    .await
                                {
                                    // This map is dead.
                                    self.port_mappers.remove(&key);
                                    self.msg_ctx.queue_tx_msg(Msg {
                                        addr,
                                        dir: MsgDirection::D2L,
                                        typ: MsgType::MapDisconnect,
                                    });
                                }
                            } else {
                                // No match for this msg.
                                eprintln!("No match for {}->{}", key.0, key.1);
                            }
                        }
                        MsgDirection::L2D => {
                            let key = (
                                addr.lst_addr.clone(),
                                addr.dst_addr.clone(),
                                addr.local_addr.clone(),
                            );
                            if let Some(tx) = self.dst_ports.get(&key) {
                                if let Err(_) = tx
                                    .send(Msg {
                                        addr: addr.clone(),
                                        dir: MsgDirection::D2L,
                                        typ,
                                    })
                                    .await
                                {
                                    // This port is dead.
                                    self.dst_ports.remove(&key);
                                    self.msg_ctx.queue_tx_msg(Msg {
                                        addr,
                                        dir: MsgDirection::D2L,
                                        typ: MsgType::MapDisconnect,
                                    });
                                }
                            }
                        }
                    }
                    // Rout msg to lst port
                }
            }
        }
    }

    async fn handle_ctl(&mut self, ctl: Ctl, oneshot_tx: oneshot::Sender<CtlRsp>) {
        match ctl {
            Ctl::Act(act) => self.handle_action(act, oneshot_tx).await,
        };
    }

    async fn handle_action(&mut self, act: Action, oneshot_tx: oneshot::Sender<CtlRsp>) {
        match act {
            Action::MapAdd { lst_addr, dst_addr } => {
                let key = (lst_addr.clone(), dst_addr.clone());
                if self.port_mappers.contains_key(&key) {
                    let _ = oneshot_tx.send(CtlRsp::Msg(
                        1,
                        format!("map {}<=>{} exist", lst_addr, dst_addr),
                    ));
                    return;
                }
                let (mapper_tx, mapper_rx) = mpsc::channel(1024);
                let mapper = PortMapper {
                    lst_addr: lst_addr.clone(),
                    dst_addr: dst_addr.clone(),
                    mapper_rx,
                    router_tx: self.router_tx.clone(),
                    ports: HashMap::new(),
                };
                // Start port mapper.
                tokio::spawn(mapper.run());
                self.port_mappers.insert(key, mapper_tx);
            }
            Action::MapRm { lst_addr, dst_addr } => {
                // Remove mapper from Endpoint.
                self.port_mappers.remove(&(lst_addr, dst_addr));
            }
            Action::MapLs => {
                let mut mapls = Vec::new();
                for (lst_addr, dst_addr) in self.port_mappers.keys() {
                    mapls.push((lst_addr.clone(), dst_addr.clone()));
                }
                let _ = oneshot_tx.send(CtlRsp::MapLs(mapls));
            }
        };
    }
}

async fn dst_port(mut addr: AddrPair, tx: mpsc::Sender<Msg>, mut rx: mpsc::Receiver<Msg>) {
    let mut data_to_write: VecDeque<Vec<u8>> = VecDeque::new();
    let mut data_writing: Option<Vec<u8>> = None;
    let mut written_bytes: usize = 0;
    let dir = MsgDirection::D2L;
    // Connect to dst addr.
    let conn = match TcpStream::connect(addr.dst_addr.clone()).await {
        Ok(conn) => conn,
        Err(e) => {
            // This port is dead.
            eprintln!("Failed to connect to {}: {}", addr.dst_addr, e);
            let _ = tx
                .send(Msg {
                    addr,
                    dir,
                    typ: MsgType::MapDisconnect,
                })
                .await;
            return;
        }
    };
    addr.remap_addr = conn.local_addr().unwrap();
    // Tell lst port that we have connected.
    if let Err(e) = tx
        .send(Msg {
            addr: addr.clone(),
            dir,
            typ: MsgType::MapConnected,
        })
        .await
    {
        // This port is dead.
        eprintln!("Failed to send msg: {}", e);
        return;
    }

    let (rh, wh) = conn.into_split();

    loop {
        tokio::select! {
            _ = rh.readable() => {
                let mut read_buf: Vec<u8> = Vec::new();
                match rh.try_read_buf(&mut read_buf) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Failed to read from {}: {}", addr.dst_addr, e);
                        // This port is dead.
                        let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                        return;
                    }
                };
                // Send msg to peer.
                if let Err(e) = tx.send(Msg {addr: addr.clone(), dir, typ: MsgType::MapData(read_buf)}).await {
                    eprintln!("Failed to send msg: {}", e);
                    return;
                }
            }
            // Write to wh.
            _ = wh.writable(), if data_to_write.len() > 0 || data_writing.is_some() => {
                if data_writing.is_some() {
                    let data = data_writing.as_ref().unwrap();
                    let len = data.len();
                    let s = match wh.try_write(&data[written_bytes..len]) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                            eprintln!("Failed to write to wh: {}", e);
                            return;
                        }
                    };
                    written_bytes += s;
                    assert!(written_bytes <= len);
                    if written_bytes == len {
                        data_writing = data_to_write.pop_front();
                        written_bytes = 0;
                    }
                } else {
                    data_writing = data_to_write.pop_front();
                    written_bytes = 0;
                }
            }
            // Receive msg from lst port.
            r = rx.recv() => {
                match r {
                    Some(Msg {addr: _, dir: _, typ}) => {
                        match typ {
                            MsgType::MapDisconnect => {
                                return;
                            }
                            MsgType::MapData(data) => {
                                // Write these data to socket.
                                data_to_write.push_back(data);
                            }
                            _ => {
                                // Unknown msg type.
                                eprintln!("Unknown msg type");
                            }
                        }
                    }
                    None => {
                        eprintln!("Failed to recv msg");
                        let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                        return;
                    }
                }
            }
        }
    }
}

async fn route(addr: SocketAddr, mut ctl_rx: mpsc::Receiver<CtlChanMsg>) {
    let (mapper_tx, mut mapper_rx) = mpsc::channel(1024);
    let mut ep = Endpoint::new(mapper_tx);
    let mut ctl_rx_closed = false;
    let mut mapper_rx_closed = false;
    // Listen to addr.
    let mut listener = match TcpListener::bind(&addr).await {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to bind to {}: {}", addr, e);
        }
    };
    let (conn, _) = accept(&mut listener).await;
    let (mut readhalf, mut writehalf) = conn.into_split();

    loop {
        tokio::select! {
            // Read from Hole.
            _ = readhalf.readable() => {
                if let Err(e) = ep.msg_ctx.handle_read(&mut readhalf) {
                    eprintln!("Failed to handle read: {}", e);
                    let (conn, _) = accept(&mut listener).await;
                    let (rh, wh) = conn.into_split();
                    readhalf = rh;
                    writehalf = wh;
                }
                ep.handle_msgs().await;
            }
            // Write to Hole.
            _ = writehalf.writable(), if ep.msg_ctx.need_to_write() => {
                if let Err(e) = ep.msg_ctx.handle_write(&mut writehalf) {
                    eprintln!("Failed to handle write: {}", e);
                    let (conn, _) = accept(&mut listener).await;
                    let (rh, wh) = conn.into_split();
                    readhalf = rh;
                    writehalf = wh;
                }
            }
            // Handle Ctl from cli
            r = ctl_rx.recv(), if !ctl_rx_closed => {
                if let Some((ctl, oneshot_tx)) = r {
                    // Handle ctl and then response.
                    ep.handle_ctl(ctl, oneshot_tx).await;
                } else {
                    eprintln!("ctl_rx closed");
                    ctl_rx_closed = true;
                }
            }
            // Receive from mapper
            r = mapper_rx.recv(), if !mapper_rx_closed => {
                if let Some(msg) = r {
                    // This msg should send to hole
                    ep.msg_ctx.queue_tx_msg(msg);
                } else {
                    eprintln!("mapper_rx closed");
                    mapper_rx_closed = true;
                }
            }
        }
    }
}

async fn accept(listener: &mut TcpListener) -> (TcpStream, SocketAddr) {
    match listener.accept().await {
        Ok(conn) => conn,
        Err(e) => {
            panic!("Failed to accept: {}", e);
        }
    }
}

struct PortMapper {
    lst_addr: SocketAddr,
    dst_addr: SocketAddr,
    // Router -> mapper
    mapper_rx: Receiver<Msg>,
    // Mapper/Port -> router
    router_tx: Sender<Msg>,
    // key: (lst_addr, dst_addr)
    ports: HashMap<(SocketAddr, SocketAddr), mpsc::Sender<Msg>>,
}

impl PortMapper {
    async fn run(mut self) {
        // Listen lst_addr
        let lst = match TcpListener::bind(&self.lst_addr).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to listen on {}: {}", self.lst_addr, e);
                return;
            }
        };
        // Wait for connection and messages.
        tokio::select! {
            // New connection.
            r = lst.accept() => {
                let (conn, addr) = match r {
                    Ok(v) => v,
                    Err(e) => {
                        // If we failed to listen, this mapper is dead.
                        eprintln!("Failed to accept {}: {}", self.lst_addr, e);
                        return;
                    }
                };
                let key = (self.lst_addr.clone(), addr.clone());
                // PortMapper -> Port
                let (port_tx, port_rx) = mpsc::channel(1024);
                self.ports.insert(key, port_tx);
                tokio::spawn(port(port_rx, self.router_tx.clone(), conn, self.lst_addr, addr, self.dst_addr));
            }
            // New msg.
            r = self.mapper_rx.recv() => {
                let msg = if let Some(msg) = r {
                    msg
                } else {
                    // Maybe this mapper is dead.
                    eprintln!("Failed to recv msg for {}", self.lst_addr);
                    return;
                };
                // Now process msg...
                self.process_msg(msg).await;
            }
        }
    }

    async fn process_msg(&mut self, msg: Msg) {
        let key = (msg.addr.lst_addr.clone(), msg.addr.dst_addr.clone());
        if let Some(tx) = self.ports.get_mut(&key) {
            let lst_addr = msg.addr.lst_addr.clone();
            if let Err(e) = tx.send(msg).await {
                eprintln!("Failed to send msg to {}: {}", lst_addr, e);
                // This port is dead.
                self.ports.remove(&key);
            }
        } else {
            // No port match for this msg, just drop it.
            eprintln!(
                "No port match for msg, local_addr: {}, lst_addr: {}, remap_addr: {}, dst_addr: {}",
                msg.addr.local_addr, msg.addr.lst_addr, msg.addr.remap_addr, msg.addr.dst_addr
            );
        }
    }
}

async fn port(
    mut rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    conn: TcpStream,
    lst_addr: SocketAddr,
    local_addr: SocketAddr,
    dst_addr: SocketAddr,
) {
    let dir = MsgDirection::L2D;
    let mut data_to_write: VecDeque<Vec<u8>> = VecDeque::new();
    let mut writing_buf: Option<Vec<u8>> = None;
    let mut written_bytes: usize = 0;
    let mut read_buf: Vec<u8> = Vec::new();
    let mut can_read = false;
    let mut addr = AddrPair {
        local_addr,
        lst_addr,
        remap_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
        dst_addr,
    };

    let (rh, wh) = conn.into_split();

    // Send a msg to the peer.
    if let Err(e) = tx
        .send(Msg {
            addr: addr.clone(),
            dir,
            typ: MsgType::MapConnecting,
        })
        .await
    {
        eprintln!("Failed to send MapConnecting to peer: {}", e);
        // This port is dead.
        return;
    };

    loop {
        tokio::select! {
            // Read from local addr.
            _ = rh.readable(), if can_read => {
                match rh.try_read_buf(&mut read_buf) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Failed to read from {}: {}", local_addr, e);
                        // This port is dead.
                        let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                        return;
                    }
                };
                let msg = Msg {
                    addr: addr.clone(),
                    dir,
                    typ: MsgType::MapData(read_buf),
                };
                // Send msg to router.
                if let Err(e) = tx.send(msg).await {
                    eprintln!("Failed to send msg to route for {}: {}", lst_addr, e);
                    // This port is dead.
                    return;
                }
                // Create a new read buffer.
                read_buf = Vec::new();
            }
            // Write to local addr.
            _ = wh.writable(), if writing_buf.is_some() || data_to_write.len() != 0 => {
                if writing_buf.is_some() {
                    // Continue to write.
                    let writing_buf_ref = writing_buf.as_ref().unwrap();
                    let len = writing_buf_ref.len();
                    let s = match wh.try_write(&writing_buf_ref[written_bytes..len]) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Failed to write to {}: {}", local_addr, e);
                            // This port is dead.
                            let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                            return;
                        }
                    };
                    written_bytes += s;
                    assert!(written_bytes <= len);
                    if written_bytes == len {
                        // This segment of data is all written.
                        writing_buf = data_to_write.pop_front();
                        written_bytes = 0;
                    }
                } else {
                    // writing_buf will be written next time.
                    writing_buf = data_to_write.pop_front();
                }
            }
            // Receive msg from router
            m = rx.recv() => {
                let Msg {addr: _addr, dir: _, typ} = if let Some(m) = m {
                    m
                } else {
                    eprintln!("Failed to receive msg from router");
                    // This port is dead.
                    let _ = tx.send(Msg {addr, dir, typ: MsgType::MapDisconnect}).await;
                    return;
                };
                // Now process msg.
                match typ {
                    MsgType::MapData(data) => {
                        data_to_write.push_back(data);
                    },
                    MsgType::MapConnected => {
                        // We can read data now.
                        addr.remap_addr = _addr.remap_addr;
                        can_read = true;
                    },
                    MsgType::MapDisconnect => {
                        // This port is dead.
                        return;
                    }
                    _ => {
                        // We can not handle this msg.
                        eprintln!("Unknown msg type");
                    },
                }
            }
        }
    }
}
