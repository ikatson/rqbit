const DHT = require('bittorrent-dht')

let dht = new DHT();
let infoHash = process.env["INFOHASH"];

dht.on('peer', function (peer, infoHash, from) {
    console.log(peer.host + ':' + peer.port)
})

dht.lookup(infoHash)