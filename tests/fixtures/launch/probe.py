import json
import os
import sys


def main():
    print(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd(), "entry": sys.argv[0]}))
