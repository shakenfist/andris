//! The TLS listener's configuration, from PEM files.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::CertificateDer;

use crate::config::open_private_file;

/// Build a TLS server configuration from a PEM certificate chain, leaf
/// first, and a PEM private key. Clients are not asked for certificates.
///
/// The key file must pass [`open_private_file`]: a regular file owned by
/// andris's user, whose mode gives group and other no access.
pub fn load_server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>> {
    let cert_shown = cert.display();
    let key_shown = key.display();

    let mut reader = BufReader::new(
        File::open(cert).with_context(|| format!("--tls-cert {cert_shown} cannot be read"))?,
    );
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<CertificateDer<'static>>, _>>()
        .with_context(|| format!("--tls-cert {cert_shown} is not valid PEM"))?;
    ensure!(
        !chain.is_empty(),
        "--tls-cert {cert_shown} holds no certificate: give a PEM certificate chain"
    );

    let mut reader = BufReader::new(open_private_file(key, "--tls-key")?);
    let private_key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("--tls-key {key_shown} is not valid PEM"))?
        .with_context(|| {
            format!("--tls-key {key_shown} holds no private key: give a PEM private key")
        })?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, private_key)
        .with_context(|| {
            format!("--tls-cert {cert_shown} and --tls-key {key_shown} do not make a usable pair")
        })?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::NamedTempFile;

    fn pem_file(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    #[test]
    fn loads_a_certificate_and_key() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = pem_file(&generated.cert.pem());
        let key = pem_file(&generated.signing_key.serialize_pem());
        assert!(load_server_config(cert.path(), key.path()).is_ok());
    }

    #[test]
    fn refuses_missing_or_empty_pem() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = pem_file(&generated.cert.pem());
        let key = pem_file(&generated.signing_key.serialize_pem());
        let empty = pem_file("");

        let error = format!(
            "{:#}",
            load_server_config(empty.path(), key.path()).unwrap_err()
        );
        assert!(error.contains("--tls-cert"), "{error}");
        let error = format!(
            "{:#}",
            load_server_config(cert.path(), empty.path()).unwrap_err()
        );
        assert!(error.contains("--tls-key"), "{error}");
        // A key where the certificate should be, and the reverse.
        assert!(load_server_config(key.path(), cert.path()).is_err());
    }

    #[test]
    fn refuses_a_key_open_to_group_or_other() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = pem_file(&generated.cert.pem());
        let key = pem_file(&generated.signing_key.serialize_pem());
        for mode in [0o640, 0o604, 0o644] {
            std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            let error = format!(
                "{:#}",
                load_server_config(cert.path(), key.path()).unwrap_err()
            );
            assert!(error.contains("--tls-key"), "{error}");
            assert!(error.contains("chmod 600"), "{error}");
        }
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(load_server_config(cert.path(), key.path()).is_ok());
    }
}
