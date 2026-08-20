use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

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
    let (mut encrypt, mut decrypt) = derive_keys(&secret, &info_hash, true);

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
    let mut vc_probe = decrypt.clone();
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
