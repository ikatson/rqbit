//! Builds BitTorrent v2 (BEP 52) and hybrid torrents for tests, the way
//! libtorrent lays them out: files in sorted path order, and in the v1 list
//! of a hybrid torrent every file but the last followed by a BEP 47 padding
//! file up to the next piece boundary.

// Helpers shared by several e2e test modules; not every module uses all of them.
#![allow(dead_code)]

use std::{collections::BTreeMap, path::Path};

use librqbit_core::{Id32, merkle};
use rand::{Rng, SeedableRng, rngs::SmallRng};
use sha1w::{ISha1, Sha1};

/// A bencode value with sorted dictionaries.
#[derive(Clone, Debug)]
pub(crate) enum B {
    I(i64),
    S(Vec<u8>),
    L(Vec<B>),
    D(BTreeMap<Vec<u8>, B>),
}

impl B {
    pub(crate) fn s(v: impl AsRef<[u8]>) -> B {
        B::S(v.as_ref().to_vec())
    }

    pub(crate) fn d() -> B {
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

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    pub(crate) fn map(&mut self) -> &mut BTreeMap<Vec<u8>, B> {
        match self {
            B::D(d) => d,
            other => panic!("not a dict: {other:?}"),
        }
    }

    pub(crate) fn at(&mut self, key: &str) -> &mut B {
        self.map().get_mut(key.as_bytes()).unwrap()
    }

    pub(crate) fn set(&mut self, key: impl AsRef<[u8]>, v: B) {
        self.map().insert(key.as_ref().to_vec(), v);
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<B> {
        self.map().remove(key.as_bytes())
    }
}

/// The files of a test torrent: (path inside the torrent, content).
pub(crate) struct V2Files {
    pub(crate) files: Vec<(String, Vec<u8>)>,
}

impl V2Files {
    /// Files of the given lengths with random content from `seed`, sorted the
    /// way a BEP 52 `file tree` orders them.
    pub(crate) fn random(spec: &[(&str, usize)], seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut files: Vec<(String, Vec<u8>)> = spec
            .iter()
            .map(|(path, len)| {
                let mut content = vec![0u8; *len];
                rng.fill_bytes(&mut content);
                ((*path).to_owned(), content)
            })
            .collect();
        files.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
        Self { files }
    }

    /// Write the files under `dir`.
    pub(crate) fn write_to(&self, dir: &Path) {
        for (path, content) in &self.files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
    }

    /// Check that `dir` holds exactly these files' contents.
    pub(crate) fn assert_equal_to(&self, dir: &Path) {
        for (path, content) in &self.files {
            let got = std::fs::read(dir.join(path))
                .unwrap_or_else(|e| panic!("error reading {path:?} in {dir:?}: {e}"));
            assert!(got == *content, "{path:?} differs");
        }
    }

    pub(crate) fn total_len(&self) -> u64 {
        self.files.iter().map(|(_, c)| c.len() as u64).sum()
    }

    /// The pieces of the v2-only layout, in order: every non-empty file
    /// starts a new piece, and its last piece ends with the file.
    pub(crate) fn v2_pieces(&self, piece_length: u32) -> Vec<&[u8]> {
        self.files
            .iter()
            .flat_map(|(_, c)| c.chunks(piece_length as usize))
            .collect()
    }

    /// The `piece layers` entry of every file bigger than a piece, by root.
    pub(crate) fn piece_layers(&self, piece_length: u32) -> Vec<(Id32, Vec<Id32>)> {
        self.files
            .iter()
            .filter(|(_, c)| c.len() as u64 > u64::from(piece_length))
            .map(|(_, c)| {
                (
                    merkle::pieces_root_from_data(c).unwrap(),
                    merkle::piece_layer_from_data(c, piece_length).unwrap(),
                )
            })
            .collect()
    }

    /// A v2-only (`hybrid == false`) or hybrid torrent named `name` over the
    /// files, with every piece layer included.
    pub(crate) fn torrent(&self, name: &str, piece_length: u32, hybrid: bool) -> B {
        let pl = u64::from(piece_length);
        let mut tree = B::d();
        let mut layers = B::d();
        let mut v1_files = Vec::new();
        let mut v1_stream = Vec::new();
        for (idx, (path, content)) in self.files.iter().enumerate() {
            let len = content.len() as u64;
            let mut props = B::d();
            props.set("length", B::I(len as i64));
            if len > 0 {
                let root = merkle::pieces_root_from_data(content).unwrap();
                props.set("pieces root", B::s(root.0));
                if len > pl {
                    let layer = merkle::piece_layer_from_data(content, piece_length).unwrap();
                    let raw: Vec<u8> = layer.iter().flat_map(|h| h.0).collect();
                    layers.set(root.0, B::S(raw));
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
            f.set("length", B::I(len as i64));
            f.set("path", B::L(path.split('/').map(B::s).collect()));
            v1_files.push(f);
            v1_stream.extend_from_slice(content);
            let pad = (pl - len % pl) % pl;
            if pad > 0 && idx + 1 < self.files.len() {
                let mut f = B::d();
                f.set("attr", B::s("p"));
                f.set("length", B::I(pad as i64));
                f.set("path", B::L(vec![B::s(".pad"), B::s(pad.to_string())]));
                v1_files.push(f);
                let pad = usize::try_from(pad).expect("test padding fits in memory");
                v1_stream.resize(v1_stream.len() + pad, 0);
            }
        }

        let mut info = B::d();
        info.set("name", B::s(name));
        info.set("piece length", B::I(i64::from(piece_length)));
        info.set("meta version", B::I(2));
        info.set("file tree", tree);
        if hybrid {
            let pieces: Vec<u8> = v1_stream
                .chunks(piece_length as usize)
                .flat_map(|p| {
                    let mut h = Sha1::new();
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
}
