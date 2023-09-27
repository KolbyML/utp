use futures::stream::{FuturesUnordered, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use utp_rs::cid;
use utp_rs::conn::ConnectionConfig;
use utp_rs::socket::UtpSocket;

const TEST_DATA: &[u8] = &[0xf0; 1_000_000];

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn socket() {
    tracing_subscriber::fmt::init();

    tracing::info!("starting socket test");

    let recv_addr = SocketAddr::from(([127, 0, 0, 1], 3400));
    let send_addr = SocketAddr::from(([127, 0, 0, 1], 3401));

    let recv = UtpSocket::bind(recv_addr).await.unwrap();
    let recv = Arc::new(recv);
    let send = UtpSocket::bind(send_addr).await.unwrap();
    let send = Arc::new(send);
    let mut handles = FuturesUnordered::new();

    let start = Instant::now();
    let num_transfers = 1500;
    for i in 0..num_transfers {
        // step up cid by two to avoid collisions
        let handle =
            initiate_transfer(i * 2, recv_addr, recv.clone(), send_addr, send.clone()).await;
        handles.push(handle.0);
        handles.push(handle.1);
    }

    while let Some(res) = handles.next().await {
        res.unwrap();
    }
    let elapsed = Instant::now() - start;
    let megabits_sent = num_transfers as f64 * TEST_DATA.len() as f64 * 8.0 / 1_000_000.0;
    let transfer_rate = megabits_sent / elapsed.as_secs_f64();
    tracing::info!("finished real udp load test of {} simultaneous transfers, in {:?}, at a rate of {:.0} Mbps", num_transfers, elapsed, transfer_rate);
}

async fn initiate_transfer(
    i: u16,
    recv_addr: SocketAddr,
    recv: Arc<UtpSocket<SocketAddr>>,
    send_addr: SocketAddr,
    send: Arc<UtpSocket<SocketAddr>>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let conn_config = ConnectionConfig::default();
    let initiator_cid = 100 + i;
    let responder_cid = 100 + i + 1;
    let recv_cid = cid::ConnectionId {
        send: initiator_cid,
        recv: responder_cid,
        peer: send_addr,
    };
    let send_cid = cid::ConnectionId {
        send: responder_cid,
        recv: initiator_cid,
        peer: recv_addr,
    };

    let recv_handle = tokio::spawn(async move {
        let mut stream = recv.accept_with_cid(recv_cid, conn_config).await.unwrap();
        let mut buf = vec![];
        let n = match stream.read_to_eof(&mut buf).await {
            Ok(num_bytes) => num_bytes,
            Err(err) => {
                let cid = stream.cid();
                tracing::error!(?cid, "read to eof error: {:?}", err);
                panic!("fail to read data");
            }
        };
        tracing::info!(cid.send = %recv_cid.send, cid.recv = %recv_cid.recv, "read {n} bytes from uTP stream");

        assert_eq!(n, TEST_DATA.len());
        assert_eq!(buf, TEST_DATA);
    });

    let send_handle = tokio::spawn(async move {
        let mut stream = send.connect_with_cid(send_cid, conn_config).await.unwrap();
        let n = stream.write(TEST_DATA).await.unwrap();
        assert_eq!(n, TEST_DATA.len());

        stream.close().await.unwrap();
    });
    (send_handle, recv_handle)
}

// Test that a new socket has zero connections
#[tokio::test]
async fn test_empty_socket_conn_count() {
    let socket_addr = SocketAddr::from(([127, 0, 0, 1], 3402));
    let socket = UtpSocket::bind(socket_addr).await.unwrap();
    assert_eq!(socket.num_connections(), 0);
}

// Test that a socket returns 2 from num_connections after connecting twice
#[tokio::test]
async fn test_socket_reports_two_connections() {
    let conn_config = ConnectionConfig::default();

    let recv_addr = SocketAddr::from(([127, 0, 0, 1], 3404));
    let recv = UtpSocket::bind(recv_addr).await.unwrap();
    let recv = Arc::new(recv);

    let send_addr = SocketAddr::from(([127, 0, 0, 1], 3405));
    let send = UtpSocket::bind(send_addr).await.unwrap();
    let send = Arc::new(send);

    let recv_one_cid = cid::ConnectionId {
        send: 100,
        recv: 101,
        peer: send_addr,
    };
    let send_one_cid = cid::ConnectionId {
        send: 101,
        recv: 100,
        peer: recv_addr,
    };

    let recv_one = Arc::clone(&recv);
    let recv_one_handle = tokio::spawn(async move {
        recv_one
            .accept_with_cid(recv_one_cid, conn_config)
            .await
            .unwrap()
    });

    let send_one = Arc::clone(&send);
    let send_one_handle = tokio::spawn(async move {
        send_one
            .connect_with_cid(send_one_cid, conn_config)
            .await
            .unwrap()
    });

    let recv_two_cid = cid::ConnectionId {
        send: 200,
        recv: 201,
        peer: send_addr,
    };
    let send_two_cid = cid::ConnectionId {
        send: 201,
        recv: 200,
        peer: recv_addr,
    };

    let recv_two = Arc::clone(&recv);
    let recv_two_handle = tokio::spawn(async move {
        recv_two
            .accept_with_cid(recv_two_cid, conn_config)
            .await
            .unwrap()
    });

    let send_two = Arc::clone(&send);
    let send_two_handle = tokio::spawn(async move {
        send_two
            .connect_with_cid(send_two_cid, conn_config)
            .await
            .unwrap()
    });

    let (tx_one, rx_one, tx_two, rx_two) = tokio::join!(
        send_one_handle,
        recv_one_handle,
        send_two_handle,
        recv_two_handle
    );
    tx_one.unwrap();
    rx_one.unwrap();
    tx_two.unwrap();
    rx_two.unwrap();

    assert_eq!(recv.num_connections(), 2);
    assert_eq!(send.num_connections(), 2);
}
