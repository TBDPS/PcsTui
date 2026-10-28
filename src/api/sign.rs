//! Signing helpers ported from BaiduPCS-Go (Apache-2.0).
//!
//! The algorithms are intentionally kept small and independently tested so that
//! an upstream API change can be diagnosed without touching the transfer engine.

use base64::Engine;
use md5::{Digest as Md5Digest, Md5};
use sha1::Sha1;

/// BaiduPCS-Go's `netdisksign.DevUID`.
pub fn dev_uid(bduss: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(bduss.as_bytes());
    format!("{}|0", hex::encode_upper(hasher.finalize()))
}

const LOCATE_DOWNLOAD_MAGIC: &[u8] = b"ebrcUYiuxaZv2XGu7KIYKxUrqfnOfpDF";

/// Compute the `rand` parameter used by `method=locatedownload`.
///
/// Returns `(rand, devuid)`. The caller supplies `time` and `devuid` to the URL.
pub fn locate_download_sign_with(uid: u64, bduss: &str, time: i64, dev_uid_value: &str) -> String {
    let mut bduss_hasher = Sha1::new();
    bduss_hasher.update(bduss.as_bytes());
    let bduss_sha1_hex = hex::encode(bduss_hasher.finalize());

    let mut hasher = Sha1::new();
    hasher.update(bduss_sha1_hex.as_bytes());
    hasher.update(uid.to_string().as_bytes());
    hasher.update(LOCATE_DOWNLOAD_MAGIC);
    hasher.update(time.to_string().as_bytes());
    hasher.update(dev_uid_value.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn locate_download_sign(uid: u64, bduss: &str, time: i64) -> (String, String) {
    let dev_uid_value = dev_uid(bduss);
    let rand = locate_download_sign_with(uid, bduss, time, &dev_uid_value);
    (rand, dev_uid_value)
}

/// Baidu sometimes returns an obfuscated MD5 such as
/// `05bf4f422q09b2afb5d21a4deda0ab62`. This is a reversible XOR/reorder
/// obfuscation, not a hash. Returns the original value when it is already hex.
pub fn decrypt_md5(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() != 32 {
        return raw.to_string();
    }
    let key = bytes[9] as char;
    if key.is_ascii_hexdigit() && !key.is_ascii_uppercase() {
        return raw.to_string();
    }
    let first = match key as u32 {
        value if value >= 'g' as u32 => format!("{:x}", value - 'g' as u32),
        _ => return raw.to_string(),
    };
    let mut transformed = String::with_capacity(32);
    transformed.push_str(&raw[..9]);
    transformed.push_str(&first);
    transformed.push_str(&raw[10..]);

    let mut output = String::with_capacity(32);
    for (index, ch) in transformed.chars().enumerate() {
        if ch == '-' {
            output.push_str(&format!("{:x}", 15 & index));
            continue;
        }
        let Some(value) = ch.to_digit(16) else {
            return raw.to_string();
        };
        output.push_str(&format!("{:x}", value ^ ((15 & index) as u32)));
    }
    if output.len() != 32 {
        return raw.to_string();
    }
    format!(
        "{}{}{}{}",
        &output[8..16],
        &output[0..8],
        &output[24..32],
        &output[16..24]
    )
}

/// RC4-like transform used by the `pan.baidu.com/api/download` signature.
/// Kept for a future fallback when `locatedownload` is unavailable.
pub fn sign2(key: &str, input: &str) -> Vec<u8> {
    let key: Vec<u32> = key.chars().map(|c| c as u32).collect();
    if key.is_empty() {
        return Vec::new();
    }
    let mut a = [0u32; 256];
    let mut p = [0u32; 256];
    for q in 0..256 {
        a[q] = key[q % key.len()];
        p[q] = q as u32;
    }
    let mut u = 0u32;
    for q in 0..256 {
        u = (u + p[q] + a[q]) % 256;
        p.swap(q, u as usize);
    }
    let mut i = 0u32;
    u = 0;
    let mut output = Vec::with_capacity(input.len());
    for ch in input.chars() {
        i = (i + 1) % 256;
        u = (u + p[i as usize]) % 256;
        p.swap(i as usize, u as usize);
        let k = p[((p[i as usize] + p[u as usize]) % 256) as usize];
        output.push((ch as u32 ^ k) as u8);
    }
    output
}

pub fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_upstream_locate_download_vector() {
        let rand = locate_download_sign_with(
            10086,
            "test_bduss",
            1_571_140_066,
            "O|1E67351CCE80B2CF48DB511CD77ACD9F",
        );
        assert_eq!(rand, "b6bb7a6f46899e181baea58798d4fdb889775c2c");
    }

    #[test]
    fn computes_dev_uid() {
        assert_eq!(dev_uid("test_bduss"), "66AE65D3C5D483FC4431FC43670A4673|0");
    }

    #[test]
    fn decrypts_obfuscated_md5() {
        assert_eq!(
            decrypt_md5("05bf4f422q09b2afb5d21a4deda0ab62"),
            "a3a27f40049c0a25640b668db4f15f2a"
        );
        assert_eq!(
            decrypt_md5("a3a27f40049c0a25640b668db4f15f2a"),
            "a3a27f40049c0a25640b668db4f15f2a"
        );
        assert_eq!(
            decrypt_md5("5a556e453k40d18c0795019694d2afa8"),
            "bdeb1c635b762b221d79624706b644f1"
        );
    }

    #[test]
    fn matches_upstream_sign2_example() {
        let output = sign2(
            "e8c7d729eea7b54551aa594f942decbe",
            "37dbe07ade9359c1aa70807e847f768c13360ad2",
        );
        assert_eq!(
            base64_encode(&output),
            "8RxCbsVeSzn2UjxJAAiV9QQs/WetOj2FJUGwjsMG6SgxFMWlLS/U1Q=="
        );
    }
}
