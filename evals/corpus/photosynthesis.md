# Photosynthesis

Written for the Margin evaluation corpus. Released under CC0.

Photosynthesis converts the energy of light into chemical energy stored in sugars. In plants and algae it takes place in chloroplasts and has two coupled stages: the light-dependent reactions in the thylakoid membranes, and the Calvin cycle in the stroma that surrounds them. The overall reaction is 6 CO2 + 6 H2O + light → C6H12O6 + 6 O2, but that summary hides where each atom goes: the oxygen released comes from water, not from carbon dioxide, which was shown with heavy-oxygen isotope labelling.

## The light-dependent reactions

### Photosystem II and the splitting of water

Light absorbed by the antenna pigments of photosystem II is funnelled to its reaction-centre chlorophyll, P680, which gives up an excited electron to the electron transport chain. P680+ is the strongest biological oxidant known, strong enough to take electrons from water. The oxygen-evolving complex, a cluster of four manganese ions and one calcium ion, splits two water molecules into four protons, four electrons and one molecule of O2 for every four photons it absorbs.

### The electron transport chain and photosystem I

Electrons leaving photosystem II pass through plastoquinone, the cytochrome b6f complex and plastocyanin to photosystem I. As plastoquinone shuttles electrons to cytochrome b6f, protons are pumped from the stroma into the thylakoid lumen. Photosystem I, whose reaction centre is P700, absorbs a second photon and lifts the electron high enough to reduce ferredoxin, and ferredoxin-NADP+ reductase uses two such electrons to make NADPH. This path, with water at one end and NADPH at the other, is called linear or non-cyclic electron flow; drawn on an energy axis it is the Z-scheme.

### Making ATP by chemiosmosis

The splitting of water and the pumping by cytochrome b6f both add protons to the lumen, so a proton gradient builds up across the thylakoid membrane. ATP synthase lets protons flow back into the stroma and uses that flow to turn ADP and phosphate into ATP. Peter Mitchell proposed this chemiosmotic mechanism in 1961; it is the same mechanism mitochondria use, run in a different membrane.

Linear flow makes somewhat less than 3 ATP for every 2 NADPH, about 2.6 by most estimates, but the Calvin cycle needs 3 ATP for every 2 NADPH, and other processes in the chloroplast need extra ATP. Cyclic electron flow closes the gap: electrons from photosystem I return to the cytochrome b6f complex instead of reducing NADP+, pumping more protons and making ATP without making NADPH or releasing oxygen.

## The Calvin cycle

The Calvin cycle fixes carbon dioxide into sugar using the ATP and NADPH made in the light. It has three phases.

1. **Carbon fixation.** The enzyme RuBisCO attaches CO2 to ribulose-1,5-bisphosphate (RuBP), a five-carbon sugar. The unstable six-carbon product immediately splits into two molecules of 3-phosphoglycerate.
2. **Reduction.** Each 3-phosphoglycerate is phosphorylated by ATP and reduced by NADPH to glyceraldehyde-3-phosphate (G3P).
3. **Regeneration.** Most of the G3P is rearranged, using more ATP, back into RuBP so the cycle can continue.

To export one G3P molecule, the cycle must fix three CO2 molecules, and it spends 9 ATP and 6 NADPH doing so. Two G3P molecules make one glucose, so a glucose costs 18 ATP and 12 NADPH.

RuBisCO is slow, catalysing only a few reactions per second, which is why it makes up a large share of the protein in leaves and is often called the most abundant protein on Earth.

## Photorespiration

RuBisCO also reacts with oxygen. When it does, it produces one 3-phosphoglycerate and one two-carbon compound, 2-phosphoglycolate, which the plant must recycle through a pathway spanning the chloroplast, the peroxisome and the mitochondrion. That pathway releases previously fixed CO2 and consumes ATP, so photorespiration wastes both carbon and energy. The oxygenation reaction becomes more frequent as temperature rises, because the enzyme's preference for CO2 over O2 falls and because CO2 dissolves less well than O2 in warm water. Hot, dry conditions make it worse: closing stomata to save water lets CO2 inside the leaf run down while O2 from the light reactions builds up.

## C4 and CAM plants

C4 plants such as maize and sugarcane separate carbon capture from the Calvin cycle in space. In the mesophyll cells, PEP carboxylase, which does not react with oxygen, fixes CO2 into a four-carbon acid. The acid moves into bundle-sheath cells arranged around the veins, where it releases CO2 at a high concentration around RuBisCO, nearly eliminating photorespiration. The price is extra ATP to regenerate the PEP acceptor, so C4 photosynthesis pays off in hot, bright climates and not in cool shade.

CAM plants such as cacti and pineapple separate the same two steps in time instead. They open their stomata at night, when it is cool and less water is lost, fix CO2 into malic acid and store it in the vacuole. During the day the stomata stay closed and the stored acid releases CO2 to the Calvin cycle. CAM saves water at the cost of slow growth.

| Pathway | First stable product | Separation of steps | Example |
|---|---|---|---|
| C3 | 3-phosphoglycerate | none | wheat, rice |
| C4 | oxaloacetate (four carbons) | in space, two cell types | maize, sugarcane |
| CAM | oxaloacetate, stored as malate | in time, night and day | pineapple, cacti |
