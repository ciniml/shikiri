//! TLS for the control server and DERP (embedded-tls, TLS 1.3): the
//! certificate chain to one of Let's Encrypt's roots, the name checked.
//! Tailscale's servers present an ECDSA chain (to ISRG Root X2) or an RSA
//! one (to ISRG Root X1): both were seen on 2026-11-11. No clock yet: the
//! validity dates are not checked.

use embedded_tls::pki::CertVerifier;
use embedded_tls::{Aes128GcmSha256, Certificate, CryptoProvider, CryptoRngCore, NoClock, TlsError, TlsVerifier};

/// ISRG Root X2 (DER; SHA-256 69:72:9B:8E:…:14:70).
pub const ISRG_ROOT_X2: &[u8] = include_bytes!("isrg-root-x2.der");
/// ISRG Root X1 (DER; SHA-256 96:BC:EC:06:…:08:C6).
pub const ISRG_ROOT_X1: &[u8] = include_bytes!("isrg-root-x1.der");
static MORE_ROOTS: [Certificate<&[u8]>; 1] = [Certificate::X509(ISRG_ROOT_X1)];

pub type Suite = Aes128GcmSha256;

/// TLS record buffers: a whole record (16 KiB and the overhead) each way
/// for reading; writes are small.
pub const READ_BUF: usize = 16640;
pub const WRITE_BUF: usize = 4096;

pub struct Provider<R> {
    rng: R,
    verifier: CertVerifier<'static, Suite, NoClock, 4096>,
}

impl<R: CryptoRngCore> Provider<R> {
    pub fn new(rng: R) -> Provider<R> {
        Provider { rng, verifier: CertVerifier::new(Certificate::X509(ISRG_ROOT_X2)).with_more_roots(&MORE_ROOTS) }
    }
}

impl<R: CryptoRngCore> CryptoProvider for Provider<R> {
    type CipherSuite = Suite;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Suite>, TlsError> {
        Ok(&mut self.verifier)
    }
}
