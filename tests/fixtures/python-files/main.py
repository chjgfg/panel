import os
import sys

if "PANEL_EXPECT_ARGUMENT" in os.environ:
    assert sys.argv[1:] == [os.environ["PANEL_EXPECT_ARGUMENT"]], repr(sys.argv)
print("PYTHON_FILE_OK", flush=True)
if "PANEL_PROBE_SLEEP" in os.environ:
    import time
    time.sleep(30)
