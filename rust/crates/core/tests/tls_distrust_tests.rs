//! Pins the TLS trust decisions pay inherits from `rustls-webpki` and
//! `webpki-roots` (reqwest's `rustls-tls` backend compiles these in; pay
//! never consults the OS store on that path).
//!
//! Regression: the eight eMudhra intermediates whose private keys are
//! recoverable from their public certificates (emudhra.we-have-a-problem.com,
//! 20 Oct 2026) must be rejected even though the chains are otherwise valid
//! and no revocation data is supplied. `rustls-webpki` 0.103.16 distrusts
//! their SPKIs; `webpki-roots` 1.0.10 drops the emSign ECC Root CA - G3.
//! A lock downgrade of either crate makes these tests fail.

use std::sync::Arc;
use std::time::Duration;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, Error, RootCertStore};

/// 2026-10-21T00:00:00Z: inside every fixture leaf's validity window.
const CAPTURE_TIME: UnixTime = UnixTime::since_unix_epoch(Duration::from_secs(1_792_540_800));

fn verifier(roots: RootCertStore) -> Arc<WebPkiServerVerifier> {
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(default_provider()))
        .build()
        .expect("verifier")
}

fn public_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

fn verify(
    verifier: &WebPkiServerVerifier,
    chain: &[CertificateDer<'static>],
    host: &str,
) -> Result<(), Error> {
    let (leaf, intermediates) = chain.split_first().expect("non-empty chain");
    verifier
        .verify_server_cert(
            leaf,
            intermediates,
            &ServerName::try_from(host.to_owned()).expect("server name"),
            &[],
            CAPTURE_TIME,
        )
        .map(|_| ())
}

fn fixture_chain(host: &str) -> Vec<CertificateDer<'static>> {
    let pem = std::fs::read(format!(
        "{}/tests/fixtures/tls/{host}.pem",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    let chain: Vec<_> = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<_, _>>()
        .expect("fixture PEM");
    assert!(
        chain.len() >= 2,
        "fixture must carry leaf + intermediate(s)"
    );
    chain
}

/// Chains served by the advisory's test hosts: leaf, the compromised
/// `EM DV TLS CA - G3B`, then either eMudhra's own root path or the Sectigo
/// `AAA Certificate Services` cross-sign (that root stays trusted).
const COMPROMISED_HOSTS: &[&str] = &[
    "dv-g3b-emudhra.we-have-a-problem.com",
    "dv-g3b-aaa-emudhra.we-have-a-problem.com",
];

#[test]
fn compromised_emudhra_chains_are_rejected() {
    let verifier = verifier(public_roots());
    for host in COMPROMISED_HOSTS {
        let err = verify(&verifier, &fixture_chain(host), host)
            .expect_err("chain through a compromised eMudhra intermediate verified");
        // Distrust must be the reason, not a broken fixture, a clock mismatch or
        // an unsupported algorithm. rustls maps webpki's `CertDistrusted` to
        // `Other` until it grows a dedicated variant.
        match err {
            Error::InvalidCertificate(CertificateError::Other(ref inner))
                if format!("{inner:?}").contains("CertDistrusted") => {}
            Error::InvalidCertificate(CertificateError::UnknownIssuer) => {}
            other => panic!("{host}: rejected for the wrong reason: {other:?}"),
        }
    }
}

#[test]
fn emsign_ecc_root_g3_is_not_a_trust_anchor() {
    // Subject DN of "emSign ECC Root CA - G3" (removed in webpki-roots 1.0.10).
    let removed = b"emSign ECC Root CA - G3";
    let present = webpki_roots::TLS_SERVER_ROOTS.iter().any(|ta| {
        ta.subject
            .as_ref()
            .windows(removed.len())
            .any(|w| w == removed)
    });
    assert!(!present, "webpki-roots still ships emSign ECC Root CA - G3");
}

/// Positive control: the same verifier and capture time accept a well-formed
/// chain rooted in an anchor we trust, so the failures above are the distrust
/// logic and not the harness.
#[test]
fn private_chain_through_trusted_root_is_accepted() {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};

    let host = "gateway.pay.test";
    let mut root_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let root = CertifiedIssuer::self_signed(root_params, KeyPair::generate().unwrap()).unwrap();

    let mut ica_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ica_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let ica = CertifiedIssuer::signed_by(ica_params, KeyPair::generate().unwrap(), &root).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![host.to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &ica)
        .unwrap();

    let mut roots = public_roots();
    roots.add(root.der().clone()).unwrap();
    let with_private_root = verifier(roots);
    let chain = vec![leaf.der().clone(), ica.der().clone()];

    verify(&with_private_root, &chain, host).expect("private chain through trusted root");
    // And the same chain against the public bundle alone is unknown, which
    // confirms `verify` is not a no-op.
    assert!(matches!(
        verify(&verifier(public_roots()), &chain, host),
        Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
    ));
}
