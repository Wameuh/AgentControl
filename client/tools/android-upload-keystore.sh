#!/usr/bin/env bash
# Génère l'upload keystore Android (signature stable des APK CI) et
# affiche les commandes à exécuter UNE fois pour créer les secrets
# GitHub Actions. Sans ce keystore, chaque build CI signait l'APK avec
# le debug keystore éphémère du runner → signature différente à chaque
# release → Android refusait l'update in-place
# (INSTALL_FAILED_UPDATE_INCOMPATIBLE).
#
# Usage :
#   client/tools/android-upload-keystore.sh
#   (le mot de passe est demandé de façon masquée s'il n'est pas fourni
#    via ANDROID_KEYSTORE_PASSWORD ; alias surchargeable via
#    ANDROID_KEY_ALIAS, chemin via ANDROID_KEYSTORE_PATH)
#
# Ensuite, dans le repo Wameuh/AgentControl :
#   gh secret set ANDROID_KEYSTORE_BASE64 < <(base64 -w0 "$KS")
#   (macOS : base64 -i "$KS" | tr -d '\n' | gh secret set ANDROID_KEYSTORE_BASE64)
#   gh secret set ANDROID_KEYSTORE_PASSWORD   # saisie interactive
#   gh secret set ANDROID_KEY_ALIAS           # 'agentcontrol' par défaut
#
# ⚠️ Le fichier .jks est la SEULE copie de la clé de signature : le
# perdre = impossibilité de publier des updates (toutes les installs
# existantes devraient être désinstallées/réinstallées). Le conserver
# en lieu sûr, HORS de tout dépôt git.
set -euo pipefail

KS="${ANDROID_KEYSTORE_PATH:-$HOME/.agentcontrol/android-upload-keystore.jks}"
ALIAS="${ANDROID_KEY_ALIAS:-agentcontrol}"

if [ -f "$KS" ]; then
  echo "keystore existant : $KS (alias « $ALIAS »)" >&2
else
  mkdir -p "$(dirname "$KS")"
  if [ -z "${ANDROID_KEYSTORE_PASSWORD:-}" ]; then
    read -rsp "Mot de passe du keystore : " ANDROID_KEYSTORE_PASSWORD; echo >&2
    local_confirm=""
    read -rsp "Confirmation : " local_confirm; echo >&2
    if [ "$ANDROID_KEYSTORE_PASSWORD" != "$local_confirm" ]; then
      echo "mot de passe différent — abandon" >&2
      exit 1
    fi
  fi
  # Même mot de passe pour le store et la clé (le workflow utilise
  # ANDROID_KEYSTORE_PASSWORD pour les deux ; ANDROID_KEY_PASSWORD
  # reste disponible si un jour ils diffèrent).
  keytool -genkey -v \
    -keystore "$KS" \
    -keyalg RSA -keysize 2048 -validity 10950 \
    -alias "$ALIAS" \
    -storepass "$ANDROID_KEYSTORE_PASSWORD" \
    -keypass "$ANDROID_KEYSTORE_PASSWORD" \
    -dname "CN=AgentControl, OU=dev, O=Wameuh, C=FR"
  chmod 600 "$KS"
  echo "keystore créé : $KS" >&2
fi

cat <<EOF

Keystore : $KS
Alias    : $ALIAS

Secrets à créer dans Wameuh/AgentControl (Settings → Secrets and
variables → Actions, ou gh secret set depuis un clone du repo) :

  base64 -w0 "$KS" | gh secret set ANDROID_KEYSTORE_BASE64
  gh secret set ANDROID_KEYSTORE_PASSWORD
  gh secret set ANDROID_KEY_ALIAS

(ANDROID_KEY_PASSWORD est optionnel : inutile si le mot de passe de la
clé est identique à celui du store — le défaut.)

Vérification après le prochain build taggé :
  unzip -p <apk> META-INF/*.RSA > /dev/null 2>&1 || true
  <build-tools>/apksigner verify --print-certs <apk>
  → le certificat SHA-256 doit être celui de ce keystore
  (keytool -list -v -keystore "$KS" -alias "$ALIAS").
EOF
