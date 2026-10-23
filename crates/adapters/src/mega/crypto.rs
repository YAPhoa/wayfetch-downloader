//! MEGA's client-side encryption: file keys, attributes, decryption, and
//! the MAC that proves a decrypted file whole.
//!
//! A file key is eight 32-bit words: the AES-128 key is the first four
//! XORed with the last four, words 4 and 5 are the CTR nonce, and words 6
//! and 7 are the expected MAC. Content is AES-128-CTR with the block
//! counter in the low 64 bits, so decryption can start at any offset.

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zeroize::Zeroizing;

use super::link::FolderKey;

type Ctr = ctr::Ctr128BE<Aes128>;

/// The first MAC chunk, and how much each following chunk grows, up to
/// [`MAX_CHUNK`].
const CHUNK_STEP: u64 = 128 * 1024;
/// The largest MAC chunk; every chunk after the eighth has this size.
const MAX_CHUNK: u64 = 1024 * 1024;
/// Longest attribute block decrypted.
const MAX_ATTRIBUTES: usize = 64 * 1024;

/// A file's key, as its link or folder listing gives it.
#[derive(Clone, PartialEq, Eq)]
pub struct FileKey {
    aes: Zeroizing<[u8; 16]>,
    nonce: [u8; 8],
    mac: [u8; 8],
}

impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKey([redacted])")
    }
}

impl FileKey {
    /// The key from its 32 raw bytes.
    pub fn from_raw(raw: &[u8; 32]) -> Self {
        let mut aes = Zeroizing::new([0u8; 16]);
        for (i, byte) in aes.iter_mut().enumerate() {
            *byte = raw[i] ^ raw[i + 16];
        }
        Self {
            aes,
            nonce: raw[16..24].try_into().expect("8 bytes"),
            mac: raw[24..32].try_into().expect("8 bytes"),
        }
    }

    /// The key of a file in a shared folder: its listed key, decrypted
    /// with the folder's key. `None` unless it decodes to 32 bytes.
    pub fn in_folder(encrypted: &str, folder: &FolderKey) -> Option<Self> {
        let mut raw = super::link::decode::<32>(encrypted)?;
        ecb_decrypt(&folder.0, raw.as_mut_slice());
        Some(Self::from_raw(&raw))
    }

    /// The file's name from its encrypted attributes, if they decrypt to
    /// MEGA's `MEGA{…}` JSON with a name in it.
    pub fn name(&self, attributes: &str) -> Option<String> {
        decrypt_name(&self.aes, attributes)
    }

    /// A decryptor for the file's content from byte `offset` on.
    pub fn decryptor(&self, offset: u64) -> Decryptor {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&self.nonce);
        let mut cipher = Ctr::new(self.aes.as_ref().into(), &iv.into());
        cipher.seek(offset);
        Decryptor(cipher)
    }

    /// A check of the decrypted content against the key's MAC.
    pub fn mac_check(&self) -> MacCheck {
        let mut start = [0u8; 16];
        start[..8].copy_from_slice(&self.nonce);
        start[8..].copy_from_slice(&self.nonce);
        MacCheck {
            cipher: Aes128::new(self.aes.as_ref().into()),
            start,
            expected: self.mac,
            chunk: start,
            meta: [0; 16],
            block: [0; 16],
            filled: 0,
            in_chunk: 0,
            chunk_len: CHUNK_STEP,
        }
    }

    /// A short digest naming this key without revealing it, for telling
    /// whether a saved partial was decrypted with the same key.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"wayfetch mega key\0");
        hasher.update(self.aes.as_ref());
        hasher.update(self.nonce);
        hasher.update(self.mac);
        hex::encode(&hasher.finalize()[..8])
    }
}

fn ecb_decrypt(key: &[u8; 16], data: &mut [u8]) {
    let cipher = Aes128::new(key.into());
    for block in data.chunks_exact_mut(16) {
        cipher.decrypt_block(block.into());
    }
}

/// Attributes are AES-128-CBC with a zero IV and zero padding.
fn decrypt_name(key: &[u8; 16], attributes: &str) -> Option<String> {
    if attributes.len() > MAX_ATTRIBUTES * 4 / 3 + 4 {
        return None;
    }
    let mut data = URL_SAFE_NO_PAD
        .decode(attributes.trim_end_matches('=').as_bytes())
        .ok()?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new(key.into());
    let mut previous = [0u8; 16];
    for block in data.chunks_exact_mut(16) {
        let encrypted: [u8; 16] = (*block).try_into().expect("16 bytes");
        cipher.decrypt_block(block.into());
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        previous = encrypted;
    }
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let json = data[..end].strip_prefix(b"MEGA")?;
    #[derive(serde::Deserialize)]
    struct Attributes {
        n: String,
    }
    let name = serde_json::from_slice::<Attributes>(json).ok()?.n;
    (!name.is_empty()).then_some(name)
}

/// Decrypts a file's content in place, from where it was started on.
pub struct Decryptor(Ctr);

impl Decryptor {
    /// Decrypts the next bytes in place.
    pub fn apply(&mut self, data: &mut [u8]) {
        self.0.apply_keystream(data);
    }
}

impl std::fmt::Debug for Decryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Decryptor")
    }
}

/// MEGA's MAC over a file's decrypted content, fed from its first byte to
/// its last. Each chunk gets a CBC-MAC that starts from the nonce twice;
/// the chunk MACs are chained by AES-CBC from zero, and the result, folded
/// to 8 bytes, must equal the key's last two words.
pub struct MacCheck {
    cipher: Aes128,
    start: [u8; 16],
    expected: [u8; 8],
    chunk: [u8; 16],
    meta: [u8; 16],
    block: [u8; 16],
    filled: usize,
    in_chunk: u64,
    chunk_len: u64,
}

impl std::fmt::Debug for MacCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacCheck").finish_non_exhaustive()
    }
}

impl MacCheck {
    /// Feeds the next bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // Chunks are multiples of 16 bytes, so a block never spans two.
            let take = (16 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 16 {
                self.absorb_block();
                if self.in_chunk == self.chunk_len {
                    self.end_chunk();
                }
            }
        }
    }

    fn absorb_block(&mut self) {
        for (c, b) in self.chunk.iter_mut().zip(self.block) {
            *c ^= b;
        }
        self.cipher.encrypt_block((&mut self.chunk).into());
        self.in_chunk += 16;
        self.filled = 0;
        self.block = [0; 16];
    }

    fn end_chunk(&mut self) {
        for (m, c) in self.meta.iter_mut().zip(self.chunk) {
            *m ^= c;
        }
        self.cipher.encrypt_block((&mut self.meta).into());
        self.chunk = self.start;
        self.in_chunk = 0;
        self.chunk_len = (self.chunk_len + CHUNK_STEP).min(MAX_CHUNK);
    }

    /// Whether everything fed matches the key's MAC. The last block of
    /// the file is padded with zeros.
    pub fn verify(mut self) -> bool {
        if self.filled > 0 {
            self.absorb_block();
        }
        if self.in_chunk > 0 {
            self.end_chunk();
        }
        let m = self.meta;
        let folded: Vec<u8> = (0..4)
            .map(|i| m[i] ^ m[i + 4])
            .chain((0..4).map(|i| m[i + 8] ^ m[i + 12]))
            .collect();
        folded == self.expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mega::link::MegaLink;
    use sha2::{Digest, Sha256};

    // From a separate reference implementation (Python `cryptography`):
    // the content below, encrypted under this key, and its attributes.
    const KEY: &str = "ERMRFxETER8-P_sk6eh2pQECAwQFBgcIJibhP_X1aLo";
    const ATTRIBUTES: &str = "15Ukrmp1IMoZnwKA3m5JcmAfMFJw_XJRv-F2R1LTvfA";
    const SIZE: usize = 2_621_450;
    const CIPHER_SHA256: &str = "cd17d882b34f7693be6516ee18b64b5d68da0b4a6ce06ff455ca82cfa6b1d1b1";
    const FOLDER_KEY: &str = "ZGVmZ2hpamtsbW5vcHFycw";
    const NODE_KEY: &str = "-50QbYsFMxwD_E-zSkk8xoF-K9Dq8NOcYHd6KUAgayg";

    fn plain() -> Vec<u8> {
        (0..SIZE).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    fn key() -> FileKey {
        FileKey::from_raw(&crate::mega::link::decode::<32>(KEY).unwrap())
    }

    /// CTR is its own inverse: encrypting the content gives the reference
    /// ciphertext, so decrypting that gives the content back.
    fn encrypted() -> Vec<u8> {
        let mut data = plain();
        key().decryptor(0).apply(&mut data);
        data
    }

    #[test]
    fn decryption_matches_the_reference_from_any_offset() {
        let cipher = encrypted();
        assert_eq!(hex::encode(Sha256::digest(&cipher)), CIPHER_SHA256);
        let plain = plain();
        for offset in [0, 1, 15, 16, 17, 131_071, 1_000_003, SIZE - 1] {
            let mut tail = cipher[offset..].to_vec();
            key().decryptor(offset as u64).apply(&mut tail);
            assert!(tail == plain[offset..], "offset {offset}");
        }
    }

    #[test]
    fn the_mac_accepts_the_file_in_any_pieces_and_nothing_else() {
        let plain = plain();
        for piece in [1, 7, 16, 4096, 65_537, SIZE] {
            let mut check = key().mac_check();
            plain.chunks(piece).for_each(|p| check.update(p));
            assert!(check.verify(), "pieces of {piece}");
        }
        let mut damaged = plain.clone();
        damaged[1_500_000] ^= 1;
        let mut check = key().mac_check();
        check.update(&damaged);
        assert!(!check.verify());
        let mut short = key().mac_check();
        short.update(&plain[..SIZE - 1]);
        assert!(!short.verify());
    }

    #[test]
    fn names_decrypt_from_attributes() {
        assert_eq!(key().name(ATTRIBUTES).as_deref(), Some("sample.bin"));
        assert_eq!(key().name("AAAA"), None);
        assert_eq!(key().name("not base64!"), None);
        let other = FileKey::from_raw(&[7; 32]);
        assert_eq!(other.name(ATTRIBUTES), None);
    }

    #[test]
    fn a_folder_listing_key_decrypts_to_the_file_key() {
        let Some(MegaLink::Folder { key: folder, .. }) = MegaLink::parse(
            &url::Url::parse(&format!("https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}")).unwrap(),
        ) else {
            panic!("not a folder link");
        };
        let key = FileKey::in_folder(NODE_KEY, &folder).unwrap();
        assert_eq!(key, self::key());
        assert_eq!(key.fingerprint(), self::key().fingerprint());
        assert_ne!(key.fingerprint(), FileKey::from_raw(&[7; 32]).fingerprint());
    }
}
