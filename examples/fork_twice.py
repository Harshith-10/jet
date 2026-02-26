import os
import sys

try:
    pid = os.fork()
    if pid == 0:
        print("Child success")
        sys.exit(0)
    else:
        os.wait()
        print("Parent success")
except Exception as e:
    print("Fork failed:", e)
    sys.exit(1)
