use std::{net::Ipv4Addr, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use librqbit::read_buf::ReadBuf;

use librqbit_core::constants::CHUNK_SIZE;
use parking_lot::RwLock;
use peer_binary_protocol::{MessageBorrowed, PIECE_MESSAGE_DEFAULT_LEN, Piece};
use tokio::{io::AsyncWriteExt, net::tcp::OwnedReadHalf};

struct ReadBufBench {
    read_buf: ReadBuf,
    in_sock: OwnedReadHalf,
}

impl ReadBufBench {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();

        let (mut sender, receiver) = tokio::join!(
            async move { listener.accept().await.unwrap().0.into_split().1 },
            async move {
                tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap()
                    .into_split()
                    .0
            }
        );

        let mut data = [0u8; CHUNK_SIZE as usize];
        rand::fill(&mut data);

        const BUF_ITS: usize = 10;
        let mut multi_msg = Vec::new();

        let mut one = vec![0u8; PIECE_MESSAGE_DEFAULT_LEN];
        let sz = MessageBorrowed::Piece(Piece::from_data(42, 43, &data[..]))
            .serialize(&mut one, &|| Default::default())
            .unwrap();

        for i in 0..BUF_ITS {
            multi_msg.extend(&one[..sz]);
        }

        tokio::spawn(async move {
            loop {
                sender.write_all(&multi_msg).await.unwrap();
            }
        });

        Self {
            read_buf: ReadBuf::new(),
            in_sock: receiver,
        }
    }

    async fn read_one(&mut self) {
        self.read_buf
            .read_message(&mut self.in_sock, Duration::from_secs(1))
            .await
            .unwrap();
    }
}

pub fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("read_buf_read_piece", move |b| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let rb = RwLock::new(rt.block_on(ReadBufBench::new()));
        b.to_async(rt)
            .iter(|| async { rb.write().read_one().await });
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
