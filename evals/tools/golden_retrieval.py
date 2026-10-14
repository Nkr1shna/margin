#!/usr/bin/env python3
"""The hand-written golden retrieval queries (the first 60 of evals/golden/retrieval.jsonl).

Each query is a question a learner would actually need answered from the corpus:
applications, comparisons, derivations and "why" questions, not recall of one sentence.
A hit is relevant when its text contains an anchor (whitespace-normalised, case-
insensitive) or, for a picture with no anchor, when it is the anchored picture.
Negative queries have no answer in the corpus. Run from the repository root:

    python3 evals/tools/golden_retrieval.py > evals/golden/retrieval.jsonl
"""
import json

F = "fourier-series.md"
P = "photosynthesis.md"
H = "heat-engines.rst"
S = "sorting.html"
B = "bayes.md"
T = "tcp-congestion.md"
RC = "pdf/rc-circuits.pdf"
GL = "pdf/glacier-scan.pdf"
LN = "pdf/lens-optics.pdf"
CARNOT = "images/carnot-pv-diagram.png"
CALVIN = "images/calvin-cycle.png"
BOAT = "images/scene-sailboat-sunset.png"
BIKE = "images/scene-red-bicycle.png"
RAYS = "folder/figures/lens-ray-diagram.png"
WEEK1 = "folder/notes/week1.md"


def r(document, anchor=None, page=None):
    out = {"document": document}
    if page is not None:
        out["page"] = page
    if anchor is not None:
        out["anchor"] = anchor
    return out


Q = [
    # ---- semantic: the words of the question are not the words of the answer ----
    ("semantic", "Why does truncating a Fourier series produce ringing next to a sharp edge, and why doesn't adding more terms make it go away?",
     [r(F, "Adding more terms does not shrink the overshoot")]),
    ("semantic", "How can I tell before integrating that a waveform's series will contain only sine terms?",
     [r(F, "has every an equal to zero, including a0, so its series contains only sines")]),
    ("semantic", "Which smoothness property of a signal decides how quickly the amplitudes of its harmonics shrink?",
     [r(F, "The amplitude of the nth harmonic falls off as 1/n")]),
    ("semantic", "Where does the oxygen given off by a leaf actually come from, and how do we know?",
     [r(P, "the oxygen released comes from water, not from carbon dioxide")]),
    ("semantic", "How does a plant make extra ATP when it needs more than the NADPH-producing pathway supplies?",
     [r(P, "Cyclic electron flow closes the gap")]),
    ("semantic", "Why do hot, dry days make the carbon-fixing enzyme waste more of a plant's energy?",
     [r(P, "closing stomata to save water lets CO2 inside the leaf run down"),
      r(P, "The oxygenation reaction becomes more frequent as temperature rises")]),
    ("semantic", "Compare how maize and cacti avoid losing carbon to the oxygen side-reaction.",
     [r(P, "C4 plants such as maize and sugarcane separate carbon capture from the Calvin cycle in space"),
      r(P, "CAM plants such as cacti and pineapple separate the same two steps in time instead")]),
    ("semantic", "What stops an engineer from building a power plant that turns all of its fuel's heat into electricity?",
     [r(H, "No cyclic device can take heat from a single reservoir and convert all of it into work")]),
    ("semantic", "Why can't petrol engines simply use a much higher compression ratio to gain efficiency?",
     [r(H, "makes it ignite before the spark, which is knock")]),
    ("semantic", "Why is heating a house with a heat pump better than with an electric resistance heater?",
     [r(H, "a heat pump always delivers more heat than the electrical work it consumes")]),
    ("semantic", "Where exactly is efficiency lost inside a real engine compared with the ideal reversible one?",
     [r(H, "the excess is entropy generated inside the engine, and it is exactly what costs efficiency"),
      r(H, "Real engines fall short of the Carnot limit because of friction")]),
    ("semantic", "Why is there a floor on how few comparisons any general-purpose sorting routine can use?",
     [r(S, "has height at least log2(n!)")]),
    ("semantic", "Which sorting approach should I use for a dataset far larger than RAM, and why?",
     [r(S, "external sorting of data too large for memory")]),
    ("semantic", "How do standard libraries keep quicksort's speed but guarantee it never degrades badly?",
     [r(S, "Introsort, used by many standard libraries, starts as quicksort and switches to heapsort")]),
    ("semantic", "How do I sort a spreadsheet by surname and then by first name using one-key sorts?",
     [r(S, "sort by first name and then stably by last name")]),
    ("semantic", "A screening test comes back positive for something rare; why might it still be unlikely that the person has it?",
     [r(B, "Most positives are false positives")]),
    ("semantic", "How should a second, independent positive result change my estimate?",
     [r(B, "a second, independent positive test would multiply the odds by 18 again")]),
    ("semantic", "In the three-door game show, why does the host's knowledge matter for whether to switch?",
     [r(B, "The answer depends on the host's rule")]),
    ("semantic", "Why does TCP treat a timeout much more harshly than three repeated acknowledgements?",
     [r(T, "A retransmission timeout, by contrast, is treated as a sign of heavy congestion")]),
    ("semantic", "What makes two TCP connections sharing a link end up with similar throughput?",
     [r(T, "move closer to equal rates with each cycle")]),
    ("semantic", "Why do oversized router buffers make a video call laggy?",
     [r(T, "adding hundreds of milliseconds of queueing delay to every packet that shares it")]),
    ("semantic", "Why is a capacitor considered fully charged after a few multiples of RC rather than never?",
     [r(RC, "After five time constants it has reached 99.3 percent", page=1)]),

    # ---- lexical: exact terms, symbols, numbers ----
    ("lexical", "Gibbs phenomenon overshoot percentage", [r(F, "an overshoot of roughly 9 percent of the jump")]),
    ("lexical", "Parseval's theorem average power harmonics", [r(F, "Parseval's theorem says the average power of a periodic signal equals the sum of the powers in its harmonics")]),
    ("lexical", "Dirichlet conditions for convergence", [r(F, "Dirichlet's conditions are sufficient for the series to converge")]),
    ("lexical", "oxygen-evolving complex manganese calcium cluster", [r(P, "a cluster of four manganese ions and one calcium ion")]),
    ("lexical", "ATP and NADPH cost of one G3P in the Calvin cycle", [r(P, "it spends 9 ATP and 6 NADPH doing so")]),
    ("lexical", "2-phosphoglycolate recycling pathway", [r(P, "2-phosphoglycolate, which the plant must recycle")]),
    ("lexical", "Carnot efficiency between 500 K and 300 K", [r(H, "can therefore never exceed 40 percent efficiency")]),
    ("lexical", "Otto cycle efficiency r = 10 gamma 1.4", [r(H, "With r = 10 and gamma = 1.4 the ideal efficiency is about 60 percent")]),
    ("lexical", "Lomuto versus Hoare partition swaps", [r(S, "it does about three times fewer swaps on average")]),
    ("lexical", "counting sort O(n + k)", [r(S, "Counting sort sorts n integers drawn from a range of size k in O(n + k) time")]),
    ("lexical", "Beta(α + s, β + f) posterior", [r(B, "the posterior is Beta(α + s, β + f)")]),
    ("lexical", "likelihood ratio 0.90 / 0.05 = 18", [r(B, "the likelihood ratio of a positive result is 0.90 / 0.05 = 18")]),
    ("lexical", "fast retransmit three duplicate acknowledgements", [r(T, "When the sender sees three duplicate acknowledgements")]),
    ("lexical", "CoDel active queue management", [r(T, "Active queue management schemes such as CoDel")]),
    ("lexical", "RC low-pass cutoff frequency formula f_c", [r(RC, "f_c = 1 / (2 pi R C)", page=3)]),
    ("lexical", "half-life of a discharging capacitor RC ln 2", [r(RC, "The time to fall to half is RC ln 2", page=2)]),

    # ---- cross-modal: answered by a picture, a scanned page, or a figure ----
    ("cross-modal", "Show me the pressure-volume diagram of the Carnot cycle with its four strokes.",
     [r(CARNOT)]),
    ("cross-modal", "Which part of the PV diagram is the isothermal compression at the cold temperature?",
     [r(CARNOT, "isothermal compression at T_cold")]),
    ("cross-modal", "Diagram of the three phases of the Calvin cycle", [r(CALVIN)]),
    ("cross-modal", "In the cycle figure, what does RuBisCO do and what comes out?",
     [r(CALVIN, "CO2 fixation by RuBisCO"), r(P, "The enzyme RuBisCO attaches CO2 to ribulose-1,5-bisphosphate")]),
    ("cross-modal", "a sailing boat silhouetted against an orange sunset over water", [r(BOAT)]),
    ("cross-modal", "photo of a red bike leaning on a brick wall", [r(BIKE)]),
    ("cross-modal", "ray diagram showing where the image forms for an object 30 cm from a 10 cm lens",
     [r(LN, page=2), r(RAYS)]),
    ("cross-modal", "What is the difference between the accumulation zone and the ablation zone of a glacier?",
     [r(GL, "The lower part, where melting and sublimation exceed snowfall, is the ablation zone", page=1)]),
    ("cross-modal", "Why does the equilibrium line altitude tell us about climate change?",
     [r(GL, "the equilibrium line altitude rises and the glacier shrinks", page=1)]),
    ("cross-modal", "How can meltwater make a glacier suddenly speed up?",
     [r(GL, "Basal sliding happens when meltwater at the bed lubricates the contact", page=1)]),
    ("cross-modal", "Is the image upright or inverted when an object is three focal lengths from a converging lens, and how big is it?",
     [r(LN, "forms a real, inverted image 15 cm behind the lens, half the size of the object", page=1),
      r(RAYS, "real inverted image")]),
    ("cross-modal", "How does a magnifying glass make things look bigger?",
     [r(LN, "the image is virtual, upright and magnified", page=1)]),

    # ---- negative: nothing in the corpus answers these ----
    ("negative", "What is the time complexity of Dijkstra's algorithm with a Fibonacci heap?", []),
    ("negative", "How does the Krebs cycle produce GTP in mitochondria?", []),
    ("negative", "Derive the Fourier transform of a Gaussian pulse.", []),
    ("negative", "What is the efficiency of the Rankine cycle with superheating?", []),
    ("negative", "How does QUIC handle connection migration between networks?", []),
    ("negative", "Explain Markov chain Monte Carlo and the Metropolis-Hastings acceptance rule.", []),
    ("negative", "What causes the Coriolis deflection of ocean currents?", []),
    ("negative", "How do inductors store energy in an RL circuit?", []),
    ("negative", "Which sorting algorithm does Python's sorted() use for strings with a custom key?", []),
    ("negative", "What is the role of chlorophyll b compared to carotenoids in shade leaves?", []),
]

assert len(Q) == 60, len(Q)
for i, (kind, query, relevant) in enumerate(Q, 1):
    scope = {"scope": "folder"} if any(x["document"].startswith("folder/") for x in relevant) and all(
        x["document"].startswith("folder/") for x in relevant) else {"scope": "corpus"}
    print(json.dumps({
        "id": f"r{i:03}",
        "query": query,
        "scope": scope,
        "relevant": relevant,
        "kind": kind,
        "generated_by": "hand",
    }, ensure_ascii=False))
