use rc4::KeyInit;
use super::*;
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

    let responder = tokio::spawn(async move {
        incoming(server_read, server_write, move |skey_hash| {
            (*skey_hash == expected_skey_hash).then_some(info_hash)
        })
        .await
    });

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
        let outcome = incoming(server_read, server_write, |_| None).await?;
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
    let responder = incoming(server_read, server_write, |candidate| {
        (candidate == &sha1(&[b"req2", &info_hash])).then_some(info_hash)
    });
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
    let responder = incoming(server_read, server_write, |candidate| {
        (candidate == &sha1(&[b"req2", &info_hash])).then_some(info_hash)
    });
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
