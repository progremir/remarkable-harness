#!/usr/bin/env python3
"""The helper's old name. A tablet that still has an earlier build of the
client starts this file; it runs helper.py."""
import os
import runpy

runpy.run_path(os.path.join(os.path.dirname(os.path.abspath(__file__)), "helper.py"), run_name="__main__")
