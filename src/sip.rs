//! A small SIP user agent (RFC 3261 over UDP) with G.711 µ-law RTP.
//!
//! Enough for a payphone line: register to a SIP server, place a call to the
//! number the firmware dialled, receive calls (which ring the phone), digest
//! authentication, BYE/CANCEL, RFC 4733 DTMF for digits dialled mid-call.

use md5::{Digest, Md5};
use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SipConfig {
    pub enabled: bool,
    pub server: String,
    pub username: String,
    pub auth_user: String,
    pub password: String,
    pub display_name: String,
    pub local_port: u16,
    pub register: bool,
    /// Dialled numbers are sent as sip:<number>@<domain>; empty = server host.
    pub domain: String,
}

impl Default for SipConfig {
    fn default() -> Self {
        SipConfig {
            enabled: false,
            server: String::new(),
            username: String::new(),
            auth_user: String::new(),
            password: String::new(),
            display_name: "Payphone".into(),
            local_port: 5062,
            register: true,
            domain: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SipCmd {
    Configure(SipConfig),
    Dial(String),
    Answer,
    Hangup,
    Dtmf(char),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SipEvent {
    Status(String),
    Registered(bool),
    Incoming(String),
    Ringing,
    EarlyMedia,
    Answered,
    Ended(String),
    Log(String),
}

/// Audio shared with the sound card: 8 kHz samples in -1..1.
#[derive(Default)]
pub struct CallAudio {
    pub from_line: VecDeque<f32>,
    pub to_line: VecDeque<f32>,
    pub active: bool,
    /// A data call: the phone's modem owns the line audio (no mic/speaker).
    pub modem: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CallState {
    Calling,
    Early,
    Talking,
    IncomingRinging,
}

struct Call {
    state: CallState,
    call_id: String,
    local_tag: String,
    remote_tag: String,
    remote_uri: String,
    remote_target: String,
    cseq: u32,
    invite_branch: String,
    invite_req: String,
    remote_rtp: Option<SocketAddr>,
    pt_event: u8,
    incoming_invite: Option<Msg>,
    route: Vec<String>,
    started: Instant,
    auth_tries: u32,
    /// Where an incoming INVITE came from (replies go back there).
    peer: Option<SocketAddr>,
    /// Payload type we send: 0 (PCMU) or 8 (PCMA), following the far end.
    tx_pt: u8,
}

#[derive(Debug, Clone)]
struct Msg {
    start: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Msg {
    fn parse(s: &str) -> Option<Msg> {
        let (head, body) = match s.find("\r\n\r\n") {
            Some(i) => (&s[..i], &s[i + 4..]),
            None => (s, ""),
        };
        let mut lines = head.split("\r\n");
        let start = lines.next()?.to_string();
        let mut headers = Vec::new();
        for l in lines {
            if l.starts_with(' ') || l.starts_with('\t') {
                if let Some(last) = headers.last_mut() {
                    let (_, v): &mut (String, String) = last;
                    v.push(' ');
                    v.push_str(l.trim());
                }
                continue;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((expand(k.trim()), v.trim().to_string()));
            }
        }
        Some(Msg { start, headers, body: body.to_string() })
    }
    fn h(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    }
    fn is_response(&self) -> bool {
        self.start.starts_with("SIP/2.0")
    }
    fn code(&self) -> u16 {
        self.start.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
    }
    fn method(&self) -> &str {
        self.start.split_whitespace().next().unwrap_or("")
    }
    fn cseq_method(&self) -> String {
        self.h("CSeq").and_then(|c| c.split_whitespace().nth(1)).unwrap_or("").to_string()
    }
}

fn expand(k: &str) -> String {
    match k {
        "v" => "Via",
        "f" => "From",
        "t" => "To",
        "i" => "Call-ID",
        "m" => "Contact",
        "l" => "Content-Length",
        "c" => "Content-Type",
        _ => k,
    }
    .to_string()
}

fn rand_hex(n: usize) -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    (0..n).map(|_| format!("{:x}", r.gen_range(0..16))).collect()
}

fn md5hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

fn param(h: &str, name: &str) -> Option<String> {
    for part in h.split([',', ' ']) {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.trim_matches('"').to_string());
            }
        }
    }
    None
}

fn tag_of(h: &str) -> String {
    h.split(';').find_map(|p| p.trim().strip_prefix("tag=").map(|s| s.to_string())).unwrap_or_default()
}

fn uri_of(h: &str) -> String {
    if let (Some(a), Some(b)) = (h.find('<'), h.find('>')) {
        h[a + 1..b].to_string()
    } else {
        h.split(';').next().unwrap_or("").trim().to_string()
    }
}

/// G.711 µ-law
pub fn ulaw_encode(x: f32) -> u8 {
    let s = (x.clamp(-1.0, 1.0) * 32124.0) as i32;
    let sign = if s < 0 { 0x80 } else { 0 };
    let mut m = s.abs() + 132;
    if m > 32635 {
        m = 32635;
    }
    let mut exp = 7;
    let mut mask = 0x4000;
    while exp > 0 && m & mask == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mant = (m >> (exp + 3)) & 0x0F;
    !(sign | (exp << 4) | mant) as u8
}

pub fn ulaw_decode(u: u8) -> f32 {
    let u = !u;
    let sign = u & 0x80;
    let exp = (u >> 4) & 7;
    let mant = u & 0x0F;
    let mut s = (((mant as i32) << 3) + 132) << exp;
    s -= 132;
    let v = if sign != 0 { -s } else { s };
    v as f32 / 32768.0
}

pub struct SipAgent {
    cfg: SipConfig,
    sock: Option<UdpSocket>,
    rtp: Option<UdpSocket>,
    server: Option<SocketAddr>,
    local_ip: String,
    local_port: u16,
    rtp_port: u16,
    public: Option<(String, u16)>,
    reg_call_id: String,
    reg_tag: String,
    reg_cseq: u32,
    reg_due: Instant,
    last_keepalive: Instant,
    reg_tries: u32,
    registered: bool,
    call: Option<Call>,
    events: Sender<SipEvent>,
    audio: Arc<Mutex<CallAudio>>,
    rtp_seq: u16,
    stats: RtpStats,
    rtp_ts: u32,
    rtp_ssrc: u32,
    rtp_next: Instant,
    dtmf_queue: VecDeque<char>,
}

impl SipAgent {
    pub fn spawn(cmds: Receiver<SipCmd>, events: Sender<SipEvent>, audio: Arc<Mutex<CallAudio>>) {
        std::thread::spawn(move || {
            let mut a = SipAgent {
                cfg: SipConfig::default(),
                sock: None,
                rtp: None,
                server: None,
                local_ip: "127.0.0.1".into(),
                local_port: 0,
                rtp_port: 0,
                public: None,
                reg_call_id: rand_hex(16),
                reg_tag: rand_hex(8),
                reg_cseq: 1,
                reg_due: Instant::now(),
                last_keepalive: Instant::now(),
                reg_tries: 0,
                registered: false,
                call: None,
                events,
                audio,
                rtp_seq: 1,
                stats: RtpStats::default(),
                rtp_ts: 0,
                rtp_ssrc: rand::random(),
                rtp_next: Instant::now(),
                dtmf_queue: VecDeque::new(),
            };
            a.run(cmds);
        });
    }

    fn log(&self, s: String) {
        let _ = self.events.send(SipEvent::Log(s));
    }
    fn status(&self, s: &str) {
        let _ = self.events.send(SipEvent::Status(s.to_string()));
    }

    fn run(&mut self, cmds: Receiver<SipCmd>) {
        let mut buf = vec![0u8; 8192];
        loop {
            // commands
            loop {
                match cmds.try_recv() {
                    Ok(c) => self.command(c),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => return,
                }
            }
            // signalling
            if let Some(s) = &self.sock {
                let s = s.try_clone().unwrap();
                for _ in 0..16 {
                    match s.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            let text = String::from_utf8_lossy(&buf[..n]).to_string();
                            if text.trim().is_empty() {
                                continue;
                            }
                            if let Some(m) = Msg::parse(&text) {
                                self.log(format!("<< {} ({})", m.start, from));
                                self.handle(m, from);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            // registration refresh
            if self.cfg.enabled && self.cfg.register && self.sock.is_some() && Instant::now() >= self.reg_due {
                self.send_register(None);
                self.reg_due = Instant::now() + Duration::from_secs(if self.registered { 90 } else { 30 });
            }
            // NAT keep-alive: home routers drop idle UDP mappings in 30-60 s,
            // after which the server's INVITE for an incoming call can't reach us.
            if self.cfg.enabled && self.sock.is_some() && self.last_keepalive.elapsed() > Duration::from_secs(15) {
                self.last_keepalive = Instant::now();
                if let (Some(s), Some(srv)) = (&self.sock, self.server) {
                    let _ = s.send_to(
                        b"

", srv,
                    );
                }
            }
            self.media();
            // ring timeout for outgoing calls that never answer
            if let Some(c) = &self.call {
                if c.state == CallState::Calling && c.started.elapsed() > Duration::from_secs(90) {
                    self.hangup("No answer");
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn command(&mut self, c: SipCmd) {
        match c {
            SipCmd::Configure(cfg) => {
                let changed = cfg != self.cfg || self.sock.is_none();
                self.cfg = cfg;
                if changed {
                    self.setup();
                }
            }
            SipCmd::Dial(num) => self.dial(&num),
            SipCmd::Answer => self.answer(),
            SipCmd::Hangup => self.hangup("Hung up"),
            SipCmd::Dtmf(ch) => self.dtmf_queue.push_back(ch),
        }
    }

    fn setup(&mut self) {
        self.sock = None;
        self.rtp = None;
        self.registered = false;
        let _ = self.events.send(SipEvent::Registered(false));
        if !self.cfg.enabled {
            self.status("Internet calling is off");
            return;
        }
        let host = self.cfg.server.trim();
        if host.is_empty() {
            self.status("No SIP server set");
            return;
        }
        let with_port = if host.contains(':') { host.to_string() } else { format!("{}:5060", host) };
        let server = match with_port.to_socket_addrs().ok().and_then(|mut a| a.find(|x| x.is_ipv4())) {
            Some(a) => a,
            None => {
                self.status(&format!("Can't find SIP server {}", host));
                return;
            }
        };
        self.server = Some(server);
        let sock = UdpSocket::bind(("0.0.0.0", self.cfg.local_port)).or_else(|_| UdpSocket::bind("0.0.0.0:0"));
        let sock = match sock {
            Ok(s) => s,
            Err(e) => {
                self.status(&format!("Can't open SIP port: {}", e));
                return;
            }
        };
        let _ = sock.set_nonblocking(true);
        // local address used to reach the server
        if let Ok(probe) = UdpSocket::bind("0.0.0.0:0") {
            if probe.connect(server).is_ok() {
                if let Ok(a) = probe.local_addr() {
                    self.local_ip = a.ip().to_string();
                }
            }
        }
        self.local_port = sock.local_addr().map(|a| a.port()).unwrap_or(5060);
        let mut rtp = None;
        for p in (16384u16..17000).step_by(2) {
            if let Ok(s) = UdpSocket::bind(("0.0.0.0", p)) {
                let _ = s.set_nonblocking(true);
                self.rtp_port = p;
                rtp = Some(s);
                break;
            }
        }
        self.rtp = rtp;
        self.sock = Some(sock);
        self.reg_due = Instant::now();
        self.status(&format!("SIP ready on {}:{}", self.local_ip, self.local_port));
        if !self.cfg.register {
            self.status("SIP ready (no registration)");
        }
    }

    fn contact_host(&self) -> (String, u16) {
        self.public.clone().unwrap_or((self.local_ip.clone(), self.local_port))
    }

    fn domain(&self) -> String {
        if !self.cfg.domain.trim().is_empty() {
            self.cfg.domain.trim().to_string()
        } else {
            self.cfg.server.trim().split(':').next().unwrap_or("").to_string()
        }
    }

    fn send(&self, text: &str) {
        if let (Some(s), Some(srv)) = (&self.sock, self.server) {
            let first = text.lines().next().unwrap_or("");
            self.log(format!(">> {}", first));
            let _ = s.send_to(text.as_bytes(), srv);
        }
    }

    fn via(&self, branch: &str) -> String {
        format!("SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}", self.local_ip, self.local_port, branch)
    }

    fn auth_header(&self, chal: &str, method: &str, uri: &str) -> String {
        let realm = param(chal, "realm").unwrap_or_default();
        let nonce = param(chal, "nonce").unwrap_or_default();
        let opaque = param(chal, "opaque");
        let qop = param(chal, "qop");
        let user = if self.cfg.auth_user.trim().is_empty() { &self.cfg.username } else { &self.cfg.auth_user };
        let ha1 = md5hex(&format!("{}:{}:{}", user, realm, self.cfg.password));
        let ha2 = md5hex(&format!("{}:{}", method, uri));
        let mut h;
        if qop.as_deref().map(|q| q.split(',').any(|x| x.trim() == "auth")).unwrap_or(false) {
            let cnonce = rand_hex(16);
            let nc = "00000001";
            let resp = md5hex(&format!("{}:{}:{}:{}:auth:{}", ha1, nonce, nc, cnonce, ha2));
            h = format!(
                "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5, cnonce=\"{}\", qop=auth, nc={}",
                user, realm, nonce, uri, resp, cnonce, nc
            );
        } else {
            let resp = md5hex(&format!("{}:{}:{}", ha1, nonce, ha2));
            h = format!("Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5", user, realm, nonce, uri, resp);
        }
        if let Some(o) = opaque {
            h.push_str(&format!(", opaque=\"{}\"", o));
        }
        h
    }

    fn send_register(&mut self, auth: Option<(&str, String)>) {
        let dom = self.domain();
        let uri = format!("sip:{}", dom);
        let (ch, cp) = self.contact_host();
        let mut r = format!(
            "REGISTER {uri} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\nFrom: \"{dn}\" <sip:{user}@{dom}>;tag={tag}\r\nTo: <sip:{user}@{dom}>\r\nCall-ID: {cid}\r\nCSeq: {cseq} REGISTER\r\nContact: <sip:{user}@{ch}:{cp}>\r\nExpires: 300\r\nUser-Agent: ElcotelEmu/1.0\r\n",
            uri = uri,
            via = self.via(&rand_hex(12)),
            dn = self.cfg.display_name,
            user = self.cfg.username,
            dom = dom,
            tag = self.reg_tag,
            cid = self.reg_call_id,
            cseq = self.reg_cseq,
            ch = ch,
            cp = cp
        );
        self.reg_cseq += 1;
        if let Some((hname, chal)) = auth {
            let a = self.auth_header(&chal, "REGISTER", &uri);
            r.push_str(&format!("{}: {}\r\n", hname, a));
        }
        r.push_str("Content-Length: 0\r\n\r\n");
        self.send(&r);
    }

    fn sdp(&self) -> String {
        let pt = self.call.as_ref().map(|c| c.tx_pt).unwrap_or(0);
        let ip = self.local_ip.clone();
        // the codec the caller offered first goes first in our answer
        let codec = if pt == 8 { "8 0" } else { "0 8" };
        format!(
            "v=0\r\no=elcotel 1 1 IN IP4 {ip}\r\ns=payphone\r\nc=IN IP4 {ip}\r\nt=0 0\r\nm=audio {port} RTP/AVP {codec} 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-15\r\na=ptime:20\r\na=sendrecv\r\n",
            ip = ip,
            codec = codec,
            port = self.rtp_port
        )
    }

    fn build_invite(&self, c: &Call, auth: Option<String>) -> String {
        let dom = self.domain();
        let (ch, cp) = self.contact_host();
        let body = self.sdp();
        let mut r = format!(
            "INVITE {ruri} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\nFrom: \"{dn}\" <sip:{user}@{dom}>;tag={tag}\r\nTo: <{ruri}>\r\nCall-ID: {cid}\r\nCSeq: {cseq} INVITE\r\nContact: <sip:{user}@{ch}:{cp}>\r\nAllow: INVITE, ACK, CANCEL, BYE, OPTIONS\r\nUser-Agent: ElcotelEmu/1.0\r\nContent-Type: application/sdp\r\n",
            ruri = c.remote_uri,
            via = self.via(&c.invite_branch),
            dn = self.cfg.display_name,
            user = self.cfg.username,
            dom = dom,
            tag = c.local_tag,
            cid = c.call_id,
            cseq = c.cseq,
            ch = ch,
            cp = cp
        );
        if let Some(a) = auth {
            r.push_str(&a);
        }
        r.push_str(&format!("Content-Length: {}\r\n\r\n{}", body.len(), body));
        r
    }

    fn dial(&mut self, number: &str) {
        if self.sock.is_none() {
            self.status("Can't call: internet calling isn't set up");
            let _ = self.events.send(SipEvent::Ended("not set up".into()));
            return;
        }
        if self.call.is_some() {
            return;
        }
        let dom = self.domain();
        let uri = if number.contains('@') { format!("sip:{}", number) } else { format!("sip:{}@{}", number, dom) };
        let c = Call {
            state: CallState::Calling,
            call_id: rand_hex(20),
            local_tag: rand_hex(8),
            remote_tag: String::new(),
            remote_uri: uri.clone(),
            remote_target: uri,
            cseq: 1,
            invite_branch: rand_hex(12),
            invite_req: String::new(),
            remote_rtp: None,
            pt_event: 101,
            incoming_invite: None,
            route: Vec::new(),
            started: Instant::now(),
            auth_tries: 0,
            peer: None,
            tx_pt: 0,
        };
        self.stats = RtpStats::default();
        let inv = self.build_invite(&c, None);
        let mut c = c;
        c.invite_req = inv.clone();
        self.call = Some(c);
        self.send(&inv);
        self.status(&format!("Calling {}", number));
    }

    fn handle(&mut self, m: Msg, from: SocketAddr) {
        if m.is_response() {
            self.response(m);
        } else {
            self.request(m, from);
        }
    }

    fn learn_public(&mut self, m: &Msg) {
        if let Some(v) = m.h("Via") {
            let rport = v.split(';').find_map(|p| p.trim().strip_prefix("rport=").and_then(|x| x.parse::<u16>().ok()));
            let recv = v.split(';').find_map(|p| p.trim().strip_prefix("received=").map(|x| x.to_string()));
            if let (Some(ip), Some(port)) = (recv, rport) {
                let new = Some((ip, port));
                if new != self.public {
                    // the server sees us at another address (NAT): register
                    // again at once so incoming calls are sent there
                    if self.public.is_none() || self.registered {
                        self.reg_due = Instant::now();
                    }
                    self.public = new;
                }
            }
        }
    }

    fn response(&mut self, m: Msg) {
        let code = m.code();
        let method = m.cseq_method();
        if method == "REGISTER" {
            self.learn_public(&m);
            if code == 401 || code == 407 {
                self.reg_tries += 1;
                if self.reg_tries > 3 {
                    self.status("Registration refused (check user name and password)");
                    return;
                }
                let (hn, chal) = if code == 401 {
                    ("Authorization", m.h("WWW-Authenticate").unwrap_or("").to_string())
                } else {
                    ("Proxy-Authorization", m.h("Proxy-Authenticate").unwrap_or("").to_string())
                };
                self.send_register(Some((hn, chal)));
            } else if (200..300).contains(&code) {
                self.reg_tries = 0;
                if !self.registered {
                    self.status("Registered with the SIP server");
                }
                self.registered = true;
                let _ = self.events.send(SipEvent::Registered(true));
            } else if code >= 300 {
                self.registered = false;
                let _ = self.events.send(SipEvent::Registered(false));
                self.status(&format!("Registration failed: {}", m.start));
            }
            return;
        }
        let Some(cid) = m.h("Call-ID").map(|s| s.to_string()) else { return };
        let Some(c) = self.call.as_mut() else { return };
        if c.call_id != cid {
            return;
        }
        if method == "INVITE" {
            if code >= 200 {
                // ACK
                let to = m.h("To").unwrap_or("").to_string();
                let tag = tag_of(&to);
                if !tag.is_empty() {
                    c.remote_tag = tag;
                }
                if code < 300 {
                    if let Some(ct) = m.h("Contact") {
                        c.remote_target = uri_of(ct);
                    }
                    c.route = m.all("Record-Route").iter().rev().map(|s| s.to_string()).collect();
                }
            }
            match code {
                100 => {}
                180 | 183 => {
                    if let Some(a) = parse_sdp(&m.body) {
                        c.remote_rtp = Some(a.0);
                        c.pt_event = a.1;
                    }
                    if c.state == CallState::Calling {
                        c.state = CallState::Early;
                    }
                    let ev = if code == 183 || c.remote_rtp.is_some() { SipEvent::EarlyMedia } else { SipEvent::Ringing };
                    let _ = self.events.send(ev);
                    self.status("Ringing");
                }
                401 | 407 => {
                    let ack = self.ack_for_error(&m);
                    self.send(&ack);
                    let c = self.call.as_mut().unwrap();
                    c.auth_tries += 1;
                    if c.auth_tries > 2 {
                        self.end_call("Call refused (authentication)");
                        return;
                    }
                    let (hn, chal) = if code == 401 {
                        ("Authorization", m.h("WWW-Authenticate").unwrap_or("").to_string())
                    } else {
                        ("Proxy-Authorization", m.h("Proxy-Authenticate").unwrap_or("").to_string())
                    };
                    let c = self.call.as_ref().unwrap();
                    let a = self.auth_header(&chal, "INVITE", &c.remote_uri);
                    let mut c2 = self.call.take().unwrap();
                    c2.cseq += 1;
                    c2.invite_branch = rand_hex(12);
                    c2.remote_tag.clear();
                    let inv = self.build_invite(&c2, Some(format!("{}: {}\r\n", hn, a)));
                    c2.invite_req = inv.clone();
                    self.call = Some(c2);
                    self.send(&inv);
                }
                200..=299 => {
                    if let Some(a) = parse_sdp(&m.body) {
                        c.tx_pt = offered_pt(&m.body);
                        c.remote_rtp = Some(a.0);
                        c.pt_event = a.1;
                    }
                    let first = c.state != CallState::Talking;
                    c.state = CallState::Talking;
                    let ack = self.ack_2xx();
                    self.send(&ack);
                    if first {
                        self.audio.lock().unwrap().active = true;
                        let _ = self.events.send(SipEvent::Answered);
                        self.status("Connected");
                    }
                }
                _ if code >= 300 => {
                    let ack = self.ack_for_error(&m);
                    self.send(&ack);
                    let reason = m.start.splitn(3, ' ').nth(2).unwrap_or("").to_string();
                    self.end_call(&format!("Call failed: {} {}", code, reason));
                }
                _ => {}
            }
        } else if method == "BYE" || method == "CANCEL" {
            // our BYE/CANCEL answered
        }
    }

    fn ack_for_error(&self, m: &Msg) -> String {
        let c = self.call.as_ref().unwrap();
        let to = m.h("To").unwrap_or("").to_string();
        let from = m.h("From").unwrap_or("").to_string();
        format!(
            "ACK {ruri} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {cid}\r\nCSeq: {cseq} ACK\r\nContent-Length: 0\r\n\r\n",
            ruri = c.remote_uri,
            via = self.via(&c.invite_branch),
            from = from,
            to = to,
            cid = c.call_id,
            cseq = c.cseq
        )
    }

    fn dialog_headers(&self, c: &Call) -> (String, String) {
        let dom = self.domain();
        let from = format!("\"{}\" <sip:{}@{}>;tag={}", self.cfg.display_name, self.cfg.username, dom, c.local_tag);
        let to = if c.remote_tag.is_empty() { format!("<{}>", c.remote_uri) } else { format!("<{}>;tag={}", c.remote_uri, c.remote_tag) };
        (from, to)
    }

    fn ack_2xx(&self) -> String {
        let c = self.call.as_ref().unwrap();
        let (from, to) = self.dialog_headers(c);
        let routes: String = c.route.iter().map(|r| format!("Route: {}\r\n", r)).collect();
        format!(
            "ACK {target} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\n{routes}From: {from}\r\nTo: {to}\r\nCall-ID: {cid}\r\nCSeq: {cseq} ACK\r\nContent-Length: 0\r\n\r\n",
            target = c.remote_target,
            via = self.via(&rand_hex(12)),
            routes = routes,
            from = from,
            to = to,
            cid = c.call_id,
            cseq = c.cseq
        )
    }

    fn request(&mut self, m: Msg, from: SocketAddr) {
        let method = m.method().to_string();
        match method.as_str() {
            "OPTIONS" => {
                let r = self.reply(&m, 200, "OK", None, "");
                self.send_to(&r, from);
            }
            "INVITE" => {
                let cid = m.h("Call-ID").unwrap_or("").to_string();
                if let Some(c) = &self.call {
                    if c.call_id == cid {
                        // retransmission / re-INVITE: answer with what we have
                        if c.state == CallState::IncomingRinging {
                            let tag = c.local_tag.clone();
                            let r = self.reply(&m, 180, "Ringing", Some(&tag), "");
                            self.send_to(&r, from);
                        }
                        if c.state == CallState::Talking {
                            let tag = c.local_tag.clone();
                            let r = self.reply(&m, 200, "OK", Some(&tag), &self.sdp());
                            self.send_to(&r, from);
                        }
                        return;
                    }
                    let r = self.reply(&m, 486, "Busy Here", None, "");
                    self.send_to(&r, from);
                    return;
                }
                let tag = rand_hex(8);
                let r = self.reply(&m, 100, "Trying", None, "");
                self.send_to(&r, from);
                let r = self.reply(&m, 180, "Ringing", Some(&tag), "");
                self.send_to(&r, from);
                let remote = m.h("From").unwrap_or("").to_string();
                let caller = uri_of(&remote);
                let rtp = parse_sdp(&m.body);
                self.call = Some(Call {
                    state: CallState::IncomingRinging,
                    call_id: cid,
                    local_tag: tag,
                    remote_tag: tag_of(&remote),
                    remote_uri: caller.clone(),
                    remote_target: m.h("Contact").map(uri_of).unwrap_or(caller.clone()),
                    cseq: 1,
                    invite_branch: rand_hex(12),
                    invite_req: String::new(),
                    remote_rtp: rtp.map(|r| r.0),
                    pt_event: rtp.map(|r| r.1).unwrap_or(101),
                    incoming_invite: Some(m.clone()),
                    route: m.all("Record-Route").iter().map(|s| s.to_string()).collect(),
                    started: Instant::now(),
                    auth_tries: 0,
                    peer: Some(from),
                    tx_pt: offered_pt(&m.body),
                });
                self.stats = RtpStats::default();
                let codecs: Vec<&str> = m.body.lines().filter(|l| l.starts_with("m=audio") || l.starts_with("a=rtpmap") || l.starts_with("c=")).collect();
                self.log(format!("caller media: {}", codecs.join(" | ")));
                let num = caller.trim_start_matches("sip:").split('@').next().unwrap_or("").to_string();
                let _ = self.events.send(SipEvent::Incoming(num.clone()));
                self.status(&format!("Incoming call from {}", num));
            }
            "ACK" => {}
            "BYE" => {
                let r = self.reply(&m, 200, "OK", None, "");
                self.send_to(&r, from);
                let cid = m.h("Call-ID").unwrap_or("");
                if self.call.as_ref().map(|c| c.call_id == cid).unwrap_or(false) {
                    self.end_call("The other side hung up");
                }
            }
            "CANCEL" => {
                let r = self.reply(&m, 200, "OK", None, "");
                self.send_to(&r, from);
                let cid = m.h("Call-ID").unwrap_or("").to_string();
                if let Some(c) = &self.call {
                    if c.call_id == cid {
                        if let Some(inv) = c.incoming_invite.clone() {
                            let tag = c.local_tag.clone();
                            let r = self.reply(&inv, 487, "Request Terminated", Some(&tag), "");
                            self.send_to(&r, from);
                        }
                        self.end_call("Caller gave up");
                    }
                }
            }
            "NOTIFY" | "INFO" | "UPDATE" => {
                let r = self.reply(&m, 200, "OK", None, "");
                self.send_to(&r, from);
            }
            _ => {
                let r = self.reply(&m, 501, "Not Implemented", None, "");
                self.send_to(&r, from);
            }
        }
    }

    fn send_to(&self, text: &str, to: SocketAddr) {
        if let Some(s) = &self.sock {
            self.log(format!(">> {}", text.lines().next().unwrap_or("")));
            let _ = s.send_to(text.as_bytes(), to);
        }
    }

    fn reply(&self, req: &Msg, code: u16, reason: &str, to_tag: Option<&str>, sdp: &str) -> String {
        let mut r = format!("SIP/2.0 {} {}\r\n", code, reason);
        for v in req.all("Via") {
            r.push_str(&format!("Via: {}\r\n", v));
        }
        for v in req.all("Record-Route") {
            r.push_str(&format!("Record-Route: {}\r\n", v));
        }
        r.push_str(&format!("From: {}\r\n", req.h("From").unwrap_or("")));
        let to = req.h("To").unwrap_or("").to_string();
        match to_tag {
            Some(t) if !to.contains("tag=") => r.push_str(&format!("To: {};tag={}\r\n", to, t)),
            _ => r.push_str(&format!("To: {}\r\n", to)),
        }
        r.push_str(&format!("Call-ID: {}\r\n", req.h("Call-ID").unwrap_or("")));
        r.push_str(&format!("CSeq: {}\r\n", req.h("CSeq").unwrap_or("")));
        let (ch, cp) = self.contact_host();
        r.push_str(&format!("Contact: <sip:{}@{}:{}>\r\nUser-Agent: ElcotelEmu/1.0\r\n", self.cfg.username, ch, cp));
        if sdp.is_empty() {
            r.push_str("Content-Length: 0\r\n\r\n");
        } else {
            r.push_str(&format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}", sdp.len(), sdp));
        }
        r
    }

    fn answer(&mut self) {
        let Some(c) = &self.call else { return };
        if c.state != CallState::IncomingRinging {
            return;
        }
        let Some(inv) = c.incoming_invite.clone() else { return };
        let tag = c.local_tag.clone();
        let r = self.reply(&inv, 200, "OK", Some(&tag), &self.sdp());
        // send back where the INVITE came from (top Via) or the server
        match c.peer {
            Some(p) => self.send_to(&r, p),
            None => self.send(&r),
        }
        if let Some(c) = self.call.as_mut() {
            c.state = CallState::Talking;
        }
        self.audio.lock().unwrap().active = true;
        let _ = self.events.send(SipEvent::Answered);
        self.status("Connected");
    }

    fn hangup(&mut self, why: &str) {
        let Some(c) = &self.call else { return };
        match c.state {
            CallState::Calling | CallState::Early => {
                // CANCEL uses the INVITE's branch
                let (from, _) = self.dialog_headers(c);
                let r = format!(
                    "CANCEL {ruri} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\nFrom: {from}\r\nTo: <{ruri}>\r\nCall-ID: {cid}\r\nCSeq: {cseq} CANCEL\r\nContent-Length: 0\r\n\r\n",
                    ruri = c.remote_uri,
                    via = self.via(&c.invite_branch),
                    from = from,
                    cid = c.call_id,
                    cseq = c.cseq
                );
                self.send(&r);
            }
            CallState::Talking => {
                let (from, to) = if let Some(inv) = c.incoming_invite.as_ref() {
                    // we are the callee: swap roles
                    let to_hdr = inv.h("To").unwrap_or("").to_string();
                    let to_hdr = if to_hdr.contains("tag=") { to_hdr } else { format!("{};tag={}", to_hdr, c.local_tag) };
                    (to_hdr, inv.h("From").unwrap_or("").to_string())
                } else {
                    self.dialog_headers(c)
                };
                let routes: String = c.route.iter().map(|r| format!("Route: {}\r\n", r)).collect();
                let r = format!(
                    "BYE {target} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\n{routes}From: {from}\r\nTo: {to}\r\nCall-ID: {cid}\r\nCSeq: {cseq} BYE\r\nContent-Length: 0\r\n\r\n",
                    target = c.remote_target,
                    via = self.via(&rand_hex(12)),
                    routes = routes,
                    from = from,
                    to = to,
                    cid = c.call_id,
                    cseq = c.cseq + 1
                );
                self.send(&r);
            }
            CallState::IncomingRinging => {
                if let Some(inv) = c.incoming_invite.clone() {
                    let tag = c.local_tag.clone();
                    let r = self.reply(&inv, 486, "Busy Here", Some(&tag), "");
                    self.send(&r);
                }
            }
        }
        self.end_call(why);
    }

    fn end_call(&mut self, why: &str) {
        if self.stats.packets > 0 || self.stats.late > 0 {
            let s = &self.stats;
            self.log(format!(
                "RTP received: {} packets, {} lost, {} late/duplicate, {} with header extensions, payload types {:?}",
                s.packets, s.lost, s.late, s.ext, s.pts
            ));
        }
        self.stats = RtpStats::default();
        self.call = None;
        {
            let mut a = self.audio.lock().unwrap();
            a.active = false;
            a.from_line.clear();
            a.to_line.clear();
        }
        self.dtmf_queue.clear();
        let _ = self.events.send(SipEvent::Ended(why.to_string()));
        self.status(why);
    }

    /// RTP: 20 ms PCMU frames both ways.
    fn media(&mut self) {
        let Some(rtp) = &self.rtp else { return };
        let mut buf = [0u8; 2048];
        let media_on = self.call.as_ref().map(|c| matches!(c.state, CallState::Talking | CallState::Early)).unwrap_or(false);
        while let Ok((n, from)) = rtp.recv_from(&mut buf) {
            if n < 12 || !media_on {
                continue;
            }
            if let Some(c) = self.call.as_mut() {
                if c.remote_rtp.is_none() {
                    c.remote_rtp = Some(from);
                }
            }
            // RTP header: version, padding, extension, CSRC count
            if buf[0] >> 6 != 2 {
                continue;
            }
            let pt = buf[1] & 0x7F;
            let cc = (buf[0] & 0x0F) as usize;
            let mut hdr = 12 + cc * 4;
            let mut end = n;
            if buf[0] & 0x10 != 0 && n >= hdr + 4 {
                let words = u16::from_be_bytes([buf[hdr + 2], buf[hdr + 3]]) as usize;
                hdr += 4 + words * 4;
                self.stats.ext += 1;
            }
            if buf[0] & 0x20 != 0 && n > hdr {
                end = n.saturating_sub(buf[n - 1] as usize);
            }
            *self.stats.pts.entry(pt).or_default() += 1;
            if (pt != 0 && pt != 8) || end <= hdr {
                continue;
            }
            let seq = u16::from_be_bytes([buf[2], buf[3]]);
            let len = end - hdr;
            let mut lost = 0usize;
            if let Some(last) = self.stats.last_seq {
                let d = seq.wrapping_sub(last);
                if d == 0 || d > 0x8000 {
                    self.stats.late += 1; // duplicate or late: already played past it
                    continue;
                }
                lost = (d - 1) as usize;
            }
            self.stats.last_seq = Some(seq);
            self.stats.packets += 1;
            self.stats.lost += lost as u64;
            let mut a = self.audio.lock().unwrap();
            // keep the timing for lost packets (up to 200 ms of silence)
            for _ in 0..(lost * len).min(1600) {
                a.from_line.push_back(0.0);
            }
            for &u in &buf[hdr..end] {
                a.from_line.push_back(if pt == 8 { alaw_decode(u) } else { ulaw_decode(u) });
            }
            while a.from_line.len() > 8000 {
                a.from_line.pop_front();
            }
        }
        let now = Instant::now();
        if now < self.rtp_next {
            return;
        }
        self.rtp_next =
            if now > self.rtp_next + Duration::from_millis(100) { now + Duration::from_millis(20) } else { self.rtp_next + Duration::from_millis(20) };
        let Some(c) = &self.call else {
            return;
        };
        if c.state != CallState::Talking {
            return;
        }
        let Some(dest) = c.remote_rtp else { return };
        let pt_event = c.pt_event;
        let mut pkt = Vec::with_capacity(172);
        // DTMF (RFC 4733): send one event as a short burst
        if let Some(ch) = self.dtmf_queue.pop_front() {
            let ev = match ch {
                '0'..='9' => ch as u8 - b'0',
                '*' => 10,
                '#' => 11,
                _ => 0,
            };
            for i in 0..6u16 {
                pkt.clear();
                let end = i >= 3;
                pkt.extend_from_slice(&[0x80, if i == 0 { 0x80 | pt_event } else { pt_event }]);
                pkt.extend_from_slice(&self.rtp_seq.to_be_bytes());
                pkt.extend_from_slice(&self.rtp_ts.to_be_bytes());
                pkt.extend_from_slice(&self.rtp_ssrc.to_be_bytes());
                let dur: u16 = 160 * (i.min(3) + 1);
                pkt.extend_from_slice(&[ev, if end { 0x80 | 10 } else { 10 }]);
                pkt.extend_from_slice(&dur.to_be_bytes());
                self.rtp_seq = self.rtp_seq.wrapping_add(1);
                let _ = rtp.send_to(&pkt, dest);
            }
            self.rtp_ts = self.rtp_ts.wrapping_add(160 * 4);
            return;
        }
        pkt.clear();
        let tx_pt = self.call.as_ref().map(|c| c.tx_pt).unwrap_or(0);
        pkt.extend_from_slice(&[0x80, tx_pt]);
        pkt.extend_from_slice(&self.rtp_seq.to_be_bytes());
        pkt.extend_from_slice(&self.rtp_ts.to_be_bytes());
        pkt.extend_from_slice(&self.rtp_ssrc.to_be_bytes());
        {
            let mut a = self.audio.lock().unwrap();
            while a.to_line.len() > 1600 {
                a.to_line.pop_front();
            }
            for _ in 0..160 {
                let s = a.to_line.pop_front().unwrap_or(0.0);
                pkt.push(if tx_pt == 8 { alaw_encode(s) } else { ulaw_encode(s) });
            }
        }
        self.rtp_seq = self.rtp_seq.wrapping_add(1);
        self.rtp_ts = self.rtp_ts.wrapping_add(160);
        let _ = rtp.send_to(&pkt, dest);
    }
}

/// (remote media address, telephone-event payload type)
fn parse_sdp(body: &str) -> Option<(SocketAddr, u8)> {
    let mut ip = None;
    let mut port = None;
    let mut ev = 101u8;
    for l in body.lines() {
        let l = l.trim();
        if let Some(r) = l.strip_prefix("c=IN IP4 ") {
            ip = Some(r.trim().to_string());
        } else if let Some(r) = l.strip_prefix("m=audio ") {
            port = r.split_whitespace().next().and_then(|p| p.parse::<u16>().ok());
        } else if let Some(r) = l.strip_prefix("a=rtpmap:") {
            if r.contains("telephone-event") {
                ev = r.split_whitespace().next().and_then(|p| p.parse().ok()).unwrap_or(101);
            }
        }
    }
    let a = format!("{}:{}", ip?, port?);
    a.parse().ok().map(|a| (a, ev))
}

#[derive(Default, Debug)]
struct RtpStats {
    packets: u64,
    lost: u64,
    late: u64,
    ext: u64,
    pts: std::collections::BTreeMap<u8, u64>,
    last_seq: Option<u16>,
}

/// G.711 A-law
pub fn alaw_decode(a: u8) -> f32 {
    let a = a ^ 0x55;
    let sign = a & 0x80;
    let exp = (a >> 4) & 7;
    let mant = (a & 0x0F) as i32;
    let mag = if exp == 0 { (mant << 4) + 8 } else { ((mant << 4) + 0x108) << (exp - 1) };
    let v = if sign != 0 { mag } else { -mag };
    v as f32 / 32768.0
}

pub fn alaw_encode(x: f32) -> u8 {
    let s = (x.clamp(-1.0, 1.0) * 32767.0) as i32;
    let sign = if s >= 0 { 0x80 } else { 0 };
    let mut m = s.abs().min(32767);
    let enc = if m < 256 {
        (m >> 4) as u8
    } else {
        let mut exp = 1;
        m >>= 4;
        while m > 0x1F {
            m >>= 1;
            exp += 1;
        }
        ((exp << 4) | (m & 0x0F)) as u8
    };
    (sign | enc) ^ 0x55
}

/// The codec the far end offered first among the ones we handle.
fn offered_pt(body: &str) -> u8 {
    for l in body.lines() {
        if let Some(r) = l.trim().strip_prefix("m=audio ") {
            for f in r.split_whitespace().skip(2) {
                match f {
                    "0" => return 0,
                    "8" => return 8,
                    _ => {}
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod g711_tests {
    use super::*;
    #[test]
    fn alaw_roundtrip() {
        for i in -100..100 {
            let x = i as f32 / 120.0;
            let y = alaw_decode(alaw_encode(x));
            assert!((x - y).abs() < 0.03, "{} -> {}", x, y);
        }
    }
}
