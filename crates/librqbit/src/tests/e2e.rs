#[tokio::test]
async fn test_e2e() {
    // 1. Create a torrent
    // Ideally (for a more complicated test) with N files, and at least N pieces that span 2 files.

    // 2. Start N servers that are serving that torrent, and return their IP:port combos.
    //    Disable DHT on each.

    // 3. Start a client with the initial peers, and download the file.

    // 4. After downloading, recheck its integrity.
}
