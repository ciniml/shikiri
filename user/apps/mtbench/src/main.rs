#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! Matter MT0 (doc/49): simple-matter's software crypto on this OS, timed
//! (SHA-256, HMAC, HKDF, AES-CCM, P-256 key generation, ECDSA, ECDH, the
//! SPAKE2+ verifier with PBKDF2 and both PASE sides), then the key-value
//! store on the SD card (set, get, overwrite, remove). `run mtbench`.
//! Built with `--permit random --permit svc:fs`. Exit 0, or the number of
//! the failed check.

use mtport::{FileKvs, KernelRng};
use simple_matter::crypto::rustcrypto::RustCrypto;
use simple_matter::crypto::spake2p::{self, Spake2pProver, Spake2pVerifier};
use simple_matter::crypto::{Crypto, P256Keypair, P256PublicKey};
use simple_matter::kvs::Kvs;
use t5_rt::{event, time_ns};

t5_rt::entry!(main);

/// Matter's minimum PBKDF2 iteration count for the PASE verifier.
const ITERATIONS: u32 = 1000;
const PASSCODE: u32 = 20202021;

/// Runs `f` `n` times; µs per run.
fn time<R>(n: u32, mut f: impl FnMut() -> R) -> (u32, R) {
    let start = time_ns();
    let mut r = f();
    for _ in 1..n {
        r = f();
    }
    (((time_ns() - start) / 1000 / n as u64) as u32, r)
}

fn main(_: usize, _: usize) -> usize {
    let crypto = RustCrypto::new(KernelRng);
    let mut data = [0u8; 16 * 1024];
    for (i, b) in data.iter_mut().enumerate() {
        *b = i as u8;
    }

    let mut out = [0u8; 32];
    let (us, _) = time(4, || crypto.sha256_oneshot(&data, &mut out));
    event!(Info, "mtbench.sha256", bytes = data.len() as u32, us = us, kib_s = (data.len() as u64 * 1_000_000 / 1024 / us.max(1) as u64) as u32);
    let (us, _) = time(100, || crypto.hmac_sha256(b"key", &data[..64], &mut out));
    event!(Info, "mtbench.hmac", bytes = 64u32, us = us);
    let mut keys = [0u8; 48];
    let (us, _) = time(100, || crypto.hkdf_sha256(b"salt", &data[..32], b"SessionKeys", &mut keys));
    event!(Info, "mtbench.hkdf", out = 48u32, us = us);

    let key = [7u8; 16];
    let nonce = [1u8; 13];
    let mut buf = [0u8; 1024 + 16];
    let (us, r) = time(20, || {
        buf[..1024].copy_from_slice(&data[..1024]);
        crypto.aes_ccm_encrypt(&key, &nonce, b"aad", &mut buf, 1024).map(|c| c.len())
    });
    if r != Ok(1024 + 16) {
        return 1;
    }
    let (us_dec, r) = time(1, || crypto.aes_ccm_decrypt(&key, &nonce, b"aad", &mut buf).map(|p| p == &data[..1024]));
    if r != Ok(true) {
        return 2;
    }
    event!(Info, "mtbench.aes_ccm", bytes = 1024u32, encrypt_us = us, decrypt_us = us_dec);

    let (us_gen, kp) = time(4, || crypto.p256_generate_keypair());
    let Ok(kp) = kp else { return 3 };
    let mut sig = [0u8; 64];
    let (us_sign, r) = time(4, || kp.sign(&data[..256], &mut sig));
    if r.is_err() {
        return 4;
    }
    let public = kp.public_key();
    let (us_verify, ok) = time(4, || public.verify(&data[..256], &sig));
    if ok != Ok(true) {
        return 5;
    }
    let Ok(peer) = crypto.p256_generate_keypair() else { return 6 };
    let peer_public = peer.public_key();
    let mut shared = [0u8; 32];
    let (us_ecdh, r) = time(4, || kp.ecdh(&peer_public, &mut shared));
    if r.is_err() {
        return 6;
    }
    event!(Info, "mtbench.p256", keygen_us = us_gen, sign_us = us_sign, verify_us = us_verify, ecdh_us = us_ecdh);

    // PASE: the device's verifier (PBKDF2), then one exchange.
    let salt = [0x5Au8; 16];
    let (us_verifier, params) = time(1, || spake2p::compute_verifier(PASSCODE, &salt, ITERATIONS));
    let Ok(params) = params else { return 7 };
    let mut rng = KernelRng;
    let (us_prover, prover) = time(1, || Spake2pProver::from_passcode(&mut rng, PASSCODE, &salt, ITERATIONS));
    let Ok(prover) = prover else { return 8 };
    let context = [0u8; 32];
    let (us_respond, r) = time(1, || Spake2pVerifier::respond(&mut rng, &context, &params, prover.share()));
    let Ok((verifier, pb)) = r else { return 9 };
    let (us_confirm, confirm) = time(1, || prover.confirm(&context, &pb));
    let Ok(confirm) = confirm else { return 10 };
    if verifier.verify(confirm.confirmation_a()).is_err() || confirm.verify_b(verifier.confirmation_b()).is_err() || confirm.shared_secret() != verifier.shared_secret() {
        return 11;
    }
    event!(Info, "mtbench.pase", iterations = ITERATIONS, verifier_us = us_verifier, prover_us = us_prover, respond_us = us_respond, confirm_us = us_confirm);

    // The key-value store on the SD card.
    let Ok(mut kvs) = FileKvs::open() else {
        event!(Error, "mtbench.no_fs");
        return 12;
    };
    let mut got = [0u8; 2048];
    let _ = kvs.remove(b"test");
    let (us_set, r) = time(1, || kvs.set(b"test", &data[..1500]));
    if r.is_err() {
        return 13;
    }
    let (us_get, r) = time(1, || kvs.get(b"test", &mut got));
    if r != Ok(Some(1500)) || got[..1500] != data[..1500] {
        return 14;
    }
    // The second write goes to the other file; the newer one wins.
    let (us_set2, r) = time(1, || kvs.set(b"test", &data[100..1100]));
    if r.is_err() || kvs.get(b"test", &mut got) != Ok(Some(1000)) || got[..1000] != data[100..1100] {
        return 15;
    }
    let r = kvs.set(b"test", &data[7..19]).and_then(|_| kvs.get(b"test", &mut got));
    if r != Ok(Some(12)) || got[..12] != data[7..19] {
        return 16;
    }
    let (us_remove, r) = time(1, || kvs.remove(b"test"));
    if r.is_err() || kvs.get(b"test", &mut got) != Ok(None) {
        return 17;
    }
    event!(Info, "mtbench.kvs", set_us = us_set, get_us = us_get, overwrite_us = us_set2, remove_us = us_remove);
    event!(Info, "mtbench.done", ok = true);
    0
}
