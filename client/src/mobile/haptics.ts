// ---------------------------------------------------------------------------
// Retour haptique (Android, Y4) — une vibration courte confirme les gestes
// « hors écran » : envoi d'un message, réponse à une permission/question,
// arrivée d'une question alors que l'app est au premier plan.
//
// Le réglage « Sons » (sndEnabled) pilote aussi la vibration : couper le
// son coupe le retour haptique. No-op hors Android et erreurs avalées —
// le desktop et le web ne changent pas.
// ---------------------------------------------------------------------------

import {
  impactFeedback,
  notificationFeedback,
} from "@tauri-apps/plugin-haptics";
import { isAndroid } from "../core/chrome";
import { sndEnabled } from "../features/sounds";

export type TickKind = "send" | "resolve" | "alert";

const TICK: Record<TickKind, () => Promise<unknown>> = {
  send: () => impactFeedback("light"),
  resolve: () => notificationFeedback("success"),
  alert: () => notificationFeedback("warning"),
};

/** Une vibration courte pour confirmer `kind`. Silencieuse hors Android,
 *  quand les sons sont coupés, ou si la plateforme refuse. */
export function tick(kind: TickKind) {
  if (!isAndroid() || !sndEnabled()) return;
  void TICK[kind]().catch(() => {});
}
