//! Policy regression tests against real loopback datagrams, without graphics or content.
use super::*;

fn socket() -> UdpSocket {
    let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    s.set_nonblocking(true).unwrap();
    s
}
fn host() -> LanSession {
    let mut h = LanSession::new(socket(), Role::Host, "server", WorldInfo::default());
    h.session = 0xabc;
    h.set_server_policy(&["Vehicles/ok.bus".into()], true);
    h
}
fn hello(h: &mut LanSession, socket: &UdpSocket, name: &str, nonce: u64) -> u32 {
    let text = format!("HELLO|{PROTOCOL}|-|{name}|Vehicles/ok.bus|maps/x/global.cfg|2026-10-03|36000|||{nonce:016X}|P1");
    h.on_hello(
        &text.split('|').collect::<Vec<_>>(),
        socket.local_addr().unwrap(),
    );
    h.peers
        .iter()
        .find(|(_, p)| p.addr == Some(socket.local_addr().unwrap()))
        .unwrap()
        .0
        .to_owned()
}
fn client(h: &mut LanSession, name: &str, nonce: u64) -> LanSession {
    let s = socket();
    let id = hello(h, &s, name, nonce);
    let mut c = LanSession::new(s, Role::Client, name, WorldInfo::default());
    c.my_id = id;
    c.host = h.local_addr();
    c.session = h.session;
    c.connected = true;
    c.receive(&mut Vec::new());
    c
}
fn request(c: &mut LanSession, bus: &str, tour: &str) {
    c.info_acc = INFO_EVERY;
    c.send_own(
        0.0,
        &Pose {
            bus: bus.into(),
            tour: tour.into(),
            flags: FLAG_VEHICLE,
            ..Default::default()
        },
    );
}

#[test]
fn policy_simultaneous_claims_have_one_owner_and_the_loser_receives_denial() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    let mut b = client(&mut h, "Bob", 11);
    request(&mut a, "Vehicles/ok.bus", "136/1");
    request(&mut b, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    assert_eq!(h.occupied_tours().len(), 1);
    assert_eq!(
        h.peers.values().filter(|p| !p.pose.tour.is_empty()).count(),
        1
    );
    a.receive(&mut Vec::new());
    b.receive(&mut Vec::new());
    assert_eq!(
        usize::from(a.tour_claim_confirmed("136/1")) + usize::from(b.tour_claim_confirmed("136/1")),
        1
    );
    let denies = [a.take_policy_rejections(), b.take_policy_rejections()].concat();
    assert_eq!(denies.len(), 1);
    assert!(denies[0].reason.contains("occupied by"));
    assert_eq!(denies[0].tour, "136/1");
}

#[test]
fn policy_owner_reconnects_without_losing_the_claim_and_old_address_cannot_release_it() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    request(&mut a, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    let replacement = socket();
    assert_eq!(hello(&mut h, &replacement, "Alice", 10), a.my_id);
    request(&mut a, "Vehicles/ok.bus", "");
    h.receive(&mut Vec::new());
    assert_eq!(h.occupied_tours()[0].player_id, a.my_id);
    let mut q = Pose {
        id: a.my_id,
        bus: "Vehicles/ok.bus".into(),
        tour: "136/1".into(),
        ..Default::default()
    };
    let text = format!("{}|P1:2", q.encode_info_reserved(16));
    h.on_info(
        &text.split('|').collect::<Vec<_>>(),
        replacement.local_addr().unwrap(),
    );
    assert_eq!(h.occupied_tours().len(), 1);
    q.tour.clear();
    let text = format!("{}|P1:3", q.encode_info_reserved(16));
    h.on_info(
        &text.split('|').collect::<Vec<_>>(),
        replacement.local_addr().unwrap(),
    );
    assert!(h.occupied_tours().is_empty());
}

#[test]
fn policy_duty_change_leave_and_timeout_release_ownership() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    let mut b = client(&mut h, "Bob", 11);
    request(&mut a, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    request(&mut a, "Vehicles/ok.bus", "139/1");
    request(&mut b, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    assert_eq!(h.occupied_tours().len(), 2);
    h.on_bye(
        &["BYE", &a.my_id.to_string()],
        a.local_addr().unwrap(),
        "BYE",
        &mut Vec::new(),
    );
    assert_eq!(h.occupied_tours().len(), 1);
    let peer = h.peers.get_mut(&b.my_id).unwrap();
    peer.has_pose = true;
    peer.last_seen = Instant::now() - h.timeout - Duration::from_secs(1);
    h.tick(0.0, &Pose::default());
    assert!(h.occupied_tours().is_empty());
}

#[test]
fn policy_vehicle_change_is_rejected_without_kicking_and_reordered_info_cannot_restore_it() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    request(&mut a, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    request(&mut a, "Vehicles/forbidden.bus", "136/1");
    h.receive(&mut Vec::new());
    assert!(h.peers.contains_key(&a.my_id));
    assert!(h.peers[&a.my_id].pose.bus.is_empty());
    assert!(h.occupied_tours().is_empty());
    a.receive(&mut Vec::new());
    let denial = a.take_policy_rejections();
    assert_eq!(denial.len(), 1);
    assert!(denial[0].vehicle);
    let old = Pose {
        id: a.my_id,
        bus: "Vehicles/ok.bus".into(),
        tour: "136/1".into(),
        ..Default::default()
    };
    let text = format!("{}|P1:1", old.encode_info_reserved(16));
    h.on_info(
        &text.split('|').collect::<Vec<_>>(),
        a.local_addr().unwrap(),
    );
    assert!(h.peers[&a.my_id].pose.bus.is_empty());
    request(&mut a, "Vehicles/ok.bus", "");
    h.receive(&mut Vec::new());
    assert_eq!(h.peers[&a.my_id].pose.bus, "Vehicles/ok.bus");
}

#[test]
fn policy_spoofed_info_denial_and_snapshot_cannot_change_ownership_or_client_state() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    let attacker = socket();
    request(&mut a, "Vehicles/ok.bus", "136/1");
    h.receive(&mut Vec::new());
    let fake = Pose {
        id: a.my_id,
        ..Default::default()
    };
    let text = format!("{}|P1:2", fake.encode_info_reserved(16));
    h.on_info(
        &text.split('|').collect::<Vec<_>>(),
        attacker.local_addr().unwrap(),
    );
    assert_eq!(h.occupied_tours().len(), 1);
    let reply = format!(
        "POLICY|{}|{}|1|1|{}",
        session_hex(h.session),
        a.my_id,
        policy::hex("denied")
    );
    a.on_policy_reply(
        &reply.split('|').collect::<Vec<_>>(),
        attacker.local_addr().unwrap(),
    );
    assert!(a.take_policy_rejections().is_empty());
    a.on_tours(
        &["TOURS", &session_hex(h.session), "500", "0", "0", "1", ""],
        attacker.local_addr().unwrap(),
    );
    assert!(a.exclusive_tours());
}

#[test]
fn policy_old_denial_cannot_cancel_a_newer_selection() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    request(&mut a, "Vehicles/ok.bus", "136/1");
    request(&mut a, "Vehicles/ok.bus", "139/1");
    let reply = format!(
        "POLICY|{}|{}|1|0|{}",
        session_hex(h.session),
        a.my_id,
        policy::hex("occupied")
    );
    a.on_policy_reply(
        &reply.split('|').collect::<Vec<_>>(),
        h.local_addr().unwrap(),
    );
    assert!(a.take_policy_rejections().is_empty());
}

#[test]
fn policy_fifty_owners_snapshot_is_fragmented_atomic_and_missing_chunk_is_unknown() {
    let mut h = host();
    let mut c = client(&mut h, "Alice", 10);
    for id in 2..52 {
        h.host_policy
            .update(
                id,
                &format!("Driver {id} Ž"),
                "Vehicles/ok.bus",
                &format!("136/{id}"),
            )
            .unwrap();
    }
    h.send_policy_snapshot(c.local_addr());
    let mut packets = Vec::new();
    let mut buf = [0u8; MAX_DATAGRAM + 1];
    while let Ok((n, from)) = c.socket.recv_from(&mut buf) {
        assert!(n <= MAX_DATAGRAM);
        packets.push((std::str::from_utf8(&buf[..n]).unwrap().to_string(), from));
    }
    assert!(packets.len() > 1);
    for (text, from) in packets.iter().rev().skip(1) {
        c.on_tours(&text.split('|').collect::<Vec<_>>(), *from);
    }
    assert!(!c.tour_status_fresh());
    assert!(c.occupied_tours().is_empty());
    let (text, from) = packets.last().unwrap();
    c.on_tours(&text.split('|').collect::<Vec<_>>(), *from);
    assert!(c.tour_status_fresh());
    assert_eq!(c.occupied_tours().len(), 50);
    c.tour_status_at = Some(Instant::now() - Duration::from_secs(6));
    assert!(!c.tour_status_fresh());
}

#[test]
fn policy_rejects_disallowed_or_legacy_hello_before_admission() {
    let mut h = host();
    let s = socket();
    for tail in [
        "Vehicles/forbidden.bus|maps/x/global.cfg|2026-10-03|0|||10|P1",
        "Vehicles/ok.bus|maps/x/global.cfg|2026-10-03|0|||10",
    ] {
        let text = format!("HELLO|{PROTOCOL}|-|Alice|{tail}");
        h.on_hello(
            &text.split('|').collect::<Vec<_>>(),
            s.local_addr().unwrap(),
        );
        assert!(h.peers.is_empty());
    }
}

#[test]
fn policy_status_json_round_trips_fifty_owners_and_legacy_is_unknown() {
    let info = ws::ServerInfo {
        name: "server".into(),
        exclusive_tours: true,
        tour_status_at: Some(Instant::now()),
        occupied_tours: (2..52)
            .map(|id| TourOccupancy {
                line: "136".into(),
                tour: id.to_string(),
                player_id: id,
                player_name: "Řidič {\"x\"} \\".into(),
            })
            .collect(),
        ..Default::default()
    };
    let decoded = ws::ServerInfo::from_json(&info.to_json()).unwrap();
    assert!(decoded.exclusive_tours);
    assert!(decoded.tour_status_fresh());
    assert_eq!(decoded.occupied_tours, info.occupied_tours);
    assert!(!ws::ServerInfo::from_json("{\"name\":\"old server\"}")
        .unwrap()
        .tour_status_fresh());
    let malformed = "{\"name\":\"server\",\"exclusive_tours\":true,\"tour_status_known\":true,\"occupied_tours\":[{}]}";
    assert!(!ws::ServerInfo::from_json(malformed)
        .unwrap()
        .tour_status_fresh());
}

#[test]
fn policy_info_revision_still_fits_the_datagram_with_maximum_display_payload() {
    let h = host();
    let mut c = LanSession::new(
        socket(),
        Role::Client,
        &"Ž".repeat(MAX_NAME),
        WorldInfo::default(),
    );
    c.my_id = 2;
    c.host = h.local_addr();
    c.connected = true;
    let p = Pose {
        bus: format!("Vehicles/{}.bus", "a".repeat(240)),
        tour: "136/1".into(),
        texts: (0..MAX_TEXTS).map(|_| "x".repeat(MAX_TEXT_LEN)).collect(),
        freetex: (0..MAX_FREETEX)
            .map(|_| "y".repeat(MAX_FREETEX_LEN))
            .collect(),
        ..Default::default()
    };
    c.info_acc = INFO_EVERY;
    c.send_own(0.0, &p);
    let mut buf = [0u8; MAX_DATAGRAM + 1];
    let mut found = false;
    while let Ok((n, _)) = h.socket.recv_from(&mut buf) {
        assert!(n <= MAX_DATAGRAM);
        if buf[..n].starts_with(b"INFO|") {
            let text = std::str::from_utf8(&buf[..n]).unwrap();
            assert!(text.ends_with("|P1:1"));
            found = true;
        }
    }
    assert!(found);
}

#[test]
fn policy_duty_waits_for_welcome_ack_and_complete_fresh_snapshot() {
    let h = host();
    let mut c = LanSession::new(socket(), Role::Client, "Alice", WorldInfo::default());
    assert!(!c.tour_claim_confirmed("136/1"));
    let welcome = format!(
        "WELCOME|{PROTOCOL}|2|{}|server|maps/x/global.cfg|2026-10-03|0|||1|P1:1",
        session_hex(h.session)
    );
    c.on_welcome(
        &welcome.split('|').collect::<Vec<_>>(),
        h.local_addr().unwrap(),
    );
    assert!(c.exclusive_tours());
    assert!(!c.tour_status_fresh());
    assert!(!c.tour_claim_confirmed("136/1"));
    c.policy_revision = 1;
    c.policy_requested = ("Vehicles/ok.bus".into(), "136/1".into());
    let ack = format!("POLICY|{}|2|1|0|", session_hex(h.session));
    c.on_policy_reply(&ack.split('|').collect::<Vec<_>>(), h.local_addr().unwrap());
    assert!(!c.tour_claim_confirmed("136/1"));
    c.on_tours(
        &["TOURS", &session_hex(h.session), "1", "1", "0", "1", ""],
        h.local_addr().unwrap(),
    );
    assert!(c.tour_claim_confirmed("136/1"));
    assert!(!c.tour_claim_confirmed("139/1"));
    c.on_tours(
        &["TOURS", &session_hex(h.session), "2", "1", "0", "2", ""],
        h.local_addr().unwrap(),
    );
    assert!(!c.tour_claim_confirmed("136/1"));
    c.on_tours(
        &["TOURS", &session_hex(h.session), "2", "1", "1", "2", ""],
        h.local_addr().unwrap(),
    );
    assert!(c.tour_claim_confirmed("136/1"));
    c.connected = false;
    assert!(!c.tour_claim_confirmed("136/1"));
    c.on_welcome(
        &welcome.split('|').collect::<Vec<_>>(),
        h.local_addr().unwrap(),
    );
    assert!(!c.tour_claim_confirmed("136/1"));
}

#[test]
fn policy_legacy_lan_duty_runs_after_welcome_without_new_policy_packets() {
    let h = host();
    let mut c = LanSession::new(socket(), Role::Client, "Alice", WorldInfo::default());
    assert!(!c.tour_claim_confirmed("136/1"));
    let welcome = format!(
        "WELCOME|{PROTOCOL}|2|{}|server|maps/x/global.cfg|2026-10-03|0|||1",
        session_hex(h.session)
    );
    c.on_welcome(
        &welcome.split('|').collect::<Vec<_>>(),
        h.local_addr().unwrap(),
    );
    assert!(!c.exclusive_tours());
    assert!(c.tour_claim_confirmed("136/1"));
}

#[test]
fn policy_walking_or_invalid_bus_cannot_claim_a_duty_or_suppress_its_ai() {
    let mut h = host();
    let mut a = client(&mut h, "Alice", 10);
    request(&mut a, "", "136/1");
    h.receive(&mut Vec::new());
    assert!(h.occupied_tours().is_empty());
    assert!(h.peers[&a.my_id].pose.tour.is_empty());
    request(&mut a, "", "");
    h.receive(&mut Vec::new());
    assert!(h.peers[&a.my_id].policy_denial.is_none());
    let p = Pose {
        id: a.my_id,
        bus: "Vehicles/ok.bus".into(),
        tour: "136/1".into(),
        ..Default::default()
    };
    let text = format!("{}|P1:3", p.encode_info_reserved(16))
        .replace("Vehicles/ok.bus", "Vehicles/../bad.bus");
    h.on_info(
        &text.split('|').collect::<Vec<_>>(),
        a.local_addr().unwrap(),
    );
    assert!(h.peers[&a.my_id].policy_denial.as_ref().unwrap().vehicle);
    assert!(h.peers[&a.my_id].pose.tour.is_empty());
    let s = socket();
    let text = format!(
        "HELLO|{PROTOCOL}|-|Bob|Vehicles/../bad.bus|maps/x/global.cfg|2026-10-03|0|||11|P1"
    );
    h.on_hello(
        &text.split('|').collect::<Vec<_>>(),
        s.local_addr().unwrap(),
    );
    assert_eq!(h.peers.len(), 1);
}
