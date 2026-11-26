//! Tests unitaires de la file de revue — helpers purs de `rq`
//! ( scheduling, packet, snapshots, persistance, verrous ).

use super::rq::*;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn thread_key_trie_et_stable() {
    let a = RqPair {
        path: "/b".into(),
        branch: "x".into(),
    };
    let b = RqPair {
        path: "/a".into(),
        branch: "y".into(),
    };
    let k1 = thread_key(&[a.clone(), b.clone()]);
    let k2 = thread_key(&[b, a]);
    assert_eq!(k1, k2, "l'ordre des paires ne change pas la clé");
    let tid = thread_tid(&k1);
    assert_eq!(tid.len(), 8);
    assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn packet_placeholders() {
    let req = RqRequest {
        id: "rq-test".into(),
        by: String::new(),
        repos: vec![RqRepo {
            path: "/repo".into(),
            branch: "feat".into(),
            base: "main".into(),
        }],
        instructions: "check the diff".into(),
        thread: "t".into(),
        session: None,
        round: 2,
        state: ReqState::Queued,
        created_at: 0.0,
        acquire_at: None,
        dispatched_at: None,
        compact_at: None,
        run_dir: "/run/dir".into(),
        snaps: vec![],
        result: None,
        error: None,
    };
    let md = repos_markdown(&req);
    let p = fill_packet(PACKET_TEMPLATE, &req, 2, &md);
    assert!(p.contains("rq-test") && p.contains("round 2"));
    assert!(p.contains("/run/dir") && p.contains("check the diff"));
    assert!(p.contains("/repo") && p.contains("`feat`"));
    assert!(!p.contains("{REQUEST}") && !p.contains("{RUN_DIR}"));
    // Le template est rempli par str::replace (pas format!) —
    // les accolades JSON doivent être SIMPLES, jamais doublées.
    assert!(!p.contains("{{") && !p.contains("}}"));
    assert!(p.contains(r#"{"findings":[{"severity""#));
}

fn req(id: &str, thread: &str, state: ReqState, created_at: f64) -> RqRequest {
    RqRequest {
        id: id.into(),
        by: String::new(),
        repos: vec![],
        instructions: String::new(),
        thread: thread.into(),
        session: None,
        round: 1,
        state,
        created_at,
        acquire_at: None,
        dispatched_at: None,
        compact_at: None,
        run_dir: String::new(),
        snaps: vec![],
        result: None,
        error: None,
    }
}

fn thread(key: &str, active: Option<String>) -> RqThread {
    RqThread {
        key: key.into(),
        tid: "tid".into(),
        pairs: vec![],
        session: None,
        title: String::new(),
        round: 0,
        active,
        last_active: 0.0,
        reap_checked_at: 0.0,
        created_at: 0.0,
    }
}

#[test]
fn pick_next_sauts_les_threads_occupees() {
    // Deux requêtes en file sur le MÊME thread + une sur un
    // autre : la plus vieille du thread libre part, celle du
    // thread occupé attend — même avec des slots disponibles.
    let mut st = RqState::default();
    st.threads.insert("T1".into(), thread("T1", None));
    st.threads
        .insert("T2".into(), thread("T2", Some("rq-a".into())));
    st.requests
        .insert("rq-1".into(), req("rq-1", "T1", ReqState::Queued, 1.0));
    st.requests
        .insert("rq-2".into(), req("rq-2", "T2", ReqState::Queued, 2.0));
    st.requests
        .insert("rq-3".into(), req("rq-3", "T1", ReqState::Queued, 3.0));
    // rq-2 est plus vieille mais son thread T2 est occupé → rq-1.
    assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
    // rq-1 passe en review → T1 occupé à son tour : plus rien
    // d'éligible (rq-3 attend la fin de rq-1).
    st.requests.get_mut("rq-1").unwrap().state = ReqState::Reviewing;
    assert_eq!(pick_next(&st), None);
    // T2 libéré → rq-2 (plus vieille que rq-3) redevient
    // éligible, T1 restant occupé → rq-2 gagne.
    st.threads.get_mut("T2").unwrap().active = None;
    assert_eq!(pick_next(&st).as_deref(), Some("rq-2"));
}

#[test]
fn pick_next_et_busy_respectent_la_reservation_reaper() {
    // Thread réservé par le reaper (`reaping`) = occupé : sa
    // requête `Queued` n'est JAMAIS dispatchée pendant la
    // suppression de sa session — intake pendant le delete ne
    // peut plus voir son thread disparaître sous elle.
    let mut st = RqState::default();
    st.threads.insert("T".into(), thread("T", None));
    st.requests
        .insert("rq-1".into(), req("rq-1", "T", ReqState::Queued, 1.0));
    st.requests
        .insert("rq-2".into(), req("rq-2", "U", ReqState::Queued, 2.0));
    assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
    st.reaping.insert("T".into());
    assert_eq!(pick_next(&st).as_deref(), Some("rq-2"));
    assert!(thread_busy(&st, "T"));
    // Réservation levée (thread conservé pour la requête
    // pending ou supprimé) → rq-1 redevient éligible si le
    // thread existe encore.
    st.reaping.remove("T");
    assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
}

#[test]
fn status_json_et_patch_cfg_state_ne_deadloquent_pas() {
    // Régression lock-order : status_json lisait `state_dir`
    // (donc cfg.read) PENDANT que le garde `state` était tenu,
    // alors que rq_patch tient cfg.write → state — inversion
    // mortelle. Ici on reproduit les deux ordres en concurrence
    // sur une sonde réelle : le patch simule rq_patch
    // (cfg.write PUIS state), status_json doit rester fluide.
    let dir = std::env::temp_dir().join(format!("rq-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let probe = std::sync::Arc::new(RqProbe::open(cfg::ReviewQueueCfg::default(), dir.clone()));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    {
        let p = std::sync::Arc::clone(&probe);
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                // Ordre rq_patch : cfg.write D'ABORD, state ensuite.
                let g = p.cfg.write().unwrap();
                let _s = p.state.lock().unwrap();
                drop(_s);
                drop(g);
            }
        });
    }
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let p = std::sync::Arc::clone(&probe);
        std::thread::spawn(move || {
            for _ in 0..500 {
                let _ = p.status_json();
            }
            let _ = tx.send(());
        });
    }
    // Si l'ordre est inversé quelque part, status_json bloque
    // indéfiniment → recv_timeout échoue (le thread fuité ne
    // empêche pas le process de sortir).
    rx.recv_timeout(Duration::from_secs(15))
        .expect("status_json deadlocked vs cfg.write→state order");
    stop.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn free_thread_libere_et_relance_l_acquire() {
    // Thread occupé par une review → ses Queued ne timeout pas
    // (thread_busy) ; à la libération, acquire_at repart de
    // zéro pour les followers du même thread seulement.
    let mut st = RqState::default();
    st.threads.insert("T".into(), thread("T", None));
    st.requests
        .insert("rq-a".into(), req("rq-a", "T", ReqState::Reviewing, 1.0));
    st.requests
        .insert("rq-b".into(), req("rq-b", "T", ReqState::Queued, 1.0));
    st.requests
        .insert("rq-c".into(), req("rq-c", "U", ReqState::Queued, 1.0));
    assert!(thread_busy(&st, "T"));
    assert!(!thread_busy(&st, "U"));
    // Libération : rq-a soldée → T libre, rq-b reçoit une
    // fenêtre acquire fraîche ; rq-c (autre thread) inchangée.
    st.requests.get_mut("rq-a").unwrap().state = ReqState::Done;
    free_thread(&mut st, "T", 500.0);
    assert!(!thread_busy(&st, "T"));
    assert_eq!(st.requests.get("rq-b").unwrap().acquire_at, Some(500.0));
    assert_eq!(st.requests.get("rq-c").unwrap().acquire_at, None);
}

#[test]
fn save_concurrent_reste_coherent() {
    // 8 threads × 5 (increment + save) : le verrou de
    // persistance sérialise snapshot→écriture→rename — le
    // fichier final est un JSON valide portant le DERNIER état
    // (reaped = 40), jamais un snapshot réordonné ni un `.part`
    // partagé.
    let dir = std::env::temp_dir().join(format!("rq-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let c = cfg::ReviewQueueCfg {
        state_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    };
    let probe = std::sync::Arc::new(RqProbe::open(c, dir.clone()));
    let mut hs = Vec::new();
    for _ in 0..8 {
        let p = std::sync::Arc::clone(&probe);
        hs.push(std::thread::spawn(move || {
            for _ in 0..5 {
                p.state.lock().unwrap().reaped += 1;
                p.save().unwrap();
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let text = std::fs::read_to_string(dir.join("review-queue-state.json")).unwrap();
    let st: RqState = serde_json::from_str(&text).unwrap();
    assert_eq!(st.reaped, 40);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undispatched_ne_prend_que_reviewing_sans_envoi() {
    // Reprise post-restart : seule une requête persistée
    // `Reviewing` SANS `dispatched_at` doit être renvoyée —
    // tout le reste (autres états, déjà envoyées) est ignoré.
    let mut st = RqState::default();
    let mut r = req("rq-a", "T", ReqState::Reviewing, 1.0);
    r.dispatched_at = None;
    st.requests.insert("rq-a".into(), r);
    let mut r = req("rq-b", "T", ReqState::Reviewing, 1.0);
    r.dispatched_at = Some(1.0);
    st.requests.insert("rq-b".into(), r);
    st.requests
        .insert("rq-c".into(), req("rq-c", "T", ReqState::Queued, 1.0));
    st.requests
        .insert("rq-d".into(), req("rq-d", "T", ReqState::Compacting, 1.0));
    assert_eq!(undispatched(&st), vec!["rq-a".to_string()]);
}

#[test]
fn unsettled_compte_le_vivant_seulement() {
    let mut st = RqState::default();
    assert_eq!(unsettled(&st), 0);
    for (id, s) in [
        ("q", ReqState::Queued),
        ("c", ReqState::Compacting),
        ("r", ReqState::Reviewing),
        ("d", ReqState::Done),
        ("f", ReqState::Failed),
        ("t", ReqState::Timeout),
        ("a", ReqState::AcquireTimeout),
    ] {
        st.requests.insert(id.into(), req(id, "T", s, 1.0));
    }
    assert_eq!(unsettled(&st), 3);
}

#[test]
fn migrate_state_file_preserve_l_historique() {
    // Changement de state_dir à file vide : le JSON d'état est
    // copié vers le nouveau dossier — un restart avant le
    // prochain save() ne perd rien. Source absente = no-op.
    let base = std::env::temp_dir().join(format!("rq-mig-{}", std::process::id()));
    let old = base.join("old");
    let new = base.join("new");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&old).unwrap();
    migrate_state_file(&old, &new).unwrap(); // rien à copier
    assert!(!new.join("review-queue-state.json").exists());
    std::fs::write(old.join("review-queue-state.json"), "{}").unwrap();
    migrate_state_file(&old, &new).unwrap();
    assert_eq!(
        std::fs::read_to_string(new.join("review-queue-state.json")).unwrap(),
        "{}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn snap_detecte_les_changements() {
    // Snapshot manuel : HEAD bidon + hash forcé — un repo factice
    // ne peut pas être comparé (git muet → before vide → pas de
    // verdict). On vérifie la porte « snapshot vide = muet ».
    let repo = RqRepo {
        path: "/nonexistent".into(),
        branch: "b".into(),
        base: String::new(),
    };
    let before = RqSnap {
        head: String::new(),
        status_hash: 0,
    };
    assert!(!snap_changed(&before, &repo));
}
