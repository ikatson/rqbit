use std::net::SocketAddr;

pub type BF = bitvec::vec::BitVec<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;

mod ratelimit {
    use governor::{
        clock::DefaultClock,
        middleware::NoOpMiddleware,
        state::{InMemoryState, NotKeyed},
        RateLimiter,
    };

    pub type RateLimit = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;
}

pub type RateLimit = ratelimit::RateLimit;
