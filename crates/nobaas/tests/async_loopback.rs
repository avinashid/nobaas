//! Two tokio-driven nodes over real loopback UDP.
use nobaas::{AsyncNode, Event, Lane, Node, Status};
use std::time::Duration;
use tokio::time::timeout;

/// The host echoes every message back on the lane it came on, until the guest leaves.
async fn echo(mut host: AsyncNode) -> usize {
    let mut echoed = 0;
    loop {
        match host.next_event().await {
            Event::Message { peer, lane, data } => {
                host.send(&peer, lane, &data).unwrap();
                echoed += 1;
            }
            Event::Disconnected { .. } => return echoed,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn connect_and_echo_without_polling() {
    let host = Node::builder().stun_servers(Vec::<String>::new()).bind("0.0.0.0:0").unwrap();
    let host = AsyncNode::from_node(host).unwrap();
    let mut guest = AsyncNode::bind("127.0.0.1:0").await.unwrap();
    let h = host.peer_id();
    let invite = host.invite();
    let host = tokio::spawn(echo(host));

    assert_eq!(guest.connect(&invite).unwrap(), h);
    assert_eq!(guest.status(&h), Status::Connecting);
    let ev = timeout(Duration::from_secs(5), guest.next_event()).await.expect("connects");
    assert!(matches!(ev, Event::Connected { peer, .. } if peer == h), "{ev:?}");
    assert_eq!(guest.status(&h), Status::Connected);

    let big = vec![7u8; 100_000];
    for i in 0..20u8 {
        guest.send(&h, Lane::Reliable, &[i]).unwrap();
    }
    guest.send(&h, Lane::Reliable, &big).unwrap();
    let mut got = Vec::new();
    while got.len() < 21 {
        let ev = timeout(Duration::from_secs(5), guest.next_event()).await.expect("echoes arrive");
        if let Event::Message { lane: Lane::Reliable, data, .. } = ev {
            got.push(data);
        }
    }
    assert_eq!(got[..20], (0..20u8).map(|i| vec![i]).collect::<Vec<_>>()[..], "reliable lane is ordered");
    assert_eq!(got[20], big);
    assert!(guest.peer_stats(&h).unwrap().rtt.is_some());

    guest.disconnect(&h);
    let echoed = timeout(Duration::from_secs(5), host).await.expect("host sees the guest leave").unwrap();
    assert_eq!(echoed, 21);
}

#[tokio::test(flavor = "current_thread")]
async fn next_event_is_cancel_safe() {
    let mut host = AsyncNode::bind("127.0.0.1:0").await.unwrap();
    let mut guest = AsyncNode::bind("127.0.0.1:0").await.unwrap();
    let h = guest.connect(&host.invite()).unwrap();
    let mut events = [Vec::new(), Vec::new()];
    let mut sent = false;
    // Race both nodes against a short tick: whichever loses is dropped mid-wait, often.
    for _ in 0..2000 {
        tokio::select! {
            e = host.next_event() => events[0].push(e),
            e = guest.next_event() => events[1].push(e),
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
        if !sent && guest.status(&h) == Status::Connected {
            for i in 0..50u8 {
                guest.send(&h, Lane::Reliable, &[i]).unwrap();
            }
            sent = true;
        }
        let n = events[0].iter().filter(|e| matches!(e, Event::Message { .. })).count();
        if n == 50 {
            break;
        }
    }
    let got: Vec<u8> = events[0]
        .iter()
        .filter_map(|e| match e {
            Event::Message { data, .. } => Some(data[0]),
            _ => None,
        })
        .collect();
    assert_eq!(got, (0..50u8).collect::<Vec<_>>(), "every message once, in order");
}

#[test]
fn async_node_is_send() {
    fn send<T: Send>() {}
    send::<AsyncNode>();
}
