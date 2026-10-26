use kasu_core::*;
use kasu_git::*;
use proptest::prelude::*;

fn command(capabilities: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_packet(
        &mut bytes,
        format!(
            "{} {} refs/heads/main\0{capabilities}\n",
            GitOid([0; 20]),
            GitOid([1; 20])
        )
        .as_bytes(),
    )
    .unwrap();
    bytes.extend(b"0000");
    bytes
}
#[test]
fn receive_framing_capabilities_and_report_status_are_separate_from_pack() {
    let mut bytes = command("report-status side-band-64k object-format=sha1 agent=test");
    bytes.extend(b"PACK remainder");
    let q = parse_receive(&bytes).unwrap();
    assert_eq!(q.pack, b"PACK remainder");
    assert_eq!(q.commands[0].new, Some(GitOid([1; 20])));
    let report = receive_report(&q, None, &[(b"refs/heads/main".to_vec(), None)]).unwrap();
    let packets = parse_packets(&report).unwrap();
    let Packet::Data(payload) = packets[0] else {
        panic!("sideband")
    };
    assert_eq!(payload[0], 1);
    assert!(
        parse_packets(&payload[1..])
            .unwrap()
            .contains(&Packet::Data(b"unpack ok\n"))
    );
    for cap in [
        "report-status-v2",
        "push-options",
        "filter",
        "object-format=sha256",
        "report-status report-status",
    ] {
        assert!(parse_receive(&command(cap)).is_err());
    }
    let atomic_command = command("report-status atomic");
    assert!(parse_receive(&atomic_command).unwrap().atomic);
    assert!(!q.atomic);
    let advert = receive_advertisement(&[], false).unwrap();
    let text = String::from_utf8(advert).unwrap();
    assert!(text.contains("capabilities^{}"));
    assert!(!text.contains("atomic"));
    assert!(
        String::from_utf8(receive_advertisement(&[], true).unwrap())
            .unwrap()
            .contains(" atomic")
    );
    let mut duplicate = command("report-status");
    duplicate.truncate(duplicate.len() - 4);
    write_packet(
        &mut duplicate,
        format!("{} {} refs/heads/main\n", GitOid([0; 20]), GitOid([2; 20])).as_bytes(),
    )
    .unwrap();
    duplicate.extend(b"0000");
    assert!(parse_receive(&duplicate).is_err());
}
proptest! {
    #[test] fn receive_commands_never_panic(bytes in prop::collection::vec(any::<u8>(),0..1024)) {let _=parse_receive(&bytes);}
}
