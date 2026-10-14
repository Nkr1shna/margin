"""The narration voice of a Margin lesson scene.

Margin's renderer writes this module next to `scene.py` and appends two lines to the
scene that call `attach(globals())`, which sets `LessonScene.voice`. What the voice does
depends on the pass, read from `.margin/pass.json`:

- "collect" (`manimgx inspect`): records each line the scene says in `.margin/lines.json`
  and returns silence of an estimated length, so the renderer knows every line up front.
- "render" (`manimgx render`): returns the audio the renderer made for the line
  (`.margin/pass.json` maps each line to its file under `.margin/`).

Do not edit: the renderer replaces this file on every render.
"""

import json
import pathlib

import manimgx as m
import numpy as np

_STATE = pathlib.Path(__file__).resolve().parent / ".margin"
_PASS = json.loads((_STATE / "pass.json").read_text(encoding="utf-8"))
_RATE = 48_000


def estimated_seconds(text: str) -> float:
    """How long a line takes to say, roughly: 2.6 words a second and a breath.
    The renderer (margin-video, src/wav.rs) uses the same estimate."""
    return max(0.8, 0.4 + len(text.split()) / 2.6)


def collect(text: str) -> m.Speech:
    """Pass 1: remember the line, say nothing for as long as it would take."""
    path = _STATE / "lines.json"
    lines = json.loads(path.read_text(encoding="utf-8")) if path.exists() else []
    if text not in lines:
        lines.append(text)
        path.write_text(json.dumps(lines, ensure_ascii=False), encoding="utf-8")
    samples = np.zeros(round(estimated_seconds(text) * _RATE), dtype=np.float32)
    return m.Speech(samples, text=text, rate=_RATE)


def speak(text: str) -> m.Speech:
    """Pass 2: the audio made for the line."""
    audio = _PASS["audio"].get(text)
    if audio is None:
        raise RuntimeError(
            f"no narration was made for the line {text!r}: a line must be the same"
            " each time the scene runs (no random or time-dependent text in say)"
        )
    return m.Speech(str(_STATE / audio), text=text)


def attach(namespace: dict[str, object]) -> None:
    """Give the scene file's LessonScene this pass's voice."""
    scene = namespace.get("LessonScene")
    if not (isinstance(scene, type) and issubclass(scene, m.Scene)):
        raise TypeError(
            "the scene file must define a class named LessonScene deriving m.Scene"
        )
    scene.voice = staticmethod(collect if _PASS["mode"] == "collect" else speak)
