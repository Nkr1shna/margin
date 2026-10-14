#!/usr/bin/env python3
"""The golden course requests and agent tasks over the eval corpus.

    python3 evals/tools/golden_courses.py evals/golden

writes evals/golden/courses.jsonl (8 course requests: prompt, documents, the outline
themes a good course covers, and per lesson the documents it must cite) and
evals/golden/agent.jsonl (10 chat questions for the agent suite: scope, the documents
an answer must cite, anchors that a good answer's cited passages contain; the last
has no answer in the corpus and must be answered as such). Written by hand, serious:
application and comparison, not recall of a sentence.
"""

import json
import os
import sys

COURSES = [
    dict(id="c01", prompt="Teach me Fourier series well enough to predict the spectrum of a square wave and explain the Gibbs overshoot to a colleague.",
         documents=["fourier-series.md"],
         themes=["Periodic signals and harmonics", "Computing coefficients and symmetry shortcuts", "The square wave worked through", "Convergence and the Gibbs phenomenon", "Parseval's theorem and energy in harmonics"],
         must_cite=[["fourier-series.md"]] * 5),
    dict(id="c02", prompt="A course on photosynthesis for a first-year biology student who needs to explain where the oxygen and the sugar come from.",
         documents=["photosynthesis.md", "images/calvin-cycle.png"],
         themes=["The light-dependent reactions", "Chemiosmosis and ATP synthase", "The Calvin cycle and its stoichiometry", "Photorespiration and the cost of RuBisCO", "C4 and CAM strategies"],
         must_cite=[["photosynthesis.md"], ["photosynthesis.md"], ["photosynthesis.md", "images/calvin-cycle.png"], ["photosynthesis.md"], ["photosynthesis.md"]]),
    dict(id="c03", prompt="I am an engineering student: teach heat engines from the first law to why real engines fall short of Carnot.",
         documents=["heat-engines.rst", "images/carnot-pv-diagram.png"],
         themes=["Efficiency and the first law", "The second law and its two statements", "The Carnot cycle on a PV diagram", "Entropy as the bookkeeping of irreversibility", "The Otto cycle and compression ratio", "Refrigerators and heat pumps"],
         must_cite=[["heat-engines.rst"], ["heat-engines.rst"], ["heat-engines.rst", "images/carnot-pv-diagram.png"], ["heat-engines.rst"], ["heat-engines.rst"], ["heat-engines.rst"]]),
    dict(id="c04", prompt="Help me reason with Bayes' theorem in medicine and everyday decisions, with numbers, not slogans.",
         documents=["bayes.md"],
         themes=["Bayes' theorem from conditional probability", "Base rates and a positive test for a rare condition", "Odds and likelihood ratios", "Conjugate priors with the beta-binomial model", "The Monty Hall problem and choosing priors"],
         must_cite=[["bayes.md"]] * 5),
    dict(id="c05", prompt="Explain how TCP decides how fast to send, ending with why modern algorithms such as BBR exist.",
         documents=["tcp-congestion.md"],
         themes=["Why senders must slow down", "Slow start", "AIMD congestion avoidance", "Fast retransmit and fast recovery", "CUBIC, BBR and bufferbloat"],
         must_cite=[["tcp-congestion.md"]] * 5),
    dict(id="c06", prompt="Compare the classic comparison sorts and prove why none can beat n log n comparisons.",
         documents=["sorting.html"],
         themes=["The decision-tree lower bound", "Merge sort", "Quicksort and its worst case", "Heapsort", "Stability and choosing a sort"],
         must_cite=[["sorting.html"]] * 5),
    dict(id="c07", prompt="A short course on RC circuits and thin lenses from my lab PDFs, enough to solve the problem sheets.",
         documents=["pdf/rc-circuits.pdf", "pdf/lens-optics.pdf"],
         themes=["Charging and discharging a capacitor", "The time constant in practice", "The thin lens equation and magnification", "Ray diagrams for real and virtual images"],
         must_cite=[["pdf/rc-circuits.pdf"], ["pdf/rc-circuits.pdf"], ["pdf/lens-optics.pdf"], ["pdf/lens-optics.pdf"]]),
    dict(id="c08", prompt="Teach me how glaciers gain and lose mass and move, using my scanned field notes.",
         documents=["pdf/glacier-scan.pdf"],
         themes=["Accumulation, ablation and the equilibrium line", "Mass balance and climate", "How glaciers flow: deformation and basal sliding"],
         must_cite=[["pdf/glacier-scan.pdf"]] * 3),
]

AGENT = [
    dict(id="a01", question="My square wave's partial sums overshoot near the jumps. Will adding more terms fix it, and how big is the overshoot?",
         scope={"scope": "corpus"}, must_cite=["fourier-series.md"], anchors=["Adding more terms does not shrink the overshoot"]),
    dict(id="a02", question="A test for a condition with 1% prevalence has 90% sensitivity and a 5% false-positive rate. Using my notes, how likely is the condition after a positive result, and why is it so low?",
         scope={"scope": "corpus"}, must_cite=["bayes.md"], anchors=[]),
    dict(id="a03", question="Why does an engine running between 600 K and 300 K never reach 100% efficiency, and what is its ceiling?",
         scope={"scope": "corpus"}, must_cite=["heat-engines.rst"], anchors=["No cyclic device can take heat from a single reservoir and convert all of it into work"]),
    dict(id="a04", question="Compare how maize and cacti avoid photorespiration, and say what each strategy costs the plant.",
         scope={"scope": "corpus"}, must_cite=["photosynthesis.md"], anchors=["C4 plants such as maize and sugarcane separate carbon capture from the Calvin cycle in space"]),
    dict(id="a05", question="After three duplicate ACKs, what does a Reno sender do to its window, and how does that differ from a timeout?",
         scope={"scope": "corpus"}, must_cite=["tcp-congestion.md"], anchors=[]),
    dict(id="a06", question="Why is quicksort usually faster than heapsort in practice even though heapsort has the better worst case?",
         scope={"scope": "corpus"}, must_cite=["sorting.html"], anchors=[]),
    dict(id="a07", question="In the folder I attached, what is the time constant of the RC circuit in the paper, and how long until the capacitor is about 99% charged?",
         scope={"scope": "folder"}, must_cite=["folder/papers/rc-circuits.pdf"], anchors=[]),
    dict(id="a08", question="Using the lens figure and notes, where does the image form for an object 30 cm from a 10 cm converging lens, and is it upright?",
         scope={"scope": "corpus"}, must_cite=["pdf/lens-optics.pdf"], anchors=["forms a real, inverted image 15 cm behind the lens, half the size of the object"]),
    dict(id="a09", question="What would happen to a glacier's equilibrium line if summers warmed, and what would the glacier do?",
         scope={"scope": "corpus"}, must_cite=["pdf/glacier-scan.pdf"], anchors=["the equilibrium line altitude rises and the glacier shrinks"]),
    dict(id="a10", question="Do my sources say anything about how CRISPR guide RNAs are designed?",
         scope={"scope": "corpus"}, must_cite=[], anchors=[]),
]


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "evals/golden"
    for name, rows in (("courses.jsonl", COURSES), ("agent.jsonl", AGENT)):
        with open(os.path.join(out, name), "w") as f:
            for row in rows:
                f.write(json.dumps(row) + "\n")


if __name__ == "__main__":
    main()
