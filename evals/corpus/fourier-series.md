# Fourier series

Written for the Margin evaluation corpus. Released under CC0.

## Periodic signals

A signal is periodic with period T when f(t + T) = f(t) for every t. The smallest such T is the fundamental period, and its reciprocal f0 = 1/T is the fundamental frequency. The angular fundamental frequency is ω0 = 2π/T. A Fourier series writes a periodic signal as a sum of sinusoids whose frequencies are integer multiples of the fundamental frequency; these multiples are called harmonics.

The idea is older than the theory that justifies it. Joseph Fourier used trigonometric series in 1807 to solve the heat equation on a bounded rod, and the claim that an arbitrary function could be expanded this way was controversial for decades. The modern statement needs conditions on the function, which the section on convergence gives.

## Computing the coefficients

For a signal with period T, the trigonometric form of the series is

    f(t) = a0/2 + Σ (an cos(n ω0 t) + bn sin(n ω0 t)),  n = 1, 2, 3, …

and the coefficients are found by projecting the signal onto each sinusoid:

    an = (2/T) ∫ f(t) cos(n ω0 t) dt   over one period
    bn = (2/T) ∫ f(t) sin(n ω0 t) dt   over one period

The projection works because sines and cosines of different harmonics are orthogonal over one period: the integral of their product is zero unless the two frequencies are equal. Multiplying the series by cos(m ω0 t) and integrating therefore kills every term except the one with n = m.

The complex exponential form is often easier to manipulate. With cn = (1/T) ∫ f(t) e^(−i n ω0 t) dt, the series is f(t) = Σ cn e^(i n ω0 t) over all integers n. For a real signal the coefficients satisfy c(−n) = conj(cn), and the two forms are related by an = 2 Re(cn) and bn = −2 Im(cn).

### Symmetry shortcuts

Symmetry removes half the work before any integral is evaluated. An even signal, f(−t) = f(t), has every bn equal to zero, so its series contains only cosines. An odd signal, f(−t) = −f(t), has every an equal to zero, including a0, so its series contains only sines. A signal with half-wave symmetry, f(t + T/2) = −f(t), contains only odd harmonics.

## Worked example: the square wave

Take the square wave that equals +1 for 0 < t < T/2 and −1 for T/2 < t < T. It is odd, so only sine terms survive, and it has half-wave symmetry, so only odd harmonics survive. Evaluating bn gives bn = 4/(nπ) for odd n and zero for even n. The series is

    f(t) = (4/π) (sin ω0 t + sin 3ω0 t / 3 + sin 5ω0 t / 5 + …)

The amplitude of the nth harmonic falls off as 1/n. That slow decay is the signature of a discontinuity: a signal whose first derivative is discontinuous but which is itself continuous, such as the triangle wave, has harmonics that fall off as 1/n², and smoother signals decay faster still.

## Convergence and the Gibbs phenomenon

Dirichlet's conditions are sufficient for the series to converge: the signal must be absolutely integrable over a period, have a finite number of maxima and minima in a period, and have a finite number of finite discontinuities in a period. Where the signal is continuous, the series converges to the signal's value. At a jump discontinuity the series converges to the midpoint of the jump.

Near a jump, the partial sums overshoot. Adding more terms does not shrink the overshoot; it only squeezes it closer to the discontinuity. For a jump of height 2, as in the square wave above, the peak of the partial sum approaches about 1.179, an overshoot of roughly 9 percent of the jump. This persistent overshoot is the Gibbs phenomenon. It is why truncating a Fourier series to filter a signal produces ringing near sharp edges, and why window functions are used to taper the coefficients instead of cutting them off abruptly.

## Parseval's theorem

Parseval's theorem says the average power of a periodic signal equals the sum of the powers in its harmonics: (1/T) ∫ |f(t)|² dt = Σ |cn|². Power is conserved between the time domain and the frequency domain. For the square wave of amplitude 1 the average power is 1, so the squared harmonic amplitudes must sum to 1; checking this numerically is a good test of a coefficient calculation.

A practical consequence: because the power in the square wave's harmonics falls as 1/n², the first three nonzero harmonics already carry about 93 percent of the total power.

## Common series

| Signal (period 2π) | Series |
|---|---|
| Square wave, ±1 | (4/π) Σ sin(nt)/n, n odd |
| Sawtooth, t on (−π, π) | 2 Σ (−1)^(n+1) sin(nt)/n |
| Triangle wave, peak 1 | (8/π²) Σ (−1)^((n−1)/2) sin(nt)/n², n odd |
| Full-wave rectified sine | 2/π − (4/π) Σ cos(2nt)/(4n² − 1) |

## Computing coefficients numerically

When a signal is only known from samples, the integrals become sums. With N equally spaced samples over one period, the discrete Fourier transform gives the coefficients up to scaling, and the fast Fourier transform computes all of them in O(N log N) operations.

```python
import numpy as np

def fourier_coefficients(samples):
    """Complex Fourier coefficients c_n of one period of a sampled signal."""
    n = len(samples)
    return np.fft.fft(samples) / n
```

Sampling imposes its own limit: harmonics above half the sampling rate fold back onto lower frequencies, which is aliasing. A signal must be band-limited, or filtered, before it is sampled.
