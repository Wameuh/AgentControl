//! Vérification d'intégrité des artefacts de release (revue v1 — I1).
//!
//! Le job `release` de build.yml publie `SHA256SUMS` (une ligne
//! `<sha256>  <fichier>` par artefact) et `SHA256SUMS.minisig`, signé
//! avec la clé minisign du projet — clé publique dans
//! `packaging/agentcontrol.pub`, compilée ici. Tout artefact qui ne
//! peut pas être authentifié est refusé par l'appelant : signature
//! minisign d'abord, puis empreinte sha256 de l'asset.
//!
//! Transition : une release qui ne publie PAS de `SHA256SUMS` (les
//! anciennes, pré-signature) est refusée par défaut ;
//! `AGENTCONTROL_ALLOW_UNSIGNED=1` réintroduit le comportement
//! historique pour le dépannage — l'appelant le journalise en warn.
//! Une signature invalide, une empreinte différente ou un manifeste
//! incomplet ne sont JAMAIS couverts par ce repli : refus sec.

use sha2::{Digest, Sha256};
use std::io::Read;

/// Nom des deux fichiers publiés par le job release.
pub const SUMS_NAME: &str = "SHA256SUMS";
pub const SIG_NAME: &str = "SHA256SUMS.minisig";

/// Fichier .pub complet (ligne de commentaire + clé base64), compilé —
/// la clé publique de vérification ne peut pas être troquée par un
/// attaquant du réseau ou du disque. Tant que le placeholder est en
/// place, toute vérification échoue « clé non configurée ».
const PUBKEY_FILE: &str = include_str!("../../../packaging/agentcontrol.pub");

/// `AGENTCONTROL_ALLOW_UNSIGNED` = 1/true : dépannage — autorise une
/// release SANS SHA256SUMS (pre-signature). Ne couvre ni signature ni
/// empreinte invalide, ni manifeste à moitié présent.
pub fn allow_unsigned() -> bool {
    std::env::var_os("AGENTCONTROL_ALLOW_UNSIGNED")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Tag de release `vX.Y.Z` avant interpolation — semver strict `X.Y.Z`
/// (+ suffixe `-pre`/`+meta` alphanumérique, `-rc.N` inclus) : rejette
/// `/`, `\`, `..`, `%`, `"` et tout ce qui altérerait une URL ou un
/// script shell. À appliquer sur TOUT tag fourni par la release —
/// `tag_name` est attacker-controlled sous le modèle I1 (compte
/// GitHub/PAT compromis) et finit interpolé dans le script ssh
/// (ensure_daemon) ou l'URL d'API (update) AVANT toute vérification de
/// signature.
pub fn valid_tag(v: &str) -> bool {
    let mut it = v.split(['-', '+']);
    let core: Vec<&str> = it.next().unwrap_or("").split('.').collect();
    if core.len() != 3
        || core
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    it.all(|tag| {
        !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    })
}

/// Clé publique minisign embarquée — `None` tant que le fichier est le
/// placeholder (decode échoue) : l'erreur « clé non configurée » reste
/// distincte d'une signature invalide.
fn pubkey() -> Result<minisign_verify::PublicKey, String> {
    minisign_verify::PublicKey::decode(PUBKEY_FILE).map_err(|e| {
        format!("release public key not configured (packaging/agentcontrol.pub placeholder?) — {e}")
    })
}

/// `<hex>  <nom>` ou `<hex> *<nom>` (mode binaire GNU) — rend la somme
/// attendue pour `name` dans `sums`, Err si absente ou malformée.
fn expected_hash<'a>(sums: &'a str, name: &str) -> Result<&'a str, String> {
    for line in sums.lines() {
        let Some((hash, file)) = line.split_once(' ') else {
            continue;
        };
        if file.trim_start().trim_start_matches('*') == name {
            if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Ok(hash);
            }
            return Err(format!("{SUMS_NAME}: malformed line for {name}"));
        }
    }
    Err(format!(
        "{name} missing from signed {SUMS_NAME} — incomplete release?"
    ))
}

/// Vérifie la signature minisign de `sums` puis l'empreinte sha256 de
/// l'asset lu via `reader`. `Ok(())` = installation autorisée ; `Err`
/// porte la cause exacte du refus (remontée telle quelle à
/// l'utilisateur).
pub fn verify(
    asset_name: &str,
    mut reader: impl Read,
    sums: &str,
    sig: &str,
) -> Result<(), String> {
    verify_with(&pubkey()?, asset_name, &mut reader, sums, sig)
}

fn verify_with(
    pk: &minisign_verify::PublicKey,
    asset_name: &str,
    reader: &mut impl Read,
    sums: &str,
    sig: &str,
) -> Result<(), String> {
    let signature = minisign_verify::Signature::decode(sig)
        .map_err(|e| format!("{SIG_NAME} unreadable — {e}"))?;
    // allow_legacy=false : nos releases sont signées en mode pré-haché
    // (défaut minisign ≥0.9) — une signature legacy serait anormale.
    pk.verify(sums.as_bytes(), &signature, false).map_err(|e| {
        format!("{SUMS_NAME}: invalid minisign signature (tampered release?) — {e}")
    })?;
    let want = expected_hash(sums, asset_name)?;
    let mut h = Sha256::new();
    std::io::copy(reader, &mut DigestWriter(&mut h))
        .map_err(|e| format!("failed to read {asset_name} for digest: {e}"))?;
    let got = hex(&h.finalize());
    if !got.eq_ignore_ascii_case(want) {
        return Err(format!(
            "{asset_name}: sha256 digest ≠ signed {SUMS_NAME} — tampered or corrupted artifact"
        ));
    }
    Ok(())
}

/// Encodage hex minimal — `{:x}` n'est pas implémenté sur l'Output
/// générique de digest 0.11 (hybrid-array sans feature std-hex).
fn hex(data: &[u8]) -> String {
    use std::fmt::Write;
    data.iter()
        .fold(String::with_capacity(data.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Adaptateur Write → Sha256 : `std::io::copy` stream l'artefact sans
/// le charger en mémoire (les .deb/.msi font des dizaines de Mo).
struct DigestWriter<'a, D: Digest>(&'a mut D);
impl<D: Digest> std::io::Write for DigestWriter<'_, D> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Signeur minisign minimal (format réel, mode pré-haché `ED`) pour
    //! produire les fixtures : la vérification est testée contre des
    //! signatures AUTHENTIQUES, pas des maquettes.
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    struct TestKey {
        sk: SigningKey,
        key_id: [u8; 8],
    }

    impl TestKey {
        fn new(seed: u8) -> Self {
            Self {
                sk: SigningKey::from_bytes(&[seed; 32]),
                key_id: [seed; 8],
            }
        }
        /// Contenu du fichier .pub (commentaire + ligne base64).
        fn pub_file(&self) -> String {
            let mut bin = Vec::with_capacity(42);
            bin.extend_from_slice(b"Ed");
            bin.extend_from_slice(&self.key_id);
            bin.extend_from_slice(self.sk.verifying_key().as_bytes());
            format!("untrusted comment: test key\n{}", B64.encode(bin))
        }
        /// Contenu du .minisig pour `msg` (mode pré-haché `ED`).
        fn sign(&self, msg: &[u8]) -> String {
            let digest = blake2::Blake2b512::digest(msg);
            let sig = self.sk.sign(&digest);
            let mut bin1 = Vec::with_capacity(74);
            bin1.extend_from_slice(b"ED");
            bin1.extend_from_slice(&self.key_id);
            bin1.extend_from_slice(&sig.to_bytes());
            let trusted = "trusted comment: test".to_string();
            let mut global_msg = Vec::with_capacity(64 + trusted.len() - 17);
            global_msg.extend_from_slice(&sig.to_bytes());
            global_msg.extend_from_slice(trusted[17..].as_bytes());
            let global = self.sk.sign(&global_msg);
            format!(
                "untrusted comment: test signature\n{}\n{}\n{}",
                B64.encode(bin1),
                trusted,
                B64.encode(global.to_bytes())
            )
        }
    }

    fn sums(entries: &[(&str, &[u8])]) -> String {
        entries
            .iter()
            .map(|(name, data)| format!("{}  {}", hex(&Sha256::digest(data)), name))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    #[test]
    fn verify_accepts_good_signature_and_hash() {
        let key = TestKey::new(7);
        let asset = b"daemon tarball bytes";
        let sums = sums(&[("agentcontrol-server-x86_64-linux.tar.gz", asset)]);
        let sig = key.sign(sums.as_bytes());
        let pk = minisign_verify::PublicKey::decode(&key.pub_file()).unwrap();
        verify_with(
            &pk,
            "agentcontrol-server-x86_64-linux.tar.gz",
            &mut &asset[..],
            &sums,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_modified_asset() {
        let key = TestKey::new(7);
        let sums = sums(&[("app.deb", b"original")]);
        let sig = key.sign(sums.as_bytes());
        let pk = minisign_verify::PublicKey::decode(&key.pub_file()).unwrap();
        let err = verify_with(&pk, "app.deb", &mut &b"modified"[..], &sums, &sig).unwrap_err();
        assert!(err.contains("sha256"), "{err}");
    }

    #[test]
    fn verify_rejects_signature_from_another_key() {
        let signer = TestKey::new(7);
        let other = TestKey::new(9);
        let sums = sums(&[("app.deb", b"data")]);
        // Signé par la bonne clé mais vérifié contre l'autre pubkey :
        // les key_id diffèrent → UnexpectedKeyId côté crate.
        let sig = signer.sign(sums.as_bytes());
        let pk = minisign_verify::PublicKey::decode(&other.pub_file()).unwrap();
        assert!(verify_with(&pk, "app.deb", &mut &b"data"[..], &sums, &sig).is_err());
        // Et une signature forgée par l'autre clé refuse pareil.
        let forged = other.sign(sums.as_bytes());
        let pk = minisign_verify::PublicKey::decode(&signer.pub_file()).unwrap();
        assert!(verify_with(&pk, "app.deb", &mut &b"data"[..], &sums, &forged).is_err());
    }

    #[test]
    fn verify_rejects_tampered_sums_and_missing_asset() {
        let key = TestKey::new(7);
        let asset = b"data";
        let sums = sums(&[("app.deb", asset)]);
        let sig = key.sign(sums.as_bytes());
        let pk = minisign_verify::PublicKey::decode(&key.pub_file()).unwrap();
        // Somme modifiée APRÈS signature : la signature ne couvre plus
        // le texte → refus à la signature, pas à l'empreinte.
        let mut tampered = sums.clone();
        tampered.replace_range(..64, &"0".repeat(64));
        let err = verify_with(&pk, "app.deb", &mut &asset[..], &tampered, &sig).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        // Asset non listé dans le manifeste signé → refus.
        let err = verify_with(&pk, "absent.deb", &mut &asset[..], &sums, &sig).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn expected_hash_parses_binary_mode() {
        let data = b"x";
        let line = format!("{} *bin.tgz\n", hex(&Sha256::digest(data)));
        assert_eq!(
            expected_hash(&line, "bin.tgz").unwrap(),
            hex(&Sha256::digest(data))
        );
        assert!(expected_hash(&line, "other.tgz").is_err());
    }

    #[test]
    fn valid_tag_whitelists_semver() {
        for ok in ["0.10.4", "1.0.0", "0.10.4-rc.9", "0.10.4-beta.1+build"] {
            assert!(valid_tag(ok), "{ok}");
        }
        for bad in [
            "",
            "1.0",
            "1.0.0/../x",
            "../etc/passwd",
            "x\"; rm -rf /; \"",
            "0.10.4%0Aevil",
            "0.10.4;x",
            "x'; y; '",
        ] {
            assert!(!valid_tag(bad), "{bad:?}");
        }
    }
}
