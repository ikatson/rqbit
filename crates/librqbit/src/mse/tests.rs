use super::*;
use crate::vectored_traits::AsyncReadVectoredIntoCompat;
use rc4::KeyInit;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

#[test]
fn default_mse_mode_is_disabled() {
    // The default must stay Disabled so merging the feature is a zero-behavior
    // change; users opt in via SessionOptions::mse_mode.
    assert_eq!(MseMode::default(), MseMode::Disabled);
}

fn handshake(info_hash: [u8; 20], peer_id: [u8; 20]) -> [u8; 68] {
    let mut bytes = [0u8; 68];
    bytes[..20].copy_from_slice(BT_PROTOCOL_PREFIX);
    bytes[28..48].copy_from_slice(&info_hash);
    bytes[48..].copy_from_slice(&peer_id);
    bytes
}

#[tokio::test]
async fn incoming_accepts_zero_length_ia_before_full_handshake() -> Result<()> {
    let info_hash = [0x42; 20];
    let expected_skey_hash = sha1(&[b"req2", &info_hash]);
    let expected_handshake = handshake(info_hash, *b"-RQ0001-012345678901");
    let (client, server) = duplex(4096);
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);

    let responder =
        tokio::spawn(
            async move { incoming(server_read, server_write, move || vec![info_hash]).await },
        );

    let initiator_dh = Dh768::from_secret([0x37; 20]);
    client_write
        .write_all(&initiator_dh.public_key_bytes())
        .await?;
    let mut responder_public = [0u8; 96];
    client_read.read_exact(&mut responder_public).await?;
    let secret = initiator_dh
        .shared_secret(&responder_public)
        .context("responder returned an invalid DH key")?;
    let (mut encrypt, mut decrypt, decrypt_key) = derive_keys(&secret, &info_hash, true);

    client_write.write_all(&sha1(&[b"req1", &secret])).await?;
    let req3 = sha1(&[b"req3", &secret]);
    client_write
        .write_all(&xor20(&expected_skey_hash, &req3))
        .await?;

    let mut pe3 = Vec::new();
    pe3.extend_from_slice(&[0u8; VC_LEN]);
    pe3.extend_from_slice(&CRYPTO_RC4.to_be_bytes());
    pe3.extend_from_slice(&0u16.to_be_bytes());
    pe3.extend_from_slice(&0u16.to_be_bytes());
    encrypt.apply_keystream(&mut pe3);
    client_write.write_all(&pe3).await?;

    let mut encrypted_vc = [0u8; VC_LEN];
    // The initiator's decrypt stream and the responder's VC encrypt stream are
    // the same keystream; rebuild an independent instance (rc4 crate has no
    // Clone) to compute the encrypted-VC pattern for the needle scan.
    let mut vc_probe = ::rc4::Rc4::new_from_slice(&decrypt_key).expect("20-byte RC4 key");
    let mut drop = [0u8; 1024];
    vc_probe.apply_keystream(&mut drop);
    vc_probe.apply_keystream(&mut encrypted_vc);
    read_scan_for_needle(&mut client_read, &encrypted_vc, MAX_PAD).await?;
    decrypt.apply_keystream(&mut encrypted_vc);
    assert_eq!(encrypted_vc, [0u8; VC_LEN]);

    let mut crypto_select = [0u8; 4];
    client_read.read_exact(&mut crypto_select).await?;
    decrypt.apply_keystream(&mut crypto_select);
    assert_eq!(u32::from_be_bytes(crypto_select), CRYPTO_RC4);

    let mut pad_d_length = [0u8; 2];
    client_read.read_exact(&mut pad_d_length).await?;
    decrypt.apply_keystream(&mut pad_d_length);
    let mut pad_d = vec![0u8; u16::from_be_bytes(pad_d_length) as usize];
    client_read.read_exact(&mut pad_d).await?;
    decrypt.apply_keystream(&mut pad_d);

    let mut encrypted_handshake = expected_handshake;
    encrypt.apply_keystream(&mut encrypted_handshake);
    client_write.write_all(&encrypted_handshake).await?;
    let payload = b"post-handshake payload";
    let mut encrypted_payload = *payload;
    encrypt.apply_keystream(&mut encrypted_payload);
    client_write.write_all(&encrypted_payload).await?;

    let outcome = responder.await??;
    match outcome {
        IncomingOutcome::Encrypted {
            mut read,
            handshake_bytes,
            info_hash: resolved_info_hash,
            ..
        } => {
            assert_eq!(resolved_info_hash, info_hash);
            assert_eq!(handshake_bytes, expected_handshake);
            let mut received_payload = [0u8; 22];
            read.read_exact(&mut received_payload).await?;
            assert_eq!(&received_payload, payload);
        }
        IncomingOutcome::Plaintext { .. } => bail!("expected encrypted outcome"),
    }
    Ok(())
}

#[tokio::test]
async fn fragmented_plaintext_prefix_is_replayed() -> Result<()> {
    let info_hash = [0x23; 20];
    let bytes = handshake(info_hash, [0x45; 20]);
    let (client, server) = duplex(256);
    let (server_read, server_write) = tokio::io::split(server);
    let sender = async move {
        let mut client = client;
        for byte in bytes {
            client.write_all(&[byte]).await?;
            tokio::task::yield_now().await;
        }
        Ok::<_, std::io::Error>(())
    };
    let receiver = async move {
        let outcome = incoming(server_read, server_write, Vec::new).await?;
        let mut read = match outcome {
            IncomingOutcome::Plaintext { read, .. } => read,
            IncomingOutcome::Encrypted { .. } => bail!("unexpected encrypted outcome"),
        };
        let mut replayed = [0u8; 68];
        read.read_exact(&mut replayed).await?;
        assert_eq!(replayed, bytes);
        Ok::<_, anyhow::Error>(())
    };
    let (sent, received) = tokio::join!(sender, receiver);
    sent?;
    received?;
    Ok(())
}
#[tokio::test]
async fn duplex_handshake_preserves_payload() -> Result<()> {
    let info_hash = [0x42; 20];
    let initial = handshake(info_hash, [0x11; 20]);
    let (client, server) = duplex(8192);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);

    let initiator = outgoing(client_read, client_write, &info_hash, &initial);
    let responder = incoming(server_read, server_write, move || vec![info_hash]);
    let (initiator_result, responder_result) = tokio::join!(initiator, responder);
    let (mut client_read, mut client_write) = match initiator_result? {
        OutgoingOutcome::Encrypted(r, w) => (r, w),
        OutgoingOutcome::PlaintextPeer => bail!("unexpected plaintext peer"),
    };
    let outcome = responder_result?;
    let (mut server_read, mut server_write, received) = match outcome {
        IncomingOutcome::Encrypted {
            read,
            write,
            handshake_bytes,
            ..
        } => (read, write, handshake_bytes),
        IncomingOutcome::Plaintext { .. } => bail!("unexpected plaintext outcome"),
    };
    assert_eq!(received, initial);

    client_write.write_all(b"client payload").await?;
    let mut client_payload = [0u8; 14];
    server_read.read_exact(&mut client_payload).await?;
    assert_eq!(&client_payload, b"client payload");

    server_write.write_all(b"server payload").await?;
    let mut server_payload = [0u8; 14];
    client_read.read_exact(&mut server_payload).await?;
    assert_eq!(&server_payload, b"server payload");
    Ok(())
}

#[tokio::test]
async fn plaintext_first_response_triggers_immediate_fallback() -> Result<()> {
    // A plaintext peer answers our Ya + PadA with its 68-byte BT handshake
    // immediately. `outgoing` must detect the `\x13BitTorrent protocol`
    // prefix and return `PlaintextPeer` well within the 10s read timeout
    // (2s sniff window is the ceiling here).
    let info_hash = [0x42; 20];
    let (client, server) = duplex(8192);
    let (client_read, client_write) = tokio::io::split(client);
    let (mut server_read, mut server_write) = tokio::io::split(server);
    let responder = async move {
        // Read and discard Ya + PadA.
        let mut discard = [0u8; 96];
        server_read.read_exact(&mut discard).await?;
        // Reply with a plaintext BT handshake.
        server_write
            .write_all(&handshake(info_hash, [0x55; 20]))
            .await?;
        Ok::<_, std::io::Error>(())
    };

    let initial = handshake(info_hash, [0x11; 20]);
    let initiator = async {
        let started = std::time::Instant::now();
        let outcome = outgoing(client_read, client_write, &info_hash, &initial).await?;
        let elapsed = started.elapsed();
        match outcome {
            OutgoingOutcome::PlaintextPeer => {
                assert!(
                    elapsed < PLAINTEXT_SNIFF_TIMEOUT + Duration::from_millis(500),
                    "plaintext fallback took {elapsed:?}, expected <= {PLAINTEXT_SNIFF_TIMEOUT:?}"
                );
                Ok::<_, anyhow::Error>(())
            }
            OutgoingOutcome::Encrypted(..) => bail!("expected plaintext peer fallback"),
        }
    };

    let (init, resp) = tokio::join!(initiator, responder);
    init?;
    resp?;
    Ok(())
}

#[tokio::test]
async fn mse_responder_still_works_after_sniff() -> Result<()> {
    // The sniff reads the first 20 bytes; an MSE responder's public key
    // must still arrive intact when it does not match the BT prefix.
    let info_hash = [0x42; 20];
    let initial = handshake(info_hash, [0x11; 20]);
    let (client, server) = duplex(8192);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);

    let initiator = outgoing(client_read, client_write, &info_hash, &initial);
    let responder = incoming(server_read, server_write, move || vec![info_hash]);
    let (initiator_result, responder_result) = tokio::join!(initiator, responder);
    let (mut client_read, mut client_write) = match initiator_result? {
        OutgoingOutcome::Encrypted(r, w) => (r, w),
        OutgoingOutcome::PlaintextPeer => bail!("unexpected plaintext peer"),
    };
    let (mut server_read, mut server_write, received) = match responder_result? {
        IncomingOutcome::Encrypted {
            read,
            write,
            handshake_bytes,
            ..
        } => (read, write, handshake_bytes),
        IncomingOutcome::Plaintext { .. } => bail!("unexpected plaintext outcome"),
    };
    assert_eq!(received, initial);
    client_write.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    server_read.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ping");
    server_write.write_all(b"pong").await?;
    client_read.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"pong");
    Ok(())
}

#[tokio::test]
async fn padb_without_vc_pattern_aborts_handshake() -> Result<()> {
    // If the responder never sends the encrypted VC within MAX_PAD bytes, the
    // PadB pattern-search must fail the handshake rather than hang or accept
    // a corrupted stream. This exercises the search bound + the formal VC
    // verification added for pattern misdetection.
    let info_hash = [0x42; 20];
    let (client, server) = duplex(4096);
    let (client_read, client_write) = tokio::io::split(client);
    let (mut server_read, mut server_write) = tokio::io::split(server);

    let responder = async move {
        // Read and discard Ya + PadA.
        let mut ya = [0u8; 96];
        server_read.read_exact(&mut ya).await?;
        // Send a fixed public key (0x01..) and a large PadB with no VC pattern.
        let yb = [0x01u8; 96];
        server_write.write_all(&yb).await?;
        let padb = vec![0x11u8; MAX_PAD + VC_LEN];
        server_write.write_all(&padb).await?;
        Ok::<_, std::io::Error>(())
    };

    let initial = handshake(info_hash, [0x11; 20]);
    let initiator = outgoing(client_read, client_write, &info_hash, &initial);
    let (initiator_result, responder_result) = tokio::join!(initiator, responder);
    responder_result?;
    assert!(
        initiator_result.is_err(),
        "outgoing should fail when VC is not found within PadB"
    );
    Ok(())
}

#[tokio::test]
async fn plaintext_prefix_stall_times_out() -> Result<()> {
    // A peer that sends only the 20-byte BT prefix and then stalls must not
    // wedge the acceptor: reading the rest of the handshake has to be bounded
    // by rwtimeout (the listener caps in-flight handshake checks at 256, so an
    // unbounded read would let a handful of peers starve all incoming slots).
    let (mut peer_side, server_side) = duplex(64);
    let (server_read, server_write) = tokio::io::split(server_side);
    peer_side.write_all(BT_PROTOCOL_PREFIX).await?;
    // Never send the remaining 48 bytes; keep the socket open.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        crate::stream_connect::accept_with_handshake(
            "127.0.0.1:1".parse()?,
            Box::new(server_read.into_vectored_compat()),
            Box::new(server_write),
            Duration::from_millis(200),
            MseMode::Enabled,
            || vec![info_hash_any()],
        ),
    )
    .await;
    // Bounded by the 200ms rwtimeout: the read of the remaining 48 bytes must
    // give up rather than hang until the 2s outer timeout.
    match result {
        Ok(Err(e)) => assert!(
            format!("{e:#}").contains("timeout"),
            "expected a read timeout, got: {e:#}"
        ),
        Ok(Ok(_)) => panic!("accept_with_handshake accepted a stalled plaintext peer"),
        Err(_) => panic!("handshake was not bounded by rwtimeout (outer 2s timeout hit)"),
    }
    Ok(())
}

fn info_hash_any() -> [u8; 20] {
    [0x42; 20]
}

#[test]
fn key_derivation_matches_independent_constants() {
    // External anchoring: every constant below was computed outside this
    // crate (Python `hashlib.sha1` + an independent RC4 implementation that was
    // itself checked against the public "Wiki"/"pedia" -> 1021BF0420 vector),
    // so this pins the key derivation, the keyA/keyB direction and the
    // drop-1024 position against something other than our own code.
    //
    // S = the shared secret of the dh768 external vector (also verified with
    // Python `pow()` in `dh768::tests::matches_external_bigint_vectors`).
    let secret: [u8; 96] = hex::decode(concat!(
        "909ea4557d5b9f43dafdc5b598850045b8689e4d652af58a63730b00c574bbe4",
        "962ab9c78b2f295e3ddb3b456f20a4c65761751bf5d79ec4dba8470fe66ed22b",
        "4a25f13528a9575607c77586785a36d560f8556b66e9c16deb87fed185ee07a7"
    ))
    .unwrap()
    .try_into()
    .unwrap();
    // SKEY = 0x00..0x13. Any 20 bytes work here; this one is fixed so the
    // expected constants below are reproducible.
    let skey: [u8; 20] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13,
    ];

    // sha1("keyA"||S||skey) / sha1("keyB"||S||skey):
    const KEY_A: [u8; 20] = [
        0x47, 0xac, 0x62, 0x2c, 0x53, 0xf6, 0xf6, 0x19, 0x40, 0xa6, 0x12, 0x9c, 0x3d, 0x4b, 0xf6,
        0x84, 0xa8, 0xae, 0xcd, 0xaf,
    ];
    const KEY_B: [u8; 20] = [
        0xd6, 0xb2, 0x18, 0x2e, 0x76, 0xdd, 0x9d, 0x93, 0x13, 0x95, 0x03, 0x19, 0x42, 0xc8, 0xcb,
        0x64, 0x63, 0xb5, 0xfe, 0x69,
    ];
    // RC4(KEY_A) keystream after dropping 1024 bytes (the initiator's encrypt
    // stream, i.e. what the responder decrypts):
    const KEYSTREAM_A: [u8; 32] = [
        0xcb, 0x96, 0x31, 0xa6, 0xba, 0xe9, 0x1d, 0x67, 0x4e, 0xbd, 0xa5, 0x5a, 0x52, 0xbc, 0xee,
        0x0a, 0x02, 0xf0, 0xe1, 0xe4, 0xb4, 0xba, 0x3c, 0x4a, 0xc7, 0x24, 0x37, 0x8d, 0xee, 0xac,
        0xb9, 0xf1,
    ];
    // RC4(KEY_B) first 8 keystream bytes after drop-1024 = the encrypted VC
    // pattern that the PadB scan searches for:
    const VC_PATTERN: [u8; 8] = [0x1e, 0x5c, 0xbf, 0xd1, 0x5f, 0xac, 0x9d, 0x20];

    // Initiator: encrypt = RC4(keyA), decrypt = RC4(keyB).
    let (mut encrypt, mut decrypt, decrypt_key) = derive_keys(&secret, &skey, true);
    assert_eq!(
        decrypt_key, KEY_B,
        "initiator decrypt key = sha1(keyB, S, SKEY)"
    );
    let mut buf = [0u8; 32];
    encrypt.apply_keystream(&mut buf);
    assert_eq!(
        buf, KEYSTREAM_A,
        "initiator encrypt keystream after drop-1024"
    );
    let mut vc = [0u8; 8];
    decrypt.apply_keystream(&mut vc);
    assert_eq!(vc, VC_PATTERN, "encrypted VC pattern searched within PadB");

    // Responder: the directions must be swapped.
    let (mut encrypt_r, mut decrypt_r, dec_key_r) = derive_keys(&secret, &skey, false);
    assert_eq!(
        dec_key_r, KEY_A,
        "responder decrypt key = sha1(keyA, S, SKEY)"
    );
    let mut buf_r = [0u8; 8];
    encrypt_r.apply_keystream(&mut buf_r);
    assert_eq!(
        buf_r, VC_PATTERN,
        "responder encrypt stream = RC4(keyB) after drop-1024"
    );
    let mut buf_r32 = [0u8; 32];
    decrypt_r.apply_keystream(&mut buf_r32);
    assert_eq!(
        buf_r32, KEYSTREAM_A,
        "responder decrypt stream = RC4(keyA) after drop-1024"
    );
}
