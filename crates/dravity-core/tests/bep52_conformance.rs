//! BEP 52 conformance vectors: pieces roots and piece layers for files at
//! every edge around leaf and piece boundaries, at three piece lengths.
//!
//! The expected hashes were computed by an independent Python
//! implementation written from the BEP text (a full merkle tree with
//! zero-filled padding leaves), not by this crate, so a shared
//! misreading of the spec cannot make both sides agree. The generator is
//! `tests/data/bep52_vectors.py`.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, CreateVersion, InputFile, TorrentCreator};
use dravity_core::merkle;
use sha2::{Digest, Sha256};

/// File contents used for every vector: byte `i` is `(7 i + 3) mod 251`.
fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) % 251) as u8).collect()
}

fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

const VECTORS: &[(u32, usize, &str, &[&str])] = &[
    (
        16384,
        1,
        "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
        &[],
    ),
    (
        16384,
        100,
        "8ccfe0e9682941c3451db3606c96dfdde905896fd071c0bb2d58d01b8c6daf7d",
        &[],
    ),
    (
        16384,
        16383,
        "3b2cb465b73a95901a4b86bc2b135243d3c51c2c2840bb0e9e04e300fe32397f",
        &[],
    ),
    (
        16384,
        16384,
        "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
        &[],
    ),
    (
        16384,
        16385,
        "cb0b7d4a5957d021a52428ba33dac06cbb4fe7c90c57d68188e216b70eacc49f",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "f8d20e598df20877e4d826246fc31ffb4615cbc059aec9ec8e5b28951d844a3f",
        ],
    ),
    (
        16384,
        32767,
        "dcb50027266eb6c3e74a71f112335dd6cf8857caceffcde3aa51362038eee817",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "abd055430c22ed32868b6ed619f24f18bc55a53aacb8e6645d6dfe10a7410503",
        ],
    ),
    (
        16384,
        32768,
        "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
        ],
    ),
    (
        16384,
        32769,
        "efbf8c2fb5d0b065a4dbd62dfab6a7dcbc21aa2f68f9e156f357b6c6683c0312",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "af193a8cdcd0e3fb39e71147e59efa5cad40763d2611f5beff34a274f514362f",
        ],
    ),
    (
        16384,
        65536,
        "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "d638f9fa5aad802f7d102a418555e919bd5330e9f4e26e71ce96a419f37bc5c0",
            "9a2bc0d78b8503528abedcb1098cca617c30a2792cadf55c42a73f330c6352dd",
        ],
    ),
    (
        16384,
        65537,
        "ab0cd527d4b38e22fdd16718c7ebc752d565d06ff438a8f056c56d67bfc2f8a0",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "d638f9fa5aad802f7d102a418555e919bd5330e9f4e26e71ce96a419f37bc5c0",
            "9a2bc0d78b8503528abedcb1098cca617c30a2792cadf55c42a73f330c6352dd",
            "9be3799f24592e94e1f7991e5f312648a509ce2fb1edbafa50a66b65c916539a",
        ],
    ),
    (
        16384,
        49157,
        "592fd1aa7fc999029cd44cea0048d72e91b6167cd688229e2c7d3e33fa9889fd",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "d638f9fa5aad802f7d102a418555e919bd5330e9f4e26e71ce96a419f37bc5c0",
            "2cfb7f7cb5ffc9a8e834f9392d39a3c5fa0cf8ee60db89d48482e5ff886ff23c",
        ],
    ),
    (
        16384,
        114688,
        "8d3e237a0a1728f237d89c68a173ab429e53abf2352dd5d2bfac7cfea60e1719",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "d638f9fa5aad802f7d102a418555e919bd5330e9f4e26e71ce96a419f37bc5c0",
            "9a2bc0d78b8503528abedcb1098cca617c30a2792cadf55c42a73f330c6352dd",
            "7134b8929526696d80e27d58378f6f01a3b87633fbcdffe4eb0f9e7e8f54c515",
            "5cbe2d7c0c3caa57d81a95c06942aa4bffd96f1ed9e3d03e6d8f45b09911cefa",
            "31c7c3dd75f67bca18e851763cc6614253f1f319a3e9662f9e987ddd8a0dd3e5",
        ],
    ),
    (
        16384,
        147457,
        "37ae5363290319c1994b5f3a1d202c7d09cd8ad9c0d1c26c7e1c37d0b3360d37",
        &[
            "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
            "6602c191d6cb5362f91f44ec41563915ee19845df7ea2186b288ce2b7591e504",
            "d638f9fa5aad802f7d102a418555e919bd5330e9f4e26e71ce96a419f37bc5c0",
            "9a2bc0d78b8503528abedcb1098cca617c30a2792cadf55c42a73f330c6352dd",
            "7134b8929526696d80e27d58378f6f01a3b87633fbcdffe4eb0f9e7e8f54c515",
            "5cbe2d7c0c3caa57d81a95c06942aa4bffd96f1ed9e3d03e6d8f45b09911cefa",
            "31c7c3dd75f67bca18e851763cc6614253f1f319a3e9662f9e987ddd8a0dd3e5",
            "8083cdf9e30b491b1cd782ce842b81f13259269f9b5c00df1ae69ef627853f27",
            "4609f40f140f663fc1efc7fa067958771b151f0b7662f137dae63714fc52de9f",
            "8de0b3c47f112c59745f717a626932264c422a7563954872e237b223af4ad643",
        ],
    ),
    (
        32768,
        1,
        "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
        &[],
    ),
    (
        32768,
        100,
        "8ccfe0e9682941c3451db3606c96dfdde905896fd071c0bb2d58d01b8c6daf7d",
        &[],
    ),
    (
        32768,
        16383,
        "3b2cb465b73a95901a4b86bc2b135243d3c51c2c2840bb0e9e04e300fe32397f",
        &[],
    ),
    (
        32768,
        16384,
        "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
        &[],
    ),
    (
        32768,
        16385,
        "cb0b7d4a5957d021a52428ba33dac06cbb4fe7c90c57d68188e216b70eacc49f",
        &[],
    ),
    (
        32768,
        32767,
        "dcb50027266eb6c3e74a71f112335dd6cf8857caceffcde3aa51362038eee817",
        &[],
    ),
    (
        32768,
        32768,
        "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
        &[],
    ),
    (
        32768,
        32769,
        "efbf8c2fb5d0b065a4dbd62dfab6a7dcbc21aa2f68f9e156f357b6c6683c0312",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "28d2d1b1aea54d3379c7ae4544097c4fc2bc0ddb4824af83a70a4e07c235e45e",
        ],
    ),
    (
        32768,
        65536,
        "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "aa998194bca61ba064c9766803e08a07ae1db27d8dec9a3026c5fb683276a939",
        ],
    ),
    (
        32768,
        65537,
        "ab0cd527d4b38e22fdd16718c7ebc752d565d06ff438a8f056c56d67bfc2f8a0",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "aa998194bca61ba064c9766803e08a07ae1db27d8dec9a3026c5fb683276a939",
            "c88dd65fa0aeacb6f76a7524ca27e657d0607692fcbb358b495af2a86294a9ac",
        ],
    ),
    (
        32768,
        49157,
        "592fd1aa7fc999029cd44cea0048d72e91b6167cd688229e2c7d3e33fa9889fd",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "fe49ce043286c53281d59385ea4e8eecd80269ecb95a0318226cdb5bfc5b699f",
        ],
    ),
    (
        32768,
        114688,
        "8d3e237a0a1728f237d89c68a173ab429e53abf2352dd5d2bfac7cfea60e1719",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "aa998194bca61ba064c9766803e08a07ae1db27d8dec9a3026c5fb683276a939",
            "6f77e2f5925abe4641778276baea83c1725a7e63cd8801cf5f4db591b89a7a59",
            "4848b3a891ff8bfd59a290f34afff938779f0c4967eb78842d767d9c76623e26",
        ],
    ),
    (
        32768,
        147457,
        "37ae5363290319c1994b5f3a1d202c7d09cd8ad9c0d1c26c7e1c37d0b3360d37",
        &[
            "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
            "aa998194bca61ba064c9766803e08a07ae1db27d8dec9a3026c5fb683276a939",
            "6f77e2f5925abe4641778276baea83c1725a7e63cd8801cf5f4db591b89a7a59",
            "e012093728db3e4d3e6f1ebe371e75a34fc4d246cbc35637577d07370e8aa7ca",
            "2276e5141a7e4640abb2b36bb281af383427e6e4fb19f135972700dde09fe851",
        ],
    ),
    (
        65536,
        1,
        "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
        &[],
    ),
    (
        65536,
        100,
        "8ccfe0e9682941c3451db3606c96dfdde905896fd071c0bb2d58d01b8c6daf7d",
        &[],
    ),
    (
        65536,
        16383,
        "3b2cb465b73a95901a4b86bc2b135243d3c51c2c2840bb0e9e04e300fe32397f",
        &[],
    ),
    (
        65536,
        16384,
        "90b834666bd99804aad5f0d312a8862f91872e635fd6063d42fe787c4e1d84ee",
        &[],
    ),
    (
        65536,
        16385,
        "cb0b7d4a5957d021a52428ba33dac06cbb4fe7c90c57d68188e216b70eacc49f",
        &[],
    ),
    (
        65536,
        32767,
        "dcb50027266eb6c3e74a71f112335dd6cf8857caceffcde3aa51362038eee817",
        &[],
    ),
    (
        65536,
        32768,
        "6b7702ec93fd7bde4fccf7123b679a0703843fe9d838e7bc9ac4eee2c26e3923",
        &[],
    ),
    (
        65536,
        32769,
        "efbf8c2fb5d0b065a4dbd62dfab6a7dcbc21aa2f68f9e156f357b6c6683c0312",
        &[],
    ),
    (
        65536,
        65536,
        "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
        &[],
    ),
    (
        65536,
        65537,
        "ab0cd527d4b38e22fdd16718c7ebc752d565d06ff438a8f056c56d67bfc2f8a0",
        &[
            "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
            "4ee5e8eb584cba83c17ac436dba46a2069e809ac2c5568be4ca2cfd4ebb43dc0",
        ],
    ),
    (
        65536,
        49157,
        "592fd1aa7fc999029cd44cea0048d72e91b6167cd688229e2c7d3e33fa9889fd",
        &[],
    ),
    (
        65536,
        114688,
        "8d3e237a0a1728f237d89c68a173ab429e53abf2352dd5d2bfac7cfea60e1719",
        &[
            "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
            "bec6ce95fc4944d642424b757d9dd49d1c90b33805c1d43e7a692108a7f82c4b",
        ],
    ),
    (
        65536,
        147457,
        "37ae5363290319c1994b5f3a1d202c7d09cd8ad9c0d1c26c7e1c37d0b3360d37",
        &[
            "88866eeb00fd9d7f8d4b79a31bf452581a9788c0190bf03a1e122a4a19111f80",
            "412aa8a525033889165edf8be55a68bb4da12c6c9630a1e3dd6b0699e5cbb7f6",
            "409b23aaaa247a4742ce0bdc717640a6675f396a76a829e09e84053a09420656",
        ],
    ),
];

#[test]
fn padding_hashes_match_the_spec() {
    // A padding leaf is 32 zero bytes, not the hash of a zero block.
    assert_eq!(merkle::pad_hash(0), [0u8; 32]);
    // SHA-256 of 64 zero bytes.
    assert_eq!(
        hex(&merkle::pad_hash(1)),
        "f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b"
    );
    let two: [u8; 32] = Sha256::digest([merkle::pad_hash(1), merkle::pad_hash(1)].concat()).into();
    assert_eq!(merkle::pad_hash(2), two);
}

#[test]
fn file_hashes_match_the_reference_vectors() {
    for &(piece_length, len, root, layer) in VECTORS {
        let got = merkle::file_hashes(&data(len), piece_length).unwrap();
        assert_eq!(
            hex(&got.root),
            root,
            "root, piece {piece_length}, len {len}"
        );
        let got_layer: Vec<String> = got.layer.iter().map(|h| hex(h)).collect();
        assert_eq!(got_layer, layer, "layer, piece {piece_length}, len {len}");
        // The root is also what the layer implies, for multi-piece files.
        if !layer.is_empty() {
            assert_eq!(
                hex(&merkle::root_from_layer(&got.layer, piece_length).unwrap()),
                root
            );
        }
    }
    // An empty file has no pieces root.
    assert!(merkle::file_hashes(&[], 16384).is_none());
}

#[test]
fn created_torrents_carry_the_reference_roots_and_layers() {
    for piece_length in [16384u32, 32768, 65536] {
        let vectors: Vec<_> = VECTORS
            .iter()
            .filter(|v| v.0 == piece_length)
            .take(6)
            .collect();
        let files: Vec<InputFile> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| InputFile::new([format!("f{i:02}")], v.1 as u64))
            .collect();
        let mut options = CreateOptions::new(CreateVersion::V2);
        options.piece_length = Some(piece_length);
        let mut c = TorrentCreator::new("set", files, false, options).unwrap();
        for v in &vectors {
            c.update(&data(v.1)).unwrap();
        }
        let m = Metainfo::from_bytes(&c.finish().unwrap()).unwrap();
        let info = m.info();
        // v2 files start on piece boundaries; the stream model shows the
        // gaps as padding entries, which have no root.
        let real: Vec<usize> = (0..info.files().len())
            .filter(|&i| !info.files()[i].is_padding())
            .collect();
        assert_eq!(real.len(), vectors.len());
        for (&i, v) in real.iter().zip(&vectors) {
            assert_eq!(info.files()[i].length(), v.1 as u64);
            assert_eq!(hex(info.pieces_root(i).unwrap()), v.2, "file {i}");
        }
        let layers = info.piece_layers();
        for v in vectors.iter().filter(|v| !v.3.is_empty()) {
            let (_, layer) = layers
                .iter()
                .find(|(root, _)| hex(root) == v.2)
                .expect("a piece layer for every multi-piece file");
            let layer: Vec<String> = layer.iter().map(|h| hex(h)).collect();
            assert_eq!(layer, v.3);
        }
        // Single-piece files have no layer (BEP 52).
        assert_eq!(
            layers.len(),
            vectors.iter().filter(|v| !v.3.is_empty()).count()
        );
        // The v2 info hash is the SHA-256 of the bencoded info dictionary.
        let digest: [u8; 32] = Sha256::digest(m.info_bytes()).into();
        assert_eq!(m.info_hash_v2().unwrap().as_bytes(), &digest);
    }
}
