"""Write a small SYNTHETIC-only CLI demo; all paths point to test peers, not Gaia."""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import write_json
from tests_support import synthetic_config

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', type=Path, default=Path(__file__).resolve().parents[1]/'synthetic.local.json')
args = parser.parse_args()
config = synthetic_config(args.output.resolve().parent)
config['output_root'] = str(args.output.resolve().parent/'synthetic-results')
write_json(args.output, config)
print(args.output.resolve())
