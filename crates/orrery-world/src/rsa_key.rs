//! An RSA key pair derived from a seed.
//!
//! A world's credentials are derived from its secret, never drawn at random
//! (rule 12), so the same world makes the same key on every run and on
//! replay. The derivation is all here, in integer arithmetic: the primes are
//! searched for from a stream of the seed's bytes, in a fixed order, with a
//! fixed test. Nothing in it comes from a key-generation library, whose
//! choices an upgrade could change, and a test pins one seed's key.

use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use sha2::Sha256;

/// The size of the modulus, the one GitHub's own keys have.
const MODULUS_BITS: u64 = 2048;

/// The public exponent every key takes.
const EXPONENT: u32 = 65537;

/// Witnesses for the primality test. The candidates come from the seed, not
/// from anyone choosing them to fool these, and a composite that size passes
/// even one of them with a chance far below that of the seed being guessed.
const WITNESSES: [u32; 8] = [2, 3, 5, 7, 11, 13, 17, 19];

/// A key pair, each half as the DER of its PKCS#1 structure.
pub struct KeyPair {
    /// `RSAPrivateKey`.
    pub private_der: Vec<u8>,
    /// `RSAPublicKey`.
    pub public_der: Vec<u8>,
}

impl KeyPair {
    /// The private key as PEM, labelled `RSA PRIVATE KEY`, as GitHub gives
    /// an app's.
    pub fn private_pem(&self) -> String {
        use base64::Engine as _;
        let body = base64::engine::general_purpose::STANDARD.encode(&self.private_der);
        let mut pem = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
        for line in body.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            pem.push('\n');
        }
        pem.push_str("-----END RSA PRIVATE KEY-----\n");
        pem
    }
}

/// The key pair `seed` derives.
pub fn derive(seed: &[u8; 32]) -> KeyPair {
    let one = BigUint::from(1u32);
    let e = BigUint::from(EXPONENT);
    let small = small_primes();
    // The larger prime is `p`, as the verifiers that take a private key
    // expect.
    let (p, q) = {
        let a = prime(seed, "p", &small);
        let b = prime(seed, "q", &small);
        if a > b { (a, b) } else { (b, a) }
    };
    let n = &p * &q;
    let (p1, q1) = (&p - &one, &q - &one);
    let d = e
        .modinv(&(&p1 * &q1))
        .expect("the exponent divides neither prime less one");
    let dp = &d % &p1;
    let dq = &d % &q1;
    let qinv = q.modinv(&p).expect("two primes share no factor");
    let public_der = sequence(&[integer(&n), integer(&e)]);
    let private_der = sequence(&[
        integer(&BigUint::from(0u32)),
        integer(&n),
        integer(&e),
        integer(&d),
        integer(&p),
        integer(&q),
        integer(&dp),
        integer(&dq),
        integer(&qinv),
    ]);
    KeyPair {
        private_der,
        public_der,
    }
}

/// The first prime at or after the number the seed's stream gives for
/// `label`, of half the modulus's size. Its two highest bits are set, so the
/// product of two is the full size, and it is not one more than a multiple
/// of the exponent, so the exponent has an inverse.
fn prime(seed: &[u8; 32], label: &str, small: &[u32]) -> BigUint {
    let bytes = (MODULUS_BITS / 16) as usize;
    let mut start = stream(seed, label, bytes);
    start[0] |= 0xc0;
    start[bytes - 1] |= 1;
    let mut candidate = BigUint::from_bytes_be(&start);
    let two = BigUint::from(2u32);
    loop {
        if is_prime(&candidate, small) && (&candidate % EXPONENT) != BigUint::from(1u32) {
            return candidate;
        }
        candidate += &two;
    }
}

/// `len` bytes of the seed's stream for `label`: HMAC-SHA256 of the label
/// and a block counter, under the seed.
fn stream(seed: &[u8; 32], label: &str, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut block = 0u32;
    while out.len() < len {
        let mut mac = Hmac::<Sha256>::new_from_slice(seed).expect("HMAC takes a key of any size");
        mac.update(b"rsa-key:");
        mac.update(label.as_bytes());
        mac.update(&block.to_be_bytes());
        out.extend_from_slice(&mac.finalize().into_bytes());
        block += 1;
    }
    out.truncate(len);
    out
}

/// The odd primes below 2000, which rule out most candidates cheaply.
fn small_primes() -> Vec<u32> {
    (3..2000u32)
        .step_by(2)
        .filter(|n| (3..*n).take_while(|d| d * d <= *n).all(|d| n % d != 0))
        .collect()
}

/// Whether an odd `n`, far larger than any small prime, is prime: no small
/// prime divides it, and it passes Miller-Rabin for each witness.
fn is_prime(n: &BigUint, small: &[u32]) -> bool {
    let zero = BigUint::from(0u32);
    if small.iter().any(|p| (n % *p) == zero) {
        return false;
    }
    let one = BigUint::from(1u32);
    let less = n - &one;
    let twos = less.trailing_zeros().unwrap_or(0);
    let odd = &less >> twos;
    WITNESSES.iter().all(|witness| {
        let mut x = BigUint::from(*witness).modpow(&odd, n);
        if x == one || x == less {
            return true;
        }
        for _ in 1..twos {
            x = (&x * &x) % n;
            if x == less {
                return true;
            }
        }
        false
    })
}

/// A DER `INTEGER` holding a number that is not negative.
fn integer(n: &BigUint) -> Vec<u8> {
    let mut content = n.to_bytes_be();
    // A leading bit that is set would read as a sign.
    if content[0] & 0x80 != 0 {
        content.insert(0, 0);
    }
    element(0x02, &content)
}

/// A DER `SEQUENCE` of elements already encoded.
fn sequence(elements: &[Vec<u8>]) -> Vec<u8> {
    element(0x30, &elements.concat())
}

fn element(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;

    #[test]
    fn a_seed_derives_one_key_of_the_full_size() {
        let key = derive(&[7u8; 32]);
        assert_eq!(key.private_der, derive(&[7u8; 32]).private_der);
        assert_ne!(key.private_der, derive(&[8u8; 32]).private_der);
        // The public half is what a world accepts as an app's key.
        crate::app_keys::check_rsa_public_key(&key.public_der).expect("an RSA public key");
        let modulus_bytes = key.public_der.len() - 4 - 4 - 5;
        assert_eq!(modulus_bytes, 257, "a 2048-bit modulus and its sign byte");
        assert!(
            key.private_pem()
                .starts_with("-----BEGIN RSA PRIVATE KEY-----\n")
        );
    }

    /// A world's key must never change, since an app keeps the one it was
    /// given: this is the key the derivation gave when it was written.
    #[test]
    fn the_derivation_is_pinned() {
        let key = derive(&[7u8; 32]);
        assert_eq!(
            hex::encode(Sha256::digest(&key.private_der)),
            "b801c0d1cc67301596e099cfd70dadcd44f0c9b113457f6efc5e2dc8d781ea55"
        );
    }
}
