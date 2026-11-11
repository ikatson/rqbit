//! BEP 44 items between the rqbit DHT and the libtorrent DHT, both on loopback: what one puts,
//! the other gets, byte for byte.

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, bail};
use bytes::Bytes;
use dht::{DhtConfig, DhtState, Id20};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(60);

/// A libtorrent DHT node on loopback. It learns about the rqbit nodes from the "add_node"
/// command, puts and gets items on command, and reports the alerts of those as events.
const DHT_PY: &str = r#"
def main():
    session = make_session(
        enable_dht=True,
        # Every node of the test is 127.0.0.1, a private address and a single IP.
        dht_restrict_routing_ips=False,
        dht_restrict_search_ips=False,
        dht_ignore_dark_internet=False,
        dht_enforce_node_id=False,
        dht_prefer_verified_node_ids=False,
        # All the rqbit nodes share one IP: do not mistake them for a flood.
        dht_block_ratelimit=100000,
        dht_upload_rate_limit=10000000,
        dht_bootstrap_nodes="",
    )
    emit("listening", port=wait_listen_port(session))
    while True:
        cmd = next_command(timeout=0.1)
        if cmd is not None:
            name = cmd["cmd"]
            if name == "quit":
                return
            elif name == "add_node":
                session.add_dht_node(("127.0.0.1", cmd["port"]))
            elif name == "put_immutable":
                session.dht_put_immutable_item(lt.bdecode(bytes.fromhex(cmd["value"])))
            elif name == "get_immutable":
                session.dht_get_immutable_item(lt.sha1_hash(bytes.fromhex(cmd["target"])))
            elif name == "get_mutable":
                session.dht_get_mutable_item(
                    bytes.fromhex(cmd["key"]), bytes.fromhex(cmd["salt"])
                )
            else:
                raise RuntimeError("unknown command %r" % name)
        for a in pop_alerts(session, 0.1):
            if isinstance(a, lt.dht_put_alert):
                emit("put", target=str(a.target), num_success=a.num_success)
            elif isinstance(a, lt.dht_immutable_item_alert):
                value = None if a.item is None else lt.bencode(a.item).hex()
                emit("immutable_item", target=str(a.target), value=value)
            elif isinstance(a, lt.dht_mutable_item_alert):
                value = None if a.item is None else lt.bencode(a.item).hex()
                emit("mutable_item", seq=a.seq, value=value)


run(main)
"#;

async fn start_node(bootstrap: &[SocketAddr]) -> anyhow::Result<dht::Dht> {
    DhtState::with_config(DhtConfig {
        listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
        bootstrap_addrs: Some(bootstrap.iter().map(|a| a.to_string()).collect()),
        ..Default::default()
    })
    .await
    .context("error starting an rqbit DHT node")
}

/// Sends `command` until libtorrent answers it with an `event` for which `done` holds. Retried
/// because libtorrent only uses the rqbit nodes once it has pinged them.
async fn until_libtorrent(
    lt: &mut Libtorrent,
    command: Value,
    event: &str,
    done: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        lt.send(&command).await?;
        let answer = lt.expect(event, WAIT).await?;
        if done(&answer) {
            return Ok(answer);
        }
        if Instant::now() > deadline {
            bail!("libtorrent never got there, last answer: {answer}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_dht_items_with_libtorrent() -> anyhow::Result<()> {
    let mut lt = Libtorrent::spawn(DHT_PY, &[]).await?;
    let listening = lt.expect("listening", WAIT).await?;
    let lt_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port_of(&listening, "port")?));

    // Two rqbit nodes, so that an item put by either side is held by a node other than the one
    // that gets it. Everybody knows everybody: a bootstrap node only seeds lookups and does not go
    // into the routing table, so the nodes are added explicitly.
    let a = start_node(&[lt_addr]).await?;
    let b = start_node(&[lt_addr]).await?;
    for node in [&a, &b] {
        lt.send(&json!({"cmd": "add_node", "port": node.listen_addr().port()}))
            .await?;
        node.add_node(lt_addr)
            .await
            .context("libtorrent did not answer a ping")?;
    }
    a.add_node(b.listen_addr()).await?;
    b.add_node(a.listen_addr()).await?;

    // libtorrent puts an immutable item, rqbit gets it.
    let value = b"12:Hello World!";
    let put = until_libtorrent(
        &mut lt,
        json!({"cmd": "put_immutable", "value": hex(value)}),
        "put",
        |e| e["num_success"].as_u64().unwrap_or(0) > 0,
    )
    .await?;
    let target: Id20 = put["target"]
        .as_str()
        .context("no target")?
        .parse()
        .context("bad target")?;
    assert_eq!(target, dht::immutable_target(value));
    assert_eq!(a.get_immutable(target).await?.as_deref(), Some(&value[..]));
    assert_eq!(b.get_immutable(target).await?.as_deref(), Some(&value[..]));

    // rqbit puts an immutable item, libtorrent gets it.
    let value = Bytes::from_static(b"d1:ai1e1:b3:xyze");
    let target = b.put_immutable(value.clone()).await?;
    let got = until_libtorrent(
        &mut lt,
        json!({"cmd": "get_immutable", "target": hex(&target.0)}),
        "immutable_item",
        |e| e["value"].is_string(),
    )
    .await?;
    assert_eq!(got["value"], json!(hex(&value)));

    // rqbit puts a mutable item with a salt, libtorrent gets it.
    let key = [5u8; 32];
    let salt = b"salt";
    let value = Bytes::from_static(b"l4:spami42ee");
    b.put_mutable(&key, salt, 7, value.clone(), None).await?;
    let got = until_libtorrent(
        &mut lt,
        json!({
            "cmd": "get_mutable",
            "key": hex(&dht::ed25519_public_key(&key)),
            "salt": hex(salt),
        }),
        "mutable_item",
        |e| e["seq"].as_i64() == Some(7),
    )
    .await?;
    assert_eq!(got["value"], json!(hex(&value)));

    lt.send(&json!({"cmd": "quit"})).await?;
    Ok(())
}
