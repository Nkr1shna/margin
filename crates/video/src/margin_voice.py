"""The narration voice of a Margin lesson scene.

Margin's renderer writes this module next to `scene.py` and appends two lines to the
scene that call `attach(globals())`, which sets `LessonScene.voice`. What the voice does
depends on the pass, read from `.margin/pass.json`:

- "collect" (`manimgx inspect`): records each line the scene says in `.margin/lines.json`
  and returns silence of an estimated length, so the renderer knows every line up front.
- "render" (`manimgx render`): returns the audio the renderer made for the line
  (`.margin/pass.json` maps each line to its file under `.margin/`), and captures the
  video's thumbnail, `poster.png`: the scene's closing poster shot (see `Poster`).

Do not edit: the renderer replaces this file on every render.
"""

import json
import os
import pathlib
import struct
import zlib

import manimgx as m
import numpy as np

_DIR = pathlib.Path(__file__).resolve().parent
_STATE = _DIR / ".margin"
_POSTER = _DIR / "poster.png"
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


# The poster shot: a scene ends with its title and key picture held still (the prompt asks
# for `self.wait(2)` as the last statement). The poster is the frame showing the video's
# last half second, which is that hold.
_POSTER_HOLD = 0.5


def png(width: int, height: int, rgba: bytes) -> bytes:
    """An 8-bit RGBA PNG of `rgba` (rows from the top), with the standard library only."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    stride = width * 4
    rows = b"".join(
        b"\0" + rgba[y * stride : (y + 1) * stride] for y in range(height)
    )
    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows, 6))
        + chunk(b"IEND", b"")
    )


def skipped(why: object) -> None:
    print(f"margin: poster skipped: {why}", flush=True)


class Poster:
    """A frame sink that keeps the pixels of the latest hold (a frame shown for at least
    `_POSTER_HOLD` seconds). Frames are drawn only as they are sent, so the closing hold
    can't be picked once the film is over: each hold is drawn, the last one kept. When the
    film ends on a hold, `write` saves it as `poster.png`. It never raises: a failure only
    means no poster."""

    def __init__(self) -> None:
        self.failed: BaseException | None = None
        self.held: tuple[int, int, bytes] | None = None  # (index, repeat, pixels)
        try:
            self.fps = float(m.config.frame_rate)
            self.width = int(m.config.pixel_width)
            self.height = int(m.config.pixel_height)
            self.min_repeat = max(1, round(_POSTER_HOLD * self.fps))
        except Exception as error:
            self.failed = error

    def see(self, frame: object) -> None:
        if self.failed is not None:
            return
        try:
            if frame.repeat >= self.min_repeat:
                self.held = (frame.index, frame.repeat, frame.pixels())
        except Exception as error:
            self.failed = error
            self.held = None

    def write(self, film: object) -> None:
        partial = _POSTER.with_name(_POSTER.name + ".partial")
        try:
            if self.failed is not None:
                return skipped(f"{type(self.failed).__name__}: {self.failed}")
            if self.held is None:
                return skipped("the video never holds still for a poster shot")
            index, repeat, pixels = self.held
            if index + repeat != film.frame_count:
                return skipped("the video doesn't end on a held poster shot")
            if len(pixels) != self.width * self.height * 4:
                return skipped(f"{len(pixels)} bytes for a {self.width}x{self.height} frame")
            if pixels == pixels[:4] * (self.width * self.height):
                return skipped("the closing frame is a single color")
            partial.write_bytes(png(self.width, self.height, pixels))
            os.replace(partial, _POSTER)
            print(f"margin: poster {_POSTER.name} (frame {index})", flush=True)
        except Exception as error:
            skipped(f"{type(error).__name__}: {error}")
            try:
                partial.unlink(missing_ok=True)
            except Exception:
                pass


def capture_poster(scene: type) -> None:
    """Wrap `scene.render` so it also captures the poster, chaining any frame sink the
    command passed (ManimGX's CLI calls `made.render(video, frames=..., plays=...)`)."""
    render = scene.render

    def render_with_poster(self, *args, frames=None, **kwargs):
        poster = Poster()

        def sink(frame):
            poster.see(frame)
            if frames is not None:
                return frames(frame)
            return None

        film = render(self, *args, frames=sink, **kwargs)
        poster.write(film)
        return film

    scene.render = render_with_poster


def attach(namespace: dict[str, object]) -> None:
    """Give the scene file's LessonScene this pass's voice (and, rendering, its poster)."""
    scene = namespace.get("LessonScene")
    if not (isinstance(scene, type) and issubclass(scene, m.Scene)):
        raise TypeError(
            "the scene file must define a class named LessonScene deriving m.Scene"
        )
    if _PASS["mode"] == "collect":
        scene.voice = staticmethod(collect)
    else:
        scene.voice = staticmethod(speak)
        capture_poster(scene)
