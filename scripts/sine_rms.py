# Windowed reduction: RMS of the demo sine wave over the tick window.
#
# Reads one input channel and emits a single reduced value each tick
# (length-1 arrays), stamped at the newest input timestamp. This shows
# the reduction shape: the output rate is the tick rate, not the input
# rate. Widen the history a script sees with `window_s` in [scripts].

import numpy as np
import numba

INPUTS = ["load/ch0"]
OUTPUTS = [{"name": "scripts.ch0_rms", "type": "float", "unit": ""}]


@numba.njit
def compute(ts, vals):
    t = ts[0]        # timestamps of load/ch0 (int64 ns)
    x = vals[0]      # its values over the last window_s seconds (float64)
    n = x.shape[0]
    if n == 0:
        return (t[:0], x[:0])            # nothing in the window yet
    rms = np.sqrt(np.sum(x * x) / n)
    return (t[-1:], np.array([rms]))     # one value at the latest time
