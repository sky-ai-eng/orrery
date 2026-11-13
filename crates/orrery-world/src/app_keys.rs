//! The public keys an app's signatures are checked against. A world holds
//! the public half only, so a snapshot of it stays safe to share (rule 12):
//! a key arrives as PEM, and is stored as its PKCS#1 `RSAPublicKey` DER.

use base64::Engine as _;

use crate::{Result, WorldError};

/// The object identifier of `rsaEncryption`, as DER encodes its content.
const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OBJECT_IDENTIFIER: u8 = 0x06;

/// The moduli an RS256 signature is checked against, in bits.
const MODULUS_BITS: std::ops::RangeInclusive<usize> = 2048..=8192;

/// The RSA public key `pem` holds, as the DER of its PKCS#1 `RSAPublicKey`.
/// `pem` is one PEM block labelled `PUBLIC KEY` (a `SubjectPublicKeyInfo`)
/// or `RSA PUBLIC KEY` (the `RSAPublicKey` itself). A private key is
/// refused, whatever it holds.
pub fn rsa_public_key_from_pem(pem: &str) -> Result<Vec<u8>> {
    let (label, der) = pem_block(pem)?;
    let key = match label {
        "RSA PUBLIC KEY" => der,
        "PUBLIC KEY" => subject_public_key(&der)?.to_vec(),
        l if l.ends_with("PRIVATE KEY") => {
            return Err(invalid(
                "this is a private key, and a world holds public keys only; \
                 send the public half, as `openssl rsa -pubout` writes it",
            ));
        }
        other => {
            return Err(invalid(format!(
                "a PEM block labelled {other:?}, not PUBLIC KEY or RSA PUBLIC KEY"
            )));
        }
    };
    check_rsa_public_key(&key)?;
    Ok(key)
}

fn invalid(message: impl Into<String>) -> WorldError {
    WorldError::InvalidAppKey(message.into())
}

/// The label and decoded body of the one PEM block `pem` holds.
fn pem_block(pem: &str) -> Result<(&str, Vec<u8>)> {
    let mut lines = pem.lines().map(str::trim).filter(|l| !l.is_empty());
    let label = lines
        .next()
        .and_then(|l| l.strip_prefix("-----BEGIN "))
        .and_then(|l| l.strip_suffix("-----"))
        .ok_or_else(|| invalid("it does not start with a PEM `-----BEGIN ...-----` line"))?;
    let end = format!("-----END {label}-----");
    let mut body = String::new();
    let mut ended = false;
    for line in lines.by_ref() {
        if line == end {
            ended = true;
            break;
        }
        body.push_str(line);
    }
    if !ended {
        return Err(invalid(format!("it has no `{end}` line")));
    }
    if lines.next().is_some() {
        return Err(invalid("it holds more than one PEM block"));
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| invalid(format!("its PEM body is not base64: {e}")))?;
    Ok((label, der))
}

/// One DER element: its tag and its content.
struct Element<'a> {
    tag: u8,
    content: &'a [u8],
}

/// Read the element at the start of `der`, and what follows it.
fn element(der: &[u8]) -> Result<(Element<'_>, &[u8])> {
    let truncated = || invalid("its DER is cut short");
    let (&tag, rest) = der.split_first().ok_or_else(truncated)?;
    let (&first, rest) = rest.split_first().ok_or_else(truncated)?;
    let (length, rest) = match first {
        short if short < 0x80 => (short as usize, rest),
        long => {
            let bytes = (long & 0x7f) as usize;
            if bytes == 0 || bytes > 4 || rest.len() < bytes {
                return Err(invalid("its DER has a length this reader does not take"));
            }
            let (length, rest) = rest.split_at(bytes);
            let length = length.iter().fold(0usize, |n, b| (n << 8) | *b as usize);
            (length, rest)
        }
    };
    if rest.len() < length {
        return Err(truncated());
    }
    let (content, rest) = rest.split_at(length);
    Ok((Element { tag, content }, rest))
}

/// The element at the start of `der`, which must carry `tag`.
fn tagged<'a>(der: &'a [u8], tag: u8, what: &str) -> Result<(&'a [u8], &'a [u8])> {
    let (found, rest) = element(der)?;
    if found.tag != tag {
        return Err(invalid(format!("its DER has no {what} where one belongs")));
    }
    Ok((found.content, rest))
}

/// The `RSAPublicKey` inside a `SubjectPublicKeyInfo`.
fn subject_public_key(der: &[u8]) -> Result<&[u8]> {
    let (info, after) = tagged(der, SEQUENCE, "SubjectPublicKeyInfo")?;
    if !after.is_empty() {
        return Err(invalid("bytes follow its SubjectPublicKeyInfo"));
    }
    let (algorithm, key) = tagged(info, SEQUENCE, "algorithm")?;
    let (oid, _parameters) = tagged(algorithm, OBJECT_IDENTIFIER, "algorithm identifier")?;
    if oid != RSA_ENCRYPTION {
        return Err(invalid("its algorithm is not RSA"));
    }
    let (bits, after) = tagged(key, BIT_STRING, "public key")?;
    if !after.is_empty() {
        return Err(invalid("bytes follow its public key"));
    }
    match bits.split_first() {
        Some((0, key)) => Ok(key),
        _ => Err(invalid("its public key is not a whole number of bytes")),
    }
}

/// The exponents an RS256 signature is checked against: at least 3, and
/// below 2^33.
const EXPONENT_BITS: std::ops::RangeInclusive<usize> = 2..=33;

/// The bytes of a positive DER integer without its sign byte. DER writes an
/// integer in the fewest bytes, with a zero byte first only when the top bit
/// would otherwise read as a sign.
fn positive<'a>(integer: &'a [u8], what: &str) -> Result<&'a [u8]> {
    match integer {
        [0, next, ..] if next & 0x80 != 0 => Ok(&integer[1..]),
        [first, ..] if *first != 0 && first & 0x80 == 0 => Ok(integer),
        _ => Err(invalid(format!(
            "its {what} is not a positive integer in DER's shortest form"
        ))),
    }
}

/// How many bits a positive integer's bytes hold.
fn bits(magnitude: &[u8]) -> usize {
    match magnitude.first() {
        Some(top) => magnitude.len() * 8 - top.leading_zeros() as usize,
        None => 0,
    }
}

/// Refuse anything but an `RSAPublicKey` an RS256 signature can be checked
/// against, so that a key a world takes is one its JWTs verify under.
fn check_rsa_public_key(der: &[u8]) -> Result<()> {
    let (key, after) = tagged(der, SEQUENCE, "RSAPublicKey")?;
    if !after.is_empty() {
        return Err(invalid("bytes follow its RSAPublicKey"));
    }
    let (modulus, rest) = tagged(key, INTEGER, "modulus")?;
    let (exponent, rest) = tagged(rest, INTEGER, "public exponent")?;
    if !rest.is_empty() {
        return Err(invalid(
            "its RSAPublicKey holds more than a modulus and an exponent",
        ));
    }
    let modulus_bits = bits(positive(modulus, "modulus")?);
    if !MODULUS_BITS.contains(&modulus_bits) {
        return Err(invalid(format!(
            "its modulus is {modulus_bits} bits; a key is {} to {} bits",
            MODULUS_BITS.start(),
            MODULUS_BITS.end()
        )));
    }
    let exponent = positive(exponent, "public exponent")?;
    if !EXPONENT_BITS.contains(&bits(exponent)) {
        return Err(invalid("its public exponent is not between 3 and 2^33"));
    }
    if exponent.last().is_none_or(|low| low % 2 == 0) {
        return Err(invalid("its public exponent is even"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 2048-bit key, as `ssh-keygen -e -m PKCS8` and `-m PEM` write its
    // public half.
    const SPKI: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAyQDurX+SGDbR9X9O5rwY
XND5rbR9L5MHPsh5fy9PCvnjEtf9vd+gbv+MKb5isMFXGXS2Jwc/zZ4mitvK/TUB
FZD5K4Q53vIVpt54aWggRB9a3KaePpA8hMrLDklc2JaVcUtOqR2U8okAqQhagx+V
Hkjnfr2FKrRaIcA/vRvx/o+9yqSP/6TP3Kp5kMpxMBuCIPs3kF2WzxnvSrw+91Qp
CljNv66agiv3iPnNWyeaOe5C/0YFg3PRLVJq03K/LT4cHaUGM/tSFJLGj9BwyxHT
67koh9CtImSoshN5v/41kPYAuxUYzKXmUr9JGENNAIaUkrl7Gr3CzFnUIFfuCpd9
uQIDAQAB
-----END PUBLIC KEY-----
";
    const PKCS1: &str = "-----BEGIN RSA PUBLIC KEY-----
MIIBCgKCAQEAyQDurX+SGDbR9X9O5rwYXND5rbR9L5MHPsh5fy9PCvnjEtf9vd+g
bv+MKb5isMFXGXS2Jwc/zZ4mitvK/TUBFZD5K4Q53vIVpt54aWggRB9a3KaePpA8
hMrLDklc2JaVcUtOqR2U8okAqQhagx+VHkjnfr2FKrRaIcA/vRvx/o+9yqSP/6TP
3Kp5kMpxMBuCIPs3kF2WzxnvSrw+91QpCljNv66agiv3iPnNWyeaOe5C/0YFg3PR
LVJq03K/LT4cHaUGM/tSFJLGj9BwyxHT67koh9CtImSoshN5v/41kPYAuxUYzKXm
Ur9JGENNAIaUkrl7Gr3CzFnUIFfuCpd9uQIDAQAB
-----END RSA PUBLIC KEY-----
";

    fn refusal(pem: &str) -> String {
        match rsa_public_key_from_pem(pem) {
            Err(WorldError::InvalidAppKey(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn both_public_key_forms_hold_the_same_key() {
        let from_spki = rsa_public_key_from_pem(SPKI).unwrap();
        let from_pkcs1 = rsa_public_key_from_pem(PKCS1).unwrap();
        assert_eq!(from_spki, from_pkcs1);
        assert_eq!(from_spki[0], SEQUENCE);
    }

    #[test]
    fn a_private_key_is_refused_by_its_label() {
        for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "ENCRYPTED PRIVATE KEY"] {
            let pem = format!("-----BEGIN {label}-----\nAAAA\n-----END {label}-----\n");
            assert!(refusal(&pem).contains("private key"), "{label}");
        }
    }

    /// A DER element with `tag` and `content`.
    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match content.len() {
            short if short < 0x80 => out.push(short as u8),
            long => {
                let bytes = long.to_be_bytes();
                let bytes = &bytes[bytes.iter().take_while(|b| **b == 0).count()..];
                out.push(0x80 | bytes.len() as u8);
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(content);
        out
    }

    /// An `RSA PUBLIC KEY` block holding `modulus` and `exponent` as DER
    /// writes them.
    fn pkcs1(modulus: &[u8], exponent: &[u8]) -> String {
        let key = der(
            SEQUENCE,
            &[der(INTEGER, modulus), der(INTEGER, exponent)].concat(),
        );
        format!(
            "-----BEGIN RSA PUBLIC KEY-----\n{}\n-----END RSA PUBLIC KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(key)
        )
    }

    #[test]
    fn a_key_no_signature_verifies_under_is_refused() {
        // 2048 bits, written with the zero byte its set top bit needs.
        let mut modulus = vec![0];
        modulus.extend([0xc5; 256]);
        assert!(rsa_public_key_from_pem(&pkcs1(&modulus, &[1, 0, 1])).is_ok());
        assert!(rsa_public_key_from_pem(&pkcs1(&modulus, &[3])).is_ok());

        assert!(refusal(&pkcs1(&modulus, &[1])).contains("between 3 and 2^33"));
        assert!(refusal(&pkcs1(&modulus, &[2, 0, 0, 0, 1])).contains("between 3 and 2^33"));
        assert!(refusal(&pkcs1(&modulus, &[1, 0, 0])).contains("even"));
        assert!(refusal(&pkcs1(&modulus, &[0, 3])).contains("shortest form"));
        assert!(refusal(&pkcs1(&modulus, &[])).contains("shortest form"));
        // The same modulus without its zero byte reads as negative, and
        // with a second one is longer than it need be.
        assert!(refusal(&pkcs1(&modulus[1..], &[3])).contains("shortest form"));
        let padded = [&[0][..], &modulus].concat();
        assert!(refusal(&pkcs1(&padded, &[3])).contains("shortest form"));
    }

    #[test]
    fn what_is_not_an_rsa_public_key_is_refused() {
        assert!(refusal("not pem").contains("BEGIN"));
        assert!(refusal("-----BEGIN PUBLIC KEY-----\nAAAA\n").contains("END"));
        assert!(refusal(&format!("{SPKI}{SPKI}")).contains("more than one"));
        assert!(
            refusal("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
                .contains("CERTIFICATE")
        );
        assert!(
            refusal("-----BEGIN PUBLIC KEY-----\n!!!!\n-----END PUBLIC KEY-----\n")
                .contains("base64")
        );
        // An Ed25519 key's SubjectPublicKeyInfo.
        let ed25519 = "-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAhgnGkYB6+BkFDjvsjkv7CowbmhBMTJN4+N79cxl4AzI=
-----END PUBLIC KEY-----
";
        assert!(refusal(ed25519).contains("not RSA"));
        // A 512-bit modulus.
        let short = "-----BEGIN RSA PUBLIC KEY-----
MEgCQQDiVhOju1EV9l7BybmB+TrMzEphH8REGk6Awdj6vcy6dRRVby6CIqNwiGmJ
coUXmF4m1DnKPziWF4eT59AG5p9XAgMBAAE=
-----END RSA PUBLIC KEY-----
";
        assert!(refusal(short).contains("512 bits"));
    }
}
