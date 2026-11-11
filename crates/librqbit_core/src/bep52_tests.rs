//! BEP 52 metainfo tests: the fixtures made with libtorrent 2.1.1 (see
//! `resources/test/bep52/make_fixtures.py`) and hand-built malformed torrents.

use std::collections::BTreeMap;

use buffers::ByteBuf;

use crate::{
    Error, Id32,
    lengths::ValidPieceIndex,
    merkle,
    metainfo_v2::{MAX_FILE_TREE_DEPTH, TorrentVersion},
    torrent_metainfo::{TorrentMetaV1, ValidatedTorrentMetaV1Info, torrent_from_bytes},
};

/// The content of a fixture file, as `content()` in make_fixtures.py: the top
/// bytes of a 32-bit LCG seeded with the FNV-1a hash of the "/"-separated path
/// relative to the torrent root.
pub(crate) fn fixture_content(path: &str, length: u64) -> Vec<u8> {
    let mut state: u32 = 0x811C9DC5;
    for b in path.as_bytes() {
        state = (state ^ u32::from(*b)).wrapping_mul(0x01000193);
    }
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        })
        .collect()
}

struct Fixture {
    name: &'static str,
    torrent: &'static [u8],
    json: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "v2_single",
        torrent: include_bytes!("resources/test/bep52/v2_single.torrent"),
        json: include_str!("resources/test/bep52/v2_single.json"),
    },
    Fixture {
        name: "v2_multi",
        torrent: include_bytes!("resources/test/bep52/v2_multi.torrent"),
        json: include_str!("resources/test/bep52/v2_multi.json"),
    },
    Fixture {
        name: "hybrid_single",
        torrent: include_bytes!("resources/test/bep52/hybrid_single.torrent"),
        json: include_str!("resources/test/bep52/hybrid_single.json"),
    },
    Fixture {
        name: "hybrid_multi",
        torrent: include_bytes!("resources/test/bep52/hybrid_multi.torrent"),
        json: include_str!("resources/test/bep52/hybrid_multi.json"),
    },
];

fn file_path<B: AsRef<[u8]>>(info: &ValidatedTorrentMetaV1Info<B>, idx: usize) -> String {
    info.iter_file_details()
        .nth(idx)
        .map(|d| d.filename.to_vec().join("/"))
        .unwrap()
}

/// The content of every file of the torrent, by file index (zeros for
/// padding files).
fn contents<B: AsRef<[u8]>>(info: &ValidatedTorrentMetaV1Info<B>) -> Vec<Vec<u8>> {
    info.iter_file_details()
        .map(|d| {
            if d.attrs().padding {
                vec![0u8; d.len as usize]
            } else {
                fixture_content(&d.filename.to_vec().join("/"), d.len)
            }
        })
        .collect()
}

/// The bytes of a piece as sent on the wire, assembled from the files.
pub(crate) fn piece_bytes<B: AsRef<[u8]>>(
    info: &ValidatedTorrentMetaV1Info<B>,
    contents: &[Vec<u8>],
    piece: ValidPieceIndex,
) -> Vec<u8> {
    let l = info.lengths();
    let start = l.piece_offset(piece);
    let end = start + u64::from(l.piece_length(piece));
    let mut out = Vec::new();
    for (fd, content) in info.iter_file_details_ext().zip(contents) {
        let f_start = fd.offset;
        let f_end = fd.offset + fd.details.len;
        let s = start.max(f_start);
        let e = end.min(f_end);
        if s < e {
            out.extend_from_slice(&content[(s - f_start) as usize..(e - f_start) as usize]);
        }
    }
    assert_eq!(out.len() as u64, end - start, "piece {piece} has holes");
    out
}

fn verify(
    info: &ValidatedTorrentMetaV1Info<ByteBuf<'_>>,
    piece: ValidPieceIndex,
    data: &[u8],
) -> bool {
    let mut v = info.piece_verifier(piece).unwrap();
    // Feed in odd-sized slices to exercise the streaming.
    for part in data.chunks(1000) {
        v.update(part);
    }
    v.finish()
}

fn check_fixture(fx: &Fixture) {
    let expected: serde_json::Value = serde_json::from_str(fx.json).unwrap();
    let torrent: TorrentMetaV1<ByteBuf> = torrent_from_bytes(fx.torrent).unwrap();

    let v2 = torrent.info_hash_v2.expect("v2 hash");
    assert_eq!(v2.as_string(), expected["info_hash_v2"].as_str().unwrap());
    let version = match expected["info_hash_v1"].as_str() {
        Some(v1) => {
            assert_eq!(torrent.info_hash.as_string(), v1);
            TorrentVersion::Hybrid
        }
        None => {
            assert_eq!(torrent.info_hash, v2.truncate_for_dht());
            TorrentVersion::V2
        }
    };

    let info = torrent.validate_with_piece_layers().unwrap();
    assert_eq!(info.version(), version, "{}", fx.name);
    assert!(info.has_all_piece_layers());
    let l = info.lengths();
    assert_eq!(
        u64::from(l.default_piece_length()),
        expected["piece_length"].as_u64().unwrap()
    );
    assert_eq!(
        u64::from(l.total_pieces()),
        expected["num_pieces"].as_u64().unwrap(),
        "{}",
        fx.name
    );

    // libtorrent pads v2-only torrents internally too; those pads are not
    // part of the v2 torrent, so compare the real files only there.
    let theirs: Vec<(String, u64, u64, bool)> = expected["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_owned(),
                f["size"].as_u64().unwrap(),
                f["offset"].as_u64().unwrap(),
                f["pad"].as_bool().unwrap(),
            )
        })
        .filter(|f| version == TorrentVersion::Hybrid || !f.3)
        .collect();
    let ours: Vec<(String, u64, u64, bool)> = info
        .iter_file_details_ext()
        .enumerate()
        .map(|(idx, fd)| {
            (
                file_path(&info, idx),
                fd.details.len,
                fd.offset,
                fd.details.attrs().padding,
            )
        })
        .collect();
    assert_eq!(ours, theirs, "{}", fx.name);
    let real_bytes: u64 = theirs.iter().filter(|f| !f.3).map(|f| f.1).sum();
    match version {
        TorrentVersion::V2 => assert_eq!(l.total_length(), real_bytes),
        _ => assert_eq!(l.total_length(), expected["total_size"].as_u64().unwrap()),
    }

    // Every piece verifies with the real content, and fails with a flipped
    // byte at either end.
    let contents = contents(&info);
    for piece in 0..l.total_pieces() {
        let piece = l.validate_piece_index(piece).unwrap();
        let v = info.piece_verifier(piece).unwrap();
        assert_eq!(v.checks_v1(), version == TorrentVersion::Hybrid);
        assert!(v.checks_v2());
        let mut data = piece_bytes(&info, &contents, piece);
        assert!(verify(&info, piece, &data), "{} piece {piece}", fx.name);
        for pos in [0, data.len() - 1] {
            data[pos] ^= 1;
            assert!(!verify(&info, piece, &data), "{} piece {piece}", fx.name);
            data[pos] ^= 1;
        }
        // Too short, too long.
        assert!(!verify(&info, piece, &data[..data.len() - 1]));
        let mut longer = data.clone();
        longer.push(0);
        assert!(!verify(&info, piece, &longer));
    }

    // pieces root and piece layers recomputed from the content.
    let pl = l.default_piece_length();
    for (pos, f) in info.v2_files().iter().enumerate() {
        let content = &contents[f.file_index];
        assert_eq!(
            merkle::pieces_root_from_data(content),
            Some(f.pieces_root),
            "{} {}",
            fx.name,
            file_path(&info, f.file_index)
        );
        if f.has_piece_layer(pl) {
            let layer = merkle::piece_layer_from_data(content, pl).unwrap();
            assert_eq!(info.piece_layer(pos), Some(&layer[..]));
            assert_eq!(
                merkle::pieces_root_from_piece_layer(&layer, pl),
                Some(f.pieces_root)
            );
        } else {
            assert!(info.piece_layer(pos).is_none());
        }
    }
}

#[test]
fn test_libtorrent_fixtures() {
    for fx in FIXTURES {
        check_fixture(fx);
    }
}

#[test]
fn test_fixture_reserialize_keeps_info_hash() {
    // Serializing the parsed info dictionary again must give the same bytes,
    // so a torrent rebuilt from its parts keeps its identity.
    for fx in FIXTURES {
        let torrent: TorrentMetaV1<ByteBuf> = torrent_from_bytes(fx.torrent).unwrap();
        let mut out = Vec::new();
        bencode::bencode_serialize_to_writer(&torrent.info.data, &mut out).unwrap();
        assert_eq!(out, torrent.info.raw_bytes.as_ref(), "{}", fx.name);
    }
}

#[test]
fn test_fixture_known_piece_layers_roundtrip() {
    for fx in FIXTURES {
        let torrent: TorrentMetaV1<ByteBuf> = torrent_from_bytes(fx.torrent).unwrap();
        let info = torrent.validate_with_piece_layers().unwrap();
        let known = info.known_piece_layers();
        let theirs = torrent.piece_layers.as_ref().unwrap();
        let mut theirs: Vec<(Vec<u8>, Vec<u8>)> = theirs
            .0
            .iter()
            .map(|(k, v)| (k.as_ref().to_vec(), v.as_ref().to_vec()))
            .collect();
        theirs.sort();
        let ours: Vec<(Vec<u8>, Vec<u8>)> = known
            .0
            .iter()
            .map(|(k, v)| (k.as_ref().to_vec(), v.as_ref().to_vec()))
            .collect();
        assert_eq!(ours, theirs, "{}", fx.name);

        // The metadata alone (a magnet) knows no layers; loading them back
        // from the rebuilt dictionary completes it.
        let bare = torrent.info.data.clone().validate().unwrap();
        assert!(!bare.has_all_piece_layers());
        bare.load_piece_layers(&known).unwrap();
        assert!(bare.has_all_piece_layers());
    }
}

/// A bencode value, to build and then break torrents by hand. Dictionaries
/// are kept sorted.
#[derive(Clone, Debug)]
enum B {
    I(i64),
    S(Vec<u8>),
    L(Vec<B>),
    D(BTreeMap<Vec<u8>, B>),
}

impl B {
    fn s(v: impl AsRef<[u8]>) -> B {
        B::S(v.as_ref().to_vec())
    }

    fn d() -> B {
        B::D(BTreeMap::new())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            B::I(i) => out.extend_from_slice(format!("i{i}e").as_bytes()),
            B::S(s) => {
                out.extend_from_slice(format!("{}:", s.len()).as_bytes());
                out.extend_from_slice(s);
            }
            B::L(l) => {
                out.push(b'l');
                l.iter().for_each(|v| v.encode(out));
                out.push(b'e');
            }
            B::D(d) => {
                out.push(b'd');
                for (k, v) in d {
                    B::S(k.clone()).encode(out);
                    v.encode(out);
                }
                out.push(b'e');
            }
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    fn map(&mut self) -> &mut BTreeMap<Vec<u8>, B> {
        match self {
            B::D(d) => d,
            other => panic!("not a dict: {other:?}"),
        }
    }

    fn at(&mut self, key: &str) -> &mut B {
        self.map().get_mut(key.as_bytes()).unwrap()
    }

    fn set(&mut self, key: &str, v: B) {
        self.map().insert(key.as_bytes().to_vec(), v);
    }

    fn remove(&mut self, key: &str) -> Option<B> {
        self.map().remove(key.as_bytes())
    }
}

/// A v2 (or hybrid) torrent over files `(path, length)` with content from
/// [`fixture_content`], built the way libtorrent builds them: files in
/// sorted path order, every file but the last followed by a padding file in
/// the v1 list when it does not end on a piece boundary.
fn build_torrent(files: &[(&str, u64)], piece_length: u32, hybrid: bool) -> B {
    let mut files: Vec<(&str, u64)> = files.to_vec();
    files.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let pl = u64::from(piece_length);

    let mut tree = B::d();
    let mut layers = B::d();
    let mut v1_files = Vec::new();
    let mut v1_stream = Vec::new();
    for (idx, (path, len)) in files.iter().enumerate() {
        let content = fixture_content(path, *len);
        let mut props = B::d();
        props.set("length", B::I(*len as i64));
        if *len > 0 {
            let root = merkle::pieces_root_from_data(&content).unwrap();
            props.set("pieces root", B::s(root.0));
            if *len > pl {
                let layer = merkle::piece_layer_from_data(&content, piece_length).unwrap();
                let raw: Vec<u8> = layer.iter().flat_map(|h| h.0).collect();
                layers.set_raw(root.0.to_vec(), B::S(raw));
            }
        }
        let mut node = &mut tree;
        for component in path.split('/') {
            node = node
                .map()
                .entry(component.as_bytes().to_vec())
                .or_insert_with(B::d);
        }
        node.set("", props);

        let mut f = B::d();
        f.set("length", B::I(*len as i64));
        f.set("path", B::L(path.split('/').map(B::s).collect()));
        v1_files.push(f);
        v1_stream.extend_from_slice(&content);
        let pad = (pl - *len % pl) % pl;
        if pad > 0 && idx + 1 < files.len() {
            let mut f = B::d();
            f.set("attr", B::s("p"));
            f.set("length", B::I(pad as i64));
            f.set("path", B::L(vec![B::s(".pad"), B::s(pad.to_string())]));
            v1_files.push(f);
            v1_stream.resize(v1_stream.len() + pad as usize, 0);
        }
    }

    let mut info = B::d();
    info.set("name", B::s("test"));
    info.set("piece length", B::I(i64::from(piece_length)));
    info.set("meta version", B::I(2));
    info.set("file tree", tree);
    if hybrid {
        use sha1w::ISha1;
        let pieces: Vec<u8> = v1_stream
            .chunks(piece_length as usize)
            .flat_map(|p| {
                let mut h = sha1w::Sha1::new();
                h.update(p);
                h.finish()
            })
            .collect();
        info.set("files", B::L(v1_files));
        info.set("pieces", B::S(pieces));
    }
    let mut torrent = B::d();
    torrent.set("info", info);
    torrent.set("piece layers", layers);
    torrent
}

impl B {
    fn set_raw(&mut self, key: Vec<u8>, v: B) {
        self.map().insert(key, v);
    }
}

fn validate(t: &B) -> crate::Result<TorrentVersion> {
    let bytes = t.to_bytes();
    let torrent: TorrentMetaV1<ByteBuf> =
        torrent_from_bytes(&bytes).map_err(|e| panic!("does not parse: {e:#}"))?;
    let info = torrent.validate_with_piece_layers()?;
    Ok(info.version())
}

const FILES: &[(&str, u64)] = &[
    ("big.bin", 200_000),
    ("dir/small.bin", 1_000),
    ("dir/sub/exact.bin", 2 * 16384),
    ("empty", 0),
];

#[test]
fn test_built_torrents_are_valid() {
    assert_eq!(
        validate(&build_torrent(FILES, 16384, false)).unwrap(),
        TorrentVersion::V2
    );
    assert_eq!(
        validate(&build_torrent(FILES, 16384, true)).unwrap(),
        TorrentVersion::Hybrid
    );
    assert_eq!(
        validate(&build_torrent(FILES, 65536, true)).unwrap(),
        TorrentVersion::Hybrid
    );
}

/// The `piece layers` entry of the file at `path`.
fn layer_of<'a>(t: &'a mut B, path: &str) -> &'a mut Vec<u8> {
    let mut node = t.at("info").at("file tree");
    for component in path.split('/') {
        node = node.at(component);
    }
    let root = match node.at("").at("pieces root") {
        B::S(s) => s.clone(),
        _ => unreachable!(),
    };
    match t.at("piece layers").map().get_mut(&root).unwrap() {
        B::S(s) => s,
        _ => unreachable!(),
    }
}

#[test]
fn test_reject_wrong_layer_length() {
    for hybrid in [false, true] {
        let mut t = build_torrent(FILES, 16384, hybrid);
        layer_of(&mut t, "big.bin").truncate(32 * 3);
        assert!(matches!(
            validate(&t),
            Err(Error::V2PieceLayersWrongSize { expected, actual: 96 }) if expected == 13 * 32
        ));

        let mut t = build_torrent(FILES, 16384, hybrid);
        layer_of(&mut t, "dir/sub/exact.bin").push(0);
        assert!(matches!(
            validate(&t),
            Err(Error::V2PieceLayersWrongSize { .. })
        ));
    }
}

#[test]
fn test_reject_layer_not_matching_root() {
    let mut t = build_torrent(FILES, 16384, false);
    layer_of(&mut t, "big.bin")[40] ^= 1;
    assert!(matches!(
        validate(&t),
        Err(Error::V2PieceLayersRootMismatch)
    ));
}

#[test]
fn test_reject_missing_layer() {
    let mut t = build_torrent(FILES, 16384, false);
    t.at("piece layers").map().clear();
    assert!(matches!(
        validate(&t),
        Err(Error::V2MissingPieceLayersEntry(name)) if name == "big.bin"
    ));
    t.remove("piece layers");
    assert!(matches!(validate(&t), Err(Error::V2MissingPieceLayers)));

    // Layers of unknown files are ignored.
    let mut t = build_torrent(FILES, 16384, false);
    t.at("piece layers")
        .set_raw(vec![7u8; 32], B::S(vec![0u8; 64]));
    validate(&t).unwrap();
}

#[test]
fn test_reject_bad_piece_length() {
    for (pl, hybrid) in [
        (8192, false),
        (16384 * 3, false),
        (16384 * 3, true),
        (0, false),
    ] {
        let mut t = build_torrent(FILES, 16384, hybrid);
        t.at("info").set("piece length", B::I(pl));
        let res = validate(&t);
        assert!(
            matches!(res, Err(Error::V2InvalidPieceLength(p)) if i64::from(p) == pl),
            "{pl}: {res:?}"
        );
    }
}

#[test]
fn test_reject_unsupported_meta_version() {
    let mut t = build_torrent(FILES, 16384, false);
    t.at("info").set("meta version", B::I(3));
    assert!(matches!(
        validate(&t),
        Err(Error::V2UnsupportedMetaVersion(3))
    ));
}

#[test]
fn test_reject_hybrid_mismatch() {
    // Different length in the v1 list.
    let mut t = build_torrent(FILES, 16384, true);
    match t.at("info").at("files") {
        B::L(files) => files[0].set("length", B::I(200_001)),
        _ => unreachable!(),
    }
    assert!(matches!(validate(&t), Err(Error::V2HybridFileListMismatch(n)) if n == "big.bin"));

    // Different path.
    let mut t = build_torrent(FILES, 16384, true);
    match t.at("info").at("files") {
        B::L(files) => files[0].set("path", B::L(vec![B::s("other.bin")])),
        _ => unreachable!(),
    }
    assert!(matches!(
        validate(&t),
        Err(Error::V2HybridFileListMismatch(_))
    ));

    // A file missing from the v1 list.
    let mut t = build_torrent(FILES, 16384, true);
    match t.at("info").at("files") {
        B::L(files) => {
            files.retain(|f| !matches!(f.clone().at("length"), B::I(32768)));
        }
        _ => unreachable!(),
    }
    assert!(matches!(
        validate(&t),
        Err(Error::V2HybridFileListMismatch(_))
    ));

    // A padding file removed: the next file no longer starts on a piece
    // boundary.
    let mut t = build_torrent(FILES, 16384, true);
    match t.at("info").at("files") {
        B::L(files) => files.remove(1),
        _ => unreachable!(),
    };
    assert!(
        matches!(validate(&t), Err(Error::V2HybridFileNotAligned(n)) if n.ends_with("small.bin"))
    );
}

#[test]
fn test_reject_bad_trees() {
    // Too deep.
    let mut path = vec!["d"; MAX_FILE_TREE_DEPTH];
    path.push("f");
    let deep = path.join("/");
    let t = build_torrent(&[(&deep, 10)], 16384, false);
    assert!(matches!(
        validate(&t),
        Err(Error::V2FileTreeTooDeep(MAX_FILE_TREE_DEPTH))
    ));
    // Just deep enough.
    let deep = path[1..].join("/");
    validate(&build_torrent(&[(&deep, 10)], 16384, false)).unwrap();

    // A node that is both a file and a directory.
    let mut t = build_torrent(&[("a/b", 10)], 16384, false);
    let file = t.at("info").at("file tree").at("a").at("b").at("").clone();
    t.at("info").at("file tree").at("a").set("", file);
    assert!(matches!(validate(&t), Err(Error::V2FileTreeMixedNode)));

    // The root is a file.
    let mut t = build_torrent(&[("a", 10)], 16384, false);
    let file = t.at("info").at("file tree").at("a").at("").clone();
    let mut root = B::d();
    root.set("", file);
    t.at("info").set("file tree", root);
    assert!(matches!(validate(&t), Err(Error::V2FileTreeRootIsFile)));

    // Path traversal and "." components.
    let t = build_torrent(&[("../evil", 10)], 16384, false);
    assert!(matches!(validate(&t), Err(Error::BadTorrentPathTraversal)));
    let t = build_torrent(&[("./x", 10)], 16384, false);
    assert!(matches!(validate(&t), Err(Error::V2FileTreeDotComponent)));
    let t = build_torrent(&[("a\\b", 10)], 16384, false);
    assert!(matches!(
        validate(&t),
        Err(Error::BadTorrentSeparatorInName)
    ));

    // No files at all.
    let mut t = build_torrent(&[("a", 10)], 16384, false);
    t.at("info").set("file tree", B::d());
    assert!(matches!(validate(&t), Err(Error::BadTorrentNoFiles)));
    // Only empty files.
    let t = build_torrent(&[("a", 0)], 16384, false);
    assert!(validate(&t).is_err());
}

#[test]
fn test_reject_bad_pieces_roots() {
    let mut t = build_torrent(&[("a", 0), ("b", 10)], 16384, false);
    t.at("info")
        .at("file tree")
        .at("a")
        .at("")
        .set("pieces root", B::s([1u8; 32]));
    assert!(matches!(
        validate(&t),
        Err(Error::V2ZeroLengthFileHasPiecesRoot)
    ));

    let mut t = build_torrent(&[("b", 10)], 16384, false);
    t.at("info")
        .at("file tree")
        .at("b")
        .at("")
        .remove("pieces root");
    assert!(matches!(
        validate(&t),
        Err(Error::V2SmallFileMissingPiecesRoot)
    ));

    let mut t = build_torrent(&[("b", 10)], 16384, false);
    t.at("info")
        .at("file tree")
        .at("b")
        .at("")
        .set("pieces root", B::s([1u8; 31]));
    assert!(matches!(validate(&t), Err(Error::V2InvalidPiecesRoot(31))));

    // Two files with the same root but different lengths.
    let mut t = build_torrent(&[("a", 10), ("b", 11)], 16384, false);
    let root = t
        .at("info")
        .at("file tree")
        .at("a")
        .at("")
        .at("pieces root")
        .clone();
    t.at("info")
        .at("file tree")
        .at("b")
        .at("")
        .set("pieces root", root);
    assert!(matches!(validate(&t), Err(Error::V2DuplicatePiecesRoot)));
}

#[test]
fn test_v2_piece_layer_learned_later() {
    // A torrent added from metadata alone can verify the pieces of small
    // files at once, and those of big files once their layer is set.
    let t = build_torrent(FILES, 16384, false);
    let bytes = t.to_bytes();
    let torrent: TorrentMetaV1<ByteBuf> = torrent_from_bytes(&bytes).unwrap();
    let info = torrent.info.data.clone().validate().unwrap();
    let big = info
        .v2_files()
        .iter()
        .position(|f| f.length == 200_000)
        .unwrap();
    let exact = info
        .v2_files()
        .iter()
        .position(|f| f.length == 32768)
        .unwrap();
    assert_eq!(
        info.missing_piece_layers().collect::<Vec<_>>(),
        vec![big, exact]
    );
    let l = info.lengths();
    let first = l.validate_piece_index(0).unwrap();
    assert!(!info.can_verify_piece(first));
    assert!(info.piece_verifier(first).is_none());
    let small = l
        .validate_piece_index(info.v2_files()[big + 1].first_piece)
        .unwrap();
    assert!(info.can_verify_piece(small));

    let content = fixture_content("big.bin", 200_000);
    let mut layer = merkle::piece_layer_from_data(&content, 16384).unwrap();
    assert!(matches!(
        info.set_piece_layer(big, &layer[1..]),
        Err(Error::V2PieceLayersWrongSize { .. })
    ));
    layer[0] = Id32::default();
    assert!(matches!(
        info.set_piece_layer(big, &layer),
        Err(Error::V2PieceLayersRootMismatch)
    ));
    let layer = merkle::piece_layer_from_data(&content, 16384).unwrap();
    assert!(info.set_piece_layer(big, &layer).unwrap());
    assert!(!info.set_piece_layer(big, &layer).unwrap());
    assert!(!info.has_all_piece_layers());
    let exact_content = fixture_content("dir/sub/exact.bin", 32768);
    let exact_layer = merkle::piece_layer_from_data(&exact_content, 16384).unwrap();
    assert!(info.set_piece_layer(exact, &exact_layer).unwrap());
    assert!(info.has_all_piece_layers());
    let mut v = info.piece_verifier(first).unwrap();
    v.update(&content[..16384]);
    assert!(v.finish());
}
