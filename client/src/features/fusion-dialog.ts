// ---------------------------------------------------------------------------
// Dialogue Fusion : seules les paires réellement proposées par l'agent
// sont sélectionnables. Pré-rempli avec la fusion courante le cas échéant.
// ---------------------------------------------------------------------------

import { $, showErr } from "../core/dom";
import { app } from "../core/state";
import { t } from "../i18n";
import { applyModelValue, fillSel, fusionParts, fusionTable, splitModel } from "./modelbar";

let fusionApplied = false;

export function openFusion() {
  const { prims, sides } = fusionTable();
  if (!prims.size) {
    showErr(t("fusion_none"));
    return;
  }
  const a = $("fu-a") as HTMLSelectElement;
  const ea = $("fu-ea") as HTMLSelectElement;
  const b = $("fu-b") as HTMLSelectElement;
  const eb = $("fu-eb") as HTMLSelectElement;
  const effLabel = (x: string) => x || "défaut";

  const refreshEB = () => {
    const effs = [...(sides.get(`${a.value}|${ea.value}`)?.get(b.value) ?? new Set<string>())];
    fillSel(eb, effs.map((e) => [e, effLabel(e)]));
  };
  const refreshB = () => {
    const m = sides.get(`${a.value}|${ea.value}`) ?? new Map<string, Set<string>>();
    fillSel(b, [...m.keys()].map((k) => [k, k]));
    refreshEB();
  };
  const refreshEA = () => {
    fillSel(ea, [...(prims.get(a.value) ?? new Set<string>())].map((e) => [e, effLabel(e)]));
    refreshB();
  };

  fillSel(a, [...prims.keys()].map((k) => [k, k]));
  a.onchange = refreshEA;
  ea.onchange = refreshB;
  b.onchange = refreshEB;

  // Pré-remplir depuis la valeur fusion courante (reconfiguration)
  const curVal =
    app.current?.config_options?.find((o) => o.id === "model" || o.category === "model")
      ?.currentValue || app.current?.model || "";
  const p = fusionParts(curVal);
  if (p && prims.has(p[0])) {
    a.value = p[0];
    refreshEA();
    if ([...ea.options].some((o) => o.value === p[1])) ea.value = p[1];
    refreshB();
    if ([...b.options].some((o) => o.value === p[2])) b.value = p[2];
    refreshEB();
    if ([...eb.options].some((o) => o.value === p[3])) eb.value = p[3];
  } else {
    refreshEA();
  }
  fusionApplied = false;
  ($("dlg-fusion") as HTMLDialogElement).showModal();
}

export function initFusionDialog() {
  $("fu-cancel").onclick = () => ($("dlg-fusion") as HTMLDialogElement).close("cancel");

  $("fu-ok").addEventListener("click", (e) => {
    e.preventDefault();
    const a = ($("fu-a") as HTMLSelectElement).value;
    const ea = ($("fu-ea") as HTMLSelectElement).value;
    const b = ($("fu-b") as HTMLSelectElement).value;
    const eb = ($("fu-eb") as HTMLSelectElement).value;
    const v = `fusion-${a}${ea ? "-" + ea : ""}-sidekick-${b}${eb ? "-" + eb : ""}`;
    if (!fusionTable().valid.has(v)) {
      showErr(t("fusion_bad") + v);
      return;
    }
    fusionApplied = true;
    ($("dlg-fusion") as HTMLDialogElement).close();
    applyModelValue(v);
  });

  // Annulation du dialogue : si le select affichait "✨ Fusion…" alors que le
  // modèle courant n'est pas une fusion, restaurer sa valeur réelle.
  ($("dlg-fusion") as HTMLDialogElement).addEventListener("close", () => {
    const md = $("model-select") as HTMLSelectElement;
    const curVal = app.current?.model ?? "";
    if (!fusionApplied && md.value === "__fusion" && !curVal.startsWith("fusion-")) {
      md.value = splitModel(curVal)[0];
    }
  });
}
