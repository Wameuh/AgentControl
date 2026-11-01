#!/bin/sh
# Test fonctionnel de pair.sh (forced command B′) — simule les lignes
# authorized_keys et SSH_ORIGINAL_COMMAND dans un HOME isolé :
#   installation de la clé de l'appareil (one-shot, retrait de la ligne
#   K, pair-ok:<fp> == vraie empreinte), mauvais nonce, commandes
#   malformées, expiration, revoke, balayage.
# Usage : sh tests/pair-sh-test.sh   — requiert ssh-keygen, flock|mkdir.
set -u
cd "$(dirname "$0")/.."
PAIR_SH="$PWD/src-tauri/pair.sh"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export HOME="$TMP"
mkdir -p "$HOME/.ssh" "$HOME/.agentcontrol"
AK="$HOME/.ssh/authorized_keys"
D="$HOME/.agentcontrol"
NONCE="0123456789abcdef0123456789abcdef"
RPORT=7331

# une paire ed25519 "appareil" + sa ligne K jetable factice
ssh-keygen -q -t ed25519 -N '' -f "$TMP/dev_key" >/dev/null
DEV_B64=$(awk '{print $2}' "$TMP/dev_key.pub")
DEV_FP=$(ssh-keygen -lf "$TMP/dev_key.pub" | awk '{print $2}')
# ligne K comme l'install la poserait (nonce en fin de commentaire)
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE" "$RPORT" "$NONCE" >> "$AK"
# état d'expiration : +5 min
echo $(( $(date +%s) + 300 )) > "$D/pair-$NONCE.exp"

fails=0
ok() { if [ "$1" = 0 ]; then echo "  PASS $2"; else echo "  FAIL $2"; fails=$((fails+1)); fi; }

run_pair() { # args: commande SSH_ORIGINAL_COMMAND simulée
  SSH_ORIGINAL_COMMAND="$1" sh "$PAIR_SH" "$NONCE" "$RPORT" 2>/dev/null
}

# --- installation nominale ---
out=$(run_pair "agentcontrol-pair ssh-ed25519 $DEV_B64 ac-test")
rc=$?
ok $rc "install nominale exit 0"
case "$out" in pair-ok:"$DEV_FP") ok 0 "pair-ok:<fp réel>";; *) ok 1 "pair-ok:<fp réel> (reçu: $out)";; esac
grep -q 'restrict,port-forwarding,permitopen="127.0.0.1:7331" ssh-ed25519 '"$DEV_B64"' agentcontrol-pair ac-test ' "$AK"
ok $? "ligne appareil posée avec restrictions bornées"
grep -q "agentcontrol-pair-tmp $NONCE\$" "$AK" && ok 1 "ligne K retirée" || ok 0 "ligne K retirée"
[ -f "$AK.bak" ] && [ "$(stat -c %a "$AK.bak" 2>/dev/null || stat -f %Lp "$AK.bak")" = "600" ]
ok $? ".bak présent mode 600"

# --- one-shot : le même nonce ne sert plus ---
out=$(run_pair "agentcontrol-pair ssh-ed25519 $DEV_B64 ac-again" 2>&1)
[ $? -ne 0 ]
ok $? "réutilisation du nonce → refus"

# --- mauvais nonce : forcé à un autre argument ---
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "aaaabbbbccccddddeeeeffff00001111" "$RPORT" "aaaabbbbccccddddeeeeffff00001111" >> "$AK"
echo $(( $(date +%s) + 300 )) > "$D/pair-aaaabbbbccccddddeeeeffff00001111.exp"
out=$(SSH_ORIGINAL_COMMAND="agentcontrol-pair ssh-ed25519 $DEV_B64 ac-x" \
      sh "$PAIR_SH" "$NONCE" "$RPORT" 2>&1)   # NONCE ≠ ligne/state
[ $? -ne 0 ]
ok $? "nonce sans état ni ligne → refus (expired/gone)"

# --- commandes malformées ---
NONCE2="22222222222222222222222222222222"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE2" "$RPORT" "$NONCE2" >> "$AK"
echo $(( $(date +%s) + 300 )) > "$D/pair-$NONCE2.exp"
run2() { SSH_ORIGINAL_COMMAND="$1" sh "$PAIR_SH" "$NONCE2" "$RPORT" 2>/dev/null; }
run2 "";                        [ $? -ne 0 ]; ok $? "commande vide → refus"
run2 "agentcontrol-pair";       [ $? -ne 0 ]; ok $? "arité 1 → refus"
run2 "agentcontrol-pair ssh-ed25519 $DEV_B64 ac-lab extra"; [ $? -ne 0 ]; ok $? "arité 5 → refus"
run2 "agentcontrol-pair ssh-rsa $DEV_B64 ac-lab";           [ $? -ne 0 ]; ok $? "type non-whitelist → refus"
run2 "agentcontrol-pair ssh-ed25519 '%%!!' ac-lab";         [ $? -ne 0 ]; ok $? "b64 sale → refus"
run2 "agentcontrol-pair ssh-ed25519 $DEV_B64 'lab;rm -rf'" ;[ $? -ne 0 ]; ok $? "label sale → refus"
run2 "agentcontrol-pair ssh-ed25519 AAAA ac-lab";           [ $? -ne 0 ]; ok $? "blob invalide (ssh-keygen) → refus"
run2 "$(printf 'agentcontrol-pair ssh-ed25519 %s ac-lab\nevil' "$DEV_B64")"; [ $? -ne 0 ]; ok $? "LF dans la commande → refus"
# la ligne K doit être INTACTE après tous ces refus
grep -q "agentcontrol-pair-tmp $NONCE2\$" "$AK"; ok $? "refus ne touchent pas la ligne K"

# --- revoke pendant la fenêtre ---
run2 "agentcontrol-pair-revoke"; ok $? "revoke → exit 0"
grep -q "agentcontrol-pair-tmp $NONCE2\$" "$AK" && ok 1 "revoke retire la ligne K" || ok 0 "revoke retire la ligne K"
[ -f "$D/pair-$NONCE2.exp" ] && ok 1 "état revoke supprimé" || ok 0 "état revoke supprimé"

# --- expiration : état passé → ligne retirée, refus ---
NONCE3="33333333333333333333333333333333"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE3" "$RPORT" "$NONCE3" >> "$AK"
echo $(( $(date +%s) - 10 )) > "$D/pair-$NONCE3.exp"
SSH_ORIGINAL_COMMAND="agentcontrol-pair ssh-ed25519 $DEV_B64 ac-late" \
  sh "$PAIR_SH" "$NONCE3" "$RPORT" 2>/dev/null
[ $? -ne 0 ]
ok $? "état expiré → refus"
grep -q "agentcontrol-pair-tmp $NONCE3\$" "$AK" && ok 1 "expirée : ligne K retirée" || ok 0 "expirée : ligne K retirée"

# --- --sweep : orphelins et états expirés nettoyés ---
NONCE4="44444444444444444444444444444444"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE4" "$RPORT" "$NONCE4" >> "$AK"
echo $(( $(date +%s) - 60 )) > "$D/pair-$NONCE4.exp"  # .exp passé
sh "$PAIR_SH" --sweep >/dev/null 2>&1
ok $? "sweep exit 0"
grep -q "agentcontrol-pair-tmp $NONCE4\$" "$AK" && ok 1 "sweep : ligne expirée retirée" || ok 0 "sweep : ligne expirée retirée"
[ -f "$D/pair-$NONCE4.exp" ] && ok 1 "sweep : .exp orphelin retiré" || ok 0 "sweep : .exp orphelin retiré"
# ligne K orpheline sans .exp : vue une fois (.seen), pas retirée de suite
NONCE5="55555555555555555555555555555555"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE5" "$RPORT" "$NONCE5" >> "$AK"
sh "$PAIR_SH" --sweep >/dev/null 2>&1
grep -q "agentcontrol-pair-tmp $NONCE5\$" "$AK" && [ -f "$D/pair-$NONCE5.seen" ]
ok $? "sweep : orpheline récente marquée .seen, conservée"
touch -d '2 days ago' "$D/pair-$NONCE5.seen" 2>/dev/null || touch -t "$(date -v-2d +%Y%m%d%H%M 2>/dev/null || echo 200001010000)" "$D/pair-$NONCE5.seen"
sh "$PAIR_SH" --sweep >/dev/null 2>&1
grep -q "agentcontrol-pair-tmp $NONCE5\$" "$AK" && ok 1 "sweep : orpheline >24h retirée" || ok 0 "sweep : orpheline >24h retirée"
# régression : un .exp vivant + sa ligne K ne doivent JAMAIS être
# balayés (l'install écrit les deux sous le même verrou — auparavant
# le sweep opportuniste détruisait le .exp frais, faute de ligne K).
NONCE6="77777777777777777777777777777777"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "$NONCE6" "$RPORT" "$NONCE6" >> "$AK"
echo $(( $(date +%s) + 300 )) > "$D/pair-$NONCE6.exp"
sh "$PAIR_SH" --sweep >/dev/null 2>&1
[ -f "$D/pair-$NONCE6.exp" ] && grep -q "agentcontrol-pair-tmp $NONCE6\$" "$AK"
ok $? "sweep : .exp vivant + ligne K préservés"
# et l'appairage reste consommable après ce sweep
out=$(SSH_ORIGINAL_COMMAND="agentcontrol-pair ssh-ed25519 $DEV_B64 ac-swept" \
      sh "$PAIR_SH" "$NONCE6" "$RPORT" 2>/dev/null)
[ $? -eq 0 ] && case "$out" in pair-ok:*) true;; *) false;; esac
ok $? "pairing consommable après un sweep"

# --- authorized_keys symlinké → refus net ---
mv "$AK" "$AK.real"; ln -s "$AK.real" "$AK"
printf 'restrict,command="%s/pair.sh %s %s" ssh-ed25519 AAAA agentcontrol-pair-tmp %s\n' \
  "$D" "66666666666666666666666666666666" "$RPORT" "66666666666666666666666666666666" >> "$AK.real"
echo $(( $(date +%s) + 300 )) > "$D/pair-66666666666666666666666666666666.exp"
SSH_ORIGINAL_COMMAND="agentcontrol-pair ssh-ed25519 $DEV_B64 ac-sym" \
  sh "$PAIR_SH" "66666666666666666666666666666666" "$RPORT" 2>/dev/null
[ $? -ne 0 ]
ok $? "authorized_keys symlinké → refus"
rm -f "$AK"; mv "$AK.real" "$AK"

# --- log : pas de secret, pas de commande brute ---
grep -q "$DEV_B64" "$D/pair.log" 2>/dev/null && ok 1 "log sans blob clé" || ok 0 "log sans blob clé"

echo ""
if [ "$fails" -eq 0 ]; then echo "Tous les tests passent."; else echo "$fails échec(s)."; fi
exit "$fails"
