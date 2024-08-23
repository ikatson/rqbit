use parking_lot::RwLock;
use std::collections::HashMap;

struct Subscription {
    url: url::Url,
}

pub struct Subscriptions {
    subs: RwLock<HashMap<String, Subscription>>,
}

impl Subscriptions {
    pub fn add(&self, url: url::Url) -> String {
        let sid = format!("uuid:{}", uuid::Uuid::new_v4());
        self.subs.write().insert(sid.clone(), Subscription { url });
        sid
    }
}
