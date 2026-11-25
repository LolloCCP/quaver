//! Announces Soloist on the local network, so that the Spotify app can pair with it.
//!
//! Soloist does this too, but with the addresses of every network adapter, in any order. On a PC
//! with unplugged or virtual ones most of them lead nowhere, and the Spotify app gives up before
//! it gets to the one that works. This announces only that one.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    time::Duration,
};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket as AsyncSocket, task::JoinHandle};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const SERVICE: &str = "_spotify-connect._tcp.local";
const INSTANCE: &str = "quaver._spotify-connect._tcp.local";
const HOST: &str = "quaver-soloist.local";
/// How long others may remember this, in seconds. It is said again well before.
const LIFETIME: u32 = 20;
const REPEAT: Duration = Duration::from_secs(5);

pub struct Announcer {
    pub address: Ipv4Addr,
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The address of the adapter that leads to the internet. Nothing is sent.
pub fn local_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 53)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

impl Announcer {
    pub fn start(address: Ipv4Addr, port: u16) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Windows and other programs listen on this port as well.
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
        // Without naming the adapter, Windows may pick one that isn't connected to anything.
        socket.join_multicast_v4(&GROUP, &address)?;
        socket.set_multicast_if_v4(&address)?;
        socket.set_multicast_ttl_v4(255)?;
        let socket = AsyncSocket::from_std(socket.into())?;
        let task = tokio::spawn(async move {
            let message = announcement(address, port);
            let group = SocketAddrV4::new(GROUP, PORT);
            let mut repeat = tokio::time::interval(REPEAT);
            let mut buffer = [0; 1500];
            loop {
                let due = tokio::select! {
                    _ = repeat.tick() => true,
                    received = socket.recv_from(&mut buffer) => match received {
                        Ok((n, from)) => {
                            let asked = asks_for_us(&buffer[..n]);
                            // Who asks from another port doesn't hear the group, and knows the answer by its id.
                            if asked && from.port() != PORT {
                                let mut direct = message.clone();
                                direct[..2].copy_from_slice(&buffer[..2]);
                                let _ = socket.send_to(&direct, from).await;
                            }
                            asked
                        }
                        // Windows reports here when an earlier message found nobody.
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            false
                        }
                    },
                };
                if due && let Err(e) = socket.send_to(&message, group).await {
                    log::info!("announce: {e}");
                }
            }
        });
        Ok(Self { address, port, task })
    }
}

fn name(dotted: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in dotted.split('.') {
        out.push(label.len() as u8);
        out.extend(label.as_bytes());
    }
    out.push(0);
    out
}

/// A question that names the service. Anything else on this port is other people's.
fn asks_for_us(message: &[u8]) -> bool {
    let Some(header) = message.get(..12) else { return false };
    let question = header[2] & 0x80 == 0 && u16::from_be_bytes([header[4], header[5]]) > 0;
    let wanted = name(SERVICE);
    question && message.len() >= wanted.len() && message.windows(wanted.len()).any(|w| w.eq_ignore_ascii_case(&wanted))
}

fn announcement(address: Ipv4Addr, port: u16) -> Vec<u8> {
    const PTR: u16 = 12;
    const TXT: u16 = 16;
    const SRV: u16 = 33;
    const A: u16 = 1;
    /// Internet class; the top bit says that older records of this name are void.
    const SHARED: u16 = 1;
    const UNIQUE: u16 = 0x8001;
    let text: Vec<u8> = ["CPath=/zc", "VERSION=1.0", "Stack=SP"].iter().flat_map(|t| [&[t.len() as u8], t.as_bytes()].concat()).collect();
    let service = [&[0, 0, 0, 0][..], &port.to_be_bytes(), &name(HOST)].concat();
    let records = [
        (SERVICE, PTR, SHARED, name(INSTANCE)),
        (INSTANCE, SRV, UNIQUE, service),
        (INSTANCE, TXT, UNIQUE, text),
        (HOST, A, UNIQUE, address.octets().to_vec()),
    ];
    // No id, an authoritative answer, no questions, and these answers.
    let mut out = vec![0, 0, 0x84, 0, 0, 0, 0, records.len() as u8, 0, 0, 0, 0];
    for (owner, kind, class, data) in records {
        out.extend(name(owner));
        out.extend(kind.to_be_bytes());
        out.extend(class.to_be_bytes());
        out.extend(LIFETIME.to_be_bytes());
        out.extend((data.len() as u16).to_be_bytes());
        out.extend(data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a record the way another program would: owner, kind, data.
    fn record(message: &[u8], at: &mut usize) -> (String, u16, Vec<u8>) {
        let mut labels = Vec::new();
        while message[*at] != 0 {
            let length = message[*at] as usize;
            labels.push(String::from_utf8(message[*at + 1..*at + 1 + length].to_vec()).unwrap());
            *at += 1 + length;
        }
        *at += 1;
        let number = |at: usize| u16::from_be_bytes([message[at], message[at + 1]]);
        let (kind, length) = (number(*at), number(*at + 8) as usize);
        let data = message[*at + 10..*at + 10 + length].to_vec();
        *at += 10 + length;
        (labels.join("."), kind, data)
    }

    #[test]
    fn the_announcement_says_where_and_only_there() {
        let message = announcement(Ipv4Addr::new(10, 0, 0, 7), 65028);
        let mut at = 12;
        let records: Vec<_> = (0..message[7]).map(|_| record(&message, &mut at)).collect();
        assert_eq!(at, message.len(), "nothing is left over");
        assert_eq!(records[0], (SERVICE.into(), 12, name(INSTANCE)));
        assert_eq!((records[1].1, &records[1].2[4..6], &records[1].2[6..]), (33, &65028u16.to_be_bytes()[..], &name(HOST)[..]));
        assert_eq!(records[2].2[..10], [&[9][..], b"CPath=/zc"].concat());
        assert_eq!(records[3], (HOST.into(), 1, vec![10, 0, 0, 7]));
        assert_eq!(records.iter().filter(|r| r.1 == 1).count(), 1);
    }

    #[test]
    fn only_questions_about_the_service_are_answered() {
        let question = |n: &str| [&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0][..], &name(n), &[0, 12, 0, 1]].concat();
        assert!(asks_for_us(&question(SERVICE)));
        assert!(asks_for_us(&question("_SPOTIFY-Connect._tcp.local")));
        assert!(!asks_for_us(&question("_http._tcp.local")));
        assert!(!asks_for_us(&announcement(Ipv4Addr::LOCALHOST, 1)), "an answer, even our own, is no question");
        for n in 0..14 {
            assert!(!asks_for_us(&vec![0; n]));
        }
    }

    /// Announces a Soloist that is already running: `QUAVER_PORT=<its port> cargo test announce_now -- --ignored`
    #[tokio::test]
    #[ignore = "announces on the real network for a minute"]
    async fn announce_now() {
        let port = std::env::var("QUAVER_PORT").unwrap().parse().unwrap();
        let address = local_address().unwrap();
        println!("announcing {address}:{port}");
        let _announcer = Announcer::start(address, port).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
