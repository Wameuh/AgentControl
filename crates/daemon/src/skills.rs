//! Skills agents embarqués — provisionnement géré (item #264).
//!
//! Les skills fournis par AgentControl (acctl, ac-wait, ac-steps…) sont
//! compilés dans le binaire (blob `agent-skills.bin` produit par
//! build.rs depuis `skills/` + `.devin/skills/` — sources de vérité
//! versionnées avec le démon) puis synchronisés dans les répertoires
//! que chaque provider scanne, pour que TOUTE install — quel que soit
//! le serveur ou le workspace — expose les skills sans copie manuelle.
//!
//! Provisionnement : à chaque spawn d'agent (SpawnHints.skills_roots)
//! et une fois au boot pour les sessions existantes. Par profil :
//! `provision_skills = false` dans `[agents.<id>]` ou globalement dans
//! config.toml.
//!
//! Sync gérée : chaque racine provisionnée porte un manifeste
//! `.agentcontrol-skills.json` (skill → fichiers → sha1). Un skill
//! listé là est REÉCRIT quand le contenu embarqué change (upgrade du
//! démon = skills à jour). Un répertoire de skill présent SANS entrée
//! manifeste n'est jamais écrasé : adoptable s'il est identique au
//! contenu embarqué (repo dont les skills sont committés), sinon warn
//! + skip — la main reste à l'utilisateur.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Blob OUT_DIR/agent-skills.bin (build.rs) :
/// u32 count, puis par entrée { u16 path_len, path utf8, u32 len, data }.
const BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/agent-skills.bin"));

/// Manifeste de provisionnement à la racine de chaque répertoire de
/// skills synchronisé (`<cwd>/.devin/skills/.agentcontrol-skills.json`…).
const MANIFEST: &str = ".agentcontrol-skills.json";

/// Un skill embarqué : nom = premier composant du chemin
/// (`ac-wait/SKILL.md` → `ac-wait`), fichiers = chemins relatifs au
/// dossier du skill.
#[derive(Debug)]
struct Skill {
    name: String,
    files: Vec<(String, Vec<u8>)>,
}

fn parse_blob() -> Vec<Skill> {
    parse_blob_inner().unwrap_or_default()
}

fn parse_blob_inner() -> Option<Vec<Skill>> {
    let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
    let mut cur = BLOB;
    let count = u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize;
    for _ in 0..count {
        let plen = u16::from_le_bytes(take(&mut cur, 2)?.try_into().ok()?) as usize;
        let path = String::from_utf8_lossy(take(&mut cur, plen)?).into_owned();
        let dlen = u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize;
        let data = take(&mut cur, dlen)?.to_vec();
        // Chemin attendu `<skill>/<fichier…>` ; un fichier lâche à la
        // racine des sources n'appartient à aucun skill → ignoré.
        let Some((name, rel)) = path.split_once('/') else {
            continue;
        };
        if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "..") {
            continue;
        }
        let skill = skills.entry(name.to_string()).or_insert_with(|| Skill {
            name: name.to_string(),
            files: Vec::new(),
        });
        skill.files.push((rel.to_string(), data));
    }
    Some(skills.into_values().collect())
}

fn take<'a>(cur: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if cur.len() < n {
        return None;
    }
    let (h, t) = cur.split_at(n);
    *cur = t;
    Some(h)
}

fn embedded() -> &'static [Skill] {
    static SKILLS: OnceLock<Vec<Skill>> = OnceLock::new();
    SKILLS.get_or_init(parse_blob)
}

/// Répertoires de skills (relatifs au cwd de la session) scannés par
/// le provider du profil — chaque entrée reçoit une copie gérée des
/// skills embarqués. Mapping calé sur les chemins documentés :
/// devin → `.devin/skills` ; claude (Claude Code) → `.claude/skills` ;
/// cursor → `.agents/skills` (standard) + `.cursor/skills` (menu `/`
/// du CLI, qui ne scanne que `.cursor`) ; codex → `.agents/skills`
/// (scope repo, `.codex/skills` déprécié) ; tout autre profil
/// (gemini, opencode, customs) → `.agents/skills`, le standard
/// cross-vendor lu par cursor/codex et la plupart des adaptateurs.
pub(crate) fn dirs_for(agent: &str) -> &'static [&'static str] {
    match agent {
        "devin" => &[".devin/skills"],
        "claude" => &[".claude/skills"],
        "cursor" => &[".agents/skills", ".cursor/skills"],
        "codex" => &[".agents/skills"],
        _ => &[".agents/skills"],
    }
}

/// Synchronise chaque racine (absolue) avec les skills embarqués.
/// Best-effort : jamais d'erreur remontée — un problème de FS est un
/// warn, le spawn ne doit pas en pâtir.
pub(crate) fn sync(roots: &[PathBuf]) {
    let skills = embedded();
    for root in roots {
        sync_root(root, skills);
    }
}

/// Manifeste persisté : skill → fichier relatif → sha1 hex du contenu
/// embarqué au moment de la pose (permet de voir ce qui a changé et
/// borne la suppression des fichiers devenus obsolètes).
type Manifest = BTreeMap<String, BTreeMap<String, String>>;

fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST)
}

fn read_manifest(root: &Path) -> Manifest {
    let Ok(text) = std::fs::read_to_string(manifest_path(root)) else {
        return Manifest::new();
    };
    #[derive(serde::Deserialize)]
    struct M {
        #[serde(default)]
        skills: Manifest,
    }
    serde_json::from_str::<M>(&text)
        .map(|m| m.skills)
        .unwrap_or_default()
}

/// Écriture atomique (.part + rename) : un manifeste tronqué par un
/// crash serait relu vide → tous les skills paraîtraient non managés
/// → warn + skip en boucle.
fn write_manifest(root: &Path, skills: &Manifest) {
    #[derive(serde::Serialize)]
    struct M<'a> {
        version: u32,
        skills: &'a Manifest,
    }
    let body = serde_json::to_string_pretty(&M { version: 1, skills })
        .unwrap_or_else(|_| "{\"version\":1,\"skills\":{}}".into());
    let tmp = root.join(format!("{MANIFEST}.part"));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, manifest_path(root));
    }
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    let d = sha1::Sha1::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Vrai si `dest` contient déjà tous les fichiers du skill avec un
/// contenu identique à l'embarqué — un tel répertoire est « adopté »
/// (manifesté managé) sans rien réécrire : c'est le cas d'un repo qui
/// commite ses propres skills au même contenu.
fn adoptable(dest: &Path, skill: &Skill) -> bool {
    skill.files.iter().all(|(rel, bytes)| {
        std::fs::read(dest.join(rel))
            .map(|d| d == *bytes)
            .unwrap_or(false)
    })
}

fn sync_root(root: &Path, skills: &[Skill]) {
    if skills.is_empty() {
        return;
    }
    let mut manifest = read_manifest(root);
    let mut dirty = false;
    for skill in skills {
        let dest = root.join(&skill.name);
        let managed = manifest.contains_key(&skill.name);
        if !managed && dest.exists() {
            if adoptable(&dest, skill) {
                // Contenu identique → on ne fait que revendiquer la
                // gestion (entrera dans le manifeste ci-dessous).
            } else {
                tracing::warn!(
                    "skills: « {} » already exists in {} without agentcontrol \
                     provenance — skipped (delete it to take the embedded \
                     version, or rename yours)",
                    skill.name,
                    root.display()
                );
                continue;
            }
        }
        // Écriture des fichiers : seuls les contenus différents sont
        // retouchés (mtime préservé → pas de churn à chaque spawn).
        let mut written = 0usize;
        let mut failed = false;
        for (rel, bytes) in &skill.files {
            let target = dest.join(rel);
            let fresh = std::fs::read(&target).map(|d| d == *bytes).unwrap_or(false);
            if fresh {
                continue;
            }
            let Some(parent) = target.parent() else {
                continue;
            };
            if std::fs::create_dir_all(parent).is_err() || std::fs::write(&target, bytes).is_err() {
                tracing::warn!(
                    "skills: write {} failed — skill « {} » skipped",
                    target.display(),
                    skill.name
                );
                failed = true;
                break;
            }
            written += 1;
        }
        if failed {
            continue;
        }
        // Purge des fichiers gérés qui ont disparu de la version
        // embarquée (rename/suppression entre deux releases) — les
        // fichiers ajoutés à la main dans un skill managé sont conservés.
        // Les rels du manifeste sont relues du disque : filtrer `..`/absolu
        // avant tout remove (un manifeste trafiqué ne doit pas écrire
        // hors du dossier du skill).
        if let Some(old) = manifest.get(&skill.name) {
            for rel in old.keys() {
                if skill.files.iter().any(|(r, _)| r == rel) {
                    continue;
                }
                if rel.starts_with('/') || rel.split('/').any(|c| c.is_empty() || c == "..") {
                    continue;
                }
                let _ = std::fs::remove_file(dest.join(rel));
                dirty = true;
            }
        }
        let files: BTreeMap<String, String> = skill
            .files
            .iter()
            .map(|(r, b)| (r.clone(), sha1_hex(b)))
            .collect();
        if manifest.get(&skill.name) != Some(&files) {
            manifest.insert(skill.name.clone(), files);
            dirty = true;
        }
        if written > 0 {
            tracing::info!(
                "skills: « {} » provisioned into {} ({} file(s) written)",
                skill.name,
                root.display(),
                written
            );
        }
    }
    // Le manifeste lui-même vit dans la racine scannée : il porte un
    // point devant → la plupart des providers ignorent les entrées
    // cachées ; même sinon ce n'est pas un dossier de skill (pas de
    // SKILL.md). On ne le crée que s'il a quelque chose à porter —
    // une racine où tout a collisionné ne doit pas apparaître.
    if dirty || (!manifest.is_empty() && !manifest_path(root).exists()) {
        if std::fs::create_dir_all(root).is_ok() {
            write_manifest(root, &manifest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, files: &[(&str, &str)]) -> Skill {
        Skill {
            name: name.into(),
            files: files
                .iter()
                .map(|(r, c)| (r.to_string(), c.as_bytes().to_vec()))
                .collect(),
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ac-skills-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sync_cree_les_fichiers_et_le_manifeste() {
        let root = tmpdir("create");
        let skills = [skill("ac-x", &[("SKILL.md", "# x"), ("ref/a.md", "a")])];
        sync_root(&root, &skills);
        assert_eq!(
            std::fs::read_to_string(root.join("ac-x/SKILL.md")).unwrap(),
            "# x"
        );
        assert!(manifest_path(&root).exists());
        let m = read_manifest(&root);
        assert!(m.contains_key("ac-x"));
        assert!(m["ac-x"].contains_key("SKILL.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_idempotent_pas_de_reecriture() {
        let root = tmpdir("idem");
        let skills = [skill("ac-x", &[("SKILL.md", "v1")])];
        sync_root(&root, &skills);
        let f = root.join("ac-x/SKILL.md");
        let before = mtime(&f);
        sync_root(&root, &skills);
        assert_eq!(mtime(&f), before, "file rewritten identically");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn sync_met_a_jour_sur_changement() {
        let root = tmpdir("update");
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "v1")])]);
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "v2")])]);
        assert_eq!(
            std::fs::read_to_string(root.join("ac-x/SKILL.md")).unwrap(),
            "v2"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collision_non_manage_jamais_ecrasee() {
        let root = tmpdir("collision");
        let dest = root.join("ac-x");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("SKILL.md"), "mien").unwrap();
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "embedded")])]);
        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "mien"
        );
        assert!(!manifest_path(&root).exists() || !read_manifest(&root).contains_key("ac-x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identique_non_managed_est_adopte() {
        let root = tmpdir("adopt");
        let dest = root.join("ac-x");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("SKILL.md"), "embedded").unwrap();
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "embedded")])]);
        // Adopté → manifesté managé : une version ultérieure embarquée
        // different sera appliquée (preuve de gestion).
        assert!(read_manifest(&root).contains_key("ac-x"));
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "v2")])]);
        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "v2"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fichier_perime_purge_conserve_extra() {
        let root = tmpdir("purge");
        sync_root(
            &root,
            &[skill("ac-x", &[("SKILL.md", "v1"), ("old.md", "o")])],
        );
        // Fichier ajouté à la main dans le skill managé.
        std::fs::write(root.join("ac-x/mine.md"), "m").unwrap();
        sync_root(&root, &[skill("ac-x", &[("SKILL.md", "v1")])]);
        assert!(!root.join("ac-x/old.md").exists());
        assert!(root.join("ac-x/mine.md").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dirs_par_profil() {
        assert_eq!(dirs_for("devin"), &[".devin/skills"]);
        assert_eq!(dirs_for("claude"), &[".claude/skills"]);
        assert!(dirs_for("cursor").contains(&".agents/skills"));
        assert!(dirs_for("cursor").contains(&".cursor/skills"));
        assert_eq!(dirs_for("codex"), &[".agents/skills"]);
        assert_eq!(dirs_for("inconnu"), &[".agents/skills"]);
    }
}
