#!/usr/bin/env python3
"""Entry point for the `wave_integrate` script node; the gate lives in gates.py."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gates import run  # noqa: E402

run("wave_integrate")
