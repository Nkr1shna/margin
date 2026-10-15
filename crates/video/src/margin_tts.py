#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.10,<3.15"
# dependencies = ["pocket-tts==3.3.0"]
#
# # Linux: the CPU build of PyTorch, not the CUDA one (pocket-tts README,
# # "CPU-only installation"). macOS wheels are CPU-only already.
# [[tool.uv.index]]
# name = "pytorch-cpu"
# url = "https://download.pytorch.org/whl/cpu"
# explicit = true
#
# [tool.uv.sources]
# torch = [{ index = "pytorch-cpu", marker = "sys_platform == 'linux'" }]
# ///
"""margin-tts: one narration line to a WAV with Kyutai's Pocket TTS, on the CPU.

Margin's lesson renderer runs this on the Linux service (docs/plan-linux-service.md
§2.7):

    margin-tts --text "A line." --output line.wav [--voice alba] [--language english]

It is a uv script: installed by copying it to `~/.local/bin/margin-tts` (executable);
the first run makes its environment and downloads the model from Hugging Face
(`kyutai/pocket-tts-without-voice-cloning`, CC-BY-4.0, no sign-in) into the Hugging Face
cache. Writes 16-bit PCM mono WAV at the model's rate (24 kHz) to `--output`, through a
temporary name, so a failed run leaves nothing there. Exit status 0 only when audio was
written; errors go to stderr.

Licences: pocket-tts is MIT; the weights are CC-BY-4.0 (credit Kyutai). Built-in voices
carry their recordings' licences (huggingface.co/kyutai/tts-voices): alba, the VCTK
voices (anna, azelma, charles, eponine, eve, fantine, george, jane, mary, michael, paul,
vera) are CC-BY-4.0, the Voice-Zero and donated voices (bill_boerst, caro_davy,
peter_yearsley, stuart_bell, javert, marius) CC0; cosette (Expresso) and jean (EARS) are
non-commercial only.

`margin-tts --version` prints "margin-tts <script version> (pocket-tts <version>)".
"""

import argparse
import array
import os
import pathlib
import sys
import wave

SCRIPT_VERSION = "1"
DEFAULT_VOICE = "alba"


def parse_args(argv):
    parser = argparse.ArgumentParser(prog="margin-tts", description=__doc__.splitlines()[0])
    parser.add_argument("--text", help="the line to say")
    parser.add_argument("--output", help="the WAV file to write")
    parser.add_argument("--voice", default=DEFAULT_VOICE, help=f"a built-in voice (default {DEFAULT_VOICE})")
    parser.add_argument("--language", default=None, help="a pocket-tts language model (default: english)")
    parser.add_argument("--version", action="store_true", help="print the version and exit")
    args = parser.parse_args(argv)
    if not args.version and (args.text is None or args.output is None):
        parser.error("--text and --output are required")
    return args


def pcm16(audio):
    """Samples in [-1, 1] (a torch tensor or any sequence) as little-endian 16-bit PCM."""
    if hasattr(audio, "detach"):
        audio = audio.detach().cpu().reshape(-1).tolist()
    pcm = array.array("h", (int(round(max(-1.0, min(1.0, float(x))) * 32767.0)) for x in audio))
    if sys.byteorder != "little":
        pcm.byteswap()
    return pcm.tobytes()


def write_wav(path, rate, pcm):
    path = pathlib.Path(path)
    partial = path.with_name(path.name + ".partial")
    with wave.open(str(partial), "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(pcm)
    os.replace(partial, path)


def main(argv):
    args = parse_args(argv)
    if args.version:
        from importlib.metadata import version

        print(f"margin-tts {SCRIPT_VERSION} (pocket-tts {version('pocket-tts')})")
        return 0
    text = args.text.strip()
    if not text:
        print("margin-tts: nothing to say", file=sys.stderr)
        return 2

    from pocket_tts import TTSModel

    model = TTSModel.load_model(language=args.language) if args.language else TTSModel.load_model()
    state = model.get_state_for_audio_prompt(args.voice)
    audio = model.generate_audio(state, text)
    pcm = pcm16(audio)
    if not pcm:
        print(f"margin-tts: no audio for {text!r}", file=sys.stderr)
        return 1
    write_wav(args.output, model.sample_rate, pcm)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
