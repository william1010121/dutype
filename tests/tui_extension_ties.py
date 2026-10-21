#!/usr/bin/env python3
"""Check stable extension ties through real TUI toggles on Linux."""
import argparse
import json
import re
import tempfile
from pathlib import Path

from tui_result_jump import Session


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output
    output.mkdir(parents=True, exist_ok=True)
    expected = ['.mp4', '.pdf', '.rs', '.zip', '(none)', '.é', '.中']
    with tempfile.TemporaryDirectory(prefix='dutype-ext-ties-') as tmp:
        root = Path(tmp) / 'tree'
        root.mkdir()
        for extension in ['zip', 'rs', 'pdf', 'mp4']:
            (root / ('f.' + extension)).write_bytes(b'x')
        for name in ['empty', 'f.é', 'f.中']:
            (root / name).touch()
        session = Session(args.binary.resolve(), root, output, 'extension-order', ('--clones',))
        session.key(b'm', 'extensions')
        orders = []
        modes = []
        assert 'dedup' not in session.screen.text().splitlines()[0]
        for index in range(12):
            # Only the upper-right breakdown pane, not the file detail pane.
            right = '\n'.join(line[81:] for line in session.screen.text().splitlines())
            right = right[right.index('Category │ Extension'):].split('╰', 1)[0]
            order = re.findall(r'\.(?:mp4|pdf|rs|zip|é|中)(?=\s)|\(none\)', right)
            assert order == expected, (order, right)
            orders.append(order)
            session.key(b'mm', f'toggle-{index}')
            screen = session.key(b'c', f'size-{index}')
            dedup = index % 2 == 0
            assert ('dedup' in screen.splitlines()[0]) == dedup, screen
            assert 'restart with --clones' not in screen, screen
            modes.append('dedup' if dedup else 'du')
        result = session.finish()
        result['orders'] = orders
        result['verified_size_modes'] = modes
    (output / 'summary.json').write_text(json.dumps(result, indent=2))
    print('12 mode/size toggles stable; Unicode and zero sizes covered; terminal restored')


if __name__ == '__main__':
    main()
