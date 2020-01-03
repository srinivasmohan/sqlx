use digest::Digest;
use num_bigint::BigUint;
use rand::{thread_rng, Rng};

// This is mostly taken from https://github.com/RustCrypto/RSA/pull/18
// For the love of crypto, please delete as much of this as possible and use the RSA crate
// directly when that PR is merged

pub fn encrypt<D: Digest>(key: &[u8], message: &[u8]) -> crate::Result<Box<[u8]>> {
    let key = std::str::from_utf8(key).map_err(|_err| {
        // TODO(@abonander): protocol_err doesn't like referring to [err]
        protocol_err!("unexpected error decoding what should be UTF-8")
    })?;

    let key = parse(key)?;

    Ok(oaep_encrypt::<_, D>(&mut thread_rng(), &key, message)?.into_boxed_slice())
}

// https://github.com/RustCrypto/RSA/blob/9f1464c43831d422d9903574aad6ab072db9f2b0/src/internals.rs#L12
fn internals_encrypt(key: &PublicKey, m: &BigUint) -> BigUint {
    m.modpow(&key.e, &key.n)
}

// https://github.com/RustCrypto/RSA/blob/9f1464c43831d422d9903574aad6ab072db9f2b0/src/internals.rs#L184
fn internals_copy_with_left_pad(dest: &mut [u8], src: &[u8]) {
    // left pad with zeros
    let padding_bytes = dest.len() - src.len();
    for el in dest.iter_mut().take(padding_bytes) {
        *el = 0;
    }
    dest[padding_bytes..].copy_from_slice(src);
}

// https://github.com/RustCrypto/RSA/blob/9f1464c43831d422d9903574aad6ab072db9f2b0/src/oaep.rs#L13
fn internals_inc_counter(counter: &mut [u8]) {
    if counter[3] == u8::max_value() {
        counter[3] = 0;
    } else {
        counter[3] += 1;
        return;
    }

    if counter[2] == u8::max_value() {
        counter[2] = 0;
    } else {
        counter[2] += 1;
        return;
    }

    if counter[1] == u8::max_value() {
        counter[1] = 0;
    } else {
        counter[1] += 1;
        return;
    }

    if counter[0] == u8::max_value() {
        counter[0] = 0u8;
        counter[1] = 0u8;
        counter[2] = 0u8;
        counter[3] = 0u8;
    } else {
        counter[0] += 1;
    }
}

// https://github.com/RustCrypto/RSA/blob/9f1464c43831d422d9903574aad6ab072db9f2b0/src/oaep.rs#L46
fn oeap_mgf1_xor<D: Digest>(out: &mut [u8], digest: &mut D, seed: &[u8]) {
    let mut counter = vec![0u8; 4];
    let mut i = 0;

    while i < out.len() {
        let mut digest_input = vec![0u8; seed.len() + 4];
        digest_input[0..seed.len()].copy_from_slice(seed);
        digest_input[seed.len()..].copy_from_slice(&counter);

        digest.input(digest_input.as_slice());
        let digest_output = &*digest.result_reset();
        let mut j = 0;
        loop {
            if j >= digest_output.len() || i >= out.len() {
                break;
            }

            out[i] ^= digest_output[j];
            j += 1;
            i += 1;
        }
        internals_inc_counter(counter.as_mut_slice());
    }
}

// https://github.com/RustCrypto/RSA/blob/9f1464c43831d422d9903574aad6ab072db9f2b0/src/oaep.rs#L75
fn oaep_encrypt<R: Rng, D: Digest>(
    rng: &mut R,
    pub_key: &PublicKey,
    msg: &[u8],
) -> crate::Result<Vec<u8>> {
    // size of [n] in bytes
    let k = (pub_key.n.bits() + 7) / 8;

    let mut digest = D::new();
    let h_size = D::output_size();

    if msg.len() > k - 2 * h_size - 2 {
        return Err(protocol_err!("mysql: password too long").into());
    }

    let mut em = vec![0u8; k];

    let (_, payload) = em.split_at_mut(1);
    let (seed, db) = payload.split_at_mut(h_size);
    rng.fill(seed);

    // Data block DB =  pHash || PS || 01 || M
    let db_len = k - h_size - 1;

    let p_hash = digest.result_reset();
    db[0..h_size].copy_from_slice(&*p_hash);
    db[db_len - msg.len() - 1] = 1;
    db[db_len - msg.len()..].copy_from_slice(msg);

    oeap_mgf1_xor(db, &mut digest, seed);
    oeap_mgf1_xor(seed, &mut digest, db);

    {
        let m = BigUint::from_bytes_be(&em);
        let c = internals_encrypt(pub_key, &m).to_bytes_be();

        internals_copy_with_left_pad(&mut em, &c);
    }

    Ok(em)
}

#[derive(Debug)]
struct PublicKey {
    n: BigUint,
    e: BigUint,
}

fn parse(key: &str) -> crate::Result<PublicKey> {
    // This takes advantage of the knowledge that we know
    // we are receiving a PKCS#8 RSA Public Key at all
    // times from MySQL

    if !key.starts_with("-----BEGIN PUBLIC KEY-----\n") {
        return Err(protocol_err!(
            "unexpected format for RSA Public Key from MySQL (expected PKCS#8); first line: {:?}",
            key.splitn(1, '\n').next()
        )
        .into());
    }

    let key_with_trailer = key.trim_start_matches("-----BEGIN PUBLIC KEY-----\n");
    let trailer_pos = key_with_trailer.find('-').unwrap_or(0);
    let inner_key = key_with_trailer[..trailer_pos].replace('\n', "");

    let inner = base64::decode(&inner_key).map_err(|_err| {
        // TODO(@abonander): protocol_err doesn't like referring to [err]
        protocol_err!("unexpected error decoding what should be base64-encoded data")
    })?;

    let len = inner.len();

    let n_bytes = &inner[(len - 257 - 5)..(len - 5)];
    let e_bytes = &inner[(len - 3)..];

    let n = BigUint::from_bytes_be(n_bytes);
    let e = BigUint::from_bytes_be(e_bytes);

    Ok(PublicKey { n, e })
}

#[cfg(test)]
mod tests {
    use super::{BigUint, PublicKey};
    use rand::rngs::adapter::ReadRng;
    use sha1::Sha1;
    use sha2::Sha256;

    const INPUT: &str = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAv9E+l0oFIoGnZmu6bdil\nI3WK79iug/hukj5QrWRrJVVCHL8rRxNsQGYPvQfXgqEnJW0Rqy2BBebNrnSMduny\nCazz1KM1h57hSI1xHGhg/o82Us1j9fUucKo0Pt3vg7xjVVcN0j1bwr96gEbt6B4Q\nt4eKZBhtle1bgoBcqFBhGfU17cnedSzMUCutM+kXTzzOTplKoqXeJpEZDTX8AP9F\nQ9JkoA22yTn8H2GROIAffm1UQS7DXXjI5OnzBJNs72oNSeK8i72xLkoSdfVw3vCu\ni+mpt4LJgAZLvzc2O4nLzu4Bljb+Mrch34HSWyxOfWzt1v9vpJfEVQ2/VZaIng6U\nUQIDAQAB\n-----END PUBLIC KEY-----\n";

    #[test]
    fn it_parses() {
        let key = super::parse(INPUT).unwrap();

        let n = &[
            0xbf, 0xd1, 0x3e, 0x97, 0x4a, 0x5, 0x22, 0x81, 0xa7, 0x66, 0x6b, 0xba, 0x6d, 0xd8,
            0xa5, 0x23, 0x75, 0x8a, 0xef, 0xd8, 0xae, 0x83, 0xf8, 0x6e, 0x92, 0x3e, 0x50, 0xad,
            0x64, 0x6b, 0x25, 0x55, 0x42, 0x1c, 0xbf, 0x2b, 0x47, 0x13, 0x6c, 0x40, 0x66, 0xf,
            0xbd, 0x7, 0xd7, 0x82, 0xa1, 0x27, 0x25, 0x6d, 0x11, 0xab, 0x2d, 0x81, 0x5, 0xe6, 0xcd,
            0xae, 0x74, 0x8c, 0x76, 0xe9, 0xf2, 0x9, 0xac, 0xf3, 0xd4, 0xa3, 0x35, 0x87, 0x9e,
            0xe1, 0x48, 0x8d, 0x71, 0x1c, 0x68, 0x60, 0xfe, 0x8f, 0x36, 0x52, 0xcd, 0x63, 0xf5,
            0xf5, 0x2e, 0x70, 0xaa, 0x34, 0x3e, 0xdd, 0xef, 0x83, 0xbc, 0x63, 0x55, 0x57, 0xd,
            0xd2, 0x3d, 0x5b, 0xc2, 0xbf, 0x7a, 0x80, 0x46, 0xed, 0xe8, 0x1e, 0x10, 0xb7, 0x87,
            0x8a, 0x64, 0x18, 0x6d, 0x95, 0xed, 0x5b, 0x82, 0x80, 0x5c, 0xa8, 0x50, 0x61, 0x19,
            0xf5, 0x35, 0xed, 0xc9, 0xde, 0x75, 0x2c, 0xcc, 0x50, 0x2b, 0xad, 0x33, 0xe9, 0x17,
            0x4f, 0x3c, 0xce, 0x4e, 0x99, 0x4a, 0xa2, 0xa5, 0xde, 0x26, 0x91, 0x19, 0xd, 0x35,
            0xfc, 0x0, 0xff, 0x45, 0x43, 0xd2, 0x64, 0xa0, 0xd, 0xb6, 0xc9, 0x39, 0xfc, 0x1f, 0x61,
            0x91, 0x38, 0x80, 0x1f, 0x7e, 0x6d, 0x54, 0x41, 0x2e, 0xc3, 0x5d, 0x78, 0xc8, 0xe4,
            0xe9, 0xf3, 0x4, 0x93, 0x6c, 0xef, 0x6a, 0xd, 0x49, 0xe2, 0xbc, 0x8b, 0xbd, 0xb1, 0x2e,
            0x4a, 0x12, 0x75, 0xf5, 0x70, 0xde, 0xf0, 0xae, 0x8b, 0xe9, 0xa9, 0xb7, 0x82, 0xc9,
            0x80, 0x6, 0x4b, 0xbf, 0x37, 0x36, 0x3b, 0x89, 0xcb, 0xce, 0xee, 0x1, 0x96, 0x36, 0xfe,
            0x32, 0xb7, 0x21, 0xdf, 0x81, 0xd2, 0x5b, 0x2c, 0x4e, 0x7d, 0x6c, 0xed, 0xd6, 0xff,
            0x6f, 0xa4, 0x97, 0xc4, 0x55, 0xd, 0xbf, 0x55, 0x96, 0x88, 0x9e, 0xe, 0x94, 0x51,
        ][..];

        let e = &[0x1, 0x0, 0x1][..];

        assert_eq!(key.n.to_bytes_be(), n);
        assert_eq!(key.e.to_bytes_be(), e);
    }

    #[test]
    fn it_encrypts_sha1() {
        // https://github.com/pyca/cryptography/blob/master/vectors/cryptography_vectors/asymmetric/RSA/pkcs-1v2-1d2-vec/oaep-int.txt

        let n = BigUint::from_bytes_be(&[
            0xbb, 0xf8, 0x2f, 0x09, 0x06, 0x82, 0xce, 0x9c, 0x23, 0x38, 0xac, 0x2b, 0x9d, 0xa8,
            0x71, 0xf7, 0x36, 0x8d, 0x07, 0xee, 0xd4, 0x10, 0x43, 0xa4, 0x40, 0xd6, 0xb6, 0xf0,
            0x74, 0x54, 0xf5, 0x1f, 0xb8, 0xdf, 0xba, 0xaf, 0x03, 0x5c, 0x02, 0xab, 0x61, 0xea,
            0x48, 0xce, 0xeb, 0x6f, 0xcd, 0x48, 0x76, 0xed, 0x52, 0x0d, 0x60, 0xe1, 0xec, 0x46,
            0x19, 0x71, 0x9d, 0x8a, 0x5b, 0x8b, 0x80, 0x7f, 0xaf, 0xb8, 0xe0, 0xa3, 0xdf, 0xc7,
            0x37, 0x72, 0x3e, 0xe6, 0xb4, 0xb7, 0xd9, 0x3a, 0x25, 0x84, 0xee, 0x6a, 0x64, 0x9d,
            0x06, 0x09, 0x53, 0x74, 0x88, 0x34, 0xb2, 0x45, 0x45, 0x98, 0x39, 0x4e, 0xe0, 0xaa,
            0xb1, 0x2d, 0x7b, 0x61, 0xa5, 0x1f, 0x52, 0x7a, 0x9a, 0x41, 0xf6, 0xc1, 0x68, 0x7f,
            0xe2, 0x53, 0x72, 0x98, 0xca, 0x2a, 0x8f, 0x59, 0x46, 0xf8, 0xe5, 0xfd, 0x09, 0x1d,
            0xbd, 0xcb,
        ]);

        let e = BigUint::from_bytes_be(&[0x11]);

        let pub_key = PublicKey { n, e };

        let message = &[
            0xd4, 0x36, 0xe9, 0x95, 0x69, 0xfd, 0x32, 0xa7, 0xc8, 0xa0, 0x5b, 0xbc, 0x90, 0xd3,
            0x2c, 0x49,
        ];

        let mut seed = &[
            0xaa, 0xfd, 0x12, 0xf6, 0x59, 0xca, 0xe6, 0x34, 0x89, 0xb4, 0x79, 0xe5, 0x07, 0x6d,
            0xde, 0xc2, 0xf0, 0x6c, 0xb5, 0x8f,
        ][..];

        let mut rng = ReadRng::new(seed);
        let cipher_text = super::oaep_encrypt::<_, Sha1>(&mut rng, &pub_key, message).unwrap();

        let expected_cipher_text = &[
            0x12, 0x53, 0xe0, 0x4d, 0xc0, 0xa5, 0x39, 0x7b, 0xb4, 0x4a, 0x7a, 0xb8, 0x7e, 0x9b,
            0xf2, 0xa0, 0x39, 0xa3, 0x3d, 0x1e, 0x99, 0x6f, 0xc8, 0x2a, 0x94, 0xcc, 0xd3, 0x00,
            0x74, 0xc9, 0x5d, 0xf7, 0x63, 0x72, 0x20, 0x17, 0x06, 0x9e, 0x52, 0x68, 0xda, 0x5d,
            0x1c, 0x0b, 0x4f, 0x87, 0x2c, 0xf6, 0x53, 0xc1, 0x1d, 0xf8, 0x23, 0x14, 0xa6, 0x79,
            0x68, 0xdf, 0xea, 0xe2, 0x8d, 0xef, 0x04, 0xbb, 0x6d, 0x84, 0xb1, 0xc3, 0x1d, 0x65,
            0x4a, 0x19, 0x70, 0xe5, 0x78, 0x3b, 0xd6, 0xeb, 0x96, 0xa0, 0x24, 0xc2, 0xca, 0x2f,
            0x4a, 0x90, 0xfe, 0x9f, 0x2e, 0xf5, 0xc9, 0xc1, 0x40, 0xe5, 0xbb, 0x48, 0xda, 0x95,
            0x36, 0xad, 0x87, 0x00, 0xc8, 0x4f, 0xc9, 0x13, 0x0a, 0xde, 0xa7, 0x4e, 0x55, 0x8d,
            0x51, 0xa7, 0x4d, 0xdf, 0x85, 0xd8, 0xb5, 0x0d, 0xe9, 0x68, 0x38, 0xd6, 0x06, 0x3e,
            0x09, 0x55,
        ][..];

        assert_eq!(&*expected_cipher_text, &*cipher_text);
    }
}
