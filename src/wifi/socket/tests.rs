use super::*;
use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    thread,
};

fn pair() -> (WifiSocket, WifiSocket) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = TcpStream::connect(address).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        WifiSocket::from_stream(client).unwrap(),
        WifiSocket::from_stream(server).unwrap(),
    )
}

#[test]
fn bytes_written_on_one_end_arrive_in_order_on_the_other_and_an_empty_read_times_out() {
    let (mut a, mut b) = pair();
    a.write(b"first", Instant::now() + Duration::from_secs(1)).unwrap();
    a.write(b"second", Instant::now() + Duration::from_secs(1)).unwrap();
    a.write(b"third", Instant::now() + Duration::from_secs(1)).unwrap();
    let expected = b"firstsecondthird";
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while received.len() < expected.len() && Instant::now() < deadline {
        let mut chunk = [0u8; 64];
        match b.read(&mut chunk, deadline) {
            Ok(count) => received.extend_from_slice(&chunk[..count]),
            Err(Error::Timeout) => {}
            Err(error) => panic!("{error:?}"),
        }
    }
    assert_eq!(received, expected);
    let started = Instant::now();
    assert_eq!(
        b.read(&mut [0u8; 8], Instant::now() + Duration::from_millis(5)),
        Err(Error::Timeout)
    );
    assert!(started.elapsed() >= Duration::from_millis(4));
}

#[test]
fn a_read_wakes_when_data_arrives_before_the_deadline() {
    let (mut a, b) = pair();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        a.write(b"late", Instant::now() + Duration::from_secs(1)).unwrap();
    });
    let started = Instant::now();
    let mut chunk = [0u8; 8];
    let count = b
        .read(&mut chunk, Instant::now() + Duration::from_millis(500))
        .unwrap();
    assert_eq!(&chunk[..count], b"late");
    assert!(started.elapsed() < Duration::from_millis(100));
    writer.join().unwrap();
}

#[test]
fn a_closed_peer_is_reported_as_closed_not_as_zero_bytes() {
    let (a, b) = pair();
    drop(a);
    let mut chunk = [0u8; 8];
    let result = b.read(&mut chunk, Instant::now() + Duration::from_millis(50));
    assert_eq!(result, Err(Error::Closed));
}

#[test]
fn a_peer_that_never_reads_stalls_the_carrier_but_never_blocks_the_caller() {
    let (mut a, _b) = pair();
    let chunk = vec![0u8; 64 * 1024];
    let mut stalled = false;
    for _ in 0..200 {
        let started = Instant::now();
        let result = a.write(&chunk, Instant::now() + Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_millis(50));
        if matches!(result, Err(Error::Overflow)) {
            break;
        }
        result.unwrap();
        if a
            .service_at(Instant::now() + Duration::from_millis(2_600))
            == Err(Error::Timeout)
        {
            stalled = true;
            break;
        }
    }
    assert!(stalled, "the stalled write was never reported");
}

/// Android's carrier monitor once read the write-start time twice while the
/// writer cleared it: "now minus zero" looked like a write stalled since the
/// start, and a healthy idle carrier was closed. Here the stall clock is one
/// field owned by the writer and compared once, and only while bytes wait.
#[test]
fn an_idle_carrier_is_never_a_stalled_one_however_long_it_was_idle() {
    let (mut a, b) = pair();
    let hour = Duration::from_secs(3_600);
    assert_eq!(a.service_at(Instant::now() + hour), Ok(()), "nothing waits, so nothing stalls");
    a.write(b"after an hour of silence", Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(a.service_at(Instant::now() + hour), Ok(()), "the kernel took it all; nothing waits");
    let mut got = [0u8; 64];
    let deadline = Instant::now() + Duration::from_secs(2);
    let count = b.read(&mut got, deadline).unwrap();
    assert_eq!(&got[..count], b"after an hour of silence");
}

#[test]
fn writing_past_the_buffer_cap_is_an_overflow_error() {
    let (mut a, _b) = pair();
    let chunk = vec![0u8; 64 * 1024];
    let mut overflow = false;
    for _ in 0..1_000 {
        match a.write(&chunk, Instant::now() + Duration::from_secs(1)) {
            Ok(()) => {}
            Err(Error::Overflow) => {
                overflow = true;
                break;
            }
            Err(error) => panic!("{error:?}"),
        }
    }
    assert!(overflow);
}

#[test]
fn the_subnet_filter_accepts_only_peers_in_the_group_network() {
    let (a, _b) = pair();
    assert!(a.peer_in_subnet(Ipv4Addr::new(127, 0, 0, 1), 8));
    assert!(!a.peer_in_subnet(Ipv4Addr::new(10, 0, 0, 1), 24));
}

#[test]
fn the_oldest_unsent_age_follows_the_first_unsent_write() {
    let (mut a, mut b) = pair();
    let chunk = vec![0u8; 64 * 1024];
    let mut queued = false;
    for _ in 0..200 {
        match a.write(&chunk, Instant::now() + Duration::from_secs(1)) {
            Ok(()) => {}
            Err(Error::Overflow) => break,
            Err(error) => panic!("{error:?}"),
        }
        if a.oldest_unsent_age(Instant::now()).is_some() {
            queued = true;
            break;
        }
    }
    assert!(queued);
    let started = Instant::now();
    let age = a.oldest_unsent_age(started + Duration::from_millis(400)).unwrap();
    assert!(age >= Duration::from_millis(390));
    // The peer drains everything; the age clears after a service pass.
    let mut scratch = vec![0u8; 256 * 1024];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match b.read(&mut scratch, Instant::now() + Duration::from_millis(20)) {
            Ok(_) => {}
            Err(Error::Timeout) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    for _ in 0..100 {
        a.service_at(Instant::now()).unwrap();
        if a.oldest_unsent_age(Instant::now()).is_none() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
        while let Ok(count) = b.read(&mut scratch, Instant::now()) {
            if count == 0 {
                break;
            }
        }
    }
    panic!("the oldest unsent age never cleared");
}

/// WP6NG finding 7: records the kernel has not started are dropped (a record
/// already partly sent is finished, so the stream stays framed); what is
/// written afterwards still arrives.
#[test]
fn unstarted_records_are_dropped_and_a_partly_sent_one_is_finished() {
    let (mut a, b) = pair();
    let chunk = vec![0u8; 64 * 1024];
    let mut written = 0usize;
    for _ in 0..200 {
        a.write(&chunk, Instant::now() + Duration::from_secs(1)).unwrap();
        written += chunk.len();
        if a.oldest_unsent_age(Instant::now()).is_some() {
            break;
        }
    }
    assert!(a.oldest_unsent_age(Instant::now()).is_some(), "the peer does not read yet: records wait");
    for _ in 0..2 {
        a.write(&chunk, Instant::now() + Duration::from_secs(1)).unwrap();
        written += chunk.len();
    }
    a.discard_unstarted();
    a.write(b"END", Instant::now() + Duration::from_secs(1)).unwrap();
    let mut received = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(10);
    while !received.ends_with(b"END") {
        assert!(Instant::now() < deadline, "END never arrived ({} bytes)", received.len());
        let _ = a.service();
        if let Ok(count) = b.read(&mut buffer, Instant::now() + Duration::from_millis(20)) {
            received.extend_from_slice(&buffer[..count]);
        }
    }
    let records = received.len() - 3;
    assert_eq!(records % chunk.len(), 0, "only whole records arrived before END");
    assert!(records < written, "the unstarted records were dropped");
}
