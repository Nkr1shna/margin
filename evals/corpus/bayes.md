# Bayesian reasoning

Written for the Margin evaluation corpus. Released under CC0.

## Bayes' theorem

Bayes' theorem relates the probability of a hypothesis H after seeing evidence E to its probability before:

    P(H | E) = P(E | H) · P(H) / P(E)

P(H) is the prior, P(E | H) the likelihood, P(H | E) the posterior, and P(E) the probability of the evidence under all hypotheses together. For a hypothesis and its negation, P(E) = P(E | H) P(H) + P(E | not H) P(not H). The theorem follows in one line from the definition of conditional probability, P(H and E) = P(H | E) P(E) = P(E | H) P(H); what makes it useful is that the likelihood is often easy to know when the posterior is what we want.

## Worked example: a positive test for a rare condition

A condition affects 1 percent of a population. A test detects it in 90 percent of people who have it (its sensitivity) and correctly comes back negative in 95 percent of people who do not (its specificity). Someone tests positive. How likely is it that they have the condition?

Out of 10,000 people, 100 have the condition and 90 of them test positive. Of the 9,900 who do not have it, 5 percent, or 495, also test positive. So 585 people test positive, and only 90 of them have the condition: the probability is 90/585, about 15.4 percent. Most positives are false positives, because the healthy group is so much larger that even a small false-positive rate produces more positives than the true cases do.

Ignoring the prior in a calculation like this is the base-rate fallacy. Counting through a population of whole people, as above, is one of the most reliable ways to avoid it.

## Odds and likelihood ratios

The same update is easier in odds form. The posterior odds equal the prior odds times the likelihood ratio:

    O(H | E) = O(H) · P(E | H) / P(E | not H)

For the test above, the prior odds are 1 to 99 and the likelihood ratio of a positive result is 0.90 / 0.05 = 18, so the posterior odds are 18 to 99, which is the same 15.4 percent. Independent pieces of evidence multiply their likelihood ratios, so a second, independent positive test would multiply the odds by 18 again, to 324 to 99, a probability of about 77 percent.

## Conjugate priors: the beta-binomial model

Estimating a success probability p from coin flips is the textbook continuous case. If the prior on p is a Beta(α, β) distribution and we observe s successes and f failures, the posterior is Beta(α + s, β + f). Because the posterior has the same family as the prior, the beta prior is called conjugate to the binomial likelihood. The parameters behave like pseudo-counts: a Beta(1, 1) prior, which is uniform, acts like having seen one success and one failure before the data. The posterior mean is (α + s) / (α + β + s + f), which moves from the prior mean toward the observed frequency as data accumulates.

## The Monty Hall problem

A prize is behind one of three doors. You pick door 1. The host, who knows where the prize is and always opens a door with no prize, opens door 3. Should you switch to door 2?

Your first pick is right with probability 1/3. If it was right, the host could open either other door; if it was wrong, the host was forced to open the only remaining empty door. Working it through Bayes' theorem: the probability the host opens door 3 is 1/2 if the prize is behind door 1, 1 if it is behind door 2, and 0 if it is behind door 3. The posterior for door 2 is (1 · 1/3) / (1/2 · 1/3 + 1 · 1/3) = 2/3. Switching wins two times in three. The answer depends on the host's rule: a host who opens a door at random and happens to reveal no prize gives no reason to switch.

## Priors and evidence in practice

Strong evidence can overcome a skeptical prior, but weak evidence cannot. A likelihood ratio of 2 barely moves a prior of one in a thousand, while a likelihood ratio of 1,000 brings it to even odds. When reports disagree about whether a surprising result is real, much of the disagreement is usually about the prior, not about the data.
