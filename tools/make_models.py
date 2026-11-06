"""Regenerate models.json from the open-source ear-web project (GPL-3.0).

Its files are only read as text / JSON; nothing downloaded is executed.
Run:  python tools/make_models.py
"""
import json
import os
import re
import urllib.request

BASE = "https://raw.githubusercontent.com/radiance-project/ear-web/main/res/js/"
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "models.json")

# Per-model behaviour that ear-web keeps in code rather than in its config file.
LISTENING_MODE = {"B172", "B195", "B168", "B179", "B197", "B184", "B185", "B175", "B189"}
THREE_STAGE_BASS = {"B189", "B186", "B195", "B197"}
NO_IN_EAR = {"B174", "B185", "B175", "B189", "B186", "B164"}
RING_ALL = {"B181"}
RING_SINGLE = {"B170", "B164", "B186", "B192"}

# Operations the UI has a translated label for.
KNOWN_OPS = {1, 2, 3, 8, 9, 10, 11, 17, 18, 19, 23, 29}


def get(name):
    return urllib.request.urlopen(BASE + name, timeout=60).read().decode()


def gesture_label(key):
    k = (key or "").lower()
    if "pinch_both" in k:
        return "g_both"
    if "rotate" in k:
        return "g_rotate"
    if "slider" in k:
        return "g_slider"
    if "double" in k and ("hold" in k or "hole" in k):
        return "g_dhold"
    if "hold" in k:
        return "g_hold"
    if "triple" in k:
        return "g_triple"
    if "double" in k:
        return "g_double"
    if "single" in k:
        return "g_single"
    return None


def gestures_of(config):
    found = []
    for group in (config.get("controls") or {}).get("earGestureGroups", []):
        for g in group.get("gestures", []):
            label = gesture_label(g.get("gestureNameKey"))
            if not g.get("isCustom") or not label:
                continue
            ops = []
            for og in g.get("operationGroups", []):
                for o in og.get("operations", []):
                    if o.get("isCustom") and o["operation"] in KNOWN_OPS and o["operation"] not in ops:
                        ops.append(o["operation"])
            if ops:
                found.append({"device": g.get("deviceType"), "button": g.get("button"),
                              "type": g["gesture"], "label": label, "ops": ops})
    return found


def main():
    pairs = re.findall(r'"([0-9A-Fa-f]{6})":\s*\{\s*"name":\s*"([^"]+)",\s*"base":\s*"([^"]+)"', get("control.js"))
    ids = {k.upper(): base for k, _, base in sorted(pairs)}
    names = {}
    for _, name, base in pairs:
        names.setdefault(base, name)
    configs = {m["id"]: m for m in json.loads(get("ear_config_file.json"))}
    models = {}
    for base, name in sorted(names.items()):
        m = configs[base]
        c = m["configs"][-1]  # newest firmware's config
        models[base] = {
            "name": name,
            "single": bool(m.get("single")),
            "anc": c.get("ancLevel") or 0,
            "fit": bool(c.get("earTipFitTest")),
            "bass_levels": (3 if base in THREE_STAGE_BASS else 5) if c.get("ultraBass") else 0,
            "dual": bool(c.get("dualConnection")),
            "dual_reboot": bool(c.get("dualConnectionReboot")),
            "hq": c.get("highQualityAudio") or 0,
            "custom_eq": c.get("customEQ") if base not in LISTENING_MODE else None,
            "eq_presets": base not in LISTENING_MODE,
            "in_ear": base not in NO_IN_EAR,
            "ring": "all" if base in RING_ALL else "single" if base in RING_SINGLE else "sides",
            "gestures": gestures_of(c),
        }
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump({"ids": ids, "models": models}, f, indent=1, ensure_ascii=False)
    print(len(ids), "ids,", len(models), "models")


if __name__ == "__main__":
    main()
