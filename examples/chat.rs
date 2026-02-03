#[macro_use]
extern crate tracing;

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use rouille::Server;
use rouille::{Request, Response};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::crypto::from_feature_flags;
use str0m::media::{Direction, KeyframeRequest, MediaData, Mid, Rid};
use str0m::media::{KeyframeRequestKind, MediaKind};
use str0m::net::Protocol;
use str0m::net::TcpType;
use str0m::{net::Receive, Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcError};

mod util;

#[derive(Debug)]
struct TcpIngress {
    source: SocketAddr,
    destination: SocketAddr,
    data: Vec<u8>,
}

#[derive(Debug)]
struct TcpEgress {
    destination: SocketAddr,
    data: Vec<u8>,
}

type TcpEgressTx = mpsc::Sender<TcpEgress>;

fn init_log() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("chat=info,str0m=info,dimpl=info"));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

pub fn main() {
    init_log();

    // Run with whatever is configured.
    from_feature_flags().install_process_default();

    let certificate = include_bytes!("cer.pem").to_vec();
    let private_key = include_bytes!("key.pem").to_vec();

    // Figure out some public IP address, since Firefox will not accept 127.0.0.1 for WebRTC traffic.
    let host_addr = util::select_host_address();

    let (tx, rx) = mpsc::sync_channel(1);

    // Spin up a UDP socket for the RTC. All WebRTC traffic is going to be multiplexed over this single
    // server socket. Clients are identified via their respective remote (UDP) socket address.
    let socket = UdpSocket::bind(format!("{host_addr}:19305")).expect("binding a random UDP port");
    let udp_addr = socket.local_addr().expect("a local socket address");
    info!("Bound UDP port: {}", udp_addr);

    let tcp_listener = TcpListener::bind(format!("{host_addr}:19305")).expect("binding a random TCP port");
    let tcp_addr = tcp_listener
        .local_addr()
        .expect("a local TCP listener address");
    info!("Bound TCP port: {}", tcp_addr);

    let (tcp_ingress_tx, tcp_ingress_rx) = mpsc::channel::<TcpIngress>();
    let (tcp_egress_tx, tcp_egress_rx) = mpsc::channel::<TcpEgress>();

    start_tcp_listener_thread(tcp_listener, tcp_ingress_tx, tcp_egress_rx);

    // The run loop is on a separate thread to the web server.
    thread::spawn(move || run(socket, tcp_ingress_rx, tcp_egress_tx, rx));

    let server = Server::new_ssl(
        "0.0.0.0:3000",
        move |request| web_request(request, udp_addr, tcp_addr, tx.clone()),
        certificate,
        private_key,
    )
    .expect("starting the web server");

    let port = server.server_addr().port();
    info!("Connect a browser to https://{:?}:{:?}", udp_addr.ip(), port);

    server.run();
}

// Handle a web request.
fn web_request(request: &Request, udp_addr: SocketAddr, tcp_addr: SocketAddr, tx: SyncSender<Rtc>) -> Response {
    if request.method() == "GET" {
        return Response::html(include_str!("chat.html"));
    }

    // Expected POST SDP Offers.
    let mut data = request.data().expect("body to be available");

    let offer: SdpOffer = serde_json::from_reader(&mut data).expect("serialized offer");
    let mut rtc = Rtc::builder()
        // Uncomment this to see statistics
        // .set_stats_interval(Some(Duration::from_secs(1)))
        .set_ice_lite(true)
        .build(Instant::now());

    // Add the shared UDP socket as a host candidate
    let candidate = Candidate::host(udp_addr, Protocol::Udp).expect("a UDP host candidate");
    rtc.add_local_candidate(candidate).unwrap();

    // Add the TCP listener socket as a host candidate (passive = we listen/accept).
    let candidate = Candidate::builder()
        .tcp()
        .host(tcp_addr)
        .tcptype(TcpType::Passive)
        .build()
        .expect("a TCP host candidate");
    rtc.add_local_candidate(candidate).unwrap();

    // Create an SDP Answer.
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .expect("offer to be accepted");

    // The Rtc instance is shipped off to the main run loop.
    tx.send(rtc).expect("to send Rtc instance");

    let body = serde_json::to_vec(&answer).expect("answer to serialize");

    Response::from_data("application/json", body)
}

/// This is the "main run loop" that handles all clients, reads and writes UdpSocket traffic,
/// and forwards media data between clients.
fn run(
    socket: UdpSocket,
    tcp_ingress_rx: Receiver<TcpIngress>,
    tcp_egress_tx: TcpEgressTx,
    rx: Receiver<Rtc>,
) -> Result<(), RtcError> {
    let mut clients: Vec<Client> = vec![];
    let mut to_propagate: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0; 2000];

    loop {
        // Clean out disconnected clients
        clients.retain(|c| c.rtc.is_alive());

        // Spawn new incoming clients from the web server thread.
        if let Some(mut client) = spawn_new_client(&rx) {
            // Add incoming tracks present in other already connected clients.
            for track in clients.iter().flat_map(|c| c.tracks_in.iter()) {
                let weak = Arc::downgrade(&track.id);
                client.handle_track_open(weak);
            }

            clients.push(client);
        }

        // Poll clients until they return timeout
        let mut timeout = Instant::now() + Duration::from_millis(100);
        for client in clients.iter_mut() {
            let t = poll_until_timeout(client, &mut to_propagate, &socket, &tcp_egress_tx);
            timeout = timeout.min(t);
        }

        // If we have an item to propagate, do that
        if let Some(p) = to_propagate.pop_front() {
            propagate(&p, &mut clients);
            continue;
        }

        // Drain any incoming TCP frames and route them to the right client.
        drain_tcp_ingress(&tcp_ingress_rx, &mut clients);

        // The read timeout is not allowed to be 0. In case it is 0, we set 1 millisecond.
        let duration = (timeout - Instant::now()).max(Duration::from_millis(1));

        socket
            .set_read_timeout(Some(duration))
            .expect("setting socket read timeout");

        if let Some(input) = read_udp_socket_input(&socket, &mut buf) {
            // The rtc.accepts() call is how we demultiplex the incoming packet to know which
            // Rtc instance the traffic belongs to.
            if let Some(client) = clients.iter_mut().find(|c| c.accepts(&input)) {
                // We found the client that accepts the input.
                client.handle_input(input);
            } else {
                // This is quite common because we don't get the Rtc instance via the mpsc channel
                // quickly enough before the browser send the first STUN.
                debug!("No client accepts UDP input: {:?}", input);
            }
        }

        // Drain again after a UDP receive (helps keep TCP latency down a bit).
        drain_tcp_ingress(&tcp_ingress_rx, &mut clients);

        // Drive time forward in all clients.
        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

fn drain_tcp_ingress(tcp_ingress_rx: &Receiver<TcpIngress>, clients: &mut [Client]) {
    loop {
        let ingress = match tcp_ingress_rx.try_recv() {
            Ok(v) => v,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => panic!("TCP ingress channel disconnected"),
        };

        let TcpIngress {
            source,
            destination,
            data,
        } = ingress;

        let Ok(receive) = Receive::new(Protocol::Tcp, source, destination, &data) else {
            continue;
        };

        let input = Input::Receive(Instant::now(), receive);
        if let Some(client) = clients.iter_mut().find(|c| c.accepts(&input)) {
            client.handle_input(input);
        } else {
            debug!("No client accepts TCP input: source={:?} destination={:?}", source, destination);
        }
    }
}

/// Receive new clients from the receiver and create new Client instances.
fn spawn_new_client(rx: &Receiver<Rtc>) -> Option<Client> {
    // try_recv here won't lock up the thread.
    match rx.try_recv() {
        Ok(rtc) => Some(Client::new(rtc)),
        Err(TryRecvError::Empty) => None,
        _ => panic!("Receiver<Rtc> disconnected"),
    }
}

/// Poll all the output from the client until it returns a timeout.
/// Collect any output in the queue, transmit data on the socket, return the timeout
fn poll_until_timeout(
    client: &mut Client,
    queue: &mut VecDeque<Propagated>,
    socket: &UdpSocket,
    tcp_egress_tx: &TcpEgressTx,
) -> Instant {
    loop {
        if !client.rtc.is_alive() {
            // This client will be cleaned up in the next run of the main loop.
            return Instant::now();
        }

        let propagated = client.poll_output(socket, tcp_egress_tx);

        if let Propagated::Timeout(t) = propagated {
            return t;
        }

        queue.push_back(propagated)
    }
}

/// Sends one "propagated" to all clients, if relevant
fn propagate(propagated: &Propagated, clients: &mut [Client]) {
    // Do not propagate to originating client.
    let Some(client_id) = propagated.client_id() else {
        // If the event doesn't have a client id, it can't be propagated,
        // (it's either a noop or a timeout).
        return;
    };

    for client in &mut *clients {
        if client.id == client_id {
            // Do not propagate to originating client.
            continue;
        }

        match &propagated {
            Propagated::TrackOpen(_, track_in) => client.handle_track_open(track_in.clone()),
            Propagated::MediaData(_, data) => client.handle_media_data_out(client_id, data),
            Propagated::KeyframeRequest(_, req, origin, mid_in) => {
                // Only one origin client handles the keyframe request.
                if *origin == client.id {
                    client.handle_keyframe_request(*req, *mid_in)
                }
            }
            Propagated::Noop | Propagated::Timeout(_) => {}
        }
    }
}

fn read_udp_socket_input<'a>(socket: &UdpSocket, buf: &'a mut Vec<u8>) -> Option<Input<'a>> {
    buf.resize(2000, 0);

    match socket.recv_from(buf) {
        Ok((n, source)) => {
            buf.truncate(n);

            let destination = socket.local_addr().unwrap();
            let Ok(receive) = Receive::new(Protocol::Udp, source, destination, buf.as_slice()) else {
                return None;
            };
            Some(Input::Receive(Instant::now(), receive))
        }

        Err(e) => match e.kind() {
            // Expected error for set_read_timeout(). One for windows, one for the rest.
            ErrorKind::WouldBlock | ErrorKind::TimedOut => None,
            _ => panic!("UdpSocket read failed: {e:?}"),
        },
    }
}

#[derive(Debug)]
struct Client {
    id: ClientId,
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    cid: Option<ChannelId>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    chosen_rid: Option<Rid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
struct TrackIn {
    origin: ClientId,
    mid: Mid,
    kind: MediaKind,
}

#[derive(Debug)]
struct TrackInEntry {
    id: Arc<TrackIn>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Debug)]
struct TrackOut {
    track_in: Weak<TrackIn>,
    state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m) | TrackOutState::Open(m) => Some(m),
        }
    }
}

impl Client {
    fn new(rtc: Rtc) -> Client {
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        Client {
            id: ClientId(next_id),
            rtc,
            pending: None,
            cid: None,
            tracks_in: vec![],
            tracks_out: vec![],
            chosen_rid: None,
        }
    }

    fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(e) = self.rtc.handle_input(input) {
            warn!("Client ({}) disconnected: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn poll_output(&mut self, socket: &UdpSocket, tcp_egress_tx: &TcpEgressTx) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }

        // Incoming tracks from other clients cause new entries in track_out that
        // need SDP negotiation with the remote peer.
        if self.negotiate_if_needed() {
            return Propagated::Noop;
        }

        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket, tcp_egress_tx),
            Err(e) => {
                warn!("Client ({}) poll_output failed: {:?}", *self.id, e);
                self.rtc.disconnect();
                Propagated::Noop
            }
        }
    }

    fn handle_output(
        &mut self,
        output: Output,
        socket: &UdpSocket,
        tcp_egress_tx: &TcpEgressTx,
    ) -> Propagated {
        match output {
            Output::Transmit(transmit) => {
                match transmit.proto {
                    Protocol::Udp => {
                        socket
                            .send_to(&transmit.contents, transmit.destination)
                            .expect("sending UDP data");
                    }
                    Protocol::Tcp => {
                        let _ = tcp_egress_tx.send(TcpEgress {
                            destination: transmit.destination,
                            data: transmit.contents.to_vec(),
                        });
                    }
                    other => {
                        debug!("Ignoring transmit for unsupported proto: {:?}", other);
                    }
                }
                Propagated::Noop
            }
            Output::Timeout(t) => Propagated::Timeout(t),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        // Ice disconnect could result in trying to establish a new connection,
                        // but this impl just disconnects directly.
                        self.rtc.disconnect();
                    }
                    Propagated::Noop
                }
                Event::MediaAdded(e) => self.handle_media_added(e.mid, e.kind),
                Event::MediaData(data) => self.handle_media_data_in(data),
                Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req),
                Event::ChannelOpen(cid, _) => {
                    self.cid = Some(cid);
                    Propagated::Noop
                }
                Event::ChannelData(data) => self.handle_channel_data(data),

                // NB: To see statistics, uncomment set_stats_interval() above.
                Event::MediaIngressStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                Event::MediaEgressStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                Event::PeerStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                _ => Propagated::Noop,
            },
        }
    }

    fn handle_media_added(&mut self, mid: Mid, kind: MediaKind) -> Propagated {
        let track_in = TrackInEntry {
            id: Arc::new(TrackIn {
                origin: self.id,
                mid,
                kind,
            }),
            last_keyframe_request: None,
        };

        // The Client instance owns the strong reference to the incoming
        // track, all other clients have a weak reference.
        let weak = Arc::downgrade(&track_in.id);
        self.tracks_in.push(track_in);

        Propagated::TrackOpen(self.id, weak)
    }

    fn handle_media_data_in(&mut self, data: MediaData) -> Propagated {
        if !data.contiguous {
            self.request_keyframe_throttled(data.mid, data.rid, KeyframeRequestKind::Fir);
        }

        Propagated::MediaData(self.id, data)
    }

    fn request_keyframe_throttled(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) {
        let Some(mut writer) = self.rtc.writer(mid) else {
            return;
        };

        let Some(track_entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };

        if track_entry
            .last_keyframe_request
            .map(|t| t.elapsed() < Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }

        _ = writer.request_keyframe(rid, kind);

        track_entry.last_keyframe_request = Some(Instant::now());
    }

    fn handle_incoming_keyframe_req(&self, mut req: KeyframeRequest) -> Propagated {
        // Need to figure out the track_in mid that needs to handle the keyframe request.
        let Some(track_out) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return Propagated::Noop;
        };
        let Some(track_in) = track_out.track_in.upgrade() else {
            return Propagated::Noop;
        };

        // This is the rid picked from incoming mediadata, and to which we need to
        // send the keyframe request.
        req.rid = self.chosen_rid;

        Propagated::KeyframeRequest(self.id, req, track_in.origin, track_in.mid)
    }

    fn negotiate_if_needed(&mut self) -> bool {
        if self.cid.is_none() || self.pending.is_some() {
            // Don't negotiate if there is no data channel, or if we have pending changes already.
            return false;
        }

        let mut change = self.rtc.sdp_api();

        for track in &mut self.tracks_out {
            if let TrackOutState::ToOpen = track.state {
                if let Some(track_in) = track.track_in.upgrade() {
                    let stream_id = track_in.origin.to_string();
                    let mid = change.add_media(
                        track_in.kind,
                        Direction::SendOnly,
                        Some(stream_id),
                        None,
                        None,
                    );
                    track.state = TrackOutState::Negotiating(mid);
                }
            }
        }

        if !change.has_changes() {
            return false;
        }

        let Some((offer, pending)) = change.apply() else {
            return false;
        };

        let Some(mut channel) = self.cid.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };

        let json = serde_json::to_string(&offer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");

        self.pending = Some(pending);

        true
    }

    fn handle_channel_data(&mut self, d: ChannelData) -> Propagated {
        if let Ok(offer) = serde_json::from_slice::<'_, SdpOffer>(&d.data) {
            self.handle_offer(offer);
        } else if let Ok(answer) = serde_json::from_slice::<'_, SdpAnswer>(&d.data) {
            self.handle_answer(answer);
        }

        Propagated::Noop
    }

    fn handle_offer(&mut self, offer: SdpOffer) {
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .expect("offer to be accepted");

        // Keep local track state in sync, cancelling any pending negotiation
        // so we can redo it after this offer is handled.
        for track in &mut self.tracks_out {
            if let TrackOutState::Negotiating(_) = track.state {
                track.state = TrackOutState::ToOpen;
            }
        }

        let mut channel = self
            .cid
            .and_then(|id| self.rtc.channel(id))
            .expect("channel to be open");

        let json = serde_json::to_string(&answer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");
    }

    fn handle_answer(&mut self, answer: SdpAnswer) {
        if let Some(pending) = self.pending.take() {
            self.rtc
                .sdp_api()
                .accept_answer(pending, answer)
                .expect("answer to be accepted");

            for track in &mut self.tracks_out {
                if let TrackOutState::Negotiating(m) = track.state {
                    track.state = TrackOutState::Open(m);
                }
            }
        }
    }

    fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        let track_out = TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        };
        self.tracks_out.push(track_out);
    }

    fn handle_media_data_out(&mut self, origin: ClientId, data: &MediaData) {
        // Figure out which outgoing track maps to the incoming media data.
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| i.origin == origin && i.mid == data.mid)
                    .is_some()
            })
            .and_then(|o| o.mid())
        else {
            return;
        };

        if data.rid.is_some() && data.rid != Some("h".into()) {
            // This is where we plug in a selection strategy for simulcast. For
            // now either let rid=None through (which would be no simulcast layers)
            // or "h" if we have simulcast (see commented out code in chat.html).
            return;
        }

        // Remember this value for keyframe requests.
        if self.chosen_rid != data.rid {
            self.chosen_rid = data.rid;
        }

        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };

        // Match outgoing pt to incoming codec.
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            warn!("Client ({}) failed: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest, mid_in: Mid) {
        let has_incoming_track = self.tracks_in.iter().any(|i| i.id.mid == mid_in);

        // This will be the case for all other client but the one where the track originates.
        if !has_incoming_track {
            return;
        }

        let Some(mut writer) = self.rtc.writer(mid_in) else {
            return;
        };

        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            // This can fail if the rid doesn't match any media.
            info!("request_keyframe failed: {:?}", e);
        }
    }
}

fn start_tcp_listener_thread(
    listener: TcpListener,
    tcp_ingress_tx: mpsc::Sender<TcpIngress>,
    tcp_egress_rx: mpsc::Receiver<TcpEgress>,
) {
    thread::spawn(move || {
        use std::collections::HashMap;

        // Connection writers keyed by peer addr (remote endpoint).
        let connections: Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Dispatcher: routes egress packets from the main loop to the per-connection writer.
        {
            let connections = connections.clone();
            thread::spawn(move || {
                while let Ok(msg) = tcp_egress_rx.recv() {
                    let tx = connections
                        .lock()
                        .expect("connections lock")
                        .get(&msg.destination)
                        .cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(msg.data);
                    } else {
                        debug!("No TCP connection for {:?}", msg.destination);
                    }
                }
            });
        }

        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    warn!("TCP accept error: {:?}", e);
                    continue;
                }
            };

            let peer = match stream.peer_addr() {
                Ok(a) => a,
                Err(e) => {
                    warn!("TCP peer_addr error: {:?}", e);
                    continue;
                }
            };

            let destination = match stream.local_addr() {
                Ok(a) => a,
                Err(_) => continue,
            };

            let _ = stream.set_nodelay(true);

            info!("Accepted TCP connection from {:?}", peer);

            let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>();
            connections
                .lock()
                .expect("connections lock")
                .insert(peer, writer_tx);

            let read_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    warn!("TCP try_clone error: {:?}", e);
                    connections.lock().expect("connections lock").remove(&peer);
                    continue;
                }
            };

            start_tcp_read_thread(read_stream, peer, destination, tcp_ingress_tx.clone(), connections.clone());
            start_tcp_write_thread(stream, peer, writer_rx, connections.clone());
        }
    });
}

fn start_tcp_read_thread(
    mut stream: TcpStream,
    source: SocketAddr,
    destination: SocketAddr,
    tcp_ingress_tx: mpsc::Sender<TcpIngress>,
    connections: Arc<Mutex<std::collections::HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>>,
) {
    thread::spawn(move || {
        loop {
            match read_rfc4571_frame(&mut stream) {
                Ok(Some(data)) => {
                    let ingress = TcpIngress {
                        source,
                        destination,
                        data,
                    };
                    if tcp_ingress_tx.send(ingress).is_err() {
                        break;
                    }
                }
                Ok(None) => break, // clean EOF
                Err(e) => {
                    debug!("TCP read error from {:?}: {:?}", source, e);
                    break;
                }
            }
        }

        let _ = connections.lock().map(|mut m| m.remove(&source));
    });
}

fn start_tcp_write_thread(
    mut stream: TcpStream,
    destination: SocketAddr,
    writer_rx: mpsc::Receiver<Vec<u8>>,
    connections: Arc<Mutex<std::collections::HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>>,
) {
    thread::spawn(move || {
        while let Ok(data) = writer_rx.recv() {
            if let Err(e) = write_rfc4571_frame(&mut stream, &data) {
                debug!("TCP write error to {:?}: {:?}", destination, e);
                break;
            }
        }
        let _ = connections.lock().map(|mut m| m.remove(&destination));
    });
}

fn read_rfc4571_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 2];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(Some(Vec::new()));
    }

    let mut data = vec![0u8; len];
    stream.read_exact(&mut data)?;
    Ok(Some(data))
}

fn write_rfc4571_frame(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    let len: u16 = data
        .len()
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(data)?;
    Ok(())
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Propagated {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),

    /// A new incoming track opened.
    TrackOpen(ClientId, Weak<TrackIn>),

    /// Data to be propagated from one client to another.
    MediaData(ClientId, MediaData),

    /// A keyframe request from one client to the source.
    KeyframeRequest(ClientId, KeyframeRequest, ClientId, Mid),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    fn client_id(&self) -> Option<ClientId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::MediaData(c, _)
            | Propagated::KeyframeRequest(c, _, _, _) => Some(*c),
            _ => None,
        }
    }
}
