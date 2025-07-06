use librqbit_tracker_comms::{TorrentStatsProvider, TrackerComms, UdpTrackerClient};

struct Dummy;

impl TorrentStatsProvider for Dummy {
    fn get(&self) -> librqbit_tracker_comms::TrackerCommsStats {
        todo!()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    TrackerComms::start(
        Default::default(),
        Default::default(),
        std::hint::black_box(Default::default()),
        std::hint::black_box(Box::new(Dummy)),
        None,
        4240,
        reqwest::Client::new(),
        UdpTrackerClient::new(Default::default(), None)
            .await
            .unwrap(),
    );
}
