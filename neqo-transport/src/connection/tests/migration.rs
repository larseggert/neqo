// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    cell::RefCell,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    rc::Rc,
    time::{Duration, Instant},
};

use neqo_common::{qdebug, Datagram, Decoder};
use test_fixture::{
    assertions::{assert_v4_path, assert_v6_path},
    fixture_init, new_neqo_qlog, now, DEFAULT_ADDR, DEFAULT_ADDR_V4,
};

use super::{
    super::{Connection, Output, State, StreamType},
    connect_fail, connect_force_idle, connect_rtt_idle, default_client, default_server,
    maybe_authenticate, new_client, new_server, send_something, zero_len_cid_client,
    CountingConnectionIdGenerator,
};
use crate::{
    cid::LOCAL_ACTIVE_CID_LIMIT,
    connection::tests::{
        assert_path_challenge_min_len, connect, send_something_paced, send_with_extra,
    },
    frame::FrameType,
    packet,
    path::MAX_PATH_PROBES,
    pmtud::Pmtud,
    stats::FrameStats,
    tparams::{PreferredAddress, TransportParameter, TransportParameterId},
    CloseReason, ConnectionId, ConnectionIdDecoder as _, ConnectionIdGenerator, ConnectionIdRef,
    ConnectionParameters, EmptyConnectionIdGenerator, Error, MIN_INITIAL_PACKET_SIZE,
};

/// This should be a valid-seeming transport parameter.
/// And it should have different values to `addr` and `addr_v4`.
const SAMPLE_PREFERRED_ADDRESS: &[u8] = &[
    0xc0, 0x00, 0x02, 0x02, 0x01, 0xbb, 0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x01, 0xbb, 0x05, 0x01, 0x02, 0x03, 0x04, 0x05, 0x03, 0x03,
    0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
];

// These tests generally use two paths:
// The connection is established on a path with the same IPv6 address on both ends.
// Migrations move to a path with the same IPv4 address on both ends.
// This simplifies validation as the same assertions can be used for client and server.
// The risk is that there is a place where source/destination local/remote is inverted.

const fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 443)
}

fn change_path(d: &Datagram, a: SocketAddr) -> Datagram {
    Datagram::new(a, a, d.tos(), &d[..])
}

const fn new_port(a: SocketAddr) -> SocketAddr {
    let (port, _) = a.port().overflowing_add(410);
    SocketAddr::new(a.ip(), port)
}

fn assert_path_challenge(
    c: &Connection,
    d: &Datagram,
    before: &FrameStats,
    dst: SocketAddr,
    padded: bool,
) {
    let after = c.stats().frame_tx;
    assert_eq!(after.path_challenge, before.path_challenge + 1);
    assert_eq!(d.source(), DEFAULT_ADDR);
    assert_eq!(d.destination(), dst);
    if padded {
        assert!(d.len() >= MIN_INITIAL_PACKET_SIZE);
    } else {
        assert!(d.len() < MIN_INITIAL_PACKET_SIZE);
    }
}

fn assert_path_response(c: &Connection, d: &Datagram, before: &FrameStats) {
    let after = c.stats().frame_tx;
    assert_eq!(after.path_response, before.path_response + 1);
    assert_eq!(d.source(), DEFAULT_ADDR);
    assert_eq!(d.destination(), DEFAULT_ADDR);
}

fn local_address(c: &Connection) -> SocketAddr {
    c.paths.primary().unwrap().borrow().local_address()
}

fn rebind(
    client: &mut Connection,
    server: &mut Connection,
    cur_path: fn(&Datagram) -> Datagram,
    new_path: fn(&Datagram) -> Datagram,
    mut now: Instant,
) -> Instant {
    qdebug!("Rebinding");
    let c1 = send_something(client, now);
    let c1_new = new_path(&c1);
    qdebug!("Rebinding to {}", c1_new.source());

    // Server will reply to modified datagram with a PATH_CHALLENGE.
    // Due to the amplification limit, this will not be padded to MIN_INITIAL_PACKET_SIZE.
    let before = server.stats().frame_tx;
    let s1 = server.process(Some(c1_new.clone()), now).dgram().unwrap();
    assert_path_challenge(server, &s1, &before, c1_new.source(), false);

    // Restore the original source address, so to the client it looks like the path has not changed.
    let s1_reb = Datagram::new(s1.source(), local_address(client), s1.tos(), &s1[..]);

    // The client should respond to the PATH_CHALLENGE, without changing paths.
    let before = client.stats().frame_tx;
    let c2 = client.process(Some(s1_reb), now).dgram().unwrap();
    assert_path_response(client, &c2, &before);

    // The server should now see the response on the new path.
    // It will send another PATH_CHALLENGE padded to MIN_INITIAL_PACKET_SIZE.
    let c2_new = new_path(&c2);
    let before = server.stats().frame_tx;
    let s2 = server.process(Some(c2_new.clone()), now).dgram().unwrap();
    assert_path_challenge(server, &s2, &before, c2_new.source(), true);

    // Restore the original source address, so to the client it looks like the path has not changed.
    let s2_reb = Datagram::new(s2.source(), local_address(client), s2.tos(), &s2[..]);

    // The client should respond to the PATH_CHALLENGE, without changing paths.
    let before = client.stats().frame_tx;
    let c3 = client.process(Some(s2_reb.clone()), now).dgram().unwrap();
    assert_path_response(client, &s2_reb, &before);

    // The server should now see the second response on the new path.
    // It will then try to probe the old path.
    let c3_new = new_path(&c3);
    let c3_cur = cur_path(&c3);
    let before = server.stats().frame_tx;
    let s3 = server.process(Some(c3_new.clone()), now).dgram().unwrap();
    assert_path_challenge(server, &s3, &before, c3_cur.source(), true);

    // Do not deliver this probe to the client.

    // Server will now ACK on the new path.
    let before = server.stats().frame_tx;
    let s4 = server.process_output(now).dgram().unwrap();
    let after = server.stats().frame_tx;
    assert_eq!(after.ack, before.ack + 1);
    assert_eq!(s4.source(), c3_new.destination());
    assert_eq!(s4.destination(), c3_new.source());

    // Restore the original source address, so to the client it looks like the path has not changed.
    let s4_reb = Datagram::new(s4.source(), local_address(client), s4.tos(), &s4[..]);

    // The client should process the ACK and go idle.
    let delay = client.process(Some(s4_reb), now).callback();
    assert_eq!(delay, ConnectionParameters::default().get_idle_timeout());

    let client_uses_zero_len_cid = client
        .paths
        .primary()
        .unwrap()
        .borrow()
        .local_cid()
        .unwrap()
        .is_empty();
    let mut total_delay = Duration::new(0, 0);
    loop {
        let before = server.stats().frame_tx;
        match server.process_output(now) {
            Output::Callback(t) => {
                total_delay += t;
                if total_delay == ConnectionParameters::default().get_idle_timeout() {
                    // Server should only hit the idle timeout here when the client uses a zero-len
                    // CID.
                    assert!(client_uses_zero_len_cid);
                    break;
                }
                now += t;
            }
            Output::Datagram(sx) => {
                total_delay = Duration::new(0, 0);
                if sx.destination() == c3_cur.source() {
                    // Old path gets path challenges.
                    assert_path_challenge(server, &sx, &before, c3_cur.source(), true);
                    // Don't deliver them.
                } else {
                    let after = server.stats().frame_tx;
                    // If the client uses a zero-len CID, the server will only PING.
                    // Otherwise, it will PING or send a RETIRE_CONNECTION_ID.
                    if client_uses_zero_len_cid {
                        assert_eq!(after.ping, before.ping + 1);
                    } else {
                        assert!(
                            after.retire_connection_id == before.retire_connection_id + 1
                                || after.ping == before.ping + 1
                        );
                    }
                    // Restore the original source address, so to the client it looks like
                    // the path has not changed.
                    let sx_r = Datagram::new(sx.source(), local_address(client), sx.tos(), &sx[..]);
                    let before = client.stats().frame_tx;
                    let cx = client.process(Some(sx_r), now).dgram().unwrap();
                    let after = client.stats().frame_tx;
                    assert_eq!(after.ack, before.ack + 1);
                    // Also deliver the ACK.
                    let cx_n = new_path(&cx);
                    server.process_input(cx_n, now);
                    if !client_uses_zero_len_cid
                        && after.new_connection_id == before.new_connection_id + 1
                    {
                        // Declare victory once the client has sent a new connection ID.
                        break;
                    }
                }
            }
            Output::None => panic!(),
        }
    }

    if !client_uses_zero_len_cid {
        // Eat up any delays before returning.
        now += client.process_output(now).callback();
        now += server.process_output(now).callback();
    }

    qdebug!("Rebinding done");
    now
}

fn inc_port(port: u16, i: usize) -> u16 {
    port.overflowing_add(i.overflowing_mul(11).0.try_into().unwrap())
        .0
}

fn inc_addr(ip: IpAddr, i: usize) -> IpAddr {
    let inc: u8 = i.overflowing_mul(11).0.try_into().unwrap();
    match ip {
        IpAddr::V4(ip) => IpAddr::V4(Ipv4Addr::from(
            ip.octets().map(|b| b.overflowing_add(inc).0),
        )),
        IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from(
            ip.octets().map(|b| b.overflowing_add(inc).0),
        )),
    }
}

fn change_source_port(d: &Datagram, i: usize) -> Datagram {
    Datagram::new(
        SocketAddr::new(d.source().ip(), inc_port(d.source().port(), i)),
        d.destination(),
        d.tos(),
        &d[..],
    )
}

fn change_source_address_and_port(d: &Datagram, i: usize) -> Datagram {
    Datagram::new(
        SocketAddr::new(inc_addr(d.source().ip(), i), inc_port(d.source().port(), i)),
        d.destination(),
        d.tos(),
        &d[..],
    )
}

fn rebind_port_with_client(client: &mut Connection) {
    let mut server = default_server();
    connect_force_idle(client, &mut server);
    let mut now = now();

    now = rebind(
        client,
        &mut server,
        |d| change_source_port(d, 0),
        |d| change_source_port(d, 1),
        now,
    );
    _ = rebind(
        client,
        &mut server,
        |d| change_source_port(d, 1),
        |d| change_source_port(d, 2),
        now,
    );
}

fn rebind_address_and_port_with_client(client: &mut Connection) {
    let mut server = default_server();
    connect_force_idle(client, &mut server);
    let mut now = now();

    now = rebind(
        client,
        &mut server,
        |d| change_source_address_and_port(d, 0),
        |d| change_source_address_and_port(d, 1),
        now,
    );
    _ = rebind(
        client,
        &mut server,
        |d| change_source_address_and_port(d, 1),
        |d| change_source_address_and_port(d, 2),
        now,
    );
}

#[test]
fn rebind_port() {
    let mut client = default_client();
    rebind_port_with_client(&mut client);
}

#[test]
fn rebind_port_zero_len_cid() {
    let mut client = zero_len_cid_client(DEFAULT_ADDR, DEFAULT_ADDR);
    rebind_port_with_client(&mut client);
}

#[test]
fn rebind_address_and_port() {
    let mut client = default_client();
    rebind_address_and_port_with_client(&mut client);
}

#[test]
fn rebind_address_and_port_zero_len_cid() {
    let mut client = zero_len_cid_client(DEFAULT_ADDR, DEFAULT_ADDR);
    rebind_address_and_port_with_client(&mut client);
}

/// This simulates an attack where a valid packet is forwarded on
/// a different path.  This shows how both paths are probed and the
/// server eventually returns to the original path.
#[test]
fn path_forwarding_attack() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let now = now();

    let dgram = send_something(&mut client, now);
    let dgram = change_path(&dgram, DEFAULT_ADDR_V4);
    server.process_input(dgram, now);

    // The server now probes the new (primary) path.
    let new_probe = server.process_output(now).dgram().unwrap();
    assert_eq!(server.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&server, &new_probe, now);
    assert_v4_path(&new_probe, false); // Can't be padded.

    // The server also probes the old path.
    let old_probe = server.process_output(now).dgram().unwrap();
    assert_path_challenge_min_len(&server, &old_probe, now);
    assert_eq!(server.stats().frame_tx.path_challenge, 2);
    assert_v6_path(&old_probe, true);

    // New data from the server is sent on the new path, but that is
    // now constrained by the amplification limit.
    let stream_id = server.stream_create(StreamType::UniDi).unwrap();
    server.stream_close_send(stream_id).unwrap();
    assert!(server.process_output(now).dgram().is_none());

    // The client should respond to the challenge on the new path.
    // The server couldn't pad, so the client is also amplification limited.
    let new_resp = client.process(Some(new_probe), now).dgram().unwrap();
    assert_eq!(client.stats().frame_rx.path_challenge, 1);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_eq!(client.stats().frame_tx.path_response, 1);
    assert_v4_path(&new_resp, false);

    // The client also responds to probes on the old path.
    let old_resp = client.process(Some(old_probe), now).dgram().unwrap();
    assert_eq!(client.stats().frame_rx.path_challenge, 2);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_eq!(client.stats().frame_tx.path_response, 2);
    assert_v6_path(&old_resp, true);

    // But the client still sends data on the old path.
    let client_data1 = send_something(&mut client, now);
    assert_v6_path(&client_data1, false); // Just data.

    // Receiving the PATH_RESPONSE from the client opens the amplification
    // limit enough for the server to respond.
    // This is padded because it includes PATH_CHALLENGE.
    let server_data1 = server.process(Some(new_resp), now).dgram().unwrap();
    assert_v4_path(&server_data1, true);
    assert_eq!(server.stats().frame_tx.path_challenge, 3);
    assert_path_challenge_min_len(&server, &server_data1, now);

    // The client responds to this probe on the new path.
    client.process_input(server_data1, now);
    let stream_before = client.stats().frame_tx.stream;
    let padded_resp = send_something(&mut client, now);
    assert_eq!(stream_before, client.stats().frame_tx.stream);
    assert_v4_path(&padded_resp, true); // This is padded!

    // But new data from the client stays on the old path.
    let client_data2 = client.process_output(now).dgram().unwrap();
    assert_v6_path(&client_data2, false);

    // The server keeps sending on the new path.
    let server_data2 = send_something(&mut server, now);
    assert_v4_path(&server_data2, false);

    // Until new data is received from the client on the old path.
    server.process_input(client_data2, now);
    // The server sends a probe on the new path.
    let server_data3 = send_something(&mut server, now);
    assert_v4_path(&server_data3, true);
    // But switches data transmission to the old path.
    let server_data4 = server.process_output(now).dgram().unwrap();
    assert_v6_path(&server_data4, false);
}

#[test]
fn migrate_immediate() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let now = now();

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), true, now)
        .unwrap();

    let client1 = send_something(&mut client, now);
    assert_v4_path(&client1, true); // Contains PATH_CHALLENGE.
    assert_path_challenge_min_len(&client, &client1, now);

    let client2 = send_something(&mut client, now);
    assert_v4_path(&client2, false); // Doesn't.

    let server_delayed = send_something(&mut server, now);

    // The server accepts the first packet and migrates (but probes).
    let server1 = server.process(Some(client1), now).dgram().unwrap();
    assert_v4_path(&server1, true);
    let server2 = server.process_output(now).dgram().unwrap();
    assert_v6_path(&server2, true);

    // The second packet has no real effect, it just elicits an ACK.
    let all_before = server.stats().frame_tx.all();
    let ack_before = server.stats().frame_tx.ack;
    let server3 = server.process(Some(client2), now).dgram();
    assert!(server3.is_some());
    assert_eq!(server.stats().frame_tx.all(), all_before + 1);
    assert_eq!(server.stats().frame_tx.ack, ack_before + 1);

    // Receiving a packet sent by the server before migration doesn't change path.
    client.process_input(server_delayed, now);
    // The client has sent two unpaced packets and this new path has no RTT estimate
    // so this might be paced.
    let (client3, _t) = send_something_paced(&mut client, now, true);
    assert_v4_path(&client3, false);
}

/// RTT estimates for paths should be preserved across migrations.
#[test]
fn migrate_rtt() {
    const RTT: Duration = Duration::from_millis(20);
    let mut client = default_client();
    let mut server = default_server();
    let now = connect_rtt_idle(&mut client, &mut server, RTT);

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), true, now)
        .unwrap();
    // The RTT might be increased for the new path, so allow a little flexibility.
    let rtt = client.paths.rtt();
    assert!(rtt > RTT);
    assert!(rtt < RTT * 2);
}

#[test]
fn migrate_immediate_fail() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let mut now = now();

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), true, now)
        .unwrap();

    let probe = client.process_output(now).dgram().unwrap();
    assert_v4_path(&probe, true); // Contains PATH_CHALLENGE.
    assert_path_challenge_min_len(&client, &probe, now);

    // -1 because first PATH_CHALLENGE already sent above
    for _ in 0..MAX_PATH_PROBES * 2 - 1 {
        let cb = client.process_output(now).callback();
        assert_ne!(cb, Duration::new(0, 0));
        now += cb;

        let before = client.stats().frame_tx;
        let probe = client.process_output(now).dgram().unwrap();
        assert_v4_path(&probe, true); // Contains PATH_CHALLENGE.
        assert_path_challenge_min_len(&client, &probe, now);
        let after = client.stats().frame_tx;
        assert_eq!(after.path_challenge, before.path_challenge + 1);
        assert_eq!(after.padding, before.padding + 1);
        assert_eq!(after.all(), before.all() + 2);

        // This might be a PTO, which will result in sending a probe.
        if let Some(probe) = client.process_output(now).dgram() {
            assert_v4_path(&probe, false); // Contains PING.
            let after = client.stats().frame_tx;
            assert_eq!(after.path_challenge, before.path_challenge + 1);
            assert_eq!(after.ping, before.ping + 1);
            assert_eq!(after.all(), before.all() + 3);
        }
    }

    let pto = client.process_output(now).callback();
    assert_ne!(pto, Duration::new(0, 0));
    now += pto;

    // The client should fall back to the original path and retire the connection ID.
    let fallback = client.process_output(now).dgram();
    assert_v6_path(&fallback.unwrap(), false);
    assert_eq!(client.stats().frame_tx.retire_connection_id, 1);
}

/// Migrating to the same path shouldn't do anything special,
/// except that the path is probed.
#[test]
fn migrate_same() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let now = now();

    client
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), true, now)
        .unwrap();

    let probe = client.process_output(now).dgram().unwrap();
    assert_v6_path(&probe, true); // Contains PATH_CHALLENGE.
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&client, &probe, now);

    let resp = server.process(Some(probe), now).dgram().unwrap();
    assert_v6_path(&resp, true);
    assert_eq!(server.stats().frame_tx.path_response, 1);
    assert_eq!(server.stats().frame_tx.path_challenge, 0);

    // Everything continues happily.
    client.process_input(resp, now);
    let contd = send_something(&mut client, now);
    assert_v6_path(&contd, false);
}

/// Migrating to the same path, if it fails, causes the connection to fail.
#[test]
fn migrate_same_fail() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let mut now = now();

    client
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), true, now)
        .unwrap();

    let probe = client.process_output(now).dgram().unwrap();
    assert_v6_path(&probe, true); // Contains PATH_CHALLENGE.
    assert_path_challenge_min_len(&client, &probe, now);

    // -1 because first PATH_CHALLENGE already sent above
    for _ in 0..MAX_PATH_PROBES * 2 - 1 {
        let cb = client.process_output(now).callback();
        assert_ne!(cb, Duration::new(0, 0));
        now += cb;

        let before = client.stats().frame_tx;
        let probe = client.process_output(now).dgram().unwrap();
        assert_v6_path(&probe, true); // Contains PATH_CHALLENGE.
        assert_path_challenge_min_len(&client, &probe, now);
        let after = client.stats().frame_tx;
        assert_eq!(after.path_challenge, before.path_challenge + 1);
        assert_eq!(after.padding, before.padding + 1);
        assert_eq!(after.all(), before.all() + 2);

        // This might be a PTO, which will result in sending a probe.
        if let Some(probe) = client.process_output(now).dgram() {
            assert_v6_path(&probe, false); // Contains PING.
            let after = client.stats().frame_tx;
            assert_eq!(after.path_challenge, before.path_challenge + 1);
            assert_eq!(after.ping, before.ping + 1);
            assert_eq!(after.all(), before.all() + 3);
        }
    }

    let pto = client.process_output(now).callback();
    assert_ne!(pto, Duration::new(0, 0));
    now += pto;

    // The client should mark this path as failed and close immediately.
    let res = client.process_output(now);
    assert!(matches!(res, Output::None));
    assert!(matches!(
        client.state(),
        State::Closed(CloseReason::Transport(Error::NoAvailablePath))
    ));
}

/// This gets the connection ID from a datagram using the default
/// connection ID generator/decoder.
pub fn get_cid(d: &Datagram) -> ConnectionIdRef<'_> {
    let gen = CountingConnectionIdGenerator::default();
    assert_eq!(d[0] & 0x80, 0); // Only support short packets for now.
    gen.decode_cid(&mut Decoder::from(&d[1..])).unwrap()
}

fn migration(mut client: Connection) {
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);
    let now = now();

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), false, now)
        .unwrap();

    let probe = client.process_output(now).dgram().unwrap();
    assert_v4_path(&probe, true); // Contains PATH_CHALLENGE.
    assert_path_challenge_min_len(&client, &probe, now);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    let probe_cid = ConnectionId::from(get_cid(&probe));

    let resp = server.process(Some(probe), now).dgram().unwrap();
    assert_v4_path(&resp, true);
    assert_path_challenge_min_len(&server, &resp, now);
    assert_eq!(server.stats().frame_tx.path_response, 1);
    assert_eq!(server.stats().frame_tx.path_challenge, 1);

    // Data continues to be exchanged on the new path.
    let client_data = send_something(&mut client, now);
    assert_ne!(get_cid(&client_data), probe_cid);
    assert_v6_path(&client_data, false);
    server.process_input(client_data, now);
    let server_data = send_something(&mut server, now);
    assert_v6_path(&server_data, false);

    // Once the client receives the probe response, it migrates to the new path.
    client.process_input(resp, now);
    assert_eq!(client.stats().frame_rx.path_challenge, 1);
    let migrate_client = send_something(&mut client, now);
    assert_v4_path(&migrate_client, true); // Responds to server probe.

    // The server now sees the migration and will switch over.
    // However, it will probe the old path again, even though it has just
    // received a response to its last probe, because it needs to verify
    // that the migration is genuine.
    server.process_input(migrate_client, now);
    let stream_before = server.stats().frame_tx.stream;
    let probe_old_server = send_something(&mut server, now);
    // This is just the double-check probe; no STREAM frames.
    assert_v6_path(&probe_old_server, true);
    assert_path_challenge_min_len(&server, &probe_old_server, now);
    assert_eq!(server.stats().frame_tx.path_challenge, 2);
    assert_eq!(server.stats().frame_tx.stream, stream_before);

    // The server then sends data on the new path.
    let migrate_server = server.process_output(now).dgram().unwrap();
    assert_v4_path(&migrate_server, false);
    assert_eq!(server.stats().frame_tx.path_challenge, 2);
    assert_eq!(server.stats().frame_tx.stream, stream_before + 1);

    // The client receives these checks and responds to the probe, but uses the new path.
    client.process_input(migrate_server, now);
    client.process_input(probe_old_server, now);
    let old_probe_resp = send_something(&mut client, now);
    assert_v6_path(&old_probe_resp, true);
    let client_confirmation = client.process_output(now).dgram().unwrap();
    assert_v4_path(&client_confirmation, false);

    // The server has now sent 2 packets, so it is blocked on the pacer.  Wait.
    let server_pacing = server.process_output(now).callback();
    assert_ne!(server_pacing, Duration::new(0, 0));
    // ... then confirm that the server sends on the new path still.
    let server_confirmation = send_something(&mut server, now + server_pacing);
    assert_v4_path(&server_confirmation, false);
}

#[test]
fn migration_graceful() {
    migration(default_client());
}

/// A client should be able to migrate when it has a zero-length connection ID.
#[test]
fn migration_client_empty_cid() {
    fixture_init();
    let client = zero_len_cid_client(DEFAULT_ADDR, DEFAULT_ADDR);
    migration(client);
}

/// Drive the handshake in the most expeditious fashion.
/// Returns the packet containing `HANDSHAKE_DONE` from the server.
fn fast_handshake(client: &mut Connection, server: &mut Connection) -> Option<Datagram> {
    let dgram = client.process_output(now()).dgram();
    let dgram2 = client.process_output(now()).dgram();
    server.process_input(dgram.unwrap(), now());
    let dgram = server.process(dgram2, now()).dgram();
    let dgram = client.process(dgram, now()).dgram();
    let dgram = server.process(dgram, now()).dgram();
    client.process_input(dgram.unwrap(), now());
    assert!(maybe_authenticate(client));
    let dgram = client.process_output(now()).dgram();
    server.process(dgram, now()).dgram()
}

fn preferred_address(hs_client: SocketAddr, hs_server: SocketAddr, preferred: SocketAddr) {
    let mtu = Pmtud::default_plpmtu(hs_client.ip());
    let assert_orig_path = |d: &Datagram, full_mtu: bool| {
        assert_eq!(
            d.destination(),
            if d.source() == hs_client {
                hs_server
            } else if d.source() == hs_server {
                hs_client
            } else {
                panic!();
            }
        );
        if full_mtu {
            assert_eq!(d.len(), mtu);
        }
    };
    let assert_toward_spa = |d: &Datagram, full_mtu: bool| {
        assert_eq!(d.destination(), preferred);
        assert_eq!(d.source(), hs_client);
        if full_mtu {
            assert_eq!(d.len(), mtu);
        }
    };
    let assert_from_spa = |d: &Datagram, full_mtu: bool| {
        assert_eq!(d.destination(), hs_client);
        assert_eq!(d.source(), preferred);
        if full_mtu {
            assert_eq!(d.len(), mtu);
        }
    };

    fixture_init();
    let (log, _contents) = new_neqo_qlog();
    let mut client = zero_len_cid_client(hs_client, hs_server);
    client.set_qlog(log);
    let spa = match preferred {
        SocketAddr::V6(v6) => PreferredAddress::new(None, Some(v6)),
        SocketAddr::V4(v4) => PreferredAddress::new(Some(v4), None),
    };
    let mut server = new_server(ConnectionParameters::default().preferred_address(spa));

    let dgram = fast_handshake(&mut client, &mut server);

    // The client is about to process HANDSHAKE_DONE.
    // It should start probing toward the server's preferred address.
    let probe = client.process(dgram, now()).dgram().unwrap();
    assert_toward_spa(&probe, true);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&client, &probe, now());
    assert_ne!(client.process_output(now()).callback(), Duration::new(0, 0));

    // Data continues on the main path for the client.
    let data = send_something(&mut client, now());
    assert_orig_path(&data, false);

    // The server responds to the probe.
    let resp = server.process(Some(probe), now()).dgram().unwrap();
    assert_from_spa(&resp, true);
    assert_eq!(server.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&server, &resp, now());
    assert_eq!(server.stats().frame_tx.path_response, 1);

    // Data continues on the main path for the server.
    server.process_input(data, now());
    let data = send_something(&mut server, now());
    assert_orig_path(&data, false);

    // Client gets the probe response back and it migrates.
    client.process_input(resp, now());
    client.process_input(data, now());
    let data = send_something(&mut client, now());
    assert_toward_spa(&data, true);
    assert_eq!(client.stats().frame_tx.stream, 2);
    assert_eq!(client.stats().frame_tx.path_response, 1);

    // The server sees the migration and probes the old path.
    let probe = server.process(Some(data), now()).dgram().unwrap();
    assert_orig_path(&probe, true);
    assert_eq!(server.stats().frame_tx.path_challenge, 2);
    assert_path_challenge_min_len(&server, &probe, now());

    // But data now goes on the new path.
    let data = send_something(&mut server, now());
    assert_from_spa(&data, false);
}

/// Migration works for a new port number.
#[test]
fn preferred_address_new_port() {
    let a = DEFAULT_ADDR;
    preferred_address(a, a, new_port(a));
}

/// Migration works for a new address too.
#[test]
fn preferred_address_new_address() {
    let mut preferred = DEFAULT_ADDR;
    preferred.set_ip(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2)));
    preferred_address(DEFAULT_ADDR, DEFAULT_ADDR, preferred);
}

/// Migration works for IPv4 addresses.
#[test]
fn preferred_address_new_port_v4() {
    let a = DEFAULT_ADDR_V4;
    preferred_address(a, a, new_port(a));
}

/// Migrating to a loopback address is OK if we started there.
#[test]
fn preferred_address_loopback() {
    let a = loopback();
    preferred_address(a, a, new_port(a));
}

fn expect_no_migration(client: &mut Connection, server: &mut Connection) {
    let dgram = fast_handshake(client, server);

    // The client won't probe now, though it could; it remains idle.
    let out = client.process(dgram, now());
    assert_ne!(out.callback(), Duration::new(0, 0));

    // Data continues on the main path for the client.
    let data = send_something(client, now());
    assert_v6_path(&data, false);
    assert_eq!(client.stats().frame_tx.path_challenge, 0);
}

fn preferred_address_ignored(spa: PreferredAddress) {
    let mut client = default_client();
    let mut server = new_server(ConnectionParameters::default().preferred_address(spa));

    expect_no_migration(&mut client, &mut server);
}

/// Using a loopback address in the preferred address is ignored.
#[test]
fn preferred_address_ignore_loopback() {
    preferred_address_ignored(PreferredAddress::new_any(None, Some(loopback())));
}

/// A preferred address in the wrong address family is ignored.
#[test]
fn preferred_address_ignore_different_family() {
    preferred_address_ignored(PreferredAddress::new_any(Some(DEFAULT_ADDR_V4), None));
}

/// Disabling preferred addresses at the client means that it ignores a perfectly
/// good preferred address.
#[test]
fn preferred_address_disabled_client() {
    let mut client = new_client(ConnectionParameters::default().disable_preferred_address());
    let mut preferred = DEFAULT_ADDR;
    preferred.set_ip(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2)));
    let spa = PreferredAddress::new_any(None, Some(preferred));
    let mut server = new_server(ConnectionParameters::default().preferred_address(spa));

    expect_no_migration(&mut client, &mut server);
}

#[test]
fn preferred_address_empty_cid() {
    fixture_init();

    let spa = PreferredAddress::new_any(None, Some(new_port(DEFAULT_ADDR)));
    let res = Connection::new_server(
        test_fixture::DEFAULT_KEYS,
        test_fixture::DEFAULT_ALPN,
        Rc::new(RefCell::new(EmptyConnectionIdGenerator::default())),
        ConnectionParameters::default().preferred_address(spa),
    );
    assert_eq!(res.unwrap_err(), Error::ConnectionIdsExhausted);
}

/// A server cannot include a preferred address if it chooses an empty connection ID.
#[test]
fn preferred_address_server_empty_cid() {
    let mut client = default_client();
    let mut server = Connection::new_server(
        test_fixture::DEFAULT_KEYS,
        test_fixture::DEFAULT_ALPN,
        Rc::new(RefCell::new(EmptyConnectionIdGenerator::default())),
        ConnectionParameters::default(),
    )
    .unwrap();

    server
        .set_local_tparam(
            TransportParameterId::PreferredAddress,
            TransportParameter::Bytes(SAMPLE_PREFERRED_ADDRESS.to_vec()),
        )
        .unwrap();

    connect_fail(
        &mut client,
        &mut server,
        Error::TransportParameter,
        Error::Peer(Error::TransportParameter.code()),
    );
}

/// A client shouldn't send a preferred address transport parameter.
#[test]
fn preferred_address_client() {
    let mut client = default_client();
    let mut server = default_server();

    client
        .set_local_tparam(
            TransportParameterId::PreferredAddress,
            TransportParameter::Bytes(SAMPLE_PREFERRED_ADDRESS.to_vec()),
        )
        .unwrap();

    connect_fail(
        &mut client,
        &mut server,
        Error::Peer(Error::TransportParameter.code()),
        Error::TransportParameter,
    );
}

/// Test that migration isn't permitted if the connection isn't in the right state.
#[test]
fn migration_invalid_state() {
    let mut client = default_client();
    assert!(client
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());

    let mut server = default_server();
    assert!(server
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());
    connect_force_idle(&mut client, &mut server);

    assert!(server
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());

    client.close(now(), 0, "closing");
    assert!(client
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());
    let close = client.process_output(now()).dgram();

    let dgram = server.process(close, now()).dgram();
    assert!(server
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());

    client.process_input(dgram.unwrap(), now());
    assert!(client
        .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), false, now())
        .is_err());
}

#[test]
fn migration_disabled() {
    let mut client = default_client();
    let mut server = new_server(ConnectionParameters::default().disable_migration(true));
    connect(&mut client, &mut server);
    assert_eq!(
        client
            .migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR), true, now())
            .unwrap_err(),
        Error::InvalidMigration
    );
}

#[test]
fn migration_invalid_address() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);

    let mut cant_migrate = |local, remote| {
        assert_eq!(
            client.migrate(local, remote, true, now()).unwrap_err(),
            Error::InvalidMigration
        );
    };

    // Providing neither address is pointless and therefore an error.
    cant_migrate(None, None);

    // Providing a zero port number isn't valid.
    let mut zero_port = DEFAULT_ADDR;
    zero_port.set_port(0);
    cant_migrate(None, Some(zero_port));
    cant_migrate(Some(zero_port), None);

    // An unspecified remote address is bad.
    let mut remote_unspecified = DEFAULT_ADDR;
    remote_unspecified.set_ip(IpAddr::V6(Ipv6Addr::from(0)));
    cant_migrate(None, Some(remote_unspecified));

    // Mixed address families is bad.
    cant_migrate(Some(DEFAULT_ADDR), Some(DEFAULT_ADDR_V4));
    cant_migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR));

    // Loopback to non-loopback is bad.
    cant_migrate(Some(DEFAULT_ADDR), Some(loopback()));
    cant_migrate(Some(loopback()), Some(DEFAULT_ADDR));
    assert_eq!(
        client
            .migrate(Some(DEFAULT_ADDR), Some(loopback()), true, now())
            .unwrap_err(),
        Error::InvalidMigration
    );
    assert_eq!(
        client
            .migrate(Some(loopback()), Some(DEFAULT_ADDR), true, now())
            .unwrap_err(),
        Error::InvalidMigration
    );
}

/// This inserts a frame into packets that provides a single new
/// connection ID and retires all others.
struct RetireAll {
    cid_gen: Rc<RefCell<dyn ConnectionIdGenerator>>,
}

impl crate::connection::test_internal::FrameWriter for RetireAll {
    fn write_frames(&mut self, builder: &mut packet::Builder<&mut Vec<u8>>) {
        // Use a sequence number that is large enough that all existing values
        // will be lower (so they get retired).  As the code doesn't care about
        // gaps in sequence numbers, this is safe, even though the gap might
        // hint that there are more outstanding connection IDs that are allowed.
        const SEQNO: u64 = 100;
        let cid = self.cid_gen.borrow_mut().generate_cid().unwrap();
        builder
            .encode_varint(FrameType::NewConnectionId)
            .encode_varint(SEQNO)
            .encode_varint(SEQNO) // Retire Prior To
            .encode_vec(1, &cid)
            .encode(&[0x7f; 16]);
    }
}

/// Test that forcing retirement of connection IDs forces retirement of all active
/// connection IDs and the use of of newer one.
#[test]
fn retire_all() {
    let mut client = default_client();
    let cid_gen: Rc<RefCell<dyn ConnectionIdGenerator>> =
        Rc::new(RefCell::new(CountingConnectionIdGenerator::default()));
    let mut server = Connection::new_server(
        test_fixture::DEFAULT_KEYS,
        test_fixture::DEFAULT_ALPN,
        Rc::clone(&cid_gen),
        ConnectionParameters::default(),
    )
    .unwrap();
    connect_force_idle(&mut client, &mut server);

    let original_cid = ConnectionId::from(get_cid(&send_something(&mut client, now())));

    let ncid = send_with_extra(&mut server, RetireAll { cid_gen }, now());

    let new_cid_before = client.stats().frame_rx.new_connection_id;
    let retire_cid_before = client.stats().frame_tx.retire_connection_id;
    client.process_input(ncid, now());
    let retire = send_something(&mut client, now());
    assert_eq!(
        client.stats().frame_rx.new_connection_id,
        new_cid_before + 1
    );
    assert_eq!(
        client.stats().frame_tx.retire_connection_id,
        retire_cid_before + LOCAL_ACTIVE_CID_LIMIT
    );

    assert_ne!(get_cid(&retire), original_cid);
}

/// During a graceful migration, if the probed path can't get a new connection ID due
/// to being forced to retire the one it is using, the migration will fail.
#[test]
fn retire_prior_to_migration_failure() {
    let mut client = default_client();
    let cid_gen: Rc<RefCell<dyn ConnectionIdGenerator>> =
        Rc::new(RefCell::new(CountingConnectionIdGenerator::default()));
    let mut server = Connection::new_server(
        test_fixture::DEFAULT_KEYS,
        test_fixture::DEFAULT_ALPN,
        Rc::clone(&cid_gen),
        ConnectionParameters::default(),
    )
    .unwrap();
    connect_force_idle(&mut client, &mut server);

    let original_cid = ConnectionId::from(get_cid(&send_something(&mut client, now())));

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), false, now())
        .unwrap();

    // The client now probes the new path.
    let probe = client.process_output(now()).dgram().unwrap();
    assert_v4_path(&probe, true);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&client, &probe, now());
    let probe_cid = ConnectionId::from(get_cid(&probe));
    assert_ne!(original_cid, probe_cid);

    // Have the server receive the probe, but separately have it decide to
    // retire all of the available connection IDs.
    let retire_all = send_with_extra(&mut server, RetireAll { cid_gen }, now());

    let resp = server.process(Some(probe), now()).dgram().unwrap();
    assert_v4_path(&resp, true);
    assert_eq!(server.stats().frame_tx.path_response, 1);
    assert_eq!(server.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&server, &resp, now());

    // Have the client receive the NEW_CONNECTION_ID with Retire Prior To.
    client.process_input(retire_all, now());
    // This packet contains the probe response, which should be fine, but it
    // also includes PATH_CHALLENGE for the new path, and the client can't
    // respond without a connection ID.  We treat this as a connection error.
    client.process_input(resp, now());
    assert!(matches!(
        client.state(),
        State::Closing {
            error: CloseReason::Transport(Error::InvalidMigration),
            ..
        }
    ));
}

/// The timing of when frames arrive can mean that the migration path can
/// get the last available connection ID.
#[test]
fn retire_prior_to_migration_success() {
    let mut client = default_client();
    let cid_gen: Rc<RefCell<dyn ConnectionIdGenerator>> =
        Rc::new(RefCell::new(CountingConnectionIdGenerator::default()));
    let mut server = Connection::new_server(
        test_fixture::DEFAULT_KEYS,
        test_fixture::DEFAULT_ALPN,
        Rc::clone(&cid_gen),
        ConnectionParameters::default(),
    )
    .unwrap();
    connect_force_idle(&mut client, &mut server);

    let original_cid = ConnectionId::from(get_cid(&send_something(&mut client, now())));

    client
        .migrate(Some(DEFAULT_ADDR_V4), Some(DEFAULT_ADDR_V4), false, now())
        .unwrap();

    // The client now probes the new path.
    let probe = client.process_output(now()).dgram().unwrap();
    assert_v4_path(&probe, true);
    assert_eq!(client.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&client, &probe, now());
    let probe_cid = ConnectionId::from(get_cid(&probe));
    assert_ne!(original_cid, probe_cid);

    // Have the server receive the probe, but separately have it decide to
    // retire all of the available connection IDs.
    let retire_all = send_with_extra(&mut server, RetireAll { cid_gen }, now());

    let resp = server.process(Some(probe), now()).dgram().unwrap();
    assert_v4_path(&resp, true);
    assert_eq!(server.stats().frame_tx.path_response, 1);
    assert_eq!(server.stats().frame_tx.path_challenge, 1);
    assert_path_challenge_min_len(&server, &resp, now());

    // Have the client receive the NEW_CONNECTION_ID with Retire Prior To second.
    // As this occurs in a very specific order, migration succeeds.
    client.process_input(resp, now());
    client.process_input(retire_all, now());

    // Migration succeeds and the new path gets the last connection ID.
    let dgram = send_something(&mut client, now());
    assert_v4_path(&dgram, false);
    assert_ne!(get_cid(&dgram), original_cid);
    assert_ne!(get_cid(&dgram), probe_cid);
}

struct GarbageWriter {}

impl crate::connection::test_internal::FrameWriter for GarbageWriter {
    fn write_frames(&mut self, builder: &mut packet::Builder<&mut Vec<u8>>) {
        // Not a valid frame type.
        builder.encode_varint(u32::MAX);
    }
}

/// Test the case that we run out of connection ID and receive an invalid frame
/// from a new path.
#[test]
fn error_on_new_path_with_no_connection_id() {
    let mut client = default_client();
    let mut server = default_server();
    connect_force_idle(&mut client, &mut server);

    let cid_gen: Rc<RefCell<dyn ConnectionIdGenerator>> =
        Rc::new(RefCell::new(CountingConnectionIdGenerator::default()));
    let retire_all = send_with_extra(&mut server, RetireAll { cid_gen }, now());

    client.process_input(retire_all, now());

    let garbage = send_with_extra(&mut server, GarbageWriter {}, now());

    let dgram = change_path(&garbage, DEFAULT_ADDR_V4);
    client.process_input(dgram, now());

    // See issue #1697. We had a crash when the client had a temporary path and
    // process_output is called.
    let closing_frames = client.stats().frame_tx.connection_close;
    drop(client.process_output(now()));
    assert!(matches!(
        client.state(),
        State::Closing {
            error: CloseReason::Transport(Error::UnknownFrameType),
            ..
        }
    ));
    // Wait until the connection is closed.
    let mut now = now();
    now += client.process_output(now).callback();
    _ = client.process_output(now);
    // No closing frames should be sent, and the connection should be closed.
    assert_eq!(client.stats().frame_tx.connection_close, closing_frames);
    assert!(matches!(
        client.state(),
        State::Closed(CloseReason::Transport(Error::UnknownFrameType))
    ));
}
