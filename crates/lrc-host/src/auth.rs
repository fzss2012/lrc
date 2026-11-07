//! 握手鉴权：HMAC-SHA256(key = password, msg = nonce_c || nonce_s)。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 一条 MAC 的字节数。
pub const MAC_LEN: usize = 32;
/// nonce 的字节数。
pub const NONCE_LEN: usize = 16;

/// 鉴权过程里的错误（正常流程下不应出现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// HMAC 不接受该长度的密钥（HMAC 对任意长度 key 都成立，正常不会发生）。
    BadKey,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::BadKey => write!(f, "auth: HMAC 密钥长度非法"),
        }
    }
}

impl std::error::Error for AuthError {}

/// 原始 HMAC-SHA256（测试用，也方便以后换 KDF）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Result<[u8; MAC_LEN], AuthError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| AuthError::BadKey)?;
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; MAC_LEN];
    out.copy_from_slice(&tag);
    Ok(out)
}

/// 握手 MAC：key = 密码，msg = nonce_c || nonce_s（不掺任何其他东西，两端必须一致）。
pub fn compute_mac(
    password: &str,
    nonce_c: &[u8; NONCE_LEN],
    nonce_s: &[u8; NONCE_LEN],
) -> Result<[u8; MAC_LEN], AuthError> {
    let mut msg = [0u8; NONCE_LEN * 2];
    msg[..NONCE_LEN].copy_from_slice(nonce_c);
    msg[NONCE_LEN..].copy_from_slice(nonce_s);
    hmac_sha256(password.as_bytes(), &msg)
}

/// 定长比较：不因首个不同字节提前返回。
pub fn ct_eq(a: &[u8; MAC_LEN], b: &[u8; MAC_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..MAC_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 校验客户端 MAC。
pub fn verify(
    password: &str,
    nonce_c: &[u8; NONCE_LEN],
    nonce_s: &[u8; NONCE_LEN],
    got: &[u8; MAC_LEN],
) -> Result<bool, AuthError> {
    let expect = compute_mac(password, nonce_c, nonce_s)?;
    Ok(ct_eq(&expect, got))
}

/// 生成服务端 nonce。
///
/// 没有引入 rand：把进程 id、系统时间纳秒、进程内随机哈希种子（由 std 从 OS 取）、
/// ASLR 下的栈地址与一个自增序号一起喂给 SHA-256，取前 16 字节。
/// 它只需要"每次握手不可预测、不重复"，真正的防重放由 HMAC 承担。
pub fn make_nonce(seq: u64) -> [u8; NONCE_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(seq.to_le_bytes());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(nanos.to_le_bytes());

    // RandomState 的 key 由 OS 提供，每个实例不同；hash 一个已知值即可取到熵。
    let mut h = RandomState::new().build_hasher();
    h.write_u64(seq);
    h.write_u128(nanos);
    hasher.update(h.finish().to_le_bytes());

    // ASLR：栈地址每次运行不同。
    let probe = seq;
    let addr = &probe as *const u64 as usize;
    hasher.update(addr.to_le_bytes());

    let digest = hasher.finalize();
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&digest[..NONCE_LEN]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn rfc4231_case_2_vector() {
        // RFC 4231 test case 2：key="Jefe", data="what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?").expect("hmac");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn nonce_order_matters() {
        let a = [1u8; NONCE_LEN];
        let b = [2u8; NONCE_LEN];
        let m1 = compute_mac("1234", &a, &b).expect("mac");
        let m2 = compute_mac("1234", &b, &a).expect("mac");
        assert_ne!(m1, m2);
        // 同样的输入必须得到同样的 MAC
        assert_eq!(m1, compute_mac("1234", &a, &b).expect("mac"));
    }

    #[test]
    fn verify_accepts_only_right_password() {
        let nc = make_nonce(1);
        let ns = make_nonce(2);
        assert_ne!(nc, ns);
        let good = compute_mac("1234", &nc, &ns).expect("mac");
        assert!(verify("1234", &nc, &ns, &good).expect("verify"));
        assert!(!verify("12345", &nc, &ns, &good).expect("verify"));
        assert!(!verify("", &nc, &ns, &good).expect("verify"));
    }

    #[test]
    fn nonce_changes_every_call() {
        let a = make_nonce(7);
        let b = make_nonce(8);
        let c = make_nonce(8);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn ct_eq_semantics() {
        let a = [9u8; MAC_LEN];
        assert!(ct_eq(&a, &a));
        let mut b = a;
        b[MAC_LEN - 1] ^= 1;
        assert!(!ct_eq(&a, &b));
    }
}
