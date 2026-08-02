# Two-input transform: element-wise difference of two channels (a - b).
#
# A multi-input script. INPUTS lists two channels, so `compute` receives
# ts/vals tuples of length two, and the panel shows one channel picker per
# input (bind each independently). Both inputs are read over the same window
# and paired by position — the demo/load channels are generated together, so
# they align. The output name templates from both inputs' stems, e.g. binding
# load/ch0 and load/ch7 publishes scripts.ch0_minus_ch7.

import numba

INPUTS = ["load/ch0", "load/ch1"]
OUTPUTS = [{"name": "scripts.{in0.stem}_minus_{in1.stem}", "type": "float", "unit": ""}]


@numba.njit
def compute(ts, vals):
    t0 = ts[0]       # timestamps of the first input (int64 ns)
    a = vals[0]      # first input's values over the window (float64)
    b = vals[1]      # second input's values
    n = min(a.shape[0], b.shape[0])
    if n == 0:
        return (t0[:0], a[:0])        # nothing to pair yet
    return (t0[:n], a[:n] - b[:n])    # element-wise a - b at ch0's timestamps
