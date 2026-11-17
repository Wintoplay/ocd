//! The PEM client certificate and private key of a provider's `settings.tls`, decoded as Bun's BoringSSL reads them
//! (`tls: {cert, key, passphrase}` on `fetch`).
//!
//! - `cert`: every `CERTIFICATE` block, the client certificate first and then its chain.
//! - `key`: the first private key block: `PRIVATE KEY` (PKCS#8), `RSA PRIVATE KEY` (PKCS#1) or `EC PRIVATE KEY`
//!   (SEC1), each optionally encrypted the legacy OpenSSL way (`Proc-Type: 4,ENCRYPTED` + `DEK-Info`, a key from
//!   `EVP_BytesToKey` with MD5: AES-128/192/256-CBC, DES-EDE3-CBC, DES-CBC), or `ENCRYPTED PRIVATE KEY` (PKCS#8):
//!   PBES2 with PBKDF2 (AES-CBC, DES-EDE3-CBC) or PKCS#12's `pbeWithSHAAnd3-KeyTripleDES-CBC` and
//!   `pbeWithSHAAnd2-KeyTripleDES-CBC`. Like BoringSSL, a PBES2 key derived with scrypt is refused.
//!
//! The passphrase only matters for an encrypted key; an encrypted key without one, or with a wrong one, fails.

use base64::Engine;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer};

/// One PEM block: its label, its headers (`Name: value` lines before the base64) and its decoded contents.
struct Block {
    label: String,
    headers: Vec<(String, String)>,
    der: Vec<u8>,
}

/// The PEM blocks of `text`; a block whose base64 doesn't decode is skipped.
fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) else { continue };
        let end = format!("-----END {label}-----");
        let mut headers = Vec::new();
        let mut body = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line == end {
                closed = true;
                break;
            }
            match line.split_once(':') {
                Some((name, value)) if body.is_empty() => headers.push((name.trim().to_string(), value.trim().to_string())),
                _ => body.push_str(line),
            }
        }
        if !closed {
            break;
        }
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body.as_bytes()) {
            out.push(Block { label: label.to_string(), headers, der });
        }
    }
    out
}

/// The certificate chain of `cert`.
pub fn certificates(cert: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let chain: Vec<CertificateDer<'static>> =
        blocks(cert).into_iter().filter(|b| b.label == "CERTIFICATE" || b.label == "X509 CERTIFICATE").map(|b| CertificateDer::from(b.der)).collect();
    if chain.is_empty() {
        return Err("no certificate found in tls.cert".into());
    }
    Ok(chain)
}

/// The private key of `key`, decrypted with `passphrase` when it is encrypted.
pub fn private_key(key: &str, passphrase: Option<&str>) -> Result<PrivateKeyDer<'static>, String> {
    for block in blocks(key) {
        let encrypted = block.headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("Proc-Type") && v.contains("ENCRYPTED"));
        let der = if encrypted {
            let info = block.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("DEK-Info")).map(|(_, v)| v.as_str());
            let passphrase = passphrase.ok_or("the key is encrypted and tls.passphrase is not set")?;
            legacy_decrypt(&block.der, info.ok_or("DEK-Info missing")?, passphrase)?
        } else {
            block.der
        };
        return match block.label.as_str() {
            "PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der))),
            "RSA PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der))),
            "EC PRIVATE KEY" => Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der))),
            "ENCRYPTED PRIVATE KEY" => {
                let passphrase = passphrase.ok_or("the key is encrypted and tls.passphrase is not set")?;
                Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8_decrypt(&der, passphrase)?)))
            }
            _ => continue,
        };
    }
    Err("no private key found in tls.key".into())
}

/// A PKCS#8 key encrypted with PBES2 (PBKDF2-SHA256, AES-256-CBC), as `openssl pkcs8 -topk8 -v2 aes-256-cbc` makes
/// it: the DER of an `EncryptedPrivateKeyInfo` (tests).
pub fn encrypt_pkcs8(der: &[u8], passphrase: &str) -> Vec<u8> {
    use pkcs8::der::Decode;
    let salt: [u8; 16] = rand::random();
    let iv: [u8; 16] = rand::random();
    let params = pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(2048, &salt, &iv).expect("PBES2 parameters");
    let info = pkcs8::PrivateKeyInfo::from_der(der).expect("a PKCS#8 key");
    info.encrypt_with_params(params, passphrase).expect("encrypt").as_bytes().to_vec()
}

// ------------------------------------------------------------------------------------------------ legacy PEM

/// `EVP_BytesToKey(cipher, MD5, salt = iv[..8], passphrase, count = 1)`: the key bytes.
fn bytes_to_key(passphrase: &[u8], salt: &[u8], len: usize) -> Vec<u8> {
    use md5::Digest;
    let mut out = Vec::with_capacity(len + 16);
    let mut previous: Vec<u8> = Vec::new();
    while out.len() < len {
        let mut hasher = md5::Md5::new();
        hasher.update(&previous);
        hasher.update(passphrase);
        hasher.update(salt);
        previous = hasher.finalize().to_vec();
        out.extend_from_slice(&previous);
    }
    out.truncate(len);
    out
}

fn cbc_decrypt<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String>
where
    C: cipher::BlockDecryptMut + cipher::BlockCipher + cipher::KeyInit,
{
    use cipher::{BlockDecryptMut, KeyIvInit};
    let decryptor = cbc::Decryptor::<C>::new_from_slices(key, iv).map_err(|e| e.to_string())?;
    decryptor.decrypt_padded_vec_mut::<cipher::block_padding::Pkcs7>(data).map_err(|_| "bad decrypt".to_string())
}

/// A traditional OpenSSL encrypted key (`DEK-Info: <cipher>,<hex iv>`).
fn legacy_decrypt(data: &[u8], info: &str, passphrase: &str) -> Result<Vec<u8>, String> {
    let (name, iv) = info.split_once(',').ok_or("malformed DEK-Info")?;
    let iv = hex::decode(iv.trim()).map_err(|_| "malformed DEK-Info")?;
    if iv.len() < 8 {
        return Err("malformed DEK-Info".into());
    }
    let pass = passphrase.as_bytes();
    let salt = &iv[..8];
    match name.trim().to_ascii_uppercase().as_str() {
        "AES-128-CBC" => cbc_decrypt::<aes::Aes128>(&bytes_to_key(pass, salt, 16), &iv, data),
        "AES-192-CBC" => cbc_decrypt::<aes::Aes192>(&bytes_to_key(pass, salt, 24), &iv, data),
        "AES-256-CBC" => cbc_decrypt::<aes::Aes256>(&bytes_to_key(pass, salt, 32), &iv, data),
        "DES-EDE3-CBC" => cbc_decrypt::<des::TdesEde3>(&bytes_to_key(pass, salt, 24), &iv, data),
        "DES-CBC" => cbc_decrypt::<des::Des>(&bytes_to_key(pass, salt, 8), &iv, data),
        other => Err(format!("unsupported key encryption {other}")),
    }
}

// ------------------------------------------------------------------------------------------------ PKCS#8

/// The DER OIDs of PKCS#12 `pbeWithSHAAnd3-KeyTripleDES-CBC` and `pbeWithSHAAnd2-KeyTripleDES-CBC`.
const PBE_SHA1_3DES: [u8; 10] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x01, 0x03];
const PBE_SHA1_2DES: [u8; 10] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x01, 0x04];

/// One DER element: (tag, contents, rest).
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, mut rest) = rest.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
        rest = &rest[n..];
        len
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// The PKCS#12 scheme of an `EncryptedPrivateKeyInfo`: (3-key, salt, iterations, encrypted data).
fn pkcs12_scheme(der: &[u8]) -> Option<(bool, &[u8], u32, &[u8])> {
    let (0x30, info, _) = element(der)? else { return None };
    let (0x30, algorithm, rest) = element(info)? else { return None };
    let (0x04, data, _) = element(rest)? else { return None };
    let (0x06, oid, params) = element(algorithm)? else { return None };
    let three = if oid == PBE_SHA1_3DES {
        true
    } else if oid == PBE_SHA1_2DES {
        false
    } else {
        return None;
    };
    let (0x30, params, _) = element(params)? else { return None };
    let (0x04, salt, rest) = element(params)? else { return None };
    let (0x02, iterations, _) = element(rest)? else { return None };
    if iterations.is_empty() || iterations.len() > 4 {
        return None;
    }
    let iterations = iterations.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32);
    Some((three, salt, iterations, data))
}

/// An `EncryptedPrivateKeyInfo`, decrypted to its `PrivateKeyInfo`.
fn pkcs8_decrypt(der: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    use pkcs8::der::Decode;
    if let Some((three, salt, iterations, data)) = pkcs12_scheme(der) {
        let password = bmp_password(passphrase);
        let key = pkcs12_kdf(&password, salt, 1, iterations, if three { 24 } else { 16 });
        let iv = pkcs12_kdf(&password, salt, 2, iterations, 8);
        let plain = if three { cbc_decrypt::<des::TdesEde3>(&key, &iv, data)? } else { cbc_decrypt::<des::TdesEde2>(&key, &iv, data)? };
        // The decrypted contents must be a `PrivateKeyInfo`.
        pkcs8::PrivateKeyInfo::from_der(&plain).map_err(|e| e.to_string())?;
        return Ok(plain);
    }
    let info = pkcs8::EncryptedPrivateKeyInfo::from_der(der).map_err(|e| e.to_string())?;
    if let pkcs8::pkcs5::EncryptionScheme::Pbes2(params) = &info.encryption_algorithm
        && params.kdf.is_scrypt()
    {
        return Err("scrypt-protected keys are not supported".into());
    }
    let decrypted = info.decrypt(passphrase).map_err(|e| e.to_string())?;
    Ok(decrypted.as_bytes().to_vec())
}

/// The passphrase as a PKCS#12 `BMPString`: big-endian UTF-16 with a terminating zero.
fn bmp_password(passphrase: &str) -> Vec<u8> {
    let mut out: Vec<u8> = passphrase.encode_utf16().flat_map(u16::to_be_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// RFC 7292 appendix B.2 with SHA-1 (`u` = 20, `v` = 64).
fn pkcs12_kdf(password: &[u8], salt: &[u8], id: u8, iterations: u32, len: usize) -> Vec<u8> {
    use sha1::Digest;
    const V: usize = 64;
    let fill = |input: &[u8]| -> Vec<u8> {
        if input.is_empty() {
            return Vec::new();
        }
        let n = V * input.len().div_ceil(V);
        input.iter().cycle().take(n).copied().collect()
    };
    let mut i: Vec<u8> = fill(salt);
    i.extend(fill(password));
    let d = [id; V];
    let mut out = Vec::with_capacity(len);
    loop {
        let mut a = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(d);
            hasher.update(&i);
            hasher.finalize().to_vec()
        };
        for _ in 1..iterations.max(1) {
            a = sha1::Sha1::digest(&a).to_vec();
        }
        out.extend_from_slice(&a);
        if out.len() >= len {
            out.truncate(len);
            return out;
        }
        let b: Vec<u8> = a.iter().cycle().take(V).copied().collect();
        for chunk in i.chunks_mut(V) {
            // chunk = (chunk + b + 1) mod 2^(8v)
            let mut carry = 1u16;
            for k in (0..V).rev() {
                let sum = chunk[k] as u16 + b[k] as u16 + carry;
                chunk[k] = sum as u8;
                carry = sum >> 8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkcs12_kdf_matches_the_rfc_vectors() {
        // OpenSSL's PKCS#12 KDF test vector (password "smeg", salt 0A58CF64530D823F, 1 iteration, id 1, 24 bytes).
        let key = pkcs12_kdf(&bmp_password("smeg"), &hex::decode("0A58CF64530D823F").unwrap(), 1, 1, 24);
        assert_eq!(hex::encode_upper(key), "8AAAE6297B6CB04642AB5B077851284EB7128F1A2A7FBCA3");
        let iv = pkcs12_kdf(&bmp_password("smeg"), &hex::decode("0A58CF64530D823F").unwrap(), 2, 1, 8);
        assert_eq!(hex::encode_upper(iv), "79993DFE048D3B76");
    }

    #[test]
    fn blocks_and_headers() {
        let text = "junk\n-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nAQID\n-----END RSA PRIVATE KEY-----\n";
        let parsed = blocks(text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "RSA PRIVATE KEY");
        assert_eq!(parsed[0].headers.len(), 2);
        assert_eq!(parsed[0].der, vec![1, 2, 3]);
        assert!(certificates("garbage").is_err());
        assert!(private_key("garbage", None).is_err());
    }

    fn pem(label: &str, headers: &[(&str, &str)], der: &[u8]) -> String {
        let mut out = format!("-----BEGIN {label}-----\n");
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\n"));
        }
        if !headers.is_empty() {
            out.push('\n');
        }
        let text = base64::engine::general_purpose::STANDARD.encode(der);
        for chunk in text.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(chunk).unwrap());
            out.push('\n');
        }
        out.push_str(&format!("-----END {label}-----\n"));
        out
    }

    fn cbc_encrypt<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8>
    where
        C: cipher::BlockEncryptMut + cipher::BlockCipher + cipher::KeyInit,
    {
        use cipher::{BlockEncryptMut, KeyIvInit};
        cbc::Encryptor::<C>::new_from_slices(key, iv).unwrap().encrypt_padded_vec_mut::<cipher::block_padding::Pkcs7>(data)
    }

    fn der_element(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if contents.len() < 0x80 {
            out.push(contents.len() as u8);
        } else {
            let len = (contents.len() as u32).to_be_bytes();
            let skip = len.iter().position(|b| *b != 0).unwrap();
            out.push(0x80 | (4 - skip) as u8);
            out.extend_from_slice(&len[skip..]);
        }
        out.extend_from_slice(contents);
        out
    }

    fn key_der() -> Vec<u8> {
        rcgen::KeyPair::generate().unwrap().serialize_der()
    }

    fn pkcs8_of(key: PrivateKeyDer<'static>) -> Vec<u8> {
        match key {
            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der().to_vec(),
            other => panic!("not PKCS#8: {other:?}"),
        }
    }

    #[test]
    fn plain_and_pbes2_encrypted_pkcs8() {
        use pkcs8::der::Decode;
        let der = key_der();
        assert_eq!(pkcs8_of(private_key(&pem("PRIVATE KEY", &[], &der), None).unwrap()), der);
        // A passphrase for an unencrypted key is ignored.
        assert_eq!(pkcs8_of(private_key(&pem("PRIVATE KEY", &[], &der), Some("x")).unwrap()), der);

        let info = pkcs8::PrivateKeyInfo::from_der(&der).unwrap();
        let params = pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(2048, &[7; 16], &[9; 16]).unwrap();
        let encrypted = info.encrypt_with_params(params, "secret").unwrap();
        let text = pem("ENCRYPTED PRIVATE KEY", &[], encrypted.as_bytes());
        assert_eq!(pkcs8_of(private_key(&text, Some("secret")).unwrap()), der);
        assert!(private_key(&text, Some("wrong")).is_err());
        assert!(private_key(&text, None).is_err());

        // BoringSSL refuses scrypt.
        let params = pkcs8::pkcs5::pbes2::Parameters::scrypt_aes256cbc(pkcs8::pkcs5::scrypt::Params::new(4, 8, 1, 32).unwrap(), &[7; 16], &[9; 16]).unwrap();
        let encrypted = info.encrypt_with_params(params, "secret").unwrap();
        assert!(private_key(&pem("ENCRYPTED PRIVATE KEY", &[], encrypted.as_bytes()), Some("secret")).is_err());
    }

    #[test]
    fn pkcs12_3des_encrypted_pkcs8() {
        let der = key_der();
        let salt = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let password = bmp_password("secret");
        for (three, oid) in [(true, PBE_SHA1_3DES), (false, PBE_SHA1_2DES)] {
            let key = pkcs12_kdf(&password, &salt, 1, 2048, if three { 24 } else { 16 });
            let iv = pkcs12_kdf(&password, &salt, 2, 2048, 8);
            let data = if three { cbc_encrypt::<des::TdesEde3>(&key, &iv, &der) } else { cbc_encrypt::<des::TdesEde2>(&key, &iv, &der) };
            let params = der_element(0x30, &[der_element(0x04, &salt), der_element(0x02, &[0x08, 0x00])].concat());
            let algorithm = der_element(0x30, &[der_element(0x06, &oid), params].concat());
            let info = der_element(0x30, &[algorithm, der_element(0x04, &data)].concat());
            let text = pem("ENCRYPTED PRIVATE KEY", &[], &info);
            assert_eq!(pkcs8_of(private_key(&text, Some("secret")).unwrap()), der);
            assert!(private_key(&text, Some("nope")).is_err());
        }
    }

    #[test]
    fn legacy_openssl_encryption() {
        let der = key_der();
        let iv = [0x11u8; 16];
        let hex_iv = hex::encode_upper(iv);
        let cases: [(&str, Vec<u8>); 3] = [
            ("AES-256-CBC", cbc_encrypt::<aes::Aes256>(&bytes_to_key(b"secret", &iv[..8], 32), &iv, &der)),
            ("AES-128-CBC", cbc_encrypt::<aes::Aes128>(&bytes_to_key(b"secret", &iv[..8], 16), &iv, &der)),
            ("DES-EDE3-CBC", cbc_encrypt::<des::TdesEde3>(&bytes_to_key(b"secret", &iv[..8], 24), &iv[..8], &der)),
        ];
        for (name, data) in cases {
            let dek = if name.starts_with("DES") { format!("{name},{}", &hex_iv[..16]) } else { format!("{name},{hex_iv}") };
            let text = pem("EC PRIVATE KEY", &[("Proc-Type", "4,ENCRYPTED"), ("DEK-Info", &dek)], &data);
            match private_key(&text, Some("secret")).unwrap() {
                PrivateKeyDer::Sec1(k) => assert_eq!(k.secret_sec1_der(), &der[..], "{name}"),
                other => panic!("{other:?}"),
            }
            assert!(private_key(&text, None).is_err());
        }
    }

    #[test]
    fn certificate_chains() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap().self_signed(&key).unwrap();
        let chain = certificates(&format!("{}\n{}", cert.pem(), cert.pem())).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].as_ref(), cert.der().as_ref());
    }
}
