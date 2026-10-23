#!/usr/bin/env python3
"""Compatibility wrapper around auto_level2.py so both entrypoints stay in sync."""
from auto_level2 import *  # noqa: F401,F403
from auto_level2 import main

if __name__ == "__main__":
    main()
