#[macro_use]
extern crate clap;
extern crate csv;
extern crate hwloc;
#[macro_use]
extern crate log;
extern crate byteorder;
extern crate env_logger;
extern crate mio;
extern crate netbench;
extern crate nix;

use std::collections::{HashMap, VecDeque};
use std::net;
use std::ops::Add;
use std::os::unix::io::AsRawFd;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;

use nix::sys::socket;
use nix::sys::time;
use nix::sys::uio;

use mio::unix::UnixReady;
use mio::Ready;

use clap::App;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

const PONG: mio::Token = mio::Token(0);

#[derive(Debug)]
struct MessageState {
    sender: socket::SockAddr,
    log: netbench::LogRecord,
}

impl MessageState {
    fn new(
        id: u64,
        sender: socket::SockAddr,
        rx_app: u64,
        rx_nic: u64,
        rx_ht: u64,
    ) -> MessageState {
        let mut log: netbench::LogRecord = Default::default();
        log.id = id;
        log.rx_app = rx_app;
        log.rx_nic = rx_nic;
        log.rx_ht = rx_ht;
        log.completed = false;

        MessageState {
            sender: sender,
            log: log,
        }
    }
}

fn network_loop(
    address: net::SocketAddr,
    socket: net::UdpSocket,
    suffix: String,
    timestamp_type: netbench::PacketTimestamp,
    app_channel: Option<(mpsc::Sender<u64>, mpsc::Receiver<(u64, u64)>)>,
) {
    let logfile = format!("latencies-rserver-{}-{}.csv", address.port(), suffix);
    let wtr = netbench::create_writer(logfile.clone(), 50 * 1024 * 1024);

    let mio_socket = mio::net::UdpSocket::from_socket(socket).expect("Can make socket");
    let poll = mio::Poll::new().expect("Can't create poll.");
    poll.register(
        &mio_socket,
        PONG,
        Ready::writable() | Ready::readable() | UnixReady::error(),
        mio::PollOpt::level(),
    ).expect("Can't register send event.");

    let mut events = mio::Events::with_capacity(1024);
    let mut recv_buf: Vec<u8> = Vec::with_capacity(8);
    recv_buf.resize(8, 0);

    let mut packet_count = 0;

    let mut time_rx: socket::CmsgSpace<[time::TimeVal; 3]> = socket::CmsgSpace::new();
    let mut time_tx: socket::CmsgSpace<[time::TimeVal; 3]> = socket::CmsgSpace::new();

    // If this fails think about allocating it on the heap instead of copy?
    assert!(std::mem::size_of::<MessageState>() < 256);
    let mut send_state: VecDeque<MessageState> = VecDeque::with_capacity(1024);
    let mut ts_state: HashMap<u64, MessageState> = HashMap::with_capacity(1024);

    loop {
        poll.poll(&mut events, Some(std::time::Duration::from_millis(100)))
            .expect("Can't poll channel");
        for event in events.iter() {
            if UnixReady::from(event.readiness()).is_error() {
                let (id, tx_nic) = netbench::retrieve_tx_timestamp(
                    mio_socket.as_raw_fd(),
                    &mut time_tx,
                    timestamp_type,
                );

                ts_state.remove(&id).map_or_else(
                    || {
                        panic!("Packet state for id {} not found?", id);
                    },
                    |mut st| {
                        debug!("Reading timestamp");
                        assert!(id == st.log.id);
                        st.log.tx_nic = tx_nic;
                        st.log.completed = true;

                        // Log all the timestamps
                        let mut logfile = wtr.lock().unwrap();
                        logfile.serialize(&st.log).expect("Can't write record.");

                        packet_count = packet_count + 1;
                    },
                );
            }

            // Receive a packet
            if event.readiness().is_readable() {
                debug!("Received packet {}", packet_count);

                // Get the packet
                let msg = socket::recvmsg(
                    mio_socket.as_raw_fd(),
                    &[uio::IoVec::from_mut_slice(&mut recv_buf)],
                    Some(&mut time_rx),
                    socket::MsgFlags::empty(),
                ).expect("Can't receive message");
                let rx_app = netbench::now();
                assert!(msg.bytes == 8);
                let mut packet_id = recv_buf
                    .as_slice()
                    .read_u64::<BigEndian>()
                    .expect("Can't parse timestamp");

                let rx_ht = app_channel.as_ref().map_or(0, |(txp, rxp)| {
                    txp.send(packet_id)
                        .expect("Can't forward data to the app thread over channel.");
                    let (payload, tst) = rxp.recv().expect("Can't receive data");
                    packet_id = payload;
                    tst
                });

                let rx_nic = netbench::read_nic_timestamp(&msg, timestamp_type);

                if packet_id == 0 {
                    debug!("Client sent 0, flushing logfile.");
                    let mut logfile = wtr.lock().unwrap();

                    // Log packets that probably didn't make it back
                    for packet in &send_state {
                        logfile.serialize(&packet.log).expect("Can't write record.");
                    }
                    for (_id, packet) in &ts_state {
                        logfile.serialize(&packet.log).expect("Can't write record.");
                    }
                    logfile.flush().expect("Can't fliush logfile");
                } else {
                    let mst = MessageState::new(
                        packet_id,
                        msg.address.expect("Need a recipient"),
                        rx_app,
                        rx_nic,
                        rx_ht,
                    );
                    send_state.push_back(mst);
                }
            }

            // Send a packet
            if event.readiness().is_writable() {
                send_state.pop_front().map(|mut st| {
                    st.log.tx_app = netbench::now();
                    recv_buf.clear();
                    recv_buf
                        .write_u64::<BigEndian>(st.log.id)
                        .expect("Can't serialize payload");

                    let bytes_sent = socket::sendto(
                        mio_socket.as_raw_fd(),
                        &recv_buf,
                        &st.sender,
                        socket::MsgFlags::empty(),
                    ).expect("Sending packet failed.");

                    debug!("sent reply");
                    assert_eq!(bytes_sent, 8);
                    ts_state.insert(st.log.id, st);
                });
            }
        }
    }
}

fn spawn_listen_pair(
    address: net::SocketAddr,
    socket: net::UdpSocket,
    timestamp_type: netbench::PacketTimestamp,
    name: String,
    on_core: Option<(usize, usize)>,
) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let (txa, rxp) = mpsc::channel();
    let (txp, rxa) = mpsc::channel();

    let t_app_name = name.clone().add("/app");
    let t_app = thread::Builder::new()
        .name(t_app_name)
        .stack_size(4096 * 10)
        .spawn(move || loop {
            on_core.map(|ids: (usize, usize)| {
                netbench::pin_thread(vec![ids.0]);
            });

            let payload = rxa.recv().expect("Can't receive data on app thread.");
            txa.send((payload, netbench::now()))
                .expect("Can't send data to network thread.");
        })
        .expect("Can't spawn application thread");

    let t_poll_name = name.clone().add("/polling");
    let t_poll = thread::Builder::new()
        .name(t_poll_name)
        .stack_size(4096 * 10)
        .spawn(move || {
            on_core.map(|ids: (usize, usize)| {
                netbench::pin_thread(vec![ids.1]);
            });
            let channel = Some((txp, rxp));
            network_loop(address, socket, name, timestamp_type, channel)
        })
        .expect("Couldn't spawn a thread");

    (t_app, t_poll)
}

fn parse_args(
    matches: &clap::ArgMatches,
) -> (
    Option<(usize, usize)>,
    net::SocketAddr,
    net::UdpSocket,
    String,
    netbench::PacketTimestamp,
) {
    let core_id: Option<u32> = matches
        .value_of("pin")
        .and_then(|c: &str| u32::from_str(c).ok());
    let iface = matches.value_of("iface").unwrap_or("enp216s0f1");
    let interface = std::ffi::CString::new(iface).expect("Can't be null");
    let port: u16 = u16::from_str(matches.value_of("port").unwrap_or("3400")).unwrap_or(3400);
    let suffix = value_t!(matches, "name", String).unwrap_or(String::from("none"));
    let timestamp = match matches.value_of("timestamp").unwrap_or("hardware") {
        "hardware" => netbench::PacketTimestamp::Hardware,
        "software" => netbench::PacketTimestamp::Software,
        "none" => netbench::PacketTimestamp::None,
        _ => unreachable!("Invalid CLI argument, may be clap bug if possible_values doesn't work?"),
    };

    let address = unsafe {
        let interface_addr = netbench::getifaceaddr(interface.as_ptr());
        match socket::SockAddr::from_libc_sockaddr(&interface_addr) {
            Some(socket::SockAddr::Inet(s)) => {
                let mut addr = s.to_std();
                addr.set_port(port);
                debug!("Found address {} for {}", addr, iface);
                addr
            }
            _ => {
                warn!("Could not find address for {:?} using 0.0.0.0", iface);
                net::SocketAddr::new(net::IpAddr::V4(net::Ipv4Addr::new(0, 0, 0, 0)), port)
            }
        }
    };
    let socket = net::UdpSocket::bind(address).expect("Couldn't bind to address");

    unsafe {
        let r =
            netbench::enable_packet_timestamps(socket.as_raw_fd(), interface.as_ptr(), timestamp);
        if r != 0 {
            panic!(
                "Failed to enable NIC timestamps (ret {}): {}",
                r,
                nix::errno::Errno::last()
            );
        }
    }

    let pin_to: Option<(usize, usize)> = core_id.and_then(|c: u32| {
        // Get the SMT threads for the Core
        let topo = hwloc::Topology::new();
        let core_depth = topo.depth_or_below_for_type(&hwloc::ObjectType::Core)
            .unwrap();
        let all_cores = topo.objects_at_depth(core_depth);

        for core in all_cores {
            for smt_thread in core.children().iter() {
                if smt_thread.os_index() == c {
                    return Some((
                        smt_thread.os_index() as usize,
                        smt_thread
                            .next_sibling()
                            .expect("CPU doesn't have SMT (check that provided core_id is the min of the pair)?")
                            .os_index() as usize,
                    ));
                }
            }
        }
        None
    });

    (pin_to, address, socket, suffix, timestamp)
}

fn main() {
    env_logger::init();
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml).get_matches();
    let (pin_to, address, socket, suffix, nic_timestamps) = parse_args(&matches);

    println!(
        "Listening on {} with process affinity set to {:?}.",
        address, pin_to
    );

    if let Some(_) = matches.subcommand_matches("smt") {
        println!(
            "Listening on {} with threads spawned on {:?}.",
            address, pin_to
        );
        let (tapp, tpoll) = spawn_listen_pair(address, socket, nic_timestamps, suffix, pin_to);

        tapp.join().expect("Can't join app-thread.");
        tpoll.join().expect("Can't join poll-thread.")
    } else if let Some(_) = matches.subcommand_matches("single") {
        pin_to.map(|ids: (usize, usize)| {
            netbench::pin_thread(vec![ids.0, ids.1]);
        });
        let mut channel = None;
        network_loop(address, socket, suffix, nic_timestamps, channel);
    }
}
